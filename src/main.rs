//! `focusrite_buffer` - Rust port of `tools/focusrite_buffer.cpp`.
//!
//! Changes the streaming buffer size and/or sample rate of a Focusrite
//! interface by issuing the same private ioctls Focusrite's Pal layer uses.
//! No `FocusritePal64.dll` involved.
//!
//! Protocol reference: `analysis/FocusritePal64.dll.md`.
//!
//! This crate is the implementation to maintain; the C++ file next to it is the
//! original reference for the wire format. `README.md` lists every intentional
//! difference between the two.
//!
//! Build: `cargo build --release --target x86_64-pc-windows-msvc`
//!
//! Usage:
//! - `focusrite_buffer 256`
//! - `focusrite_buffer buffer 256`
//! - `focusrite_buffer rate 48000`
//! - `focusrite_buffer rate 48000 buffer 256`
//! - `focusrite_buffer --help`

// The shipped binary must not panic: `unwrap`, `expect`, `panic!`, indexing and
// unchecked arithmetic are denied below in `Cargo.toml`. Tests are exempt,
// because a failing assertion is exactly what a test is for.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

use std::env;
use std::ffi::c_void;
use std::fmt;
use std::io;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SPINT_ACTIVE, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_NO_MORE_ITEMS, ERROR_SHARING_VIOLATION, GENERIC_READ,
    GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_SHARE_MODE,
    FILE_WRITE_DATA, GetFileAttributesW, GetFileVersionInfoSizeW, GetFileVersionInfoW,
    INVALID_FILE_ATTRIBUTES, OPEN_EXISTING, SYNCHRONIZE, VS_FIXEDFILEINFO, VerQueryValueW,
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

/// `CTL_CODE(0x22, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`: driver descriptor.
const IOCTL_GET_VERSION: u32 = 0x0022_2000;
/// `CTL_CODE(0x22, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS)`: DICE command.
const IOCTL_COMMAND: u32 = 0x0022_2008;

/// Section Pal's device base class uses for every command in this tool.
const SECTION_APP: u16 = 0x0400;

/// `{u64 token, u16 section, u16 id, u32 reserved}`.
const HEADER_LEN: usize = 16;
/// `{u32 status, u32 length}`.
const RESPONSE_HEADER_LEN: usize = 8;

/// Size of the configuration blob `Cmd::GetConfig` returns.
const CONFIG_LEN: usize = 0x58;
/// Offset of the sample rate inside that blob.
const OFF_SAMPLE_RATE: usize = 0x0C;
/// Offset of the buffer size, in samples, inside that blob.
const OFF_BUFFER_SIZE: usize = 0x14;

/// Pal refuses drivers whose API word is older than this.
const MIN_DRIVER_API: u32 = 0x7000;

/// Where the official installer puts the driver.
const DRIVER_PATH: &str = "C:\\Windows\\System32\\drivers\\FocusriteUsb.sys";
/// Where the official installer puts Pal.
const PAL_PATH: &str = "C:\\Program Files\\Focusrite\\Drivers\\FocusritePal64.dll";

/// How long the driver gets to answer one ioctl, as in Pal's `ioctl()`.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a cancelled request gets to finish before the process gives up.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
/// Pause between commands, so the device settles before the next one.
const SETTLE: Duration = Duration::from_millis(250);

/// Buffer sizes the Notifier's own window offers, in samples.
const NOTIFIER_SAMPLES: [u32; 11] = [16, 32, 48, 64, 96, 128, 160, 192, 256, 512, 1024];
/// Sample rates the Notifier's own window offers, in Hz.
const NOTIFIER_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Fallback list of interface GUIDs Pal hard-codes; discovery via the registry
/// is preferred because a driver build may register different ones.
const KNOWN_GUIDS: [GUID; 3] = [
    GUID::from_u128(0xac4d0455_50d7_4498_b3cd_9a41d130b759),
    GUID::from_u128(0x429a7462_f176_411f_8da9_ae6c0a258098),
    GUID::from_u128(0x429a7462_f176_411e_8da9_ae6c0a258098),
];

const REG_NAME_CAP: usize = 128;
const REG_INSTANCE_CAP: usize = 2048;

/// Exit code: everything the user asked for is in effect.
const EXIT_OK: i32 = 0;
/// Exit code: the driver rejected a command, or a request failed.
const EXIT_FAILED: i32 = 1;
/// Exit code: the arguments made no sense.
const EXIT_USAGE: i32 = 2;
/// Exit code: the commands were accepted, but the read-back shows something else.
const EXIT_UNVERIFIED: i32 = 3;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// A streaming buffer size in samples.
///
/// A newtype rather than a bare `u32` because the driver payload for the buffer
/// size and the one for the sample rate are both four little-endian bytes; with
/// plain integers, swapping them would compile and silently do the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Samples(u32);

impl Samples {
    /// The value the driver expects on the wire.
    const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Samples {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A sample rate in Hz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RateHz(u32);

impl RateHz {
    /// The value the driver expects on the wire.
    const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for RateHz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Command ids inside [`SECTION_APP`]. `#[repr(u16)]` keeps the wire numbers in
/// one place instead of scattering them through call sites.
#[repr(u16)]
#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Tell the driver a client is present.
    Attach = 1,
    /// Tell the driver the client is gone.
    Detach = 2,
    /// Read the configuration blob.
    GetConfig = 4,
    /// Set the sample rate.
    SetSampleRate = 5,
    /// Set the buffer size.
    SetBufferSize = 9,
}

/// A complete command: the id plus, for the setters, the value to send.
///
/// Modelling the payload as a variant rather than a `&[u8]` argument means the
/// value and the command id cannot get out of step.
#[derive(Clone, Copy, Debug)]
enum Command {
    Attach,
    Detach,
    GetConfig,
    SetSampleRate(RateHz),
    SetBufferSize(Samples),
}

impl Command {
    /// The wire id of this command.
    const fn cmd(self) -> Cmd {
        match self {
            Self::Attach => Cmd::Attach,
            Self::Detach => Cmd::Detach,
            Self::GetConfig => Cmd::GetConfig,
            Self::SetSampleRate(_) => Cmd::SetSampleRate,
            Self::SetBufferSize(_) => Cmd::SetBufferSize,
        }
    }

    /// Payload bytes that follow the 16-byte header.
    fn payload(self) -> Vec<u8> {
        match self {
            Self::Attach | Self::Detach | Self::GetConfig => Vec::new(),
            Self::SetSampleRate(rate) => rate.get().to_le_bytes().to_vec(),
            Self::SetBufferSize(samples) => samples.get().to_le_bytes().to_vec(),
        }
    }

    /// Payload bytes the driver returns; 0 for commands without a reply.
    const fn response_len(self) -> usize {
        match self {
            Self::GetConfig => CONFIG_LEN,
            Self::Attach | Self::Detach | Self::SetSampleRate(_) | Self::SetBufferSize(_) => 0,
        }
    }
}

impl fmt::Display for Command {
    /// Prints as `0x400/0x9`, the form the driver trace uses.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{SECTION_APP:X}/0x{:X}", self.cmd() as u16)
    }
}

/// The part of the driver's configuration blob this tool cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Config {
    sample_rate: RateHz,
    buffer_size: Samples,
}

