# focusrite_buffer (Rust)

Rust port of `tools/focusrite_buffer.cpp`. Sets a Focusrite interface's
streaming buffer size and/or sample rate by issuing the same private ioctls
Focusrite's Pal layer uses — no `FocusritePal64.dll`.

Protocol reference: `analysis/FocusritePal64.dll.md`.

## Build

Requires Rust 1.85 or newer (edition 2024). It links against the Windows SDK
import libraries, so you need either a native MSVC toolchain or a MinGW
cross-compiler. Dependencies (`windows` 0.58, `anyhow`) come from crates.io and
are pinned in `Cargo.lock`.

### On Windows (MSVC, recommended)

With the Visual Studio Build Tools installed (the same `cl.exe` the C++ version
uses):

```bat
rustup target add x86_64-pc-windows-msvc
cd tools\focusrite-buffer-rs
cargo build --release
```

Result: `target\release\focusrite_buffer.exe`.

### On Debian/Ubuntu (cross-compile a MinGW build)

```bash
sudo apt-get install -y gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cd tools/focusrite-buffer-rs
cargo build --release --target x86_64-pc-windows-gnu
```

Result: `target/x86_64-pc-windows-gnu/release/focusrite_buffer.exe`. This is the
build checked in next to this README; it runs on Windows (Rust's std is linked
statically, only system DLLs are imported) but is not the MSVC artifact you get
from the native build. On Fedora the package is `mingw64-gcc`; other
distributions use a similar name.

## Run

```bat
focusrite_buffer 256                          :: buffer size (legacy form)
focusrite_buffer buffer 256
focusrite_buffer rate 48000
focusrite_buffer rate 48000 buffer 256        :: rate first, then buffer
```

Close Focusrite Notifier / Focusrite Control first: Pal opens the control
interface with share mode 0, so a second handle fails with error 32.

Expected output:

```
installed versions:
  C:\Windows\System32\drivers\FocusriteUsb.sys  4.143.0.261
Focusrite interface classes (registry):
  {ac4d0455-50d7-4498-...}  [pal]
    ##?#ROOT#FOCUSRITEUSBNEW#0000#{ac4d0455-...}
interfaces found:
  AC4D0455-50D7-4498 [active] \pal
    \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
trying: \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
  open rw      -> ok
device:     \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
driver:     0x00007000 0x00007000 0x00000105 0x8048F000 (API 0x7000)
  [0x400/0x4] status=0 len=0 io=96
before:     sample rate 44100, buffer 512
  [0x400/0x9] status=0 len=0 io=8
set:        buffer size 256 samples
  [0x400/0x4] status=0 len=0 io=96
after:      sample rate 44100, buffer 256
ok: verified
```

## How it maps to the C++ version

| C++ | Rust |
|---|---|
| `SetupDi*` + registry walk | same calls via `windows` |
| `CreateFileA` | `CreateFileW` (device paths are Unicode) |
| `OVERLAPPED` + event + `GetOverlappedResult` | same, with a 5 s timeout |
| `malloc` + `free` | `Vec<u64>`/`Vec<u8>` |
| `CloseHandle`/`RegCloseKey`/`SetupDiDestroyDeviceInfoList` on every path | `Drop` guards (`Handle`, `RegKey`, `DeviceInfoSet`) |

Behaviour is deliberately identical: registry discovery, the `\pal`
reference-string filter, tolerance for the phantom PCIe node (error 1359),
`API >= 0x7000` check, the three access-mask attempts, and the command trace.
The response `length` field is ignored when copying, because the driver leaves
it at 0 and writes `8 + requested` bytes (Pal does the same).

## Status

- Type-checked with `cargo check --target x86_64-pc-windows-msvc`.
- Pure helpers (GUID parsing, `\pal`/`vid_1235` matching, config offsets, argv
  parsing) were exercised separately on Linux.
- Not executed here (no Windows host). The write path is the same frame the C++
  version was verified with: `{0x400, 9, u32}` for the buffer,
  `{0x400, 5, u32 Hz}` for the sample rate, read back from the config blob at
  `+0x14` and `+0x0C`.
