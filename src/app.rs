//! What the tool actually does, in order: open, attach, read, apply, verify.

use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::Foundation::HANDLE;

use crate::cli::{
    EXIT_FAILED, EXIT_OK, EXIT_UNVERIFIED, EXIT_USAGE, Invocation, Options, parse_invocation,
    print_usage, warn_about_unlisted,
};
use crate::interface::path_text;
use crate::protocol::{Command, MIN_DRIVER_API};
use crate::win::discovery::open_control_interface;
use crate::win::ioctl::{command, read_config};
use crate::win::version::print_file_version;

/// Where the official installer puts the driver.
const DRIVER_PATH: &str = "C:\\Windows\\System32\\drivers\\FocusriteUsb.sys";
/// Where the official installer puts Pal.
const PAL_PATH: &str = "C:\\Program Files\\Focusrite\\Drivers\\FocusritePal64.dll";

/// Pause between commands, so the device settles before the next one.
const SETTLE: Duration = Duration::from_millis(250);

/// Keeps the driver in the attached state for as long as it lives.
///
/// A guard rather than a `bool` plus a detach at the end of `run`: an early
/// return or an error can then never leave the device attached.
struct Attachment {
    handle: HANDLE,
}

impl Attachment {
    /// Attach, or report why not and return `None` so the caller can decide.
    pub(crate) fn acquire(handle: HANDLE) -> Option<Self> {
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

/// Give the device a moment to apply a change.
///
/// Without this the read-back can still report the previous value; the C++ tool
/// sleeps the same 250 ms for the same reason.
fn settle() {
    std::thread::sleep(SETTLE);
}

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

pub(crate) fn run(args: &[String]) -> Result<i32> {
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
    let _attachment = Attachment::acquire(opened.handle.raw());

    match read_config(opened.handle.raw()) {
        Ok(config) => println!(
            "before:     sample rate {}, buffer {}",
            config.sample_rate, config.buffer_size
        ),
        Err(error) => eprintln!("could not read current settings: {error:#}"),
    }

    let applied = apply_settings(opened.handle.raw(), options);
    let verified = verify(opened.handle.raw(), options);

    Ok(if applied { verified } else { EXIT_FAILED })
}
