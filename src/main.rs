//! `focusrite_buffer` - Rust port of `tools/focusrite_buffer.cpp`.
//!
//! Changes the streaming buffer size and/or sample rate of a Focusrite
//! interface by issuing the same private ioctls Focusrite's Pal layer uses.
//! No `FocusritePal64.dll` involved.
//!
//! Protocol reference: `analysis/FocusritePal64.dll.md`.
//!
//! Build: `cargo build --release --target x86_64-pc-windows-msvc`
//!
//! Usage:
//! - `focusrite_buffer 256`
//! - `focusrite_buffer buffer 256`
//! - `focusrite_buffer rate 48000`
//! - `focusrite_buffer rate 48000 buffer 256`

use std::env;
use std::ffi::c_void;
use std::io;
use std::mem::{size_of, size_of_val, zeroed};
use std::ptr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SPINT_ACTIVE, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_NO_MORE_ITEMS, HANDLE, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE,
    GetFileVersionInfoSizeW, GetFileVersionInfoW, OPEN_EXISTING, VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows::Win32::System::IO::{CancelIo, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegCloseKey, RegEnumKeyExA, RegOpenKeyExA,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{GUID, PCSTR, PCWSTR, PSTR};

// ---------------------------------------------------------------------------
// Protocol constants (driver 4.143.0.261)
// ---------------------------------------------------------------------------

const IOCTL_GET_VERSION: u32 = 0x0022_2000; // CTL_CODE(0x22, 0x800, BUFFERED, ANY)
const IOCTL_COMMAND: u32 = 0x0022_2008; // CTL_CODE(0x22, 0x802, BUFFERED, ANY)

const SECTION_APP: u16 = 0x0400;

const CMD_ATTACH: u16 = 1;
const CMD_DETACH: u16 = 2;
const CMD_GET_CONFIG: u16 = 4;
const CMD_SET_RATE: u16 = 5;
const CMD_SET_BUFSIZE: u16 = 9;

const CONFIG_SIZE: usize = 0x58;
const OFF_SAMPLE_RATE: usize = 0x0C;
const OFF_BUFFER_SIZE: usize = 0x14;

/// Fallback list of interface GUIDs Pal hard-codes; discovery via the registry
/// is preferred because a driver build may register different ones.
const KNOWN_GUIDS: [GUID; 3] = [
    GUID::from_u128(0xac4d0455_50d7_4498_b3cd_9a41d130b759),
    GUID::from_u128(0x429a7462_f176_411f_8da9_ae6c0a258098),
    GUID::from_u128(0x429a7462_f176_411e_8da9_ae6c0a258098),
];

/// Access masks to try when opening the control interface, in order.
const ACCESS_ATTEMPTS: [(u32, &str); 3] = [
    (0xC000_0000, "rw"),      // GENERIC_READ | GENERIC_WRITE
    (0x0010_0003, "rw-data"), // FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE
    (0, "none"),
];

const REG_NAME_CAP: usize = 128;
const REG_INSTANCE_CAP: usize = 2048;

// ---------------------------------------------------------------------------
// RAII wrappers
// ---------------------------------------------------------------------------

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn i32_of(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn usize_of(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn rd32(buf: &[u8], offset: usize) -> u32 {
    buf.get(offset..offset + 4)
        .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
        .map_or(0, u32::from_le_bytes)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `path` without its trailing NUL, for display.
fn path_text(path: &[u16]) -> String {
    let body = path.split_last().map_or(path, |(_, rest)| rest);
    String::from_utf16_lossy(body)
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// ASCII case-insensitive comparison of a UTF-16 code unit against a byte.
const fn ascii_ci(code_unit: u16, lower: u16) -> bool {
    (code_unit | 0x20) == (lower | 0x20)
}

/// Pal only accepts interface paths containing its reference string `\pal`.
fn contains_pal(path: &[u16]) -> bool {
    path.windows(4).any(|window| match window {
        [a, b, c, d] => {
            *a == u16::from(b'\\')
                && ascii_ci(*b, u16::from(b'p'))
                && ascii_ci(*c, u16::from(b'a'))
                && ascii_ci(*d, u16::from(b'l'))
        }
        _ => false,
    })
}

/// ASCII case-insensitive substring test on a UTF-16 device path.
fn contains_ci_wide(haystack: &[u16], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(&code_unit, &byte)| ascii_ci(code_unit, u16::from(byte)))
    })
}

/// Accepts the braced GUID form used by the `DeviceClasses` registry keys.
fn parse_guid(text: &str) -> Option<GUID> {
    let trimmed = text.trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = trimmed.split('-').collect();
    let [p0, p1, p2, p3, p4] = parts.as_slice() else {
        return None;
    };
    if p0.len() != 8 || p1.len() != 4 || p2.len() != 4 || p3.len() != 4 || p4.len() != 12 {
        return None;
    }

    let mut tail = [0u8; 8];
    for (index, byte) in tail.iter_mut().take(2).enumerate() {
        *byte = u8::from_str_radix(p3.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    for (index, byte) in tail.iter_mut().skip(2).enumerate() {
        *byte = u8::from_str_radix(p4.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }

    Some(GUID::from_values(
        u32::from_str_radix(p0, 16).ok()?,
        u16::from_str_radix(p1, 16).ok()?,
        u16::from_str_radix(p2, 16).ok()?,
        tail,
    ))
}

fn print_file_version(path: &str) {
    let wide_path = wide(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide_path.as_ptr()), None);
        if size == 0 {
            return; // not installed
        }
        let mut data = vec![0u8; usize_of(size)];
        if GetFileVersionInfoW(
            PCWSTR(wide_path.as_ptr()),
            0,
            size,
            data.as_mut_ptr().cast::<c_void>(),
        )
        .is_err()
        {
            println!("  {path} (version unreadable)");
            return;
        }

        let mut block: *mut c_void = ptr::null_mut();
        let mut len = 0u32;
        let found = VerQueryValueW(
            data.as_ptr().cast::<c_void>(),
            windows::core::w!("\\"),
            &raw mut block,
            &raw mut len,
        )
        .as_bool();
        if found && !block.is_null() && usize_of(len) >= size_of::<VS_FIXEDFILEINFO>() {
            let info = &*block.cast::<VS_FIXEDFILEINFO>();
            println!(
                "  {path}  {}.{}.{}.{}",
                info.dwFileVersionMS >> 16,
                info.dwFileVersionMS & 0xffff,
                info.dwFileVersionLS >> 16,
                info.dwFileVersionLS & 0xffff
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

struct Candidate {
    path: Vec<u16>, // NUL-terminated
    pal: bool,
    active: bool,
}

/// Scan one interface class key's instances. Prints what it finds and reports
/// whether the class has any Focusrite instance and one with `#pal`.
fn scan_class_instances(key: HKEY, class_name: &str) -> (bool, bool) {
    let mut focusrite = false;
    let mut has_pal = false;
    let mut other: Option<Vec<u8>> = None;

    for index in 0.. {
        let mut instance = [0u8; REG_INSTANCE_CAP];
        let mut instance_len = u32_of(REG_INSTANCE_CAP);
        let rc = unsafe {
            RegEnumKeyExA(
                key,
                index,
                PSTR(instance.as_mut_ptr()),
                &raw mut instance_len,
                None,
                PSTR(ptr::null_mut()),
                None,
                None,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != WIN32_ERROR(0) {
            continue;
        }
        let instance = instance.get(..usize_of(instance_len)).unwrap_or(&[]);
        if !contains_ci(instance, b"vid_1235") && !contains_ci(instance, b"focusrite") {
            continue;
        }
        focusrite = true;
        if contains_ci(instance, b"#pal") {
            has_pal = true;
            println!("  {class_name}  [pal]");
            println!("    {}", String::from_utf8_lossy(instance));
        } else if other.is_none() {
            other = Some(instance.to_vec());
        }
    }

    if let Some(instance) = other {
        println!(
            "  {class_name}  (other instance, e.g. {})",
            String::from_utf8_lossy(&instance)
        );
    }

    (focusrite, has_pal)
}

/// Collect interface class GUIDs that have a Focusrite instance with the
/// `\pal` reference string. This finds the right GUID even when the installed
/// driver build differs from the one this tool was written against.
fn registered_focusrite_classes() -> Vec<GUID> {
    let mut found = Vec::new();

    let mut classes = HKEY::default();
    let open = unsafe {
        RegOpenKeyExA(
            HKEY_LOCAL_MACHINE,
            windows::core::s!("SYSTEM\\CurrentControlSet\\Control\\DeviceClasses"),
            0,
            KEY_READ | KEY_WOW64_64KEY,
            &raw mut classes,
        )
    };
    if open != WIN32_ERROR(0) {
        println!("  (cannot read DeviceClasses: error {})", open.0);
        return found;
    }
    let classes = RegKey(classes);

    for index in 0.. {
        let mut name = [0u8; REG_NAME_CAP];
        let mut name_len = u32_of(REG_NAME_CAP);
        let rc = unsafe {
            RegEnumKeyExA(
                classes.0,
                index,
                PSTR(name.as_mut_ptr()),
                &raw mut name_len,
                None,
                PSTR(ptr::null_mut()),
                None,
                None,
            )
        };
        if rc == ERROR_NO_MORE_ITEMS {
            break;
        }
        if rc != WIN32_ERROR(0) {
            continue;
        }
        let class_name =
            std::str::from_utf8(name.get(..usize_of(name_len)).unwrap_or(&[])).unwrap_or("");

        let mut sub = HKEY::default();
        let sub_rc =
            unsafe { RegOpenKeyExA(classes.0, PCSTR(name.as_ptr()), 0, KEY_READ, &raw mut sub) };
        if sub_rc != WIN32_ERROR(0) {
            continue;
        }
        let sub = RegKey(sub);

        let (focusrite, has_pal) = scan_class_instances(sub.0, class_name);

        if focusrite
            && has_pal
            && let Some(guid) = parse_guid(class_name)
            && !found.contains(&guid)
        {
            found.push(guid);
        }
    }

    found
}

fn candidates_for(guid: &GUID) -> (Vec<Candidate>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;

    let set = match unsafe {
        SetupDiGetClassDevsW(
            Some(ptr::from_ref(guid)),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    } {
        Ok(set) => DeviceInfoSet(set),
        Err(_) => return (out, skipped),
    };

    let mut index = 0u32;
    loop {
        let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        interface.cbSize = u32_of(size_of::<SP_DEVICE_INTERFACE_DATA>());
        let more = unsafe {
            SetupDiEnumDeviceInterfaces(set.0, None, ptr::from_ref(guid), index, &raw mut interface)
        };
        if more.is_err() {
            break;
        }
        index += 1;

        let mut needed = 0u32;
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &raw const interface,
                None,
                0,
                Some(&raw mut needed),
                None,
            )
        };
        if needed == 0 {
            continue;
        }

        // The detail struct is variable length and needs 4-byte alignment.
        let mut buffer: Vec<u64> = vec![0; usize::try_from(needed).unwrap_or(0).div_ceil(8)];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe {
            (*detail).cbSize = u32_of(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>());
        }

        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &raw const interface,
                Some(detail),
                needed,
                None,
                None,
            )
        }
        .is_err()
        {
            continue;
        }

        let start = unsafe { (*detail).DevicePath.as_ptr() };
        let mut length = 0usize;
        while unsafe { *start.add(length) } != 0 {
            length += 1;
        }
        let path: Vec<u16> = unsafe { std::slice::from_raw_parts(start, length + 1) }.to_vec();

        if contains_pal(&path) {
            out.push(Candidate {
                path,
                pal: true,
                active: (interface.Flags & SPINT_ACTIVE) != 0,
            });
        } else if contains_ci_wide(&path, b"vid_1235") {
            out.push(Candidate {
                path,
                pal: false,
                active: (interface.Flags & SPINT_ACTIVE) != 0,
            });
        } else {
            skipped += 1;
        }
    }

    (out, skipped)
}

fn open_control_interface() -> Result<(Handle, Vec<u16>, [u32; 4])> {
    let mut guids = registered_focusrite_classes();
    for known in KNOWN_GUIDS {
        if !guids.contains(&known) {
            guids.push(known);
        }
    }

    let mut candidates = Vec::new();
    println!("\ninterfaces found:");
    for guid in &guids {
        let (found, skipped) = candidates_for(guid);
        if found.is_empty() {
            println!(
                "  {:08X}-{:04X}-{:04X}: no control interface",
                guid.data1, guid.data2, guid.data3
            );
        } else {
            for candidate in &found {
                println!(
                    "  {:08X}-{:04X}-{:04X} [{}]{}",
                    guid.data1,
                    guid.data2,
                    guid.data3,
                    if candidate.active {
                        "active"
                    } else {
                        "inactive"
                    },
                    if candidate.pal { " \\pal" } else { "" }
                );
                println!("    {}", path_text(&candidate.path));
            }
            candidates.extend(found);
        }
        if skipped > 0 {
            println!("  ... {skipped} non-control interface(s) skipped");
        }
    }

    let mut last_code = 0u32;

    // Pal only accepts "\pal"; try those first, then the rest.
    for pass in 0..2 {
        for candidate in &candidates {
            if candidate.pal != (pass == 0) {
                continue;
            }
            println!("\ntrying: {}", path_text(&candidate.path));
            for (access, label) in ACCESS_ATTEMPTS {
                match try_open(&candidate.path, access) {
                    Ok((handle, info)) => {
                        println!("  open {label:<7} -> ok");
                        return Ok((handle, candidate.path.clone(), info));
                    }
                    Err(error) => {
                        let code = error.raw_os_error().unwrap_or(0);
                        println!("  open {label:<7} -> error {code}");
                        if code > 0 && (code == 32 || last_code == 0) {
                            last_code = u32::try_from(code).unwrap_or(0);
                        }
                    }
                }
            }
        }
    }

    if last_code == 32 {
        bail!("device is in use - close Focusrite Notifier / Focusrite Control first");
    }
    bail!("could not open a control interface (last error {last_code})")
}

fn try_open(path: &[u16], access: u32) -> io::Result<(Handle, [u32; 4])> {
    let handle = match unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | FILE_ATTRIBUTE_NORMAL,
            None,
        )
    } {
        Ok(handle) => Handle(handle),
        Err(_) => return Err(io::Error::last_os_error()),
    };

    let mut info = [0u32; 4];
    let mut bytes = 0u32;
    let probed = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_GET_VERSION,
            None,
            0,
            Some(info.as_mut_ptr().cast::<c_void>()),
            u32_of(size_of_val(&info)),
            Some(&raw mut bytes),
            None,
        )
    };
    match probed {
        Ok(()) if bytes >= 8 => Ok((handle, info)),
        Ok(()) => Err(io::Error::from_raw_os_error(1157)), // ERROR_NO_ASSOCIATION
        Err(_) => Err(io::Error::last_os_error()),
    }
}

// ---------------------------------------------------------------------------
// DICE commands
// ---------------------------------------------------------------------------

fn command(h: HANDLE, section: u16, id: u16, payload: &[u8], out_cap: usize) -> Result<Vec<u8>> {
    // 16-byte header {u64 token, u16 section, u16 id, u32 0} + payload.
    let mut packet = Vec::with_capacity(16 + payload.len());
    packet.extend_from_slice(&0u64.to_le_bytes());
    packet.extend_from_slice(&section.to_le_bytes());
    packet.extend_from_slice(&id.to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(payload);

    let mut response = vec![0u8; 8 + out_cap];
    let mut bytes = 0u32;

    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .context("CreateEventW for the command event")?;
    let event = Handle(event);

    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event.0;

    let started = unsafe {
        DeviceIoControl(
            h,
            IOCTL_COMMAND,
            Some(packet.as_ptr().cast::<c_void>()),
            u32_of(packet.len()),
            Some(response.as_mut_ptr().cast::<c_void>()),
            u32_of(response.len()),
            Some(&raw mut bytes),
            Some(&raw mut overlapped),
        )
    };

    if started.is_err() {
        let code = unsafe { windows::Win32::Foundation::GetLastError() };
        if code != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(i32_of(code.0)))
                .with_context(|| format!("DeviceIoControl 0x{section:X}/0x{id:X}"));
        }
        if unsafe { WaitForSingleObject(event.0, 5_000) } != WAIT_OBJECT_0 {
            let _ = unsafe { CancelIo(h) };
            bail!("command 0x{section:X}/0x{id:X} timed out");
        }
        unsafe { GetOverlappedResult(h, &raw const overlapped, &raw mut bytes, true) }
            .with_context(|| format!("GetOverlappedResult 0x{section:X}/0x{id:X}"))?;
    }

    let status = rd32(&response, 0);
    let length = rd32(&response, 4);
    println!("  [0x{section:X}/0x{id:X}] status={status} len={length} io={bytes}");

    if status != 0 {
        bail!("driver rejected 0x{section:X}/0x{id:X}: status {status} (0x{status:X})");
    }

    Ok(response.get(8..).map_or_else(Vec::new, <[u8]>::to_vec))
}

fn read_config(h: HANDLE) -> Result<(u32, u32)> {
    let data =
        command(h, SECTION_APP, CMD_GET_CONFIG, &[], CONFIG_SIZE).context("read config blob")?;
    Ok((rd32(&data, OFF_SAMPLE_RATE), rd32(&data, OFF_BUFFER_SIZE)))
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Options {
    buffer: Option<u32>,
    rate: Option<u32>,
}

fn parse_uint(text: &str) -> Option<u32> {
    match text.parse::<u32>() {
        Ok(value) if value > 0 => Some(value),
        _ => None,
    }
}

fn parse_options(args: &[String]) -> Option<Options> {
    let mut opt = Options {
        buffer: None,
        rate: None,
    };

    let mut index = 0;
    while index < args.len() {
        match args.get(index).map(String::as_str) {
            Some("buffer") => {
                opt.buffer = Some(parse_uint(args.get(index + 1)?)?);
                index += 2;
            }
            Some("rate") => {
                opt.rate = Some(parse_uint(args.get(index + 1)?)?);
                index += 2;
            }
            _ if args.len() == 1 => {
                // legacy form: focusrite_buffer 256
                opt.buffer = Some(parse_uint(args.first()?)?);
                index += 1;
            }
            _ => return None,
        }
    }

    if opt.buffer.is_none() && opt.rate.is_none() {
        return None;
    }
    Some(opt)
}

fn usage() {
    eprintln!("usage: focusrite_buffer <samples>                  (buffer size)");
    eprintln!("       focusrite_buffer buffer <samples>");
    eprintln!("       focusrite_buffer rate <hz>");
    eprintln!("       focusrite_buffer rate <hz> buffer <samples>");
    eprintln!();
    eprintln!("  Notifier's buffer list: 16 32 48 64 96 128 160 192 256 512 1024");
    eprintln!("  Common rates:           44100 48000 88200 96000 176_400 192_000");
    eprintln!();
    eprintln!("  The driver decides what it accepts. When both are given the sample");
    eprintln!("  rate is applied first, then the buffer size.");
}

fn known_size(value: u32) -> bool {
    [16, 32, 48, 64, 96, 128, 160, 192, 256, 512, 1024].contains(&value)
}

fn known_rate(value: u32) -> bool {
    [44_100, 48_000, 88_200, 96_000, 176_400, 192_000].contains(&value)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn apply_settings(h: HANDLE, opt: Options) -> i32 {
    let mut result = 0;

    if let Some(rate) = opt.rate {
        match command(h, SECTION_APP, CMD_SET_RATE, &rate.to_le_bytes(), 0) {
            Ok(_) => println!("set:        sample rate {rate} Hz"),
            Err(error) => {
                eprintln!("set sample rate failed: {error:#}");
                result = 1;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    if let Some(buffer) = opt.buffer {
        match command(h, SECTION_APP, CMD_SET_BUFSIZE, &buffer.to_le_bytes(), 0) {
            Ok(_) => println!("set:        buffer size {buffer} samples"),
            Err(error) => {
                eprintln!("set buffer size failed: {error:#}");
                result = 1;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    result
}

fn report(handle: HANDLE, opt: Options) {
    match read_config(handle) {
        Ok((rate, buffer)) => {
            println!("after:      sample rate {rate}, buffer {buffer}");
            let rate_ok = opt.rate.is_none_or(|want| want == rate);
            let buffer_ok = opt.buffer.is_none_or(|want| want == buffer);
            if rate_ok && buffer_ok {
                println!("ok: verified");
                return;
            }
            print!("note: read-back differs from the request");
            if let Some(want) = opt.rate.filter(|want| *want != rate) {
                print!(" (rate {rate}, wanted {want})");
            }
            if let Some(want) = opt.buffer.filter(|want| *want != buffer) {
                print!(" (buffer {buffer}, wanted {want})");
            }
            println!();
        }
        Err(error) => println!("set accepted (read-back unavailable: {error:#})"),
    }
}

fn run() -> Result<i32> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(opt) = parse_options(&args) else {
        usage();
        return Ok(2);
    };

    if let Some(size) = opt.buffer
        && !known_size(size)
    {
        eprintln!("note: {size} is not in the Notifier's list, sending anyway");
    }
    if let Some(rate) = opt.rate
        && !known_rate(rate)
    {
        eprintln!("note: {rate} Hz is not a common rate, sending anyway");
    }

    println!("installed versions:");
    print_file_version("C:\\Windows\\System32\\drivers\\FocusriteUsb.sys");
    print_file_version("C:\\Program Files\\Focusrite\\Drivers\\FocusritePal64.dll");
    println!();

    println!("Focusrite interface classes (registry):");
    let (handle, path, version) = open_control_interface()?;

    println!("\ndevice:     {}", path_text(&path));
    println!(
        "driver:     0x{:08X} 0x{:08X} 0x{:08X} 0x{:08X} (API 0x{:X})",
        version[0], version[1], version[2], version[3], version[1]
    );

    // Same sequence Pal performs when it opens the device.
    let attached = match command(handle.0, SECTION_APP, CMD_ATTACH, &[], 0) {
        Ok(_) => true,
        Err(error) => {
            eprintln!("attach failed (continuing anyway): {error:#}");
            false
        }
    };

    match read_config(handle.0) {
        Ok((rate, buffer)) => println!("before:     sample rate {rate}, buffer {buffer}"),
        Err(error) => eprintln!("could not read current settings: {error:#}"),
    }

    let code = apply_settings(handle.0, opt);
    report(handle.0, opt);

    if attached {
        let _ = command(handle.0, SECTION_APP, CMD_DETACH, &[], 0);
    }

    Ok(code)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    }
}
