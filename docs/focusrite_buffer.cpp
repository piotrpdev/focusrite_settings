// focusrite_buffer.cpp
//
// Proof of concept: change the streaming buffer size and/or sample rate of a
// Focusrite interface by talking to the FocusriteUsb kernel driver directly -
// no FocusritePal64.dll, no C++ ABI games.
//
// Build (x64 Native Tools Command Prompt for VS2019+):
//     cl /nologo /EHsc /W4 focusrite_buffer.cpp
// Run:
//     focusrite_buffer.exe 256                      (buffer size)
//     focusrite_buffer.exe buffer 256
//     focusrite_buffer.exe rate 48000
//     focusrite_buffer.exe rate 48000 buffer 256
//
// Prerequisites:
//   - Focusrite driver installed and the interface plugged in.
//   - Close "Focusrite Notifier", "Focusrite Control" and anything else that
//     uses Pal: it opens this device handle with share mode 0, so a second open
//     fails with ERROR_SHARING_VIOLATION (32).
//
// Where the magic numbers come from (see FocusritePal64.dll.md):
//   Pal's DiceRootDriverWin opens the device with CreateFileA(..., GENERIC_READ|
//   GENERIC_WRITE, share 0, OPEN_EXISTING, FILE_FLAG_OVERLAPPED|NORMAL), then:
//     - DeviceIoControl(0x222000)                  -> driver API version
//     - command {section 0x400, id 1}              -> attach/init
//     - command {section 0x400, id 4}              -> read 0x58-byte config blob
//          blob + 0x0C = sample rate
//          blob + 0x14 = buffer size (samples)
//     - command {section 0x400, id 5, u32 hz}      -> set sample rate
//     - command {section 0x400, id 9, u32 value}   -> set buffer size
//     - command {section 0x400, id 2}              -> detach/stop on close
//   Command packet: 16-byte header {u64 token=0, u16 section, u16 id, u32 0}
//   followed by the payload. Response: {u32 status, u32 length, u8 data[]}.
//   The driver leaves "length" at 0 and writes 8 + requested bytes (io=96 for
//   the 0x58 config read), so copy the requested size like Pal does.
//
// If this hangs on the command ioctl, open the handle without
// FILE_FLAG_OVERLAPPED and drop the OVERLAPPED plumbing in ioctl_xfer().

#define _CRT_SECURE_NO_WARNINGS
#define WIN32_LEAN_AND_MEAN

#include <windows.h>
#include <setupapi.h>
#include <winver.h>
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#pragma comment(lib, "setupapi.lib")
#pragma comment(lib, "version.lib")
#pragma comment(lib, "advapi32.lib")

// ---------------------------------------------------------------------------
// Protocol constants (driver 4.143.0.261)
// ---------------------------------------------------------------------------

static const DWORD IOCTL_GET_VERSION = 0x222000; // CTL_CODE(0x22, 0x800, BUFFERED, ANY)
static const DWORD IOCTL_COMMAND     = 0x222008; // CTL_CODE(0x22, 0x802, BUFFERED, ANY)

static const unsigned short SECTION_APP = 0x0400;

static const unsigned short CMD_ATTACH      = 1;
static const unsigned short CMD_DETACH      = 2;
static const unsigned short CMD_GET_CONFIG  = 4;
static const unsigned short CMD_SET_RATE    = 5;
static const unsigned short CMD_SET_BUFSIZE = 9;

static const DWORD CONFIG_SIZE     = 0x58;
static const DWORD OFF_SAMPLE_RATE = 0x0C;
static const DWORD OFF_BUFFER_SIZE = 0x14;