impl Config {
    /// Parse the blob `Cmd::GetConfig` returns.
    fn from_blob(blob: &[u8]) -> Result<Self> {
        if blob.len() < CONFIG_LEN {
            bail!("config blob is {} bytes, expected {CONFIG_LEN}", blob.len());
        }
        let sample_rate =
            rd32(blob, OFF_SAMPLE_RATE).context("sample rate missing from the config blob")?;
        let buffer_size =
            rd32(blob, OFF_BUFFER_SIZE).context("buffer size missing from the config blob")?;
        Ok(Self {
            sample_rate: RateHz(sample_rate),
            buffer_size: Samples(buffer_size),
        })
    }
}

/// The 16-byte descriptor `IOCTL_GET_VERSION` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DriverVersion {
    /// Printed for support; the meaning is not known.
    word0: u32,
    /// API version, the word Pal compares against [`MIN_DRIVER_API`].
    api: u32,
    /// Printed for support; the meaning is not known.
    word2: u32,
    /// Printed for support; the meaning is not known.
    word3: u32,
}

impl DriverVersion {
    /// Bytes the driver writes for the descriptor.
    const SIZE: usize = 4 * size_of::<u32>();

    /// Decode the descriptor, or `None` when the driver wrote too little.
    fn parse(raw: &[u8]) -> Option<Self> {
        Some(Self {
            word0: rd32(raw, 0)?,
            api: rd32(raw, size_of::<u32>())?,
            word2: rd32(raw, 2 * size_of::<u32>())?,
            word3: rd32(raw, 3 * size_of::<u32>())?,
        })
    }
}

/// A control interface that is open and has answered the version probe.
struct Opened {
    handle: Handle,
    /// NUL-terminated device interface path the handle was opened on.
    path: Vec<u16>,
    version: DriverVersion,
}

// ---------------------------------------------------------------------------
// RAII wrappers
// ---------------------------------------------------------------------------

/// Owns a Win32 handle and closes it on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `Handle` is the only owner of this handle and is never cloned.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Owns a `SetupAPI` device information set and destroys it on drop.
struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: `DeviceInfoSet` is the only owner and is never cloned.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Owns a registry key and closes it on drop.
struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: `RegKey` is the only owner and is never cloned.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Keeps the driver in the attached state for as long as it lives.
///
/// A guard rather than a `bool` plus a detach at the end of `run`: an early
/// return or an error can then never leave the device attached.
struct Attachment {
    handle: HANDLE,
}

impl Attachment {
    /// Attach, or report why not and return `None` so the caller can decide.
    fn acquire(handle: HANDLE) -> Option<Self> {
        match command(handle, Command::Attach) {
            Ok(_) => Some(Self { handle }),
            Err(error) => {
                eprintln!("attach failed (continuing anyway): {error:#}");
                None
            }
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // The handle outlives this guard, so the raw value is still valid here.
        if let Err(error) = command(self.handle, Command::Detach) {
            eprintln!("detach failed: {error:#}");
        }
    }
}

/// One access mask to try when opening the control interface.
struct AccessAttempt {
    mask: u32,
    label: &'static str,
}

/// Access masks to try, in order: the mask Pal uses, then the narrower data
/// access rights, then no access rights at all.
const ACCESS_ATTEMPTS: [AccessAttempt; 3] = [
    AccessAttempt {
        mask: GENERIC_READ.0 | GENERIC_WRITE.0,
        label: "rw",
    },
    AccessAttempt {
        mask: FILE_READ_DATA.0 | FILE_WRITE_DATA.0 | SYNCHRONIZE.0,
        label: "rw-data",
    },
    AccessAttempt {
        mask: 0,
        label: "none",
    },
];

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// `value` as a `u32`, saturating instead of wrapping.
///
/// Every argument is a buffer length or a struct size, so the saturation branch
/// is unreachable in practice. It exists because the Win32 APIs take `u32` and
/// `as` would truncate silently.
fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// `value` as an `i32`, saturating instead of wrapping.
fn i32_of(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// A duration as the `u32` milliseconds `WaitForSingleObject` expects.
fn timeout_millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

/// The Win32 code behind a `windows` error, with the `HRESULT` wrapper removed.
///
/// windows-rs stores Win32 codes as `HRESULT_FROM_WIN32(code)`, so comparing the
/// raw `HRESULT` against `ERROR_*` constants never matches.
const fn win32_code(error: &windows::core::Error) -> u32 {
    /// Facility 7 (`FACILITY_WIN32`) shifted into place.
    const FACILITY_WIN32: u32 = 0x8007_0000;

    let hresult = error.code().0.cast_unsigned();
    if hresult & 0xFFFF_0000 == FACILITY_WIN32 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}

/// Whether `error` carries the given Win32 code.
const fn is_win32(error: &windows::core::Error, code: WIN32_ERROR) -> bool {
    win32_code(error) == code.0
}

/// Convert a `windows` error into an `io::Error`, keeping the Win32 code.
fn to_io_error(error: &windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(i32_of(win32_code(error)))
}

/// The error reported when the driver does not answer in time.
fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no answer within {} s", COMMAND_TIMEOUT.as_secs()),
    )
}

