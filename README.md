# focusrite_settings

Set a Focusrite audio interface's streaming buffer size and/or sample rate from
the command line, by issuing the same private ioctls Focusrite's own Pal layer
uses — without linking or loading `FocusritePal64.dll`.

```bat
focusrite_settings buffer 256            :: set the buffer
focusrite_settings rate 48000            :: set the sample rate
focusrite_settings rate 48000 buffer 256 :: both, rate first
```

![Diagram of one run: parse the command line, find the interface through the
registry and SetupAPI, open one handle, send the ioctl requests with their
16-byte command header, and report the exit code](docs/diagram.jpg)

*What a run does: parse the arguments, find the interface, open one handle, send
nine ioctls, then report what the device actually took.*

Useful when Focusrite Control is not installed, when you want the settings
scriptable, or when you want to know what your interface is actually running at.

> **Unofficial.** This is not affiliated with or endorsed by Focusrite. It talks
> to undocumented driver internals that were recovered by static analysis (see
> [`docs/`](docs/)), not to a published API. A driver update can change or break
> any of it. It sets hardware state and is used at your own risk.

The `docs/` folder holds the analysis this is built on and the original C++
version of the tool; the Rust port here is the maintained one.

## Build

Requires Rust 1.88 or newer (edition 2024 with let-chains) and Windows 10/11 on
x64. It links against the Windows SDK import libraries, so you need either a
native MSVC toolchain or a MinGW cross-compiler. Dependencies (`windows` 0.58, `anyhow`) come from crates.io and
are pinned in `Cargo.lock`; pass `--locked` so a build never silently resolves
different versions.

### On Windows (MSVC, recommended)

With the Visual Studio Build Tools installed (the same `cl.exe` the C++ version
in `docs/` uses):

```bat
rustup target add x86_64-pc-windows-msvc
cargo build --release --locked
```

Result: `target\release\focusrite_settings.exe`.

### On Debian/Ubuntu (cross-compile a MinGW build)

```bash
sudo apt-get install -y gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --locked --target x86_64-pc-windows-gnu
```

