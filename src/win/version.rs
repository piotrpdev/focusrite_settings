//! The "installed versions" block: what the driver and Pal report about
//! themselves.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

use windows::Win32::Storage::FileSystem::{
    GetFileAttributesW, GetFileVersionInfoSizeW, GetFileVersionInfoW, INVALID_FILE_ATTRIBUTES,
    VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows::core::PCWSTR;

use crate::interface::wide;

/// Print the file version of `path`, or why there is none.
pub(crate) fn print_file_version(path: &str) {
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