/// Read a little-endian `u32` at `offset`, or `None` when the buffer is short.
fn rd32(buf: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(size_of::<u32>())?;
    let bytes = buf.get(offset..end)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Split the `{u32 status, u32 length}` header off a command response.
fn response_header(response: &[u8]) -> Option<(u32, u32)> {
    let (status, length) = response
        .get(..RESPONSE_HEADER_LEN)?
        .split_at_checked(size_of::<u32>())?;
    Some((
        u32::from_le_bytes(status.try_into().ok()?),
        u32::from_le_bytes(length.try_into().ok()?),
    ))
}

/// `s` as the NUL-terminated UTF-16 the `...W` APIs take.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `path` without its trailing NUL, for display.
fn path_text(path: &[u16]) -> String {
    let body = path.split_last().map_or(path, |(_, rest)| rest);
    String::from_utf16_lossy(body)
}

/// A space-separated decimal list, for the usage text.
fn numbers(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// ASCII case-insensitive substring search on bytes.
fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    // `windows(0)` panics, and a short haystack cannot contain the needle.
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
    let trimmed = text.trim_matches(['{', '}']);
    let parts: Vec<&str> = trimmed.split('-').collect();
    let [data1, data2, data3, clock_seq, node] = parts.as_slice() else {
        return None;
    };
    if [
        data1.len(),
        data2.len(),
        data3.len(),
        clock_seq.len(),
        node.len(),
    ] != [8, 4, 4, 4, 12]
    {
        return None;
    }

    // The last 16 hex digits are the 8-byte node, in order.
    let node = u64::from_str_radix(&format!("{clock_seq}{node}"), 16).ok()?;

    Some(GUID::from_values(
        u32::from_str_radix(data1, 16).ok()?,
        u16::from_str_radix(data2, 16).ok()?,
        u16::from_str_radix(data3, 16).ok()?,
        node.to_be_bytes(),
    ))
}

/// Print the file version of `path`, or why there is none.
fn print_file_version(path: &str) {
    let wide_path = wide(path);
    // SAFETY: `wide_path` is NUL-terminated and outlives the call.
    if unsafe { GetFileAttributesW(PCWSTR(wide_path.as_ptr())) } == INVALID_FILE_ATTRIBUTES {
        println!("  {path}  (not installed)");
        return;
    }

    // SAFETY: `wide_path` is NUL-terminated; the second argument is optional.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wide_path.as_ptr()), None) };
    if size == 0 {
        println!("  {path}  (no version resource)");
        return;
    }
    let Some(capacity) = usize::try_from(size).ok() else {
        println!("  {path}  (version unreadable)");
        return;
    };

    let mut data = vec![0u8; capacity];
    // SAFETY: `data` has room for `size` bytes, which is what the API writes.
    let read = unsafe {
        GetFileVersionInfoW(
            PCWSTR(wide_path.as_ptr()),
            0,
            size,
            data.as_mut_ptr().cast::<c_void>(),
        )
    };
    if read.is_err() {
        println!("  {path}  (version unreadable)");
        return;
    }

    let mut block: *mut c_void = ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `data` is the version block returned above; `block` and `len` are
    // valid out-parameters, and the returned pointer points into `data`, which
    // stays alive until this function returns.
    let found = unsafe {
        VerQueryValueW(
            data.as_ptr().cast::<c_void>(),
            windows::core::w!("\\"),
            &raw mut block,
            &raw mut len,
        )
    }
    .as_bool();

    if found
        && !block.is_null()
        && usize::try_from(len).unwrap_or(0) >= size_of::<VS_FIXEDFILEINFO>()
    {
        // SAFETY: the API reported at least one `VS_FIXEDFILEINFO` at `block`.
        let info = unsafe { &*block.cast::<VS_FIXEDFILEINFO>() };
        println!(
            "  {path}  {}.{}.{}.{}",
            info.dwFileVersionMS >> 16,
            info.dwFileVersionMS & 0xffff,
            info.dwFileVersionLS >> 16,
            info.dwFileVersionLS & 0xffff
        );
    } else {
        println!("  {path}  (version unreadable)");
    }
}

// ---------------------------------------------------------------------------
// Overlapped ioctl round trip
// ---------------------------------------------------------------------------

/// One overlapped `DeviceIoControl` round trip, like Pal's `ioctl()`.
///
/// `output` must stay borrowed for the whole call: the driver writes into it
/// when the request completes, not when `DeviceIoControl` returns.
fn ioctl_xfer(handle: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<u32> {
    // SAFETY: a manual-reset event with no name; the `Handle` guard closes it.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|error| to_io_error(&error))?;
    let event = Handle(event);

    // SAFETY: `OVERLAPPED` is a plain C struct; all-zero is its documented
    // "nothing else set" state, and `hEvent` is filled in on the next line.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event.0;

    let mut transferred = 0u32;
    // SAFETY: both buffers outlive the call; on the pending path the request is
    // drained before this function returns, so neither can be freed early.
    let request = unsafe {
        DeviceIoControl(
            handle,
            code,
            Some(input.as_ptr().cast::<c_void>()),
            u32_of(input.len()),
            Some(output.as_mut_ptr().cast::<c_void>()),
            u32_of(output.len()),
            Some(&raw mut transferred),
            Some(&raw mut overlapped),
        )
    };

    match request {
        Ok(()) => Ok(transferred),
        Err(error) if is_win32(&error, ERROR_IO_PENDING) => {
            // SAFETY: the event was created above and is signalled by the
            // I/O manager when this request completes.
            if unsafe { WaitForSingleObject(event.0, timeout_millis(COMMAND_TIMEOUT)) }
                != WAIT_OBJECT_0
            {
                cancel_and_drain(handle, event.0, &overlapped);
                return Err(timed_out());
            }

            // SAFETY: the event is signalled, so the request has completed.
            unsafe {
                GetOverlappedResult(handle, &raw const overlapped, &raw mut transferred, false)
            }
            .map_err(|error| to_io_error(&error))?;
            Ok(transferred)
        }
        Err(error) => Err(to_io_error(&error)),
    }
}

/// Cancel a pending request and wait until the I/O manager has really finished
/// with the caller's buffers.
///
/// `CancelIo` only *requests* cancellation; it can return while the driver is
/// still writing into the output buffer. Freeing that buffer at this point would
/// let the kernel write into freed memory, so this drains the request and, if
/// the driver never completes it, aborts instead of returning.
///
/// `CancelIo` only cancels requests issued by the calling thread, which is why
/// every ioctl in this tool is synchronous on the main thread.
fn cancel_and_drain(handle: HANDLE, event: HANDLE, overlapped: &OVERLAPPED) {
    // SAFETY: `handle` is open and `overlapped` belongs to the pending request.
    let _ = unsafe { CancelIo(handle) };

    // SAFETY: the event belongs to the request being cancelled.
    if unsafe { WaitForSingleObject(event, timeout_millis(CANCEL_TIMEOUT)) } == WAIT_OBJECT_0 {
        let mut transferred = 0u32;
        // SAFETY: the request has completed; `ERROR_OPERATION_ABORTED` is the
        // expected result here and is deliberately ignored.
        let _ = unsafe { GetOverlappedResult(handle, overlapped, &raw mut transferred, false) };
        return;
    }

    eprintln!(
        "the driver never completed a cancelled request; \
         aborting rather than freeing a buffer it may still write to"
    );
    std::process::abort();
}

// ---------------------------------------------------------------------------
// DICE commands
// ---------------------------------------------------------------------------

/// Send one command and return its payload.
fn command(handle: HANDLE, cmd: Command) -> Result<Vec<u8>> {
    let payload = cmd.payload();
    let mut packet = Vec::with_capacity(HEADER_LEN + payload.len());
    // The token is 0 in every Pal call site and the driver does not use it.
    packet.extend_from_slice(&0u64.to_le_bytes());
    packet.extend_from_slice(&SECTION_APP.to_le_bytes());
    packet.extend_from_slice(&(cmd.cmd() as u16).to_le_bytes());
    packet.extend_from_slice(&0u32.to_le_bytes());
    packet.extend_from_slice(&payload);

    // The driver leaves `length` at 0 and writes `8 + requested` bytes, so ask
    // for exactly that and ignore the reported length (Pal does the same).
    let mut response = vec![0u8; RESPONSE_HEADER_LEN + cmd.response_len()];
    let transferred = ioctl_xfer(handle, IOCTL_COMMAND, &packet, &mut response)
        .with_context(|| format!("DeviceIoControl {cmd}"))?;

    let (status, length) =
        response_header(&response).context("command response shorter than its 8-byte header")?;
    println!("  [{cmd}] status={status} len={length} io={transferred}");

    if status != 0 {
        bail!("driver rejected {cmd}: status {status} (0x{status:X})");
    }

    Ok(response.split_off(RESPONSE_HEADER_LEN))
}

/// Read the current sample rate and buffer size from the driver.
fn read_config(handle: HANDLE) -> Result<Config> {
    let blob = command(handle, Command::GetConfig).context("read config blob")?;
    Config::from_blob(&blob)
}

/// Give the device a moment to apply a change.
///
/// Without this the read-back can still report the previous value; the C++ tool
/// sleeps the same 250 ms for the same reason.
fn settle() {
    std::thread::sleep(SETTLE);
}

// ---------------------------------------------------------------------------
// Device discovery
// ---------------------------------------------------------------------------

/// One interface path that might be the control interface.
struct Candidate {
    /// NUL-terminated device interface path, as `CreateFileW` wants it.
    path: Vec<u16>,
    /// Whether the path carries Pal's `\pal` reference string.
    is_pal: bool,
    /// Whether `SetupAPI` reports the interface as active.
    active: bool,
}

/// What one `DeviceClasses` key holds for Focusrite.
#[derive(Clone, Copy, Debug)]
struct ClassScan {
    /// Whether any instance belongs to Focusrite at all.
    focusrite: bool,
    /// Whether one of them has the `\pal` reference string.
    has_pal: bool,
}

/// Scan one interface class key's instances and print what it finds.
fn scan_class_instances(key: HKEY, class_name: &str) -> ClassScan {
    let mut scan = ClassScan {
        focusrite: false,
        has_pal: false,
    };
    let mut other: Option<Vec<u8>> = None;

    for index in 0.. {
        let mut instance = [0u8; REG_INSTANCE_CAP];
        let mut instance_len = u32_of(REG_INSTANCE_CAP);
        // SAFETY: `instance` has room for `REG_INSTANCE_CAP` bytes and
        // `instance_len` says so; the API writes a NUL-terminated name into it.
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

        let instance = instance.get(..usize::try_from(instance_len).unwrap_or(0));
        let Some(instance) = instance else {
            continue;
        };
        if !contains_ci(instance, b"vid_1235") && !contains_ci(instance, b"focusrite") {
            continue;
        }

        scan.focusrite = true;
        if contains_ci(instance, b"#pal") {
            scan.has_pal = true;
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

    scan
}

/// Collect interface class GUIDs that have a Focusrite instance with the
/// `\pal` reference string.
///
/// This finds the right GUID even when the installed driver build differs from
/// the one this tool was written against.
fn registered_focusrite_classes() -> Vec<GUID> {
    let mut classes = HKEY::default();
    // SAFETY: the path is a NUL-terminated literal and `classes` is a valid
    // out-parameter; the key is closed by the guard below.
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
        return Vec::new();
    }
    let classes = RegKey(classes);

    let mut found = Vec::new();
    for index in 0.. {
        let mut name = [0u8; REG_NAME_CAP];
        let mut name_len = u32_of(REG_NAME_CAP);
        // SAFETY: `name` has room for `REG_NAME_CAP` bytes and `name_len` says
        // so; the API writes a NUL-terminated name into it.
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

        let class_name = name
            .get(..usize::try_from(name_len).unwrap_or(0))
            .and_then(|raw| std::str::from_utf8(raw).ok())
            .unwrap_or("");

        let mut sub = HKEY::default();
        // SAFETY: `name` is NUL-terminated by the API above and `sub` is a valid
        // out-parameter; the key is closed by the guard.
        let sub_rc =
            unsafe { RegOpenKeyExA(classes.0, PCSTR(name.as_ptr()), 0, KEY_READ, &raw mut sub) };
        if sub_rc != WIN32_ERROR(0) {
            continue;
        }
        let sub = RegKey(sub);

        let scan = scan_class_instances(sub.0, class_name);
        if scan.focusrite
            && scan.has_pal
            && let Some(guid) = parse_guid(class_name)
            && !found.contains(&guid)
        {
            found.push(guid);
        }
    }

    found
}

/// Interface paths of one interface class.
///
/// The error case carries the Win32 code from `SetupDiGetClassDevsW` so the
/// caller can tell "no such device" from "`SetupAPI` failed".
fn candidates_for(guid: &GUID) -> Result<(Vec<Candidate>, usize), u32> {
    // SAFETY: `guid` outlives the call; the returned set is owned by the guard.
    let set = match unsafe {
        SetupDiGetClassDevsW(
            Some(ptr::from_ref(guid)),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    } {
        Ok(set) => DeviceInfoSet(set),
        Err(error) => return Err(win32_code(&error)),
    };

    let mut candidates = Vec::new();
    let mut skipped = 0usize;
    let mut index = 0u32;
    loop {
        // SAFETY: `SP_DEVICE_INTERFACE_DATA` is a plain C struct; all-zero with
        // `cbSize` set is its documented input state.
        let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        interface.cbSize = u32_of(size_of::<SP_DEVICE_INTERFACE_DATA>());
        // SAFETY: the set is open and `interface` is a valid out-parameter.
        match unsafe {
            SetupDiEnumDeviceInterfaces(set.0, None, ptr::from_ref(guid), index, &raw mut interface)
        } {
            Ok(()) => index += 1,
            Err(error) if is_win32(&error, ERROR_NO_MORE_ITEMS) => break,
            Err(error) => {
                eprintln!(
                    "  setupapi: could not enumerate interface {index}: {}",
                    to_io_error(&error)
                );
                break;
            }
        }

        let mut needed = 0u32;
        // This call is expected to fail with ERROR_INSUFFICIENT_BUFFER: it only
        // reports how much room the variable-length detail struct needs.
        let _sizing = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &raw const interface,
                None,
                0,
                Some(&raw mut needed),
                None,
            )
        };
        let Some(size) = usize::try_from(needed).ok().filter(|size| *size > 0) else {
            continue;
        };

        // The detail struct is variable length and needs 4-byte alignment, so
        // back it with `u64`s and then fill in its `cbSize`.
        let mut buffer: Vec<u64> = vec![0; size.div_ceil(size_of::<u64>())];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();

        // SAFETY: `buffer` has at least `size` bytes and is aligned for the
        // struct; `detail` is the only reference to it.
        unsafe {
            (*detail).cbSize = u32_of(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>());
        }

        // SAFETY: same buffer and size as the sizing call above.
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

        // How many UTF-16 units of path can fit in the buffer the API filled.
        // The scan is bounded by that, so a path without a terminator cannot
        // run off the end of an allocation.
        let units = size.saturating_sub(std::mem::offset_of!(
            SP_DEVICE_INTERFACE_DETAIL_DATA_W,
            DevicePath
        )) / size_of::<u16>();
        // SAFETY: the API wrote `DevicePath` into `buffer`, and `units` is the
        // number of UTF-16 units that buffer has room for from there on.
        let units = unsafe { std::slice::from_raw_parts((*detail).DevicePath.as_ptr(), units) };
        let Some(length) = units.iter().position(|unit| *unit == 0) else {
            skipped += 1;
            continue;
        };
        let Some(path) = units.get(..=length) else {
            continue;
        };
        let path = path.to_vec();

        let is_active = (interface.Flags & SPINT_ACTIVE) != 0;
        if contains_pal(&path) {
            candidates.push(Candidate {
                path,
                is_pal: true,
                active: is_active,
            });
        } else if contains_ci_wide(&path, b"vid_1235") {
            candidates.push(Candidate {
                path,
                is_pal: false,
                active: is_active,
            });
        } else {
            skipped += 1;
        }
    }

    Ok((candidates, skipped))
}

/// Open one candidate path and run the version handshake Pal performs.
fn try_open(path: &[u16], access: u32) -> io::Result<(Handle, DriverVersion)> {
    // SAFETY: `path` is NUL-terminated and outlives the call; share mode 0,
    // OPEN_EXISTING and the overlapped flag mirror Pal's own open call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map(Handle)
    .map_err(|error| to_io_error(&error))?;

    let mut raw = [0u8; DriverVersion::SIZE];
    let transferred = ioctl_xfer(handle.0, IOCTL_GET_VERSION, &[], &mut raw)?;
    // Pal requires at least the first two words of the descriptor.
    if transferred < u32_of(2 * size_of::<u32>()) {
        // The interface exists but does not speak the driver protocol.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("version descriptor is only {transferred} bytes"),
        ));
    }

    let version = DriverVersion::parse(&raw).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "short driver version descriptor",
        )
    })?;
    Ok((handle, version))
}

