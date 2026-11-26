//! Text for `curl --config -`, so that URLs, headers, and credentials stay off the argument list.

use std::fmt;

/// A curl option that rcmake sends on standard input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurlOption {
    /// `url`
    Url,
    /// `header`
    Header,
    /// `user`
    User,
}

impl CurlOption {
    const fn name(self) -> &'static [u8] {
        match self {
            Self::Url => b"url",
            Self::Header => b"header",
            Self::User => b"user",
        }
    }
}

/// A value holds a byte that curl config text cannot carry (NUL, 0x1A, or CR/LF in an HTTP header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnencodableByte(pub u8);

impl fmt::Display for UnencodableByte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            b'\r' => write!(f, "HTTP header cannot contain carriage return (0x0d)"),
            b'\n' => write!(f, "HTTP header cannot contain newline (0x0a)"),
            other => write!(f, "curl config cannot encode byte 0x{other:02x}"),
        }
    }
}

impl std::error::Error for UnencodableByte {}

/// Config text for `curl --config -`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurlConfig(Vec<u8>);

impl CurlConfig {
    /// Append one option whose value is the concatenation of `parts`.
    ///
    /// Returns [`UnencodableByte`] without modifying the buffer when any part
    /// contains NUL (`0x00`) or Ctrl-Z (`0x1A`), or when `option` is
    /// [`CurlOption::Header`] and any part contains CR (`\r`) or LF (`\n`).
    pub fn push(&mut self, option: CurlOption, parts: &[&[u8]]) -> Result<(), UnencodableByte> {
        let bytes = || parts.iter().flat_map(|part| part.iter().copied());
        let forbidden = |byte: u8| {
            matches!(byte, 0x00 | 0x1A)
                || (option == CurlOption::Header && matches!(byte, b'\r' | b'\n'))
        };
        if let Some(byte) = bytes().find(|&byte| forbidden(byte)) {
            return Err(UnencodableByte(byte));
        }
        self.0.extend_from_slice(option.name());
        self.0.extend_from_slice(b" = \"");
        for byte in bytes() {
            match byte {
                b'\\' => self.0.extend_from_slice(b"\\\\"),
                b'"' => self.0.extend_from_slice(b"\\\""),
                b'\t' => self.0.extend_from_slice(b"\\t"),
                b'\n' => self.0.extend_from_slice(b"\\n"),
                b'\r' => self.0.extend_from_slice(b"\\r"),
                0x0B => self.0.extend_from_slice(b"\\v"),
                _ => self.0.push(byte),
            }
        }
        self.0.extend_from_slice(b"\"\n");
        Ok(())
    }

    /// Append one HTTP `header` option after rejecting CR (`\r`), LF (`\n`), NUL, and 0x1A.
    pub fn push_header(&mut self, value: &[u8]) -> Result<(), UnencodableByte> {
        self.push(CurlOption::Header, &[value])
    }

    /// Borrow the formatted config text.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Return the text for standard input.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{CurlConfig, CurlOption, UnencodableByte};

    /// Reference decoder matching curl's `unslashquote()` in `src/tool_parsecfg.c`.
    fn decode_single_line(line: &[u8], expected_option: &[u8]) -> Vec<u8> {
        let rest = line
            .strip_prefix(expected_option)
            .expect("missing option name")
            .strip_prefix(b" = \"")
            .expect("missing delimiter")
            .strip_suffix(b"\"\n")
            .expect("missing closing quote and newline");
        assert!(
            !rest.contains(&b'\n'),
            "raw newline inside quoted config value"
        );
        let mut out = Vec::with_capacity(rest.len());
        let mut iter = rest.iter().copied();
        while let Some(byte) = iter.next() {
            if byte == b'\\' {
                let escaped = iter.next().expect("dangling backslash");
                match escaped {
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b'v' => out.push(0x0B),
                    other => out.push(other),
                }
            } else {
                assert_ne!(byte, b'"', "unescaped quote inside value");
                out.push(byte);
            }
        }
        out
    }

    #[test]
    fn round_trips_every_encodable_byte_through_curl_unslashquote_rules() {
        for byte in 1_u8..=255 {
            if byte == 0x1A {
                continue;
            }
            let input = [b'a', byte, b'z'];
            let mut config = CurlConfig::default();
            config
                .push(CurlOption::User, &[&input[..2], &input[2..]])
                .expect("encodable byte");
            let decoded = decode_single_line(config.as_bytes(), b"user");
            assert_eq!(decoded, input, "mismatch for byte 0x{byte:02x}");
        }
    }

    #[test]
    fn rejects_crlf_in_http_headers_without_mutating_config() {
        for (input, expected) in [
            (
                b"X-Test: a\r\nX-Injected: b".as_slice(),
                UnencodableByte(b'\r'),
            ),
            (
                b"Authorization: Bearer tok\nX-Injected: b".as_slice(),
                UnencodableByte(b'\n'),
            ),
            (
                b"Proxy-Authorization: Basic abc\rX-Injected: b".as_slice(),
                UnencodableByte(b'\r'),
            ),
        ] {
            let mut config = CurlConfig::default();
            config
                .push_header(b"X-Safe: ok")
                .expect("valid header before rejection");
            let before = config.as_bytes().to_vec();
            assert_eq!(config.push_header(input), Err(expected));
            assert_eq!(config.push(CurlOption::Header, &[input]), Err(expected));
            assert_eq!(config.into_bytes(), before);
        }
    }

    #[test]
    fn rejects_nul_and_sub_without_leaving_partial_output() {
        for forbidden in [0x00_u8, 0x1A] {
            let mut empty = CurlConfig::default();
            assert_eq!(
                empty.push(
                    CurlOption::Url,
                    &[b"https://example.invalid/", &[forbidden]]
                ),
                Err(UnencodableByte(forbidden))
            );
            assert!(empty.into_bytes().is_empty());

            let mut existing = CurlConfig::default();
            existing
                .push(CurlOption::User, &[b"u:p"])
                .expect("valid user");
            let before = existing.as_bytes().to_vec();
            assert_eq!(
                existing.push(CurlOption::Header, &[b"X-Test: ", &[forbidden]]),
                Err(UnencodableByte(forbidden))
            );
            assert_eq!(existing.into_bytes(), before);
        }
    }
}