// Device interface GUIDs Pal hard-codes (in the 4.143.0.261 build):
//   {AC4D0455-...} USB, {429A7462-...-411F} / -411E PCIe.
// They are only a fallback: an installed driver build may register a different
// GUID, so the tool also discovers Focusrite interface classes from
// HKLM\SYSTEM\CurrentControlSet\Control\DeviceClasses.
//
// The GUID alone is not enough: a driver can register several interfaces under
// the same GUID with different reference strings, and Pal only accepts the one
// whose DevicePath contains "\pal" (FUN_180008D60 searches for that literal).
static const GUID kKnownGuids[] = {
    {0xAC4D0455, 0x50D7, 0x4498, {0xB3, 0xCD, 0x9A, 0x41, 0xD1, 0x30, 0xB7, 0x59}},
    {0x429A7462, 0xF176, 0x411F, {0x8D, 0xA9, 0xAE, 0x6C, 0x0A, 0x25, 0x80, 0x98}},
    {0x429A7462, 0xF176, 0x411E, {0x8D, 0xA9, 0xAE, 0x6C, 0x0A, 0x25, 0x80, 0x98}},
};

// Pal uses GENERIC_READ|GENERIC_WRITE. The fallbacks exist because a driver
// build may be stricter than the one this PoC was derived from.
struct AccessAttempt {
    DWORD       access;
    const char* name;
};
static const AccessAttempt kAccess[] = {
    {GENERIC_READ | GENERIC_WRITE,                  "rw"},
    {FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE, "rw-data"},
    {0,                                             "none"},
};

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#pragma pack(push, 1)
struct CommandHeader {
    unsigned long long token;   // 0 in every Pal call site
    unsigned short     section;
    unsigned short     id;
    unsigned int       reserved; // 0
};
struct ResponseHeader {
    unsigned int status;  // 0 = ok, otherwise the driver rejected the command
    unsigned int length;  // bytes of data following the header
};
#pragma pack(pop)

static unsigned int rd32(const void* p)
{
    unsigned int v = 0;
    memcpy(&v, p, sizeof(v));
    return v;
}

// ---------------------------------------------------------------------------
// I/O helpers
// ---------------------------------------------------------------------------

// Overlapped round trip with a timeout, like Pal's ioctl().
static bool ioctl_xfer(HANDLE h, DWORD code, const void* in, DWORD inLen,
                       void* out, DWORD outLen, DWORD* transferred)
{
    OVERLAPPED ov;
    ZeroMemory(&ov, sizeof(ov));
    ov.hEvent = CreateEventA(NULL, TRUE, FALSE, NULL);
    if (!ov.hEvent)
        return false;

    DWORD bytes = 0;
    BOOL  ok = DeviceIoControl(h, code, const_cast<void*>(in), inLen,
                               out, outLen, &bytes, &ov);
    DWORD err = ok ? 0 : GetLastError();

    if (!ok && err == ERROR_IO_PENDING) {
        if (WaitForSingleObject(ov.hEvent, 5000) != WAIT_OBJECT_0) {
            CancelIo(h);
            GetOverlappedResult(h, &ov, &bytes, TRUE);
            CloseHandle(ov.hEvent);
            SetLastError(ERROR_TIMEOUT);
            return false;
        }
        ok = GetOverlappedResult(h, &ov, &bytes, FALSE);
        err = ok ? 0 : GetLastError();
    }
    CloseHandle(ov.hEvent);
    if (!ok)
        SetLastError(err); // CloseHandle() above may have clobbered it

    if (transferred)
        *transferred = bytes;
    return ok != FALSE;
}

