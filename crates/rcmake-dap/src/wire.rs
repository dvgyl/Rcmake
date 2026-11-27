//! Debug Adapter Protocol message framing: `Content-Length: <n>\r\n\r\n<body>`.
//!
//! The client is untrusted. The reader limits the header block to
//! [`MAX_HEADER_BYTES`] and the body to [`MAX_BODY_BYTES`]. It reads the body in
//! pieces, so a large `Content-Length` value does not allocate memory before the data
//! arrives.

use std::io::{self, BufRead, BufReader, Read};

/// Largest header block, including the empty line at its end.
pub const MAX_HEADER_BYTES: usize = 1024;

/// Largest message body.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Error in the framing of a received message. The session ends after an error.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The header block is longer than [`MAX_HEADER_BYTES`].
    HeaderTooLong,
    /// A header line has no `:`.
    MalformedHeader,
    /// The header block has no `Content-Length` field.
    MissingLength,
    /// The header block has more than one `Content-Length` field.
    DuplicateLength,
    /// The `Content-Length` value is not a decimal number.
    InvalidLength,
    /// The `Content-Length` value is larger than [`MAX_BODY_BYTES`].
    BodyTooLarge,
    /// The stream ended inside a message.
    Truncated,
    /// The stream reported an error.
    Io(io::ErrorKind),
}

/// Reads framed messages from a byte stream.
pub struct FrameReader<R> {
    inner: BufReader<R>,
}

impl<R: Read> FrameReader<R> {
    /// Wrap `inner`.
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::with_capacity(8 * 1024, inner),
        }
    }

    /// Read the next message body. Return `Ok(None)` if the stream ends between
    /// messages.
    ///
    /// # Errors
    ///
    /// Return a [`FrameError`] for a malformed or oversized message, or a stream error.
    pub fn read_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let Some(header) = self.read_header()? else {
            return Ok(None);
        };
        let length = parse_header(&header)?;
        let mut body = Vec::with_capacity(length.min(64 * 1024));
        let mut limited = (&mut self.inner).take(length as u64);
        limited
            .read_to_end(&mut body)
            .map_err(|error| FrameError::Io(error.kind()))?;
        if body.len() != length {
            return Err(FrameError::Truncated);
        }
        Ok(Some(body))
    }

    /// Read bytes up to and including the first `\r\n\r\n`.
    fn read_header(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut header = Vec::new();
        loop {
            let available = match self.inner.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(FrameError::Io(error.kind())),
            };
            if available.is_empty() {
                return if header.is_empty() {
                    Ok(None)
                } else {
                    Err(FrameError::Truncated)
                };
            }
            let mut used = 0;
            let mut done = false;
            for &byte in available {
                used += 1;
                header.push(byte);
                if header.ends_with(b"\r\n\r\n") {
                    done = true;
                    break;
                }
                if header.len() >= MAX_HEADER_BYTES {
                    break;
                }
            }
            self.inner.consume(used);
            if done {
                return Ok(Some(header));
            }
            if header.len() >= MAX_HEADER_BYTES {
                return Err(FrameError::HeaderTooLong);
            }
        }
    }
}

/// Get the `Content-Length` value of a header block that ends with `\r\n\r\n`.
fn parse_header(header: &[u8]) -> Result<usize, FrameError> {
    let block = header.strip_suffix(b"\r\n\r\n").unwrap_or(header);
    let mut length = None;
    for line in block.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            return Err(FrameError::MalformedHeader);
        };
        let (name, value) = line.split_at(colon);
        let value = value.get(1..).unwrap_or_default().trim_ascii();
        if !name.trim_ascii().eq_ignore_ascii_case(b"Content-Length") {
            continue;
        }
        if length.is_some() {
            return Err(FrameError::DuplicateLength);
        }
        length = Some(parse_length(value)?);
    }
    length.ok_or(FrameError::MissingLength)
}

