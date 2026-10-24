//! Device interface identities and the string matching done on them.
//!
//! `Guid` is deliberately our own type rather than `windows::core::GUID`: it
//! keeps parsing testable on any host, and the Win32 conversion lives at the
//! platform boundary in `crate::win`.

/// A device interface GUID, split the way the registry spells it.
///
/// The fields are the same four the braced string has, so a parsed GUID is
/// compared and formatted without going through a Win32 type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Guid {
    /// First field: eight hex digits.
    pub(crate) data1: u32,
    /// Second field: four hex digits.
    pub(crate) data2: u16,
    /// Third field: four hex digits.
    pub(crate) data3: u16,
    /// Last eight bytes, in the order the registry prints them.
    pub(crate) node: [u8; 8],
}

impl Guid {
    /// Build a GUID from the single-number form, for the known-GUID list.
    pub(crate) const fn from_u128(value: u128) -> Self {
        let bytes = value.to_be_bytes();
        let [b0, b1, b2, b3, b4, b5, b6, b7] = [
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ];
        let [b8, b9, b10, b11, b12, b13, b14, b15] = [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ];
        Self {
            data1: u32::from_be_bytes([b0, b1, b2, b3]),
            data2: u16::from_be_bytes([b4, b5]),
            data3: u16::from_be_bytes([b6, b7]),
            node: [b8, b9, b10, b11, b12, b13, b14, b15],
        }
    }
}

/// Accepts the braced GUID form used by the `DeviceClasses` registry keys.
pub(crate) fn parse(text: &str) -> Option<Guid> {
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

    Some(Guid {
        data1: u32::from_str_radix(data1, 16).ok()?,
        data2: u16::from_str_radix(data2, 16).ok()?,
        data3: u16::from_str_radix(data3, 16).ok()?,
        node: node.to_be_bytes(),
    })
}

/// `s` as the NUL-terminated UTF-16 the `...W` APIs take.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `path` without its trailing NUL, for display.
pub(crate) fn path_text(path: &[u16]) -> String {
    let body = path.split_last().map_or(path, |(_, rest)| rest);
    String::from_utf16_lossy(body)
}

/// A space-separated decimal list, for the usage text.
pub(crate) fn numbers(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// ASCII case-insensitive substring search on bytes.
pub(crate) fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
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
pub(crate) fn contains_pal(path: &[u16]) -> bool {
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
pub(crate) fn contains_ci_wide(haystack: &[u16], needle: &[u8]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- GUID parsing ------------------------------------------------------

    #[test]
    fn parses_a_braced_guid() {
        let guid = parse("{ac4d0455-50d7-4498-b3cd-9a41d130b759}").unwrap();
        assert_eq!(
            guid,
            Guid::from_u128(0xac4d0455_50d7_4498_b3cd_9a41d130b759)
        );
    }

    #[test]
    fn parses_an_unbraced_guid() {
        let guid = parse("429A7462-F176-411E-8DA9-AE6C0A258098").unwrap();
        assert_eq!(
            guid,
            Guid::from_u128(0x429a7462_f176_411e_8da9_ae6c0a258098)
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
            assert!(parse(text).is_none(), "{text} should not parse");
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
}
