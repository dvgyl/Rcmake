//! Argument quoting for POSIX shells, Windows command lines, and CMake.

use crate::BString;

/// Quote a POSIX shell word if it is empty or contains bytes that need quoting.
#[must_use]
pub fn quote(arg: &[u8]) -> BString {
    if !arg.is_empty()
        && arg
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(b))
    {
        return arg.into();
    }
    quote_always(arg)
}

/// Put a POSIX shell word in single quotes and escape embedded single quotes.
#[must_use]
pub fn quote_always(arg: &[u8]) -> BString {
    let mut result = BString::from("'");
    for &byte in arg {
        if byte == b'\'' {
            result.extend_from_slice(b"'\\''");
        } else {
            result.push(byte);
        }
    }
    result.push(b'\'');
    result
}

/// Append a double-quoted CMake argument. Escape backslashes, quotes, and dollar signs.
pub fn cmake_quote(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &byte in bytes {
        if matches!(byte, b'\\' | b'"' | b'$') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b'"');
}

/// Append one argument using Microsoft command-line quoting.
pub fn windows_argument(word: &[u8], out: &mut Vec<u8>, force_quote: bool) {
    if !force_quote && !word.is_empty() && !word.iter().any(|b| matches!(b, b' ' | b'\t' | b'"')) {
        out.extend_from_slice(word);
        return;
    }
    out.push(b'"');
    let mut slashes = 0;
    for &byte in word {
        if byte == b'\\' {
            slashes += 1;
        } else {
            let count = if byte == b'"' {
                2 * slashes + 1
            } else {
                slashes
            };
            out.extend(std::iter::repeat_n(b'\\', count));
            slashes = 0;
            out.push(byte);
        }
    }
    out.extend(std::iter::repeat_n(b'\\', 2 * slashes));
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_cmake_arguments() {
        let mut out = b"set(VAR ".to_vec();
        cmake_quote(&mut out, b"\\\"$\"\xff");
        assert_eq!(out, b"set(VAR \"\\\\\\\"\\$\\\"\xff\"");
    }

    #[test]
    fn quotes_windows_arguments() {
        for (word, forced, expected) in [
            (b"plain".as_slice(), false, b"plain".as_slice()),
            (b"plain", true, br#""plain""#),
            (b"", false, br#""""#),
            (
                br"C:\MSVC Tools\cl.exe",
                false,
                br#""C:\MSVC Tools\cl.exe""#,
            ),
            (br"C:\100% complete\", false, br#""C:\100% complete\\""#),
            (br#"a\"b"#, false, br#""a\\\"b""#),
            (b"a\tb", false, b"\"a\tb\""),
        ] {
            let mut out = b" ".to_vec();
            windows_argument(word, &mut out, forced);
            assert_eq!(out.strip_prefix(b" "), Some(expected));
        }
    }

    #[test]
    fn quotes_shell_words_without_changing_their_bytes() {
        assert_eq!(quote(b"a@b+z-1:/x"), "a@b+z-1:/x");
        assert_eq!(quote_always(b"a@b"), "'a@b'");
        assert_eq!(quote(b""), "''");
        assert_eq!(quote(b"a'b"), "'a'\\''b'");
        assert_eq!(quote(b"\xff"), BString::from(b"'\xff'".as_slice()));
    }
}
