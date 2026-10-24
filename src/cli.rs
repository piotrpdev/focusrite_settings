//! Command-line parsing and the text the tool prints about itself.

use std::fmt;

use crate::interface::numbers;
use crate::protocol::{RateHz, Samples};

/// What the user asked for. Both fields are optional because either one on its
/// own is a complete request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    pub(crate) buffer: Option<Samples>,
    pub(crate) rate: Option<RateHz>,
}

/// What one command line means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// Print the usage text and stop.
    Help,
    /// Open the device and apply these settings.
    Run(Options),
}

/// Why a command line was rejected.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
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

impl fmt::Display for Error {
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
fn positive_integer(key: &'static str, value: &str) -> Result<u32, Error> {
    match value.parse::<u32>() {
        Ok(parsed) if parsed > 0 => Ok(parsed),
        _ => Err(Error::NotAPositiveInteger {
            key,
            value: value.to_owned(),
        }),
    }
}

/// Parse the arguments after the program name.
pub(crate) fn parse_invocation(args: &[String]) -> Result<Invocation, Error> {
    let mut options = Options::default();
    let mut iter = args.iter();

    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "buffer" => {
                if options.buffer.is_some() {
                    return Err(Error::Duplicate { key: "buffer" });
                }
                let value = iter.next().ok_or(Error::MissingValue { key: "buffer" })?;
                options.buffer = Some(Samples::new(positive_integer("buffer", value)?));
            }
            "rate" => {
                if options.rate.is_some() {
                    return Err(Error::Duplicate { key: "rate" });
                }
                let value = iter.next().ok_or(Error::MissingValue { key: "rate" })?;
                options.rate = Some(RateHz::new(positive_integer("rate", value)?));
            }
            // Legacy form: `focusrite_buffer 256` means the buffer size.
            token if args.len() == 1 => {
                options.buffer = Some(Samples::new(positive_integer("samples", token)?));
            }
            token => return Err(Error::Unexpected(token.to_owned())),
        }
    }

    if options.buffer.is_none() && options.rate.is_none() {
        return Err(Error::NothingToDo);
    }
    Ok(Invocation::Run(options))
}

/// Print the usage text.
pub(crate) fn print_usage() {
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
pub(crate) fn warn_about_unlisted(options: Options) {
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

/// Buffer sizes the Notifier's own window offers, in samples.
const NOTIFIER_SAMPLES: [u32; 11] = [16, 32, 48, 64, 96, 128, 160, 192, 256, 512, 1024];
/// Sample rates the Notifier's own window offers, in Hz.
const NOTIFIER_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Exit code: everything the user asked for is in effect.
pub(crate) const EXIT_OK: i32 = 0;
/// Exit code: the driver rejected a command, or a request failed.
pub(crate) const EXIT_FAILED: i32 = 1;
/// Exit code: the arguments made no sense.
pub(crate) const EXIT_USAGE: i32 = 2;
/// Exit code: the commands were accepted, but the read-back shows something else.
pub(crate) const EXIT_UNVERIFIED: i32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    // -- Command line ------------------------------------------------------

    #[test]
    fn accepts_the_legacy_form() {
        assert_eq!(
            parse_invocation(&args(&["256"])),
            Ok(Invocation::Run(Options {
                buffer: Some(Samples::new(256)),
                rate: None,
            }))
        );
    }

    #[test]
    fn accepts_both_keys_in_any_order() {
        let expected = Invocation::Run(Options {
            buffer: Some(Samples::new(256)),
            rate: Some(RateHz::new(48_000)),
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
        assert_eq!(parse_invocation(&args(&[])), Err(Error::NothingToDo));
        assert_eq!(
            parse_invocation(&args(&["buffer"])),
            Err(Error::MissingValue { key: "buffer" })
        );
        assert_eq!(
            parse_invocation(&args(&["rate"])),
            Err(Error::MissingValue { key: "rate" })
        );
        assert_eq!(
            parse_invocation(&args(&["buffer", "0"])),
            Err(Error::NotAPositiveInteger {
                key: "buffer",
                value: "0".to_owned(),
            })
        );
        assert_eq!(
            parse_invocation(&args(&["256abc"])),
            Err(Error::NotAPositiveInteger {
                key: "samples",
                value: "256abc".to_owned(),
            })
        );
        assert_eq!(
            parse_invocation(&args(&["4294967296"])),
            Err(Error::NotAPositiveInteger {
                key: "samples",
                value: "4294967296".to_owned(),
            })
        );
        assert_eq!(
            parse_invocation(&args(&["buffer", "256", "buffer", "512"])),
            Err(Error::Duplicate { key: "buffer" })
        );
        assert_eq!(
            parse_invocation(&args(&["256", "257"])),
            Err(Error::Unexpected("256".to_owned()))
        );
    }

    // -- Known values ------------------------------------------------------

    #[test]
    fn knows_the_notifier_lists() {
        assert!(known_size(Samples::new(16)));
        assert!(known_size(Samples::new(1024)));
        assert!(!known_size(Samples::new(100)));
        assert!(known_rate(RateHz::new(44_100)));
        assert!(known_rate(RateHz::new(192_000)));
        assert!(!known_rate(RateHz::new(30_000)));
    }
}
