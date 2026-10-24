//! `focusrite_buffer` - set a Focusrite interface's buffer size or sample rate.
//!
//! Changes the streaming buffer size and/or sample rate of a Focusrite
//! interface by issuing the same private ioctls Focusrite's Pal layer uses.
//! No `FocusritePal64.dll` involved.
//!
//! Protocol reference: `docs/FocusritePal64.dll.md`; the driver side of the
//! handshake is in `docs/FocusriteUsbSwRoot.sys.md`.
//!
//! This is the maintained implementation; `docs/focusrite_buffer.cpp` is the
//! original C++ tool kept as a reference, and `README.md` lists every intentional
//! difference between the two.
//!
//! Layout:
//! - `cli` parses the command line, `protocol` the wire format, `interface` the
//!   device paths and GUIDs. Those three are platform-free, which is what lets
//!   `cargo test` run their tests on any host.
//! - `win` is the only module that names a Windows type, and the only one with
//!   `unsafe` in it. `app` sequences the calls.
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
// unchecked arithmetic are denied in `Cargo.toml`. Tests are exempt, because a
// failing assertion is exactly what a test is for.
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
// Off Windows the platform layer is compiled out, so everything that only it
// uses looks dead to the compiler. The tests still exercise the pure modules.
#![cfg_attr(not(windows), allow(dead_code))]

mod cli;
mod interface;
mod protocol;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match app::run(&args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(cli::EXIT_FAILED);
        }
    }
}

/// The tool only does anything on Windows. Off Windows the binary exists so
/// that `cargo test` can build the crate and run the parsers' tests.
#[cfg(not(windows))]
fn main() {
    eprintln!("focusrite_buffer only runs on Windows; `cargo test` covers the parsers elsewhere");
    std::process::exit(cli::EXIT_FAILED);
}