// One DICE command. `out`/`outCap` may be NULL/0 for commands without a reply.
static bool dice_command(HANDLE h, unsigned short section, unsigned short id,
                         const void* data, DWORD len,
                         void* out, DWORD outCap, DWORD* outLen)
{
    unsigned char packet[16 + 64];
    unsigned char response[8 + 256];

    if (len > 64 || outCap > 256)
        return false;

    CommandHeader* hdr = reinterpret_cast<CommandHeader*>(packet);
    hdr->token    = 0;
    hdr->section  = section;
    hdr->id       = id;
    hdr->reserved = 0;
    if (len)
        memcpy(packet + 16, data, len);

    DWORD got = 0;
    if (!ioctl_xfer(h, IOCTL_COMMAND, packet, 16 + len, response, 8 + outCap, &got)) {
        fprintf(stderr, "command %u/%u: ioctl failed (error %lu)\n",
                section, id, GetLastError());
        return false;
    }
    if (got < 8)
        return false;

    const ResponseHeader* rsp = reinterpret_cast<const ResponseHeader*>(response);
    printf("  [0x%X/0x%X] status=%u len=%u io=%lu\n",
           section, id, rsp->status, rsp->length, got);
    if (rsp->status != 0) {
        fprintf(stderr, "driver rejected %u/%u: status %u (0x%X)\n",
                section, id, rsp->status, rsp->status);
        return false;
    }

    if (outLen)
        *outLen = rsp->length;
    // Pal copies the requested size regardless of the reported length, so do the
    // same; the buffer was zeroed by the caller and is ours to read.
    if (out && outCap)
        memcpy(out, response + 8, outCap);
    return true;
}

// 16-byte driver API descriptor; Pal rejects drivers whose word at +4 < 0x7000.
static bool get_driver_version(HANDLE h, unsigned int info[4])
{
    DWORD got = 0;
    ZeroMemory(info, 4 * sizeof(info[0]));
    if (!ioctl_xfer(h, IOCTL_GET_VERSION, NULL, 0, info, 4 * sizeof(info[0]), &got))
        return false;
    return got >= 8;
}

static void print_file_version(const char* path)
{
    if (GetFileAttributesA(path) == INVALID_FILE_ATTRIBUTES)
        return;

    DWORD dummy = 0;
    DWORD size = GetFileVersionInfoSizeA(path, &dummy);
    if (!size) {
        printf("  %s (no version resource)\n", path);
        return;
    }
    void* buf = malloc(size);
    if (!buf)
        return;
    if (GetFileVersionInfoA(path, 0, size, buf)) {
        VS_FIXEDFILEINFO* fi = NULL;
        UINT len = 0;
        if (VerQueryValueA(buf, "\\", reinterpret_cast<LPVOID*>(&fi), &len) &&
            fi && len >= sizeof(*fi)) {
            printf("  %s  %u.%u.%u.%u\n", path,
                   (unsigned)HIWORD(fi->dwFileVersionMS), (unsigned)LOWORD(fi->dwFileVersionMS),
                   (unsigned)HIWORD(fi->dwFileVersionLS), (unsigned)LOWORD(fi->dwFileVersionLS));
            free(buf);
            return;
        }
    }
    printf("  %s (version unreadable)\n", path);
    free(buf);
}

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

static HANDLE try_open(const char* path, DWORD access, unsigned int info[4],
                       DWORD* err)
{
    HANDLE h = CreateFileA(path, access, 0, NULL, OPEN_EXISTING,
                           FILE_FLAG_OVERLAPPED | FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        *err = GetLastError();
        return INVALID_HANDLE_VALUE;
    }
    if (get_driver_version(h, info))
        return h;

    *err = GetLastError();
    CloseHandle(h);
    return INVALID_HANDLE_VALUE;
}

// True if the path contains Pal's interface reference string.
static bool contains_pal(const char* s)
{
    for (; *s; ++s) {
        if (s[0] == '\\' &&
            (s[1] == 'p' || s[1] == 'P') &&
            (s[2] == 'a' || s[2] == 'A') &&
            (s[3] == 'l' || s[3] == 'L'))
            return true;
    }
    return false;
}

static bool contains_ci(const char* haystack, const char* needle)
{
    size_t n = strlen(needle);
    for (; *haystack; ++haystack) {
        size_t i = 0;
        while (i < n && haystack[i] &&
               tolower((unsigned char)haystack[i]) == tolower((unsigned char)needle[i]))
            ++i;
        if (i == n)
            return true;
    }
    return false;
}