/// Find and open the driver's control interface.
///
/// Pal only accepts paths with the `\pal` reference string, so those are tried
/// first and the plain `vid_1235` paths are kept as a fallback.
fn open_control_interface() -> Result<Opened> {
    let mut guids = registered_focusrite_classes();
    for known in KNOWN_GUIDS {
        if !guids.contains(&known) {
            guids.push(known);
        }
    }
    let mut candidates = Vec::new();
    println!("\ninterfaces found:");
    for guid in &guids {
        let prefix = format!("{:08X}-{:04X}-{:04X}", guid.data1, guid.data2, guid.data3);
        match candidates_for(guid) {
            Err(code) => println!("  {prefix}: SetupDiGetClassDevs failed (error {code})"),
            Ok((found, skipped)) => {
                if found.is_empty() {
                    println!("  {prefix}: no control interface");
                } else {
                    for candidate in &found {
                        println!(
                            "  {prefix} [{}]{}",
                            if candidate.active {
                                "active"
                            } else {
                                "inactive"
                            },
                            if candidate.is_pal { " \\pal" } else { "" }
                        );
                        println!("    {}", path_text(&candidate.path));
                    }
                    candidates.extend(found);
                }
                if skipped > 0 {
                    println!("  ... {skipped} non-control interface(s) skipped");
                }
            }
        }
    }

    // Prefer `\pal` paths; `sort_by_key` is stable, so discovery order holds
    // within each group.
    candidates.sort_by_key(|candidate| !candidate.is_pal);

    let mut failures: Vec<io::Error> = Vec::new();
    for candidate in &candidates {
        println!("\ntrying: {}", path_text(&candidate.path));
        for attempt in ACCESS_ATTEMPTS {
            match try_open(&candidate.path, attempt.mask) {
                Ok((handle, version)) => {
                    println!("  open {:<7} -> ok", attempt.label);
                    return Ok(Opened {
                        handle,
                        path: candidate.path.clone(),
                        version,
                    });
                }
                Err(error) => {
                    println!("  open {:<7} -> {}", attempt.label, describe(&error));
                    failures.push(error);
                }
            }
        }
    }

    // Being locked out by another client is worth its own message: it is the
    // one failure the user can fix.
    if failures
        .iter()
        .any(|error| error.raw_os_error() == Some(i32_of(ERROR_SHARING_VIOLATION.0)))
    {
        bail!("device is in use - close Focusrite Notifier / Focusrite Control first");
    }
    match failures.last() {
        Some(last) => bail!("could not open a control interface (last error: {last})"),
        None => bail!("could not open a control interface (no candidates)"),
    }
}

