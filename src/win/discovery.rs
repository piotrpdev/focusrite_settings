//! Finding the control interface: the registry walk, `SetupAPI` enumeration and
//! the open attempts that follow.

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr;

use anyhow::{Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SPINT_ACTIVE, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{
    ERROR_NO_MORE_ITEMS, ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_SHARE_MODE,
    FILE_WRITE_DATA, OPEN_EXISTING, SYNCHRONIZE,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RegEnumKeyExA, RegOpenKeyExA,
};
use windows::core::{GUID, PCSTR, PCWSTR, PSTR};

use crate::interface::{self, Guid, contains_ci, contains_ci_wide, contains_pal, path_text};
use crate::protocol::{DriverVersion, IOCTL_GET_VERSION};
use crate::win::guards::{DeviceInfoSet, Handle, REG_INSTANCE_CAP, REG_NAME_CAP, RegKey};
use crate::win::ioctl::{ioctl_xfer, is_win32, to_io_error, win32_code};
use crate::win::{i32_of, to_windows_guid, u32_of};

/// Fallback list of interface GUIDs Pal hard-codes; discovery via the registry
/// is preferred because a driver build may register different ones.
const KNOWN_GUIDS: [Guid; 3] = [
    Guid::from_u128(0xac4d0455_50d7_4498_b3cd_9a41d130b759),
    Guid::from_u128(0x429a7462_f176_411f_8da9_ae6c0a258098),
    Guid::from_u128(0x429a7462_f176_411e_8da9_ae6c0a258098),
];

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
fn registered_focusrite_classes() -> Vec<Guid> {
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
    let classes = RegKey::new(classes);

    let mut found = Vec::new();
    for index in 0.. {
        let mut name = [0u8; REG_NAME_CAP];
        let mut name_len = u32_of(REG_NAME_CAP);
        // SAFETY: `name` has room for `REG_NAME_CAP` bytes and `name_len` says
        // so; the API writes a NUL-terminated name into it.
        let rc = unsafe {
            RegEnumKeyExA(
                classes.raw(),
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
        let sub_rc = unsafe {
            RegOpenKeyExA(
                classes.raw(),
                PCSTR(name.as_ptr()),
                0,
                KEY_READ,
                &raw mut sub,
            )
        };
        if sub_rc != WIN32_ERROR(0) {
            continue;
        }
        let sub = RegKey::new(sub);

        let scan = scan_class_instances(sub.raw(), class_name);
        if scan.focusrite
            && scan.has_pal
            && let Some(guid) = interface::parse(class_name)
            && !found.contains(&guid)
        {
            found.push(guid);
        }
    }

    found
}

/// The interface path of one enumerated device interface.
///
/// `None` means `SetupAPI` could not describe it, or the driver left a path
/// without a terminator, so it cannot be opened.
fn interface_path(set: HDEVINFO, interface: &SP_DEVICE_INTERFACE_DATA) -> Option<Vec<u16>> {
    let mut needed = 0u32;
    // This call is expected to fail with ERROR_INSUFFICIENT_BUFFER: it only
    // reports how much room the variable-length detail struct needs.
    // SAFETY: the set is open and both out-parameters are valid.
    let _sizing = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            std::ptr::from_ref(interface),
            None,
            0,
            Some(&raw mut needed),
            None,
        )
    };
    let size = usize::try_from(needed).ok().filter(|size| *size > 0)?;

    // The detail struct is variable length and needs 4-byte alignment, so back
    // it with `u64`s and then fill in its `cbSize`.
    let mut buffer: Vec<u64> = vec![0; size.div_ceil(size_of::<u64>())];
    let detail = buffer
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();

    // SAFETY: `buffer` has at least `size` bytes and is aligned for the struct;
    // `detail` is the only reference to it.
    unsafe {
        (*detail).cbSize = u32_of(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>());
    }

    // SAFETY: same buffer and size as the sizing call above.
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            set,
            std::ptr::from_ref(interface),
            Some(detail),
            needed,
            None,
            None,
        )
    }
    .ok()?;

    // How many UTF-16 units of path can fit in the buffer the API filled. The
    // scan is bounded by that, so a path without a terminator cannot run off the
    // end of an allocation.
    let units = size.saturating_sub(std::mem::offset_of!(
        SP_DEVICE_INTERFACE_DETAIL_DATA_W,
        DevicePath
    )) / size_of::<u16>();
    // SAFETY: the API wrote `DevicePath` into `buffer`, and `units` is the
    // number of UTF-16 units that buffer has room for from there on.
    let units = unsafe { std::slice::from_raw_parts((*detail).DevicePath.as_ptr(), units) };
    let length = units.iter().position(|unit| *unit == 0)?;
    Some(units.get(..=length)?.to_vec())
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
        Ok(set) => DeviceInfoSet::new(set),
        Err(error) => return Err(win32_code(&error)),
    };

    let mut candidates = Vec::new();
    let mut skipped = 0usize;
    let mut slot = 0u32;
    loop {
        // SAFETY: `SP_DEVICE_INTERFACE_DATA` is a plain C struct; all-zero with
        // `cbSize` set is its documented input state.
        let mut interface: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        interface.cbSize = u32_of(size_of::<SP_DEVICE_INTERFACE_DATA>());
        // SAFETY: the set is open and `interface` is a valid out-parameter.
        match unsafe {
            SetupDiEnumDeviceInterfaces(
                set.raw(),
                None,
                ptr::from_ref(guid),
                slot,
                &raw mut interface,
            )
        } {
            Ok(()) => slot += 1,
            Err(error) if is_win32(&error, ERROR_NO_MORE_ITEMS) => break,
            Err(error) => {
                eprintln!(
                    "  setupapi: could not enumerate interface {slot}: {}",
                    to_io_error(&error)
                );
                break;
            }
        }

        let Some(path) = interface_path(set.raw(), &interface) else {
            // `SetupAPI` could not describe this interface, or wrote a path
            // without a terminator: either way it cannot be the control one.
            eprintln!("  setupapi: no usable path for interface {slot}");
            continue;
        };

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
    .map(Handle::new)
    .map_err(|error| to_io_error(&error))?;

    let mut raw = [0u8; DriverVersion::SIZE];
    let transferred = ioctl_xfer(handle.raw(), IOCTL_GET_VERSION, &[], &mut raw)?;
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

/// A control interface that is open and has answered the version probe.
pub(crate) struct Opened {
    pub(crate) handle: Handle,
    /// NUL-terminated device interface path the handle was opened on.
    pub(crate) path: Vec<u16>,
    pub(crate) version: DriverVersion,
}

/// Find and open the driver's control interface.
///
/// Pal only accepts paths with the `\pal` reference string, so those are tried
/// first and the plain `vid_1235` paths are kept as a fallback.
pub(crate) fn open_control_interface() -> Result<Opened> {
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
        let windows_guid = to_windows_guid(*guid);
        match candidates_for(&windows_guid) {
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
