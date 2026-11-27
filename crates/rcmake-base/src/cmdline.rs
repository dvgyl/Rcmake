//! Shell-style command-line argument splitting matching CMake's `separate_arguments`.

#![forbid(unsafe_code)]

use crate::BString;
use crate::list_ops::is_space as space;

/// Lazy iterator splitting a Unix or Windows command line into raw byte arguments.
#[derive(Clone, Debug)]
pub struct Arguments<'a> {
    input: &'a [u8],
    offset: usize,
    windows: bool,
}

impl<'a> Arguments<'a> {
    /// Construct a command-line splitter over `input`.
    ///
    /// Pass `windows = false` for Unix shell quoting (`UNIX_COMMAND`) or
    /// `windows = true` for Windows command-line quoting (`WINDOWS_COMMAND`).
    #[must_use]
    pub const fn new(input: &'a [u8], windows: bool) -> Self {
        Self {
            input,
            offset: 0,
            windows,
        }
    }

    /// Return the unconsumed suffix of the original input buffer.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        self.input.get(self.offset..).unwrap_or_default()
    }
}

impl Iterator for Arguments<'_> {
    type Item = BString;

    fn next(&mut self) -> Option<Self::Item> {
        let mut input = self.rest().iter().copied().peekable();
        let mut consumed = 0;
        while input.peek().is_some_and(|byte| space(*byte)) {
            input.next();
            consumed += 1;
        }
        input.peek()?;
        let mut quote = None;
        let mut quoted = false;
        let mut token = Vec::new();
        while let Some(byte) = input.next_if(|byte| quote.is_some() || !space(*byte)) {
            consumed += 1;
            match byte {
                b'\\' if !self.windows => {
                    if let Some(escaped) = input.next() {
                        token.push(escaped);
                        consumed += 1;
                    }
                }
                b'\\' => {
                    let mut slashes = 1;
                    while input.next_if_eq(&b'\\').is_some() {
                        consumed += 1;
                        slashes += 1;
                    }
                    if input.next_if_eq(&b'"').is_some() {
                        consumed += 1;
                        token.extend(std::iter::repeat_n(b'\\', slashes / 2));
                        if slashes % 2 == 0 {
                            quote = if quote.is_some() { None } else { Some(b'"') };
                        } else {
                            token.push(b'"');
                        }
                    } else {
                        token.extend(std::iter::repeat_n(b'\\', slashes));
                    }
                }
                b'"' | b'\'' if byte == b'"' || !self.windows => {
                    quoted = true;
                    if quote == Some(byte) {
                        quote = None;
                    } else if quote.is_none() {
                        quote = Some(byte);
                    } else {
                        token.push(byte);
                    }
                }
                _ => token.push(byte),
            }
        }
        self.offset += consumed;
        (quoted || !token.is_empty()).then(|| token.into())
    }
}

/// Split a Unix-style command line (`UNIX_COMMAND`) into raw byte arguments.
#[must_use]
pub fn split_unix(input: &[u8]) -> Vec<BString> {
    Arguments::new(input, false).collect()
}

/// Split a Windows-style command line (`WINDOWS_COMMAND`) into raw byte arguments.
#[must_use]
pub fn split_windows(input: &[u8]) -> Vec<BString> {
    Arguments::new(input, true).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_unix_quotes_and_escapes() {
        let args = split_unix(br#"cc -DFOO="a b" -I'dir with space' -DBAR=x\ y ""#);
        let expected: Vec<BString> = ["cc", "-DFOO=a b", "-Idir with space", "-DBAR=x y", ""]
            .into_iter()
            .map(BString::from)
            .collect();
        assert_eq!(args, expected);
    }

    #[test]
    fn splits_windows_backslashes_and_quotes() {
        let args = split_windows(br#"cl.exe /I"C:\Program Files\SDK\\" /DNAME=\"ok\" 'lit'"#);
        let expected: Vec<BString> = [
            "cl.exe",
            r"/IC:\Program Files\SDK\",
            r#"/DNAME="ok""#,
            "'lit'",
        ]
        .into_iter()
        .map(BString::from)
        .collect();
        assert_eq!(args, expected);
    }
}