/// An `io::Error` as the one-line diagnostic the open loop prints.
fn describe(error: &io::Error) -> String {
    error
        .raw_os_error()
        .map_or_else(|| error.to_string(), |code| code.to_string())
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// What the user asked for. Both fields are optional because either one on its
/// own is a complete request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Options {
    buffer: Option<Samples>,
    rate: Option<RateHz>,
}

/// What one command line means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Invocation {
    /// Print the usage text and stop.
    Help,
    /// Open the device and apply these settings.
    Run(Options),
}

/// Why a command line was rejected.
#[derive(Debug, PartialEq, Eq)]
enum CliError {
    /// A key was given without a value.
    MissingValue { key: &'static str },
    /// A value was not a positive integer.
    NotAPositiveInteger { key: &'static str, value: String },
    /// The same key was given twice.
    Duplicate { key: &'static str },
    /// A token was not a key and not the legacy single-number form.
    Unexpected(String),
    /// Nothing to do.
    NothingToDo,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue { key } => write!(f, "{key} needs a value"),
            Self::NotAPositiveInteger { key, value } => {
                write!(f, "{key}: '{value}' is not a positive integer")
            }
            Self::Duplicate { key } => write!(f, "{key} given twice"),
            Self::Unexpected(token) => write!(f, "unexpected argument '{token}'"),
            Self::NothingToDo => write!(f, "nothing to do"),
        }
    }
}

/// A value that must parse as a non-zero `u32`.
fn positive_integer(key: &'static str, value: &str) -> Result<u32, CliError> {
    match value.parse::<u32>() {
        Ok(parsed) if parsed > 0 => Ok(parsed),
        _ => Err(CliError::NotAPositiveInteger {
            key,
            value: value.to_owned(),
        }),
    }
}

/// Parse the arguments after the program name.
fn parse_invocation(args: &[String]) -> Result<Invocation, CliError> {
    let mut options = Options::default();
    let mut iter = args.iter();

    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "buffer" => {
                if options.buffer.is_some() {
                    return Err(CliError::Duplicate { key: "buffer" });
                }
                let value = iter
                    .next()
                    .ok_or(CliError::MissingValue { key: "buffer" })?;
                options.buffer = Some(Samples(positive_integer("buffer", value)?));
            }
            "rate" => {
                if options.rate.is_some() {
                    return Err(CliError::Duplicate { key: "rate" });
                }
                let value = iter.next().ok_or(CliError::MissingValue { key: "rate" })?;
                options.rate = Some(RateHz(positive_integer("rate", value)?));
            }
            // Legacy form: `focusrite_buffer 256` means the buffer size.
            token if args.len() == 1 => {
                options.buffer = Some(Samples(positive_integer("samples", token)?));
            }
            token => return Err(CliError::Unexpected(token.to_owned())),
        }
    }

    if options.buffer.is_none() && options.rate.is_none() {
        return Err(CliError::NothingToDo);
    }
    Ok(Invocation::Run(options))
}

