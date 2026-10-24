//! RAII wrappers for the Win32 resources the tool acquires.
//!
//! Each guard is the only owner of its handle and closes it on drop, so no
//! error path can leak one.

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    HDEVINFO, SetupDiDestroyDeviceInfoList,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Registry::{HKEY, RegCloseKey};

/// Room for one registry key name, including its terminator.
pub(crate) const REG_NAME_CAP: usize = 128;
/// Room for one `DeviceClasses` instance name, including its terminator.
pub(crate) const REG_INSTANCE_CAP: usize = 2048;

/// Owns a Win32 handle and closes it on drop.
pub(crate) struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `Handle` is the only owner of this handle and is never cloned.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Owns a `SetupAPI` device information set and destroys it on drop.
pub(crate) struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: `DeviceInfoSet` is the only owner and is never cloned.
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Owns a registry key and closes it on drop.
pub(crate) struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: `RegKey` is the only owner and is never cloned.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl Handle {
    /// Take ownership of a handle.
    pub(crate) const fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    /// The raw handle, for the calls that need one.
    pub(crate) const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl DeviceInfoSet {
    /// Take ownership of a device information set.
    pub(crate) const fn new(set: HDEVINFO) -> Self {
        Self(set)
    }

    /// The raw set, for the calls that need one.
    pub(crate) const fn raw(&self) -> HDEVINFO {
        self.0
    }
}

impl RegKey {
    /// Take ownership of a registry key.
    pub(crate) const fn new(key: HKEY) -> Self {
        Self(key)
    }

    /// The raw key, for the calls that need one.
    pub(crate) const fn raw(&self) -> HKEY {
        self.0
    }
}
