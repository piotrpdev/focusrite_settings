//! The overlapped ioctl round trip and the DICE command sender.

use std::ffi::c_void;
use std::io;
use std::mem::zeroed;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0, WIN32_ERROR};
use windows::Win32::System::IO::{CancelIo, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::protocol::{
    Command, Config, HEADER_LEN, IOCTL_COMMAND, RESPONSE_HEADER_LEN, SECTION_APP, response_header,
};
use crate::win::guards::Handle;
use crate::win::{i32_of, u32_of};

/// How long the driver gets to answer one ioctl, as in Pal's `ioctl()`.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a cancelled request gets to finish before the process gives up.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// A duration as the `u32` milliseconds `WaitForSingleObject` expects.
fn timeout_millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX)
}

/// The Win32 code behind a `windows` error, with the `HRESULT` wrapper removed.
///
/// windows-rs stores Win32 codes as `HRESULT_FROM_WIN32(code)`, so comparing the
/// raw `HRESULT` against `ERROR_*` constants never matches.
pub(crate) const fn win32_code(error: &windows::core::Error) -> u32 {
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
pub(crate) const fn is_win32(error: &windows::core::Error, code: WIN32_ERROR) -> bool {
    win32_code(error) == code.0
}

/// Convert a `windows` error into an `io::Error`, keeping the Win32 code.
pub(crate) fn to_io_error(error: &windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(i32_of(win32_code(error)))
}

/// The error reported when the driver does not answer in time.
fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no answer within {} s", COMMAND_TIMEOUT.as_secs()),
    )
}

/// One overlapped `DeviceIoControl` round trip, like Pal's `ioctl()`.
///
/// `output` must stay borrowed for the whole call: the driver writes into it
/// when the request completes, not when `DeviceIoControl` returns.
pub(crate) fn ioctl_xfer(
    handle: HANDLE,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<u32> {
    // SAFETY: a manual-reset event with no name; the `Handle` guard closes it.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|error| to_io_error(&error))?;
    let event = Handle::new(event);

    // SAFETY: `OVERLAPPED` is a plain C struct; all-zero is its documented
    // "nothing else set" state, and `hEvent` is filled in on the next line.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event.raw();

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
            if unsafe { WaitForSingleObject(event.raw(), timeout_millis(COMMAND_TIMEOUT)) }
                != WAIT_OBJECT_0
            {
                cancel_and_drain(handle, event.raw(), &overlapped);
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

/// Send one command and return its payload.
pub(crate) fn command(handle: HANDLE, cmd: Command) -> Result<Vec<u8>> {
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
pub(crate) fn read_config(handle: HANDLE) -> Result<Config> {
    let blob = command(handle, Command::GetConfig).context("read config blob")?;
    Config::from_blob(&blob)
}
