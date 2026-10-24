//! Everything that talks to Win32.
//!
//! The rest of the crate is platform-free; this module is the only place that
//! names a Windows type, so it is also the only place that is `unsafe`.

pub(crate) mod discovery;
pub(crate) mod guards;
pub(crate) mod ioctl;
pub(crate) mod version;

use crate::interface::Guid;

/// The Win32 spelling of a parsed interface GUID.
pub(crate) const fn to_windows_guid(guid: Guid) -> windows::core::GUID {
    windows::core::GUID::from_values(guid.data1, guid.data2, guid.data3, guid.node)
}

/// `value` as a `u32`, saturating instead of wrapping.
///
/// Every argument is a buffer length or a struct size, so the saturation branch
/// is unreachable in practice. It exists because the Win32 APIs take `u32` and
/// `as` would truncate silently.
pub(crate) fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// `value` as an `i32`, saturating instead of wrapping.
pub(crate) fn i32_of(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