fn parse_length(value: &[u8]) -> Result<usize, FrameError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(FrameError::InvalidLength);
    }
    let mut length: usize = 0;
    for &digit in value {
        length = length
            .checked_mul(10)
            .and_then(|n| n.checked_add(usize::from(digit - b'0')))
            .filter(|&n| n <= MAX_BODY_BYTES)
            .ok_or(FrameError::BodyTooLarge)?;
    }
    Ok(length)
}

/// Frame `body` as one message.
#[must_use]
pub fn encode(body: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that returns at most `step` bytes for each call.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn frames(data: &[u8], step: usize) -> Vec<Result<Option<Vec<u8>>, FrameError>> {
        let mut reader = FrameReader::new(Trickle { data, step });
        let mut out = Vec::new();
        loop {
            let frame = reader.read_frame();
            let stop = !matches!(frame, Ok(Some(_)));
            out.push(frame);
            if stop {
                return out;
            }
        }
    }

    #[test]
    fn split_header_and_body_reassemble() {
        let mut data = encode(b"{\"a\":1}");
        data.extend(encode(b"[]"));
        for step in [1, 2, 3, 7, 1024] {
            assert_eq!(
                frames(&data, step),
                vec![
                    Ok(Some(b"{\"a\":1}".to_vec())),
                    Ok(Some(b"[]".to_vec())),
                    Ok(None)
                ],
                "step {step}"
            );
        }
    }

    #[test]
    fn other_headers_and_case_are_accepted() {
        let data = b"content-length: 2\r\nContent-Type: x\r\n\r\n{}";
        assert_eq!(frames(data, 5)[0], Ok(Some(b"{}".to_vec())));
    }

    #[test]
    fn body_over_limit_is_rejected_without_reading_it() {
        let data = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        assert_eq!(
            frames(data.as_bytes(), 64)[0],
            Err(FrameError::BodyTooLarge)
        );
        let exact = format!("Content-Length: {MAX_BODY_BYTES}\r\n\r\nshort");
        assert_eq!(frames(exact.as_bytes(), 64)[0], Err(FrameError::Truncated));
        let huge = "Content-Length: 99999999999999999999999999\r\n\r\n";
        assert_eq!(
            frames(huge.as_bytes(), 64)[0],
            Err(FrameError::BodyTooLarge)
        );
    }

    #[test]
    fn malformed_headers_are_errors() {
        let cases: [(&[u8], FrameError); 6] = [
            (b"Content-Length: 1x\r\n\r\n", FrameError::InvalidLength),
            (b"Content-Length: -1\r\n\r\n", FrameError::InvalidLength),
            (b"Content-Length:\r\n\r\n", FrameError::InvalidLength),
            (
                b"Content-Length: 1\r\nContent-Length: 1\r\n\r\nx",
                FrameError::DuplicateLength,
            ),
            (b"X: 1\r\n\r\n", FrameError::MissingLength),
            (b"garbage\r\n\r\n", FrameError::MalformedHeader),
        ];
        for (data, error) in cases {
            assert_eq!(frames(data, 3)[0], Err(error), "{data:?}");
        }
    }

    #[test]
    fn header_over_limit_is_rejected() {
        let data = vec![b'a'; MAX_HEADER_BYTES * 4];
        assert_eq!(frames(&data, 100)[0], Err(FrameError::HeaderTooLong));
    }

    #[test]
    fn end_inside_message_is_truncated() {
        assert_eq!(frames(b"Content-Le", 4)[0], Err(FrameError::Truncated));
        assert_eq!(
            frames(b"Content-Length: 5\r\n\r\nab", 4)[0],
            Err(FrameError::Truncated)
        );
    }

    #[test]
    fn random_bytes_never_panic() {
        // A deterministic xorshift generator: 1 MiB of noise, with some valid headers.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut data = Vec::with_capacity(1 << 20);
        while data.len() < 1 << 20 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if state.is_multiple_of(97) {
                data.extend_from_slice(b"Content-Length: 12\r\n\r\n");
            }
            data.push((state >> 24) as u8);
        }
        for step in [1, 13, 4096] {
            let mut reader = FrameReader::new(Trickle { data: &data, step });
            for _ in 0..(1 << 20) {
                match reader.read_frame() {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }
}
