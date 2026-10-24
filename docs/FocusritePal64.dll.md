# FocusritePal64.dll — Pal layer behind the Notifier's settings UI

Artifact: `FocusritePal64.dll`, from the 4.143.0.261 driver package
sha256: `333f3e219c87b0468b5827565f803c66bafca896f727ab7bef576b986eda7988`
Image base `0x180000000`. x64, 488 procedures, stripped except for one exported entry point
(`Pal::System::createSystem`, `0x180006EB0`) and full RTTI.

Confidence tags: **[obs]** read from the binary; **[inf]** inferred (mostly C++ layout / naming).

This file resolves the parts of `Focusrite Notifier.exe.md` §4 and §9 that were previously
"unknown by construction".

---

## 1. Class layout (RTTI-derived)

| Class | TD | vtable | slots |
|---|---|---|---|
| `Pal::Device` (abstract) | `0x18001A458` | `0x180013908` | 46 |
| `Pal::DeviceBase` | `0x18001A2A0` | `0x180013758` | 53 |
| `Pal::DiceDevice` (concrete) | `0x18001A5A0` | `0x180013CA0` | 53 |
| `Pal::DeviceDelegate` | `0x18001A428` | `0x180013B00` | 9 |
| `Pal::DiceRootDriver` | `0x18001A6D0` | `0x180013FC0` | 4 |
| `Pal::DiceRootDriverWin` | `0x18001A810` | `0x1800142D0` | 4 |
| `Pal::DiceDriverManagerWin` | `0x18001A628` | `0x180013F40` | 3 |
| `Pal::USBManager` | `0x18001A8C8` | `0x180014310` | 3 |

In `Pal::Device`'s vtable almost every slot past `0x0C0` is the pure-call thunk `0x18000EF40`;
`DiceDevice` overrides them. `DiceDevice` is allocated by `std::make_shared` in
`FUN_18000B560` (`_Ref_count_obj2<Pal::DiceDevice>`); the object keeps two `shared_ptr`s to driver
objects at `+0x138` and `+0x140`, and the delegate pointer at `+0x10` (set by
`Pal::Device` slot `+0x08` = `FUN_180001DF0`, pure store). **[obs]**

## 2. The slots the Notifier actually calls

`DiceDevice` (and thus `Pal::Device`) slot table — all **[obs]** except the C++ method names **[inf]**:

| slot | DiceDevice impl | what it does |
|---|---|---|
| `+0x40` | `0x180008650` | **setSampleRate(uint)** → DICE command `section 0x400, id 5`, 4-byte payload |
| `+0x48` | `0x1800086F0` | getSampleRate → `*(int*)(driver+0x1C)` (cached) |
| `+0x50` | `0x180008700` | **setClockSource(uint)** → `section 0x400, id 6`, 4-byte payload |
| `+0x58` | `0x1800087A0` | getClockSource → `*(u16*)(device+0x150)` |
| `+0x60` | `0x1800087B0` | isSynced → `*(int*)(device+0x154) != 0` |
| `+0x138` | `0x180004350` | generic property exchange (`0x401` opcode + property id; used by the Notifier for latency, firmware, safe mode, channel lists) |
| `+0x148` | `0x180008BF0` | get `*(int*)(driver+0x28)` (latency-in samples) **[inf]** |
| `+0x150` | `0x180008C00` | get `*(int*)(driver+0x2C)` (latency-out samples) **[inf]** |
| **`+0x158`** | `0x180008C10` | **getBufferSize → `*(int*)(driver+0x24)`** |
| **`+0x160`** | **`0x180008BA0`** | **setBufferSize(uint)** → DICE command `section 0x400, id 9`, 4-byte payload |
| `+0x190` | `0x1800087C0` | onNotification(bitmask): re-reads clock select/lock, accumulates flags |
| `+0x198` | `0x180008460` | read command (section/id/length/out buffer) |