/// Print the usage text.
fn print_usage() {
    eprintln!("usage: focusrite_buffer <samples>                  (buffer size)");
    eprintln!("       focusrite_buffer buffer <samples>");
    eprintln!("       focusrite_buffer rate <hz>");
    eprintln!("       focusrite_buffer rate <hz> buffer <samples>");
    eprintln!("       focusrite_buffer --help");
    eprintln!();
    eprintln!("  Notifier's buffer list: {}", numbers(&NOTIFIER_SAMPLES));
    eprintln!("  Common rates:           {}", numbers(&NOTIFIER_RATES));
    eprintln!();
    eprintln!("  The driver decides what it accepts; anything else is sent anyway with");
    eprintln!("  a note. When both are given the sample rate is applied first.");
    eprintln!();
    eprintln!("exit codes: {EXIT_OK} verified, {EXIT_FAILED} driver error,");
    eprintln!("            {EXIT_USAGE} usage, {EXIT_UNVERIFIED} read-back differs");
}

/// Whether the Notifier's own window would offer this buffer size.
fn known_size(samples: Samples) -> bool {
    NOTIFIER_SAMPLES.contains(&samples.get())
}

/// Whether the Notifier's own window would offer this sample rate.
fn known_rate(rate: RateHz) -> bool {
    NOTIFIER_RATES.contains(&rate.get())
}