static int hex_nibble(char c)
{
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

// Accepts "{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}" (the DeviceClasses key form).
static bool parse_guid_text(const char* s, GUID* g)
{
    if (*s == '{')
        ++s;

    char hex[33];
    int n = 0;
    for (; *s && *s != '}' && n < 32; ++s) {
        if (*s == '-')
            continue;
        if (hex_nibble(*s) < 0)
            return false;
        hex[n++] = *s;
    }
    if (n != 32)
        return false;

    unsigned char b[16];
    for (int i = 0; i < 16; ++i)
        b[i] = static_cast<unsigned char>((hex_nibble(hex[2 * i]) << 4) | hex_nibble(hex[2 * i + 1]));

    g->Data1 = (unsigned long)b[0] << 24 | (unsigned long)b[1] << 16 |
               (unsigned long)b[2] << 8 | b[3];
    g->Data2 = (unsigned short)(b[4] << 8 | b[5]);
    g->Data3 = (unsigned short)(b[6] << 8 | b[7]);
    memcpy(g->Data4, b + 8, 8);
    return true;
}

static bool guid_seen(const GUID* list, int count, const GUID& g)
{
    for (int i = 0; i < count; ++i)
        if (memcmp(&list[i], &g, sizeof(GUID)) == 0)
            return true;
    return false;
}

// Walk DeviceClasses and collect every interface class that has an entry whose
// symbolic link mentions a Focusrite device. This finds the real GUID even when
// the installed driver build differs from the one this PoC was derived from.
static int discover_guid_from_registry(GUID* out, int max)
{
    HKEY classes_key;
    if (RegOpenKeyExA(HKEY_LOCAL_MACHINE,
                      "SYSTEM\\CurrentControlSet\\Control\\DeviceClasses",
                      0, KEY_READ | KEY_WOW64_64KEY, &classes_key) != ERROR_SUCCESS) {
        printf("  (cannot read DeviceClasses: error %lu)\n", GetLastError());
        return 0;
    }

    int count = 0;
    int classes = 0;
    int instances = 0;
    char cls[128];
    for (DWORD i = 0;; ++i) {
        DWORD clsLen = sizeof(cls);
        LONG rc = RegEnumKeyExA(classes_key, i, cls, &clsLen, NULL, NULL, NULL, NULL);
        if (rc == ERROR_NO_MORE_ITEMS)
            break;
        if (rc != ERROR_SUCCESS)
            continue;

        ++classes;
        HKEY hk;
        if (RegOpenKeyExA(classes_key, cls, 0, KEY_READ, &hk) != ERROR_SUCCESS)
            continue;

        bool focusrite = false;
        int others = 0;
        char first_other[2048];
        first_other[0] = '\0';

        char inst[2048];
        for (DWORD j = 0;; ++j) {
            DWORD instLen = sizeof(inst);
            if (RegEnumKeyExA(hk, j, inst, &instLen, NULL, NULL, NULL, NULL) != ERROR_SUCCESS)
                break;
            ++instances;
            if (!contains_ci(inst, "vid_1235") && !contains_ci(inst, "focusrite"))
                continue;

            focusrite = true;
            if (contains_ci(inst, "#pal")) {
                printf("  %s  [pal]\n    %s\n", cls, inst);
            } else {
                if (others == 0)
                    snprintf(first_other, sizeof(first_other), "%s", inst);
                ++others;
            }
        }
        RegCloseKey(hk);

        if (others)
            printf("  %s  (%d other instance%s, e.g. %s)\n",
                   cls, others, others == 1 ? "" : "s", first_other);

        // Collect every class that has a Focusrite instance. Paths containing
        // "\pal" are still preferred when opening, so audio/MIDI instances are
        // only reached if no control interface works.
        if (focusrite && count < max) {
            GUID g;
            if (parse_guid_text(cls, &g) && !guid_seen(out, count, g))
                out[count++] = g;
        }
    }
    RegCloseKey(classes_key);
    if (count == 0)
        printf("  (scanned %d interface classes / %d instances, none matched)\n",
               classes, instances);
    return count;
}

struct Candidate {
    char path[512];
    bool pal;
    bool active;
};

static HANDLE open_control_interface(char* pathOut, size_t pathCap, unsigned int info[4],
                                     DWORD* lastErr)
{
    *lastErr = ERROR_DEVICE_NOT_CONNECTED;

    Candidate cands[64];
    int count = 0;

    GUID guids[40];
    int guidCount = 0;

    printf("Focusrite interface classes (registry):\n");
    guidCount = discover_guid_from_registry(guids, 40);
    if (guidCount == 0)
        printf("  none found\n");
    for (size_t i = 0; i < sizeof(kKnownGuids) / sizeof(kKnownGuids[0]); ++i) {
        if (!guid_seen(guids, guidCount, kKnownGuids[i]) && guidCount < 40)
            guids[guidCount++] = kKnownGuids[i];
    }

    printf("\ninterfaces found:\n");
    for (int g = 0; g < guidCount; ++g) {
        const GUID& guid = guids[g];

        HDEVINFO set = SetupDiGetClassDevsA(&guid, NULL, NULL,
                                            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if (set == INVALID_HANDLE_VALUE) {
            printf("  %08lX-%04X-%04X: SetupDiGetClassDevs failed (error %lu)\n",
                   (unsigned long)guid.Data1, guid.Data2, guid.Data3, GetLastError());
            continue;
        }

        SP_DEVICE_INTERFACE_DATA did;
        did.cbSize = sizeof(did);

        unsigned kept = 0;
        unsigned skipped = 0;
        for (DWORD i = 0;
             SetupDiEnumDeviceInterfaces(set, NULL, &guid, i, &did);
             ++i) {
            DWORD needed = 0;
            SetupDiGetDeviceInterfaceDetailA(set, &did, NULL, 0, &needed, NULL);
            if (needed == 0)
                continue;

            PSP_DEVICE_INTERFACE_DETAIL_DATA_A detail =
                static_cast<PSP_DEVICE_INTERFACE_DETAIL_DATA_A>(malloc(needed));
            if (!detail)
                break;
            detail->cbSize = sizeof(SP_DEVICE_INTERFACE_DETAIL_DATA_A);

            if (SetupDiGetDeviceInterfaceDetailA(set, &did, detail, needed, &needed, NULL)) {
                bool pal = contains_pal(detail->DevicePath);

                // Only the "\pal" control interfaces and the interface on the
                // VID_1235 devnode are interesting. Skipping the rest avoids
                // opening audio/KS interfaces and keeps the candidate list small.
                if (pal || contains_ci(detail->DevicePath, "vid_1235")) {
                    ++kept;
                    printf("  %08lX-%04X-%04X [%s]%s\n    %s\n",
                           (unsigned long)guid.Data1, guid.Data2, guid.Data3,
                           (did.Flags & SPINT_ACTIVE) ? "active" : "inactive",
                           pal ? " \\pal" : "",
                           detail->DevicePath);

                    if (count < static_cast<int>(sizeof(cands) / sizeof(cands[0]))) {
                        snprintf(cands[count].path, sizeof(cands[count].path), "%s",
                                 detail->DevicePath);
                        cands[count].pal = pal;
                        cands[count].active = (did.Flags & SPINT_ACTIVE) != 0;
                        ++count;
                    }
                } else {
                    ++skipped;
                }
            }
            free(detail);
        }
        if (kept == 0)
            printf("  %08lX-%04X-%04X: no control interface\n",
                   (unsigned long)guid.Data1, guid.Data2, guid.Data3);
        else if (skipped)
            printf("  ... %u non-control interface(s) skipped\n", skipped);
        SetupDiDestroyDeviceInfoList(set);
    }

    // Pal only accepts paths containing "\pal"; try those first, then the rest in
    // case a driver build uses a different reference string.
    for (int pass = 0; pass < 2; ++pass) {
        for (int c = 0; c < count; ++c) {
            if (cands[c].pal != (pass == 0))
                continue;

            printf("\ntrying: %s\n", cands[c].path);
            for (size_t a = 0; a < sizeof(kAccess) / sizeof(kAccess[0]); ++a) {
                unsigned int v[4] = {0, 0, 0, 0};
                DWORD err = 0;
                HANDLE h = try_open(cands[c].path, kAccess[a].access, v, &err);
                if (h != INVALID_HANDLE_VALUE) {
                    snprintf(pathOut, pathCap, "%s", cands[c].path);
                    memcpy(info, v, sizeof(v));
                    printf("  open %-7s -> ok\n", kAccess[a].name);
                    return h;
                }
                printf("  open %-7s -> error %lu\n", kAccess[a].name, err);
                if (err == ERROR_SHARING_VIOLATION || *lastErr == ERROR_DEVICE_NOT_CONNECTED)
                    *lastErr = err;
            }
        }
    }
    return INVALID_HANDLE_VALUE;
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

static bool known_size(unsigned value)
{
    static const unsigned kSizes[] = {16, 32, 48, 64, 96, 128, 160, 192, 256, 512, 1024};
    for (size_t i = 0; i < sizeof(kSizes) / sizeof(kSizes[0]); ++i)
        if (kSizes[i] == value)
            return true;
    return false;
}

static bool known_rate(unsigned value)
{
    static const unsigned kRates[] = {44100, 48000, 88200, 96000, 176400, 192000};
    for (size_t i = 0; i < sizeof(kRates) / sizeof(kRates[0]); ++i)
        if (kRates[i] == value)
            return true;
    return false;
}

static bool parse_uint(const char* s, unsigned* out)
{
    if (!s || !*s)
        return false;
    char* end = NULL;
    unsigned long v = strtoul(s, &end, 10);
    if (end == s || *end != '\0' || v == 0)
        return false;
    *out = static_cast<unsigned>(v);
    return true;
}

struct Options {
    bool     set_buffer;
    unsigned buffer;
    bool     set_rate;
    unsigned rate;
};

static int usage(void)
{
    fprintf(stderr,
            "usage: focusrite_buffer <samples>                  (buffer size)\n"
            "       focusrite_buffer buffer <samples>\n"
            "       focusrite_buffer rate <hz>\n"
            "       focusrite_buffer rate <hz> buffer <samples>\n"
            "\n"
            "  Notifier's buffer list: 16 32 48 64 96 128 160 192 256 512 1024\n"
            "  Common rates:           44100 48000 88200 96000 176400 192000\n"
            "\n"
            "  The driver decides what it accepts. When both are given the sample\n"
            "  rate is applied first, then the buffer size.\n");
    return 2;
}

static bool parse_options(int argc, char** argv, Options* opt)
{
    ZeroMemory(opt, sizeof(*opt));

    for (int i = 1; i < argc; ++i) {
        unsigned value = 0;

        if (strcmp(argv[i], "buffer") == 0) {
            if (i + 1 >= argc || !parse_uint(argv[++i], &value))
                return false;
            opt->set_buffer = true;
            opt->buffer = value;
        } else if (strcmp(argv[i], "rate") == 0) {
            if (i + 1 >= argc || !parse_uint(argv[++i], &value))
                return false;
            opt->set_rate = true;
            opt->rate = value;
        } else if (argc == 2) {
            // legacy form: focusrite_buffer 256
            if (!parse_uint(argv[i], &value))
                return false;
            opt->set_buffer = true;
            opt->buffer = value;
        } else {
            return false;
        }
    }
    return opt->set_buffer || opt->set_rate;
}

// Read the 0x58-byte config blob and pull out the two fields we care about.
static bool read_config(HANDLE h, unsigned char* config, unsigned* rate, unsigned* buffer)
{
    memset(config, 0, CONFIG_SIZE);
    DWORD len = 0;
    if (!dice_command(h, SECTION_APP, CMD_GET_CONFIG, NULL, 0, config, CONFIG_SIZE, &len))
        return false;
    *rate = rd32(config + OFF_SAMPLE_RATE);
    *buffer = rd32(config + OFF_BUFFER_SIZE);
    return true;
}

int main(int argc, char** argv)
{
    Options opt;
    if (!parse_options(argc, argv, &opt))
        return usage();

    if (opt.set_buffer && !known_size(opt.buffer))
        fprintf(stderr, "note: %u is not in the Notifier's list, sending anyway\n", opt.buffer);
    if (opt.set_rate && !known_rate(opt.rate))
        fprintf(stderr, "note: %u Hz is not a common rate, sending anyway\n", opt.rate);

    printf("installed versions:\n");
    print_file_version("C:\\Windows\\System32\\drivers\\FocusriteUsb.sys");
    print_file_version("C:\\Program Files\\Focusrite\\Drivers\\FocusritePal64.dll");
    printf("\n");

    char path[512];
    unsigned int ver[4] = {0, 0, 0, 0};
    DWORD openErr = 0;
    HANDLE h = open_control_interface(path, sizeof(path), ver, &openErr);
    if (h == INVALID_HANDLE_VALUE) {
        if (openErr == ERROR_SHARING_VIOLATION)
            fprintf(stderr, "device is in use - close Focusrite Notifier / Focusrite Control first\n");
        else
            fprintf(stderr, "could not open a control interface (last error %lu)\n", openErr);
        return 1;
    }

    printf("\ndevice:     %s\n", path);
    printf("driver:     0x%08X 0x%08X 0x%08X 0x%08X (API 0x%X)\n",
           ver[0], ver[1], ver[2], ver[3], ver[1]);

    // Same sequence Pal performs when it opens the device.
    bool attached = dice_command(h, SECTION_APP, CMD_ATTACH, NULL, 0, NULL, 0, NULL);
    if (!attached)
        fprintf(stderr, "attach failed (continuing anyway)\n");

    unsigned char config[CONFIG_SIZE];
    unsigned rate0 = 0, buffer0 = 0;
    if (read_config(h, config, &rate0, &buffer0)) {
        printf("config raw: %08X %08X %08X %08X %08X %08X %08X\n",
               rd32(config + 0), rd32(config + 4), rd32(config + 8), rd32(config + 12),
               rd32(config + 16), rd32(config + 20), rd32(config + 24));
        printf("before:     sample rate %u, buffer %u\n", rate0, buffer0);
    } else {
        fprintf(stderr, "could not read current settings\n");
    }

    int result = 0;

    if (opt.set_rate) {
        unsigned int value = opt.rate;
        if (!dice_command(h, SECTION_APP, CMD_SET_RATE,
                          &value, sizeof(value), NULL, 0, NULL)) {
            fprintf(stderr, "set sample rate failed\n");
            result = 1;
        } else {
            printf("set:        sample rate %u Hz\n", opt.rate);
        }
        Sleep(250); // let the driver settle before touching the buffer
    }

    if (opt.set_buffer) {
        unsigned int value = opt.buffer;
        if (!dice_command(h, SECTION_APP, CMD_SET_BUFSIZE,
                          &value, sizeof(value), NULL, 0, NULL)) {
            fprintf(stderr, "set buffer size failed\n");
            result = 1;
        } else {
            printf("set:        buffer size %u samples\n", opt.buffer);
        }
        Sleep(250);
    }

    unsigned rate1 = 0, buffer1 = 0;
    if (read_config(h, config, &rate1, &buffer1)) {
        printf("after:      sample rate %u, buffer %u\n", rate1, buffer1);

        bool rate_ok = !opt.set_rate || rate1 == opt.rate;
        bool buffer_ok = !opt.set_buffer || buffer1 == opt.buffer;
        if (rate_ok && buffer_ok) {
            printf("ok: verified\n");
        } else {
            printf("note: read-back differs from the request");
            if (!rate_ok)
                printf(" (rate %u, wanted %u)", rate1, opt.rate);
            if (!buffer_ok)
                printf(" (buffer %u, wanted %u)", buffer1, opt.buffer);
            printf("\n");
        }
    } else {
        printf("set accepted (read-back unavailable)\n");
    }

    if (attached)
        dice_command(h, SECTION_APP, CMD_DETACH, NULL, 0, NULL, 0, NULL);
    CloseHandle(h);
    return result;
}