So `FUN_14001cdd0` in the Notifier calling `(pal->vtbl[0x160])(pal, samples)` maps to
`0x180008BA0`, which is exactly:

```c
void Pal::DiceDevice::setBufferSize(uint32_t samples) {
    (*(driver->vtbl[3]))(driver, /*token=*/0, /*section=*/0x400, /*id=*/9,
                         &samples, 4, nullptr, nullptr);
}
```

and the getter the Notifier uses (delegate callback `FUN_1400200C0`) is `+0x158` → `driver+0x24`.

## 3. How a command reaches the driver

`DiceDevice::setBufferSize` → `DiceRootDriverWin` slot `+0x18` = `FUN_18000C6C0`
(this is the DICE "command" method; the same path handles sample rate, clock, and stream
stop/start). **[obs]**

1. `FUN_18000C6C0(this, token, section, id, data, len, response, respLen)` builds a request packet:
   16-byte header `{ u64 token; u16 section; u16 id; u32 0; }` followed by the payload
   (4 bytes for buffer size). Allocation is `len+0x10`.
2. It calls `FUN_18000CD00(this, /*op=*/2, packet, packetLen, respBuf, respLen)`.
3. `FUN_18000CD00` ("`Pal::DiceRootDriverWin::ioctl()`") issues the **overlapped buffered ioctl**:
   `DeviceIoControl(hDriver, 0x222008, packet, len, resp, respLen, &bytes, &overlapped)`, waits via
   `GetOverlappedResult(..., TRUE)`, and retries on `ERROR_IO_PENDING` / `ERROR_INVALID_HANDLE`
   for up to 50,000,000 ticks (≈5 s, 100 ns units); otherwise it throws
   `"DeviceIoControl failed fatally!"`.
   `0x222008` decodes as `CTL_CODE(0x22, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS)` — i.e. a
   vendor device type `0x22`, function `0x802`, buffered I/O.
4. The response is `{ u32 status; u32 length; u8 data[] }`; nonzero `status` throws
   `"Device returned an error: N"`.

Other ioctls on the same handle: **[obs]**

| ioctl | CTL_CODE decode | use |
|---|---|---|
| `0x222000` | dev `0x22`, fn `0x800`, buffered | driver version/API handshake. Word 1 (`buf+4`) is compared against `0x7000` with `jbe`; a larger value throws `"Driver API mismatch. PAL %d != Driver minimum %d"` |
| `0x222008` | dev `0x22`, fn `0x802`, buffered | send DICE command (above) |
| `0x22200C` | dev `0x22`, fn `0x803`, buffered | async notification wait (16-byte buffer, overlapped) |

## 4. Opening the driver

`DiceRootDriverWin` slot `+0x08` = `FUN_18000BE70`: **[obs]**

* `CreateFileA(path, 0xC0000000 /*GENERIC_READ|WRITE*/, 0, NULL, OPEN_EXISTING,
  0x40000080 /*FILE_FLAG_OVERLAPPED|FILE_ATTRIBUTE_NORMAL*/, NULL)`
  on the path string stored in the driver object (`std::string` at `+0x178`); retries until
  `_Xtime_get_ticks()` advances 100,000,000 (≈10 s, 100 ns units) and throws
  `"CreateFile error opening driver: N"` / `"Driver opening timed out"`.
* Version handshake with ioctl `0x222000`.
* `(driver->vtbl[3])(driver, 0, 0x400, 1, 0,0,0,0)` — DICE "start/attach" command (id 1).
* Starts `_beginthreadex(FUN_18000D9E0 → thunk_FUN_18000D1D0)` = the notification reader.

The path itself is produced by `DiceDriverManagerWin` using SetupAPI
(`SetupDiGetClassDevsA`/`SetupDiEnumDeviceInterfaces`/`SetupDiGetDeviceInterfaceDetailA`); the GUID
is stored as raw bytes in `.rdata` and was not recovered in this pass. **[unknown]**