/// Mention values the Notifier would not offer, without refusing them.
fn warn_about_unlisted(options: Options) {
    if let Some(samples) = options.buffer
        && !known_size(samples)
    {
        eprintln!("note: {samples} is not in the Notifier's list, sending anyway");
    }
    if let Some(rate) = options.rate
        && !known_rate(rate)
    {
        eprintln!("note: {rate} Hz is not a common rate, sending anyway");
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Send the requested changes, rate first as documented.
///
/// Returns whether every command was accepted by the driver.
fn apply_settings(handle: HANDLE, options: Options) -> bool {
    let mut applied = true;

    if let Some(rate) = options.rate {
        match command(handle, Command::SetSampleRate(rate)) {
            Ok(_) => println!("set:        sample rate {rate} Hz"),
            Err(error) => {
                eprintln!("set sample rate failed: {error:#}");
                applied = false;
            }
        }
        settle();
    }

    if let Some(buffer) = options.buffer {
        match command(handle, Command::SetBufferSize(buffer)) {
            Ok(_) => println!("set:        buffer size {buffer} samples"),
            Err(error) => {
                eprintln!("set buffer size failed: {error:#}");
                applied = false;
            }
        }
        settle();
    }

    applied
}

/// Read the settings back and report whether the request took effect.
fn verify(handle: HANDLE, options: Options) -> i32 {
    let config = match read_config(handle) {
        Ok(config) => config,
        Err(error) => {
            // The driver accepted the command; without a read-back there is
            // simply nothing to compare against.
            println!("set accepted (read-back unavailable: {error:#})");
            return EXIT_OK;
        }
    };
    println!(
        "after:      sample rate {}, buffer {}",
        config.sample_rate, config.buffer_size
    );

    let mut differences = Vec::new();
    if let Some(wanted) = options.rate.filter(|wanted| *wanted != config.sample_rate) {
        differences.push(format!("rate {}, wanted {wanted}", config.sample_rate));
    }
    if let Some(wanted) = options
        .buffer
        .filter(|wanted| *wanted != config.buffer_size)
    {
        differences.push(format!("buffer {}, wanted {wanted}", config.buffer_size));
    }

    if differences.is_empty() {
        println!("ok: verified");
        return EXIT_OK;
    }

    println!(
        "note: read-back differs from the request ({})",
        differences.join(", ")
    );
    EXIT_UNVERIFIED
}

fn run(args: &[String]) -> Result<i32> {
    let options = match parse_invocation(args) {
        Ok(Invocation::Help) => {
            print_usage();
            return Ok(EXIT_OK);
        }
        Ok(Invocation::Run(options)) => options,
        Err(error) => {
            eprintln!("{error}");
            print_usage();
            return Ok(EXIT_USAGE);
        }
    };

    warn_about_unlisted(options);

    println!("installed versions:");
    print_file_version(DRIVER_PATH);
    print_file_version(PAL_PATH);
    println!();

    println!("Focusrite interface classes (registry):");
    let opened = open_control_interface()?;

    println!("\ndevice:     {}", path_text(&opened.path));
    println!(
        "driver:     0x{:08X} 0x{:08X} 0x{:08X} 0x{:08X} (API 0x{:X})",
        opened.version.word0,
        opened.version.api,
        opened.version.word2,
        opened.version.word3,
        opened.version.api
    );

    if opened.version.api < MIN_DRIVER_API {
        bail!(
            "driver API 0x{:X} is older than 0x{MIN_DRIVER_API:X}; Focusrite's Pal layer \
             refuses such a driver, so these command ids may not mean what this tool assumes",
            opened.version.api
        );
    }

    // Same sequence Pal performs when it opens the device. The guard detaches
    // on drop, so every exit path below undoes the attach.
    let _attachment = Attachment::acquire(opened.handle.0);

    match read_config(opened.handle.0) {
        Ok(config) => println!(
            "before:     sample rate {}, buffer {}",
            config.sample_rate, config.buffer_size
        ),
        Err(error) => eprintln!("could not read current settings: {error:#}"),
    }

    let applied = apply_settings(opened.handle.0, options);
    let verified = verify(opened.handle.0, options);

    Ok(if applied { verified } else { EXIT_FAILED })
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(EXIT_FAILED);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    // -- GUID parsing ------------------------------------------------------

    #[test]
    fn parses_a_braced_guid() {
        let guid = parse_guid("{ac4d0455-50d7-4498-b3cd-9a41d130b759}").unwrap();
        assert_eq!(
            guid,
            GUID::from_u128(0xac4d0455_50d7_4498_b3cd_9a41d130b759)
        );
    }

    #[test]
    fn parses_an_unbraced_guid() {
        let guid = parse_guid("429A7462-F176-411E-8DA9-AE6C0A258098").unwrap();
        assert_eq!(
            guid,
            GUID::from_u128(0x429a7462_f176_411e_8da9_ae6c0a258098)
        );
    }

    #[test]
    fn rejects_malformed_guids() {
        for text in [
            "",
            "{ac4d0455-50d7-4498-b3cd}",
            "{ac4d0455-50d7-4498-b3cd-9a41d130b759-0}",
            "{ac4d045-50d7-4498-b3cd-9a41d130b759}",
            "{ac4d0455-50d7-4498-b3cd-9a41d130b75g}",
            "{ac4d0455-50d7-4498-b3cd-9a41d130b75}",
        ] {
            assert!(parse_guid(text).is_none(), "{text} should not parse");
        }
    }

    // -- Substring helpers -------------------------------------------------

    #[test]
    fn matches_ascii_case_insensitively() {
        assert!(contains_ci(b"USB\\VID_1235&PID_8211", b"vid_1235"));
        assert!(!contains_ci(b"USB\\VID_1235", b"vid_1236"));
        assert!(!contains_ci(b"short", b"vid_1235"));
        // An empty needle would make `windows(0)` panic.
        assert!(!contains_ci(b"anything", b""));
    }

    #[test]
    fn finds_pal_reference_strings() {
        let path: Vec<u16> = wide("\\\\?\\root#focusriteusbnew#0000#{guid}\\PAL");
        assert!(contains_pal(&path));
        assert!(contains_pal(&wide("\\pal")));

        let without: Vec<u16> = wide("\\\\?\\usb#vid_1235&pid_8211#5&1#{guid}\\usbdev");
        assert!(!contains_pal(&without));
        // Shorter than the needle: `windows(4)` would panic on an empty slice.
        assert!(!contains_pal(&[]));
        assert!(!contains_pal(&wide("\\pa")));
    }

    #[test]
    fn matches_wide_substrings() {
        let path: Vec<u16> = wide("\\\\?\\USB#VID_1235&PID_8211#5&2");
        assert!(contains_ci_wide(&path, b"vid_1235"));
        assert!(!contains_ci_wide(&path, b"vid_1236"));
        assert!(!contains_ci_wide(&path, b""));
    }

    // -- Path helpers ------------------------------------------------------

    #[test]
    fn wide_is_nul_terminated() {
        assert_eq!(wide("ab"), vec![0x61, 0x62, 0]);
        assert_eq!(wide(""), vec![0]);
    }

    #[test]
    fn path_text_drops_the_terminator() {
        assert_eq!(path_text(&wide("\\\\.\\x")), "\\\\.\\x");
        assert_eq!(path_text(&[]), "");
        assert_eq!(path_text(&[0x61, 0]), "a");
    }

    // -- Wire helpers ------------------------------------------------------

    #[test]
    fn reads_little_endian_words() {
        assert_eq!(rd32(&[1, 0, 0, 0], 0), Some(1));
        assert_eq!(rd32(&[0, 0, 0, 0, 0x58, 0x02, 0, 0], 4), Some(600));
        // Short buffers are `None`, never a plausible zero.
        assert_eq!(rd32(&[1, 0, 0], 0), None);
        assert_eq!(rd32(&[], 0), None);
        // Offsets that would overflow are rejected instead of panicking.
        assert_eq!(rd32(&[0; 8], usize::MAX), None);
    }

    #[test]
    fn splits_the_response_header() {
        let mut response = vec![0u8; RESPONSE_HEADER_LEN];
        response[0] = 0; // status
        response[4] = 7; // length
        assert_eq!(response_header(&response), Some((0, 7)));
        assert_eq!(response_header(&[0; 7]), None);
        assert_eq!(response_header(&[]), None);
    }

    #[test]
    fn every_command_frames_consistently() {
        assert_eq!(Command::Attach.to_string(), "0x400/0x1");
        assert_eq!(Command::Detach.to_string(), "0x400/0x2");
        assert_eq!(Command::GetConfig.to_string(), "0x400/0x4");
        assert_eq!(
            Command::SetSampleRate(RateHz(48_000)).to_string(),
            "0x400/0x5"
        );
        assert_eq!(
            Command::SetBufferSize(Samples(256)).to_string(),
            "0x400/0x9"
        );

        assert_eq!(Command::Attach.payload(), Vec::<u8>::new());
        assert_eq!(
            Command::SetSampleRate(RateHz(48_000)).payload(),
            48_000u32.to_le_bytes()
        );
        assert_eq!(
            Command::SetBufferSize(Samples(256)).payload(),
            256u32.to_le_bytes()
        );

        // Only the config read has a reply.
        assert_eq!(Command::GetConfig.response_len(), CONFIG_LEN);
        assert_eq!(Command::Attach.response_len(), 0);
        assert_eq!(Command::SetBufferSize(Samples(256)).response_len(), 0);
    }

    // -- Config blob -------------------------------------------------------

    fn blob(rate: u32, buffer: u32) -> Vec<u8> {
        let mut blob = vec![0u8; CONFIG_LEN];
        blob[OFF_SAMPLE_RATE..OFF_SAMPLE_RATE + 4].copy_from_slice(&rate.to_le_bytes());
        blob[OFF_BUFFER_SIZE..OFF_BUFFER_SIZE + 4].copy_from_slice(&buffer.to_le_bytes());
        blob
    }

    #[test]
    fn parses_the_config_blob() {
        let config = Config::from_blob(&blob(44_100, 512)).unwrap();
        assert_eq!(config.sample_rate, RateHz(44_100));
        assert_eq!(config.buffer_size, Samples(512));
    }

    #[test]
    fn rejects_a_short_config_blob() {
        assert!(Config::from_blob(&blob(48_000, 256)[..0x10]).is_err());
        assert!(Config::from_blob(&[]).is_err());
    }

    // -- Driver descriptor -------------------------------------------------

    #[test]
    fn parses_the_driver_descriptor() {
        let mut raw = [0u8; DriverVersion::SIZE];
        raw[0..4].copy_from_slice(&0x7000u32.to_le_bytes());
        raw[4..8].copy_from_slice(&0x7000u32.to_le_bytes());
        raw[8..12].copy_from_slice(&0x105u32.to_le_bytes());
        raw[12..16].copy_from_slice(&0x8048_F000u32.to_le_bytes());

        let version = DriverVersion::parse(&raw).unwrap();
        assert_eq!(version.api, 0x7000);
        assert_eq!(version.word2, 0x105);
        assert!(version.api >= MIN_DRIVER_API);

        assert_eq!(
            DriverVersion::parse(&raw[..4]),
            None,
            "a 4-byte descriptor is not a version"
        );
    }

    // -- Command line ------------------------------------------------------

    #[test]
    fn accepts_the_legacy_form() {
        assert_eq!(
            parse_invocation(&args(&["256"])),
            Ok(Invocation::Run(Options {
                buffer: Some(Samples(256)),
                rate: None,
            }))
        );
    }

    #[test]
    fn accepts_both_keys_in_any_order() {
        let expected = Invocation::Run(Options {
            buffer: Some(Samples(256)),
            rate: Some(RateHz(48_000)),
        });
        assert_eq!(
            parse_invocation(&args(&["rate", "48000", "buffer", "256"])),
            Ok(expected)
        );
        assert_eq!(
            parse_invocation(&args(&["buffer", "256", "rate", "48000"])),
            Ok(expected)
        );
    }

    #[test]
    fn treats_help_as_a_request() {
        assert_eq!(parse_invocation(&args(&["--help"])), Ok(Invocation::Help));
        assert_eq!(parse_invocation(&args(&["-h"])), Ok(Invocation::Help));
        assert_eq!(
            parse_invocation(&args(&["rate", "48000", "--help"])),
            Ok(Invocation::Help)
        );
    }

    #[test]
    fn rejects_bad_command_lines() {
        assert_eq!(parse_invocation(&args(&[])), Err(CliError::NothingToDo));
        assert_eq!(
            parse_invocation(&args(&["buffer"])),
            Err(CliError::MissingValue { key: "buffer" })
        );
        assert_eq!(
            parse_invocation(&args(&["rate"])),
            Err(CliError::MissingValue { key: "rate" })
        );
        assert_eq!(
            parse_invocation(&args(&["buffer", "0"])),
            Err(CliError::NotAPositiveInteger {
                key: "buffer",
                value: "0".to_owned(),
            })
        );
        assert_eq!(
            parse_invocation(&args(&["256abc"])),
            Err(CliError::NotAPositiveInteger {
                key: "samples",
                value: "256abc".to_owned(),
            })
        );
        assert_eq!(
            parse_invocation(&args(&["4294967296"])),
            Err(CliError::NotAPositiveInteger {
                key: "samples",
                value: "4294967296".to_owned(),
            })
        );
        assert_eq!(
            parse_invocation(&args(&["buffer", "256", "buffer", "512"])),
            Err(CliError::Duplicate { key: "buffer" })
        );
        assert_eq!(
            parse_invocation(&args(&["256", "257"])),
            Err(CliError::Unexpected("256".to_owned()))
        );
    }

    // -- Known values ------------------------------------------------------

    #[test]
    fn knows_the_notifier_lists() {
        assert!(known_size(Samples(16)));
        assert!(known_size(Samples(1024)));
        assert!(!known_size(Samples(100)));
        assert!(known_rate(RateHz(44_100)));
        assert!(known_rate(RateHz(192_000)));
        assert!(!known_rate(RateHz(30_000)));
    }
}