Result: `target/x86_64-pc-windows-gnu/release/focusrite_settings.exe`. This runs on
Windows (Rust's std is linked statically, only system DLLs are imported) but is
not the MSVC artifact you get from the native build. On Fedora the package is
`mingw64-gcc`; other distributions use a similar name.

## Test

```bat
cargo test --locked
```

`cargo test` runs on any host. The Win32 layer is `#[cfg(windows)]`, so off
Windows the crate compiles with `cli`, `protocol` and `interface` only, and the
harness runs their tests; on Windows the same command type-checks the platform
layer too. Running the binary is the only thing that needs Windows.

The tests cover the platform-free helpers only — GUID parsing, the `\pal` and
`vid_1235` matchers, path handling, little-endian reads, the config and driver
descriptor parsers, and the whole command-line grammar. Nothing that opens a
device is tested; that needs a real Scarlett Solo and an exclusive handle, so it
stays a manual step.

## Run

```bat
focusrite_settings 256                           :: buffer size (legacy form)
focusrite_settings buffer 256
focusrite_settings rate 48000
focusrite_settings rate 48000 buffer 256         :: rate first, then buffer
focusrite_settings --help
```

Close Focusrite Notifier / Focusrite Control first: Pal opens the control
interface with share mode 0, so a second handle fails with error 32.

Exit codes:

| code | meaning |
|---|---|
| `0` | everything requested is in effect (or the read-back was unavailable) |
| `1` | the driver rejected a command, or a request failed |
| `2` | the command line made no sense (the reason is printed above the usage) |
| `3` | the driver accepted the commands but the read-back shows other values |

Exit code `3` is new in this port: a request that silently did not stick used to
exit `0` with a `note:` line. The note is still printed.

### Output

The shape of a successful run (this is from the C++ tool's verified session; the
Rust port prints the same trace, with the differences listed below):

```
installed versions:
  C:\Windows\System32\drivers\FocusriteUsb.sys  4.143.0.261
  C:\Program Files\Focusrite\Drivers\FocusritePal64.dll  (not installed)
Focusrite interface classes (registry):
  {ac4d0455-50d7-4498-b3cd-9a41d130b759}  [pal]
    ##?#ROOT#FOCUSRITEUSBNEW#0000#{ac4d0455-...}
interfaces found:
  AC4D0455-50D7-4498 [active] \pal
    \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
trying: \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
  open rw      -> ok
device:     \\?\root#focusriteusbnew#0000#{ac4d0455-...}\pal
driver:     0x00007000 0x00007000 0x00000105 0x8048F000
             api 0x7000, wants Pal API <= 0x7000 (this tool: 0x7000), build 261, stamp 0x8048F000
  [0x400/0x1] status=0 len=0 io=8
  [0x400/0x4] status=0 len=0 io=96
before:     sample rate 44100, buffer 512
  [0x400/0x9] status=0 len=0 io=8
set:        buffer size 256 samples
  [0x400/0x4] status=0 len=0 io=96
after:      sample rate 44100, buffer 256
ok: verified
  [0x400/0x2] status=0 len=0 io=8
```

## Layout

| file | what lives there | builds off Windows |
|---|---|---|
| `src/main.rs` | entry point, exit codes, `#[cfg(windows)]` gates | yes |
| `src/cli.rs` | `Options`, `Invocation`, `Error`, argument parsing, usage text, exit codes | yes |
| `src/protocol.rs` | `Samples`/`RateHz`/`Cmd`/`Command`, `Config`, `DriverVersion`, ioctl and blob constants, byte readers | yes |
| `src/interface.rs` | `Guid` and its parser, UTF-16 path helpers, `\pal`/`vid_1235` matchers | yes |
| `src/app.rs` | `run`: open, attach, read, apply, verify; the attach guard | no |
| `src/win/mod.rs` | the only `#[cfg(windows)]` module root; GUID conversion, small casts | no |
| `src/win/guards.rs` | `Handle`, `DeviceInfoSet`, `RegKey` RAII owners | no |
| `src/win/ioctl.rs` | `ioctl_xfer`, the timeout drain, `command`, `read_config`, Win32 error mapping | no |
| `src/win/discovery.rs` | registry walk, `SetupAPI` enumeration, the open attempts, `Opened` | no |
| `src/win/version.rs` | the installed-versions block | no |

`cli`, `protocol` and `interface` never mention `windows`, which is what makes
`cargo test` possible off Windows. `win` is also the only module containing
`unsafe`, so the platform boundary and the safety boundary are the same line.

## Docs

Everything this tool believes about the driver is in here, with the artifact
sha256 and the address each claim comes from:

| file | what it is |
|---|---|
| [`docs/FocusritePal64.dll.md`](docs/FocusritePal64.dll.md) | Pal: the command framing, every slot this tool mirrors, the notification path, and the `0x222000` handshake |
| [`docs/FocusriteUsbSwRoot.sys.md`](docs/FocusriteUsbSwRoot.sys.md) | the driver that answers the ioctls, and what the 16-byte descriptor's four words mean |
| [`docs/Focusrite Notifier.exe.md`](<docs/Focusrite Notifier.exe.md>) | the tray app: how its settings window talks to Pal, kept because it is the reference for what the settings mean |
| [`docs/focusrite_buffer.cpp`](docs/focusrite_buffer.cpp) | the original C++ tool, which only handled the buffer size; kept as the reference for the wire format |

The notes on the binaries are reverse-engineering output, not documentation from
Focusrite; where a claim was not established they say so, and
[`docs/NOTICE.md`](docs/NOTICE.md) spells out that no Focusrite binary is
redistributed here.

## How it maps to the C++ version

| C++ | Rust |
|---|---|
| `SetupDi*` + registry walk | same calls via `windows` |
| `CreateFileA` | `CreateFileW` (device paths are Unicode) |
| `OVERLAPPED` + event + `GetOverlappedResult` | the same in `ioctl_xfer`, with a 5 s timeout |
| `malloc` + `free` on every path | `Vec<u8>`/`Vec<u64>` |
| `CloseHandle`/`RegCloseKey`/`SetupDiDestroyDeviceInfoList` by hand | `Drop` guards (`Handle`, `RegKey`, `DeviceInfoSet`, `Attachment`) |
| `unsigned short id` parameters | `Command` enum, so an id and its payload cannot get out of step |
| `unsigned int info[4]` | `DriverVersion` |

The wire format is unchanged: 16-byte `{u64 token, u16 section, u16 id, u32 0}`
header plus payload, ioctl `0x222008` on a handle opened with share mode 0, and
the `0x222000` handshake first. The response `length` field is ignored when
copying, because the driver leaves it at 0 and writes `8 + requested` bytes (Pal
does the same).

## Intentional differences from the C++ tool

Protocol behaviour is otherwise identical, token for token. These are the
deliberate divergences:

1. **A timed-out request is drained before its buffers are freed.** `CancelIo`
   only *requests* cancellation; it can return while the driver is still writing
   into the output buffer. The Rust port waits for the request to complete
   (`cancel_and_drain`) and aborts the process if the driver never does, rather
   than free a buffer the kernel may still write to. The C++ tool has the same
   drain (`CancelIo` + `GetOverlappedResult(..., TRUE)`); the first draft of the
   port had dropped it.
2. **`CancelIo` semantics depend on the calling thread**, so every ioctl here is
   issued and waited on synchronously from the main thread. Do not move a
   command into a worker thread without revisiting that.
3. **The driver API gate is enforced, in Pal's direction.** The descriptor's
   word 1 is what the driver requires of Pal; Pal accepts it when it is at or
   below its own `0x7000` (`cmp [buffer+4], 0x7000; jbe continue`) and throws
   "Driver API mismatch" otherwise. The port applies the same rule and exits `1`;
   the C++ tool only prints the word. The descriptor's four words are decoded in
   `docs/FocusriteUsbSwRoot.sys.md`: word 0 is the driver's own API, word 1
   the requirement, word 2 the build number (`261`), word 3 an unexplained
   `0x8048F000` that Pal stores and never reads.
4. **A path without a NUL within the buffer the API filled is skipped** instead
   of being scanned past the end of the allocation.
5. **Output details:** missing files get a `(not installed)` line rather than
   nothing, a read-back mismatch prints one comma-separated list, and the usage
   text lists `--help` and the exit codes.
6. **Registry-driven GUID discovery** was already in the port and not in the
   original: the class GUIDs are found under `DeviceClasses`, with the three GUIDs
   Pal hard-codes as a fallback.

## Design notes

- **Synchronous by design.** This is a binary crate that blocks on
  `DeviceIoControl`; an async runtime would add a task, a reactor and a
  `Send`/`'static` boundary for nothing, and would break `CancelIo`'s
  same-thread requirement.
- **The 250 ms settle between commands** is the C++ tool's `Sleep(250)`, with
  the same reason: without it the read-back can still report the previous value.
- **The 5 s timeout** is Pal's own wait; a device that needs longer is a device
  that is not answering.
- **`opt-level = "s"`, `strip = true`, `panic = "abort"`.** This is a
  few-hundred-millisecond CLI that owns a device handle and exits; size beats
  speed, and there is nothing to unwind for. The cost of `panic = "abort"` is
  that a panic hook does not run in release builds.
- **`overflow-checks = true` in release.** The frame arithmetic is small and
  provably in bounds, but the tool runs once per change and a wrap-around would
  be far more expensive than the checks.
- **No `arithmetic_side_effects` lint.** Enabling it produces nine hits, all on
  buffer-length arithmetic and loop counters that cannot overflow in practice;
  the noise is not worth it. `indexing_slicing` is denied instead, which forces
  the `get`/`split_at_checked` style where it matters.
- **No `default-features = false` on `windows`.** Its only default feature is
  `std`, which the crate needs.
- **One crate, three platform-free modules.** A second crate or a `lib` target
  would force `pub` and `missing_docs` on everything the binary touches; `mod`
  files keep the surface `pub(crate)` while still letting the pure half build
  anywhere.

## Status

- `cargo clippy --all-targets` is clean under `all`, `pedantic` and `nursery`
  with `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` and friends
  denied.
- `cargo test` runs the 20 parser/helper tests on a Linux host (all passing);
  the Win32 layer is type-checked for `x86_64-pc-windows-msvc` and linked for
  `x86_64-pc-windows-gnu`, but has no automated test.
- Not executed against a device here (no Windows host). The write path is the
  same frame the C++ version was verified with: `{0x400, 9, u32}` for the buffer,
  `{0x400, 5, u32 Hz}` for the sample rate, read back from the config blob at
  `+0x14` and `+0x0C`.
- `cargo audit` has not been run against the locked dependencies.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