## 5. Notification path back to the app (this is what produces the toast)

Reader thread `FUN_18000D1D0`: **[obs]**

1. Loops on `DeviceIoControl(h, 0x22200C, NULL, 0, buf16, 16, NULL, &overlapped)` +
   `GetOverlappedResult` (waits for the driver to push a notification).
2. Parses the 16-byte notification: `u32 type`, `u32 item`, `u64 value` (layout as used:
   `local_98`, `local_94`, `local_90`).
3. Dispatching:
   * type `1` → refresh all (`FUN_18000AA60`, `AD70`, `AB30`, `B4A0`)
   * type `2` → refresh `AA60`, `AD70`, `AB30`
   * type `8` → refresh `AA60`, `AB30`
   * type `0x20` → for each device bound to this driver (`driver+0xC0` vector of `shared_ptr`s),
     call `device->vtbl[0x190]` (`DiceDevice::onNotification`) with `local_94` as bitmask.
4. `FUN_18000AB30` is the change detector. It compares live caches against snapshots and calls the
   **delegate virtual methods on every device** (`device->delegate->vtbl[slot]`): **[obs]**

| cache | snapshot | delegate slot | meaning |
|---|---|---|---|
| `driver+0x18` | `driver+0x70` | `+0x28` | `clockSourceChanged()` |
| `driver+0x1C` | `driver+0x74` | `+0x20` | `sampleRateChanged()` |
| `driver+0x20` | `driver+0x78` | — (state only) | |
| **`driver+0x24`** | **`driver+0x7C`** | **`+0x38`** | **`bufferSizeChanged()`** |

Assembly at `0x18000ACCC`: `mov rax,[rcx]; mov rax,[rax+0x38]; call …` inside the loop over
`driver+0xC0`. That is the call that lands in the Notifier's `FUN_1400200C0` and produces
`WM_APP+2 (0x402)` with flag `0x40` → the tray balloon.

The `Pal::DeviceDelegate` base vtable (`0x180013B00`) has `[0]=dtor`, `[1..8]=0x1800012E0`
(empty default handlers). The Notifier overrides `+0x08, +0x10, +0x18, +0x20, +0x28, +0x30, +0x38`,
so the mapping above is consistent with the Notifier-side identification done earlier.

## 6. End-to-end, corrected

```
Focusrite Notifier  FUN_14001cdd0   (combo 1007, CBN_CLOSEUP, item data = samples)
  → Pal::Device::vtbl[0x160]  = Pal::DiceDevice 0x180008BA0   setBufferSize(samples)
    → Pal::DiceRootDriverWin::vtbl[0x18] = 0x18000C6C0  builds {token, 0x400, 9, 0} + u32
      → Pal::DiceRootDriverWin::ioctl  0x18000CD00
        → DeviceIoControl(hFocusriteUsb, 0x222008, req, ...)      [METHOD_BUFFERED]
          FocusriteUsb.sys (KMDF) applies / forwards to the DICE stream
Pal notification thread 0x18000D1D0
  ← DeviceIoControl(hFocusriteUsb, 0x22200C, NULL, 0, notif16, ...)
  → DiceDevice::onNotification (0x1800087C0) updates caches
  → FUN_18000AB30 compares driver+0x24 vs +0x7C
    → delegate->vtbl[0x38] = Notifier 0x1400200C0   bufferSizeChanged()
      → broadcast 0x402 / 0x40 → tray FUN_14000ba60 → Shell_NotifyIconA(NIF_INFO) toast
```

Key nuance: Pal's `setBufferSize` **does not** wait for or return a confirmation. The toast comes
from the independent driver→Pal notification, so an external buffer-size change would produce the
same balloon.

## 7. Driver file the handle belongs to

`Drivers/FocusriteCustom.inf` / `FocusriteMedia.inf` + `Drivers/amd64/`:

