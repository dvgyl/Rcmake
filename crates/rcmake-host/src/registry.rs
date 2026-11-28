//! Windows registry queries used by search commands and `cmake_host_system_information`.

use rcmake_base::BString;

/// A predefined Windows registry hive.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RegistryRoot {
    /// HKEY_CURRENT_USER.
    CurrentUser,
    /// HKEY_LOCAL_MACHINE.
    LocalMachine,
    /// HKEY_CLASSES_ROOT.
    ClassesRoot,
    /// HKEY_CURRENT_CONFIG.
    CurrentConfig,
    /// HKEY_USERS.
    Users,
}

/// The registry view used for a query.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RegistryView {
    /// The 32-bit registry view.
    Bits32,
    /// The 64-bit registry view.
    Bits64,
}

/// Return the first UTF-16 code unit of `separator`, or `0` (NUL) when omitted or empty.
fn first_separator_unit(separator: Option<&[u8]>) -> u16 {
    separator
        .filter(|sep| !sep.is_empty())
        .and_then(|sep| String::from_utf8_lossy(sep).encode_utf16().next())
        .unwrap_or(0)
}

/// Format a `REG_MULTI_SZ` UTF-16 buffer into a CMake `;`-separated list.
///
/// Matches CMake 4.5.1's `cmWindowsRegistry::ReadValue`: excludes only the final UTF-16 NUL
/// terminator from separator replacement, replaces occurrences of the first UTF-16 code unit
/// of `separator` (or `\0` when omitted or empty) with `;`, and stops at the first remaining
/// NUL code unit. Thus `a\0b\0\0` produces `a;b;` while `a\0b\0` produces `a;b`.
fn format_multi_sz_units(units: &mut [u16], separator: Option<&[u8]>) -> BString {
    if units.is_empty() {
        return BString::default();
    }
    let replace_end = if units.last() == Some(&0) {
        units.len() - 1
    } else {
        units.len()
    };
    let sep = first_separator_unit(separator);
    if let Some(prefix) = units.get_mut(..replace_end) {
        for unit in prefix {
            if *unit == sep {
                *unit = u16::from(b';');
            }
        }
    }
    let visible_len = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    let visible = units.get(..visible_len).unwrap_or_default();
    String::from_utf16_lossy(visible).into()
}

/// Format a `REG_MULTI_SZ` byte payload (NUL-separated UTF-8) into a CMake `;`-separated list.
///
/// Only the final NUL code unit is excluded from separator replacement (matching CMake 4.5.1's
/// `cmWindowsRegistry::ReadValue`). When `separator` is `None` or empty, prior NUL code units
/// are replaced with `;` (e.g., `a\0b\0\0` -> `a;b;`, `a\0b\0` -> `a;b`). When a non-empty
/// `separator` is supplied, occurrences of its first UTF-16 code unit are replaced with `;`
/// and decoding stops at the first remaining NUL code unit.
#[must_use]
pub fn format_multi_sz(raw: &[u8], separator: Option<&[u8]>) -> BString {
    let mut units: Vec<u16> = String::from_utf8_lossy(raw).encode_utf16().collect();
    format_multi_sz_units(&mut units, separator)
}

/// Decode a Windows `REG_MULTI_SZ` UTF-16LE byte buffer into a CMake `;`-separated list.
#[must_use]
pub fn decode_multi_sz_utf16le(raw_bytes: &[u8], separator: Option<&[u8]>) -> BString {
    let mut units: Vec<u16> = raw_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    format_multi_sz_units(&mut units, separator)
}

#[cfg(test)]
mod tests {
    use super::{decode_multi_sz_utf16le, format_multi_sz};

    #[test]
    fn decodes_reg_multi_sz_with_default_and_custom_separator() {
        let utf16le =
            |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(u16::to_le_bytes).collect() };

        // Matches registry_host64bit.reg VALUE_MULTI_SZ: "data1\0data2\0"
        let single_nul = utf16le("data1\0data2\0");
        assert_eq!(decode_multi_sz_utf16le(&single_nul, None), "data1;data2");
        assert_eq!(
            decode_multi_sz_utf16le(&single_nul, Some(b"")),
            "data1;data2"
        );

        // Double-NUL terminated REG_MULTI_SZ excludes only the final NUL (matching CMake 4.5.1):
        // "a\0b\0\0" -> "a;b;"
        let double_nul = utf16le("a\0b\0\0");
        assert_eq!(decode_multi_sz_utf16le(&double_nul, None), "a;b;");
        assert_eq!(decode_multi_sz_utf16le(&double_nul, Some(b"")), "a;b;");
        assert_eq!(format_multi_sz(b"a\0b\0\0", None), "a;b;");

        // Matches registry_host64bit.reg VALUE2_MULTI_SZ: "data1|data2\0"
        let pipe_sep = utf16le("data1|data2\0");
        assert_eq!(
            decode_multi_sz_utf16le(&pipe_sep, Some(b"|")),
            "data1;data2"
        );
        assert_eq!(decode_multi_sz_utf16le(&pipe_sep, None), "data1|data2");

        // Multi-char SEPARATOR uses only its first UTF-16 code unit (matching cmWindowsRegistry).
        let multi_char = utf16le("data1||data2\0");
        assert_eq!(
            decode_multi_sz_utf16le(&multi_char, Some(b"||")),
            "data1;;data2"
        );
        let pipe_x = utf16le("data1|x|data2\0");
        assert_eq!(
            decode_multi_sz_utf16le(&pipe_x, Some(b"|x")),
            "data1;x;data2"
        );

        // Non-NUL separator does not replace remaining NULs and stops at the first remaining NUL.
        let stops_at_nul = utf16le("data1|data2\0data3\0\0");
        assert_eq!(
            decode_multi_sz_utf16le(&stops_at_nul, Some(b"|")),
            "data1;data2"
        );
        assert_eq!(
            format_multi_sz(b"data1|data2\0data3\0\0", Some(b"|")),
            "data1;data2"
        );
        assert_eq!(format_multi_sz(b"\0data1\0\0", Some(b"|")), "");

        // Non-ASCII single UTF-16 code unit separator.
        let non_ascii = utf16le("data1\u{00A7}data2\0");
        assert_eq!(
            decode_multi_sz_utf16le(&non_ascii, Some("\u{00A7}extra".as_bytes())),
            "data1;data2"
        );

        assert_eq!(format_multi_sz(b"\0", None), "");
        assert_eq!(format_multi_sz(b"\0\0", None), ";");
        assert_eq!(format_multi_sz(b"only\0", None), "only");
        assert_eq!(format_multi_sz(b"only\0\0", None), "only;");
    }
}
