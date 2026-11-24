//! POSIX shell quoting for byte strings.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_words_without_changing_their_bytes() {
        assert_eq!(quote(b"a@b+z-1:/x"), "a@b+z-1:/x");
        assert_eq!(quote_always(b"a@b"), "'a@b'");
        assert_eq!(quote(b""), "''");
        assert_eq!(quote(b"a'b"), "'a'\\''b'");
        assert_eq!(quote(b"\xff"), BString::from(b"'\xff'".as_slice()));
    }
}
