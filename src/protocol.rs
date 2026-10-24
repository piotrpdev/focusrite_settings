//! The driver wire format: command ids, payloads and the configuration blob.
//!
//! Nothing here touches Win32; the shapes are the ones Pal uses, and the
//! constants are the ones recovered from `FocusriteUsb.sys`.

use std::fmt;
use std::mem::size_of;

use anyhow::{Context, Result, bail};

/// A streaming buffer size in samples.
///
/// A newtype rather than a bare `u32` because the driver payload for the buffer
/// size and the one for the sample rate are both four little-endian bytes; with
/// plain integers, swapping them would compile and silently do the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Samples(u32);

impl Samples {
    /// Wrap a buffer size in samples.
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The value the driver expects on the wire.
    pub(crate) const fn get(self) -> u32 {
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
pub(crate) struct RateHz(u32);

impl RateHz {
    /// Wrap a sample rate in Hz.
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The value the driver expects on the wire.
    pub(crate) const fn get(self) -> u32 {
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
pub(crate) enum Cmd {
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
pub(crate) enum Command {
    Attach,
    Detach,
    GetConfig,
    SetSampleRate(RateHz),
    SetBufferSize(Samples),
}

impl Command {
    /// The wire id of this command.
    pub(crate) const fn cmd(self) -> Cmd {
        match self {
            Self::Attach => Cmd::Attach,
            Self::Detach => Cmd::Detach,
            Self::GetConfig => Cmd::GetConfig,
            Self::SetSampleRate(_) => Cmd::SetSampleRate,
            Self::SetBufferSize(_) => Cmd::SetBufferSize,
        }
    }

    /// Payload bytes that follow the 16-byte header.
    pub(crate) fn payload(self) -> Vec<u8> {
        match self {
            Self::Attach | Self::Detach | Self::GetConfig => Vec::new(),
            Self::SetSampleRate(rate) => rate.get().to_le_bytes().to_vec(),
            Self::SetBufferSize(samples) => samples.get().to_le_bytes().to_vec(),
        }
    }

    /// Payload bytes the driver returns; 0 for commands without a reply.
    pub(crate) const fn response_len(self) -> usize {
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
pub(crate) struct Config {
    pub(crate) sample_rate: RateHz,
    pub(crate) buffer_size: Samples,
}

impl Config {
    /// Parse the blob `Cmd::GetConfig` returns.
    pub(crate) fn from_blob(blob: &[u8]) -> Result<Self> {
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
///
/// `FocusriteUsbSwRoot.sys` writes four hard-coded dwords here (`0x1400118d3` in
/// that driver: `0x7000, 0x7000, 0x105, 0x8048F000`), and Pal reads exactly two
/// things out of them. The field meanings are what Pal's code does, not guesses:
/// see `analysis/FocusriteUsbSwRoot.sys.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DriverVersion {
    /// Word 0: the driver's own API version. Pal never compares it; it only
    /// lands in Pal's mismatch message, which prints it under the label
    /// "Driver minimum".
    pub(crate) api: u32,
    /// Word 1: the Pal API the driver requires. Pal compares it against its own
    /// [`PAL_API_VERSION`] and refuses the device when the driver wants more.
    pub(crate) pal_api: u32,
    /// Word 2: the driver's build number (`261` in `4.143.0.261`).
    pub(crate) build: u32,
    /// Word 3: an unexplained constant (`0x8048F000` in this package), which
    /// Pal copies into its device object and never reads.
    pub(crate) stamp: u32,
}

impl DriverVersion {
    /// Bytes the driver writes for the descriptor.
    pub(crate) const SIZE: usize = 4 * size_of::<u32>();

    /// Decode the descriptor, or `None` when the driver wrote too little.
    pub(crate) fn parse(raw: &[u8]) -> Option<Self> {
        Some(Self {
            api: rd32(raw, 0)?,
            pal_api: rd32(raw, size_of::<u32>())?,
            build: rd32(raw, 2 * size_of::<u32>())?,
            stamp: rd32(raw, 3 * size_of::<u32>())?,
        })
    }

    /// Whether this driver accepts the Pal API this tool implements.
    ///
    /// Pal's own test is `cmp dword ptr [buffer+4], 0x7000` followed by
    /// `jbe <continue>`: a driver that requires a newer Pal than the one running
    /// is refused, and one that requires an older one is fine. This tool stands
    /// in for Pal, so it applies the same rule.
    pub(crate) const fn accepts_pal_api(&self) -> bool {
        self.pal_api <= PAL_API_VERSION
    }
}

/// Section Pal's device base class uses for every command in this tool.
pub(crate) const SECTION_APP: u16 = 0x0400;

/// `{u64 token, u16 section, u16 id, u32 reserved}`.
pub(crate) const HEADER_LEN: usize = 16;
/// `{u32 status, u32 length}`.
pub(crate) const RESPONSE_HEADER_LEN: usize = 8;

/// Size of the configuration blob `Cmd::GetConfig` returns.
pub(crate) const CONFIG_LEN: usize = 0x58;
/// Offset of the sample rate inside that blob.
pub(crate) const OFF_SAMPLE_RATE: usize = 0x0C;
/// Offset of the buffer size, in samples, inside that blob.
pub(crate) const OFF_BUFFER_SIZE: usize = 0x14;

/// The Pal API this tool implements, i.e. the value Pal compiles in and compares
/// the driver's requirement against (`0x7000` = "7.0" as Pal prints it: 28672).
pub(crate) const PAL_API_VERSION: u32 = 0x7000;
/// `CTL_CODE(0x22, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`: driver descriptor.
pub(crate) const IOCTL_GET_VERSION: u32 = 0x0022_2000;
/// `CTL_CODE(0x22, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS)`: DICE command.
pub(crate) const IOCTL_COMMAND: u32 = 0x0022_2008;

/// Read a little-endian `u32` at `offset`, or `None` when the buffer is short.
fn rd32(buf: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(size_of::<u32>())?;
    let bytes = buf.get(offset..end)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Split the `{u32 status, u32 length}` header off a command response.
pub(crate) fn response_header(response: &[u8]) -> Option<(u32, u32)> {
    let (status, length) = response
        .get(..RESPONSE_HEADER_LEN)?
        .split_at_checked(size_of::<u32>())?;
    Some((
        u32::from_le_bytes(status.try_into().ok()?),
        u32::from_le_bytes(length.try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(version.pal_api, 0x7000);
        assert_eq!(version.build, 0x105);
        assert_eq!(version.stamp, 0x8048_F000);
        assert!(version.accepts_pal_api());

        assert_eq!(
            DriverVersion::parse(&raw[..4]),
            None,
            "a 4-byte descriptor is not a version"
        );
    }

    #[test]
    fn accepts_drivers_that_do_not_want_a_newer_pal() {
        let requiring = |pal_api: u32| DriverVersion {
            api: 0x7000,
            pal_api,
            build: 0x105,
            stamp: 0x8048_F000,
        };

        // What 4.143.0.261 reports, and an older driver: both fine.
        assert!(requiring(PAL_API_VERSION).accepts_pal_api());
        assert!(requiring(0x6000).accepts_pal_api());
        // A driver that wants a newer Pal than this tool implements: refused,
        // exactly as Focusrite's Pal refuses it.
        assert!(!requiring(PAL_API_VERSION + 1).accepts_pal_api());
    }
}