| file | service | role |
|---|---|---|
| `FocusriteUsb.sys` | `FocusriteUsb` | USB root driver Pal opens; source paths `source\driver\usb\audstream.cpp`, `isocurb.cpp`, `UsbAudioDevice.cpp`; KMDF (`WDFLDR` imports) |
| `FocusriteUsbAudio.sys` | `FocusriteUsbAudio` | WDM/KS audio adapter (`source\driver\audio\audio_adapter.cpp`) |
| `FocusriteUsbMidi.sys` | `FocusriteUsbMidi` | MIDI function driver |
| `FocusriteUsbSwRoot.sys` | `FocusriteUsbSwRoot` | software root enumerator |
| `FocusritePCIe*.sys` | — | PCIe/Thunderbolt variants (not used by Scarlett Solo) |

`ServiceBinary = %12%\FocusriteUsb.sys` in the INF ⇒ installed as
`C:\Windows\System32\drivers\FocusriteUsb.sys`. Pal's `DiceRootDriverWin` handle is therefore the
`FocusriteUsb` root device; the ioctl codes above are its private command channel.

## 8. Direct driver access (used by `focusrite_buffer.cpp`)

Pal enumerates device interfaces itself; the GUIDs are hard-coded in
`DiceDriverManagerWin`'s ctor `0x180008F80`, which pushes three 16-byte values into a
`std::vector<GUID>` at `+0x80`: **[obs]**

| GUID | role |
|---|---|
| `{429A7462-F176-411E-8DA9-AE6C0A258098}` | PCIe control interface (`\pal`) on `ROOT\FOCUSRITEPCIENEW\0000` |
| `{AC4D0455-50D7-4498-B3CD-9A41D130B759}` | USB control interface (`\pal`) on `ROOT\FOCUSRITEUSBNEW\0000`; also `\usbdev` on `USB\VID_1235&PID_8211\…` |
| `{429A7462-F176-411F-8DA9-AE6C0A258098}` | third interface (not present on the test machine) |

Note the byte layout in the ctor: the four dword stores for the USB GUID are
`0AC4D0455h, 449850D7h, 419ACDB3h, 59B730D1h`, so **Data3 = 0x4498** (reading the
second word of the second dword), not 0x9844. The live registry confirms
`{ac4d0455-50d7-4498-…}`.

`0x1800099F0` iterates them: `SetupDiGetClassDevsA(&guid, NULL, NULL, 0x12
/*DIGCF_PRESENT|DIGCF_DEVICEINTERFACE*/)`, then `0x180008D60` does
`SetupDiEnumDeviceInterfaces` + `SetupDiGetDeviceInterfaceDetailA` and returns the first
`DevicePath` containing `\pal`. Runtime layout on a Scarlett Solo 3rd Gen:

```
\\?\root#focusriteusbnew#0000#{ac4d0455-50d7-4498-…}\pal    <- control (Pal opens this)
\\?\usb#vid_1235&pid_8211#<serial>#{ac4d0455-50d7-4498-…}\usbdev
\\?\root#focusritepcienew#0000#{429a7462-f176-411e-…}\pal   <- phantom when no PCIe card
```

Command ids in `section 0x400` now known **[obs]**: `1` = attach (sent by
`FUN_18000BE70` right after the version ioctl), `2` = detach (sent on close,
`FUN_18000C500`), `4` = read the 0x58-byte configuration blob (`FUN_18000AA60`) into
`driver+0x10`, `5` = set sample rate, `6` = set clock source, `9` = set buffer size.

The config blob offsets follow from the driver cache fields the getters read:
`+0x0C` sample rate (`+0x1C`), `+0x14` buffer size (`+0x24`); both are
`blob_offset - 0x10`. Reading `{0x400, 4}` therefore gives a read-back for verification.

Constraints for a standalone client **[obs/inf]**: `CreateFileA` uses share mode `0`, so the
handle is exclusive and conflicts with a running Notifier/Focusrite Control.

The `ioctl 0x222000` handshake reads 16 bytes and uses them as follows (`0x18000BE70`,
observed): `cmp dword ptr [rsp+0x54], 0x7000; jbe <continue>` — i.e. **word 1 must be at or
below Pal's built-in `0x7000`**, not at or above it. If word 1 is greater, Pal builds
`"Driver API mismatch. PAL 28672 != Driver minimum <word 0>"` and throws. Words 2 and 3 are
copied as one u64 into the device object at `+0x1A8` and never read again; word 0 is only
printed by that message. See `FocusriteUsbSwRoot.sys.md` for what the driver puts
in the four words.

### Runtime observations (test machine, driver 4.143.0.0)

* The live interface path confirms the reference string: the only interface present was
  `\\?\root#focusritepcienew#0000#{429A7462-F176-411E-8DA9-AE6C0A258098}\pal`.
* That node is a **root-enumerated** PCIe device (`ROOT\FOCUSRITEPCIENEW\0000`); `CreateFile`
  on it returns `1359` (`STATUS_INTERNAL_ERROR`) with `GENERIC_RW`, `FILE_READ_DATA|FILE_WRITE_DATA|
  SYNCHRONIZE` and with access `0` — i.e. the driver's create dispatch rejects it because the
  device is not backed by working hardware. Do not treat 1359 as "wrong path".
* No `{AC4D0455-…}` or `{429A7462-…-411F}` interfaces were registered at all on that machine.
* Installed `FocusriteUsb.sys` reports file version **4.143.0.0**, while Device Manager /
  `DEVPKEY_Device_DriverVersion` reports **4.143.0.261** — the package version, matching the
  shipped copy. The `0x222000` descriptor is `0x7000 0x7000 0x0105 0x8048F000`; words 0 and
  1 are the API words, word 2 is the `261` build number, word 3 is an unexplained stamp.
* A standalone client works end-to-end: open
  `\\?\root#focusriteusbnew#0000#{AC4D0455-50D7-4498-…}\pal`, send `{0x400, 9, u32 samples}`,
  and the Notifier shows its "new audio settings" balloon — i.e. the driver notified Pal's
  `DiceRootDriverWin` and `bufferSizeChanged` fired, exactly as traced in §5.
* Read-back (`{0x400, 4}` into 0x58 bytes) returns `status = 0` but `length = 0`;
  the driver still writes the full 0x60 bytes (`io = 96`). Pal ignores `length` and copies the
  requested size, so a client must do the same. Observed config layout on a Solo 3rd Gen
  (44100 Hz / 512 samples):

  ```
  0x00 F8FCC000  0x04 FFFFA40E  0x08 0x29 (41)
  0x0C 0xAC44 (sample rate)     0x10 0
  0x14 0x200  (buffer size)     0x18 0x10
  ```

  `setBufferSize` = `{0x400, 9, u32}`; reading back after a 250 ms wait shows the new value,
  and the Notifier's settings dropdown reflects the change.
* Sample rate uses the same path: `{0x400, 5, u32 Hz}` (Pal slot `+0x40` → `0x180008650`).
  Verified 44100 → 48000: config `+0x0C` reads back 48000, the Notifier dropdown updates and
  the `sampleRateChanged` balloon fires. Apply the rate before the buffer size, since the
  driver can clamp the buffer range after a rate change.

## 9. Still unknown

* The meaning of `section 0x400` ids `1/2/4` at the driver level (the wire/IOCTL framing is
  known; `FocusriteUsb.sys` is KMDF and contains no literal `0x222008`/`0x22200C` constants,
  so the ioctl dispatch was not traced there).
* Property ids passed through slot `+0x138` (`0x401` opcode with ids `0xD, 0x12, 0x18, 0x1A, 0x1E,
  0x1F, 0x20, 0x23`) — only the call sites from the Notifier are known.
* Whether `0x400/1` and `0x400/2` are required before a set, and whether the driver persists
  the buffer size across reboot/ASIO sessions.
