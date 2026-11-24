//! Size limits for configure journal records.

use std::io::{self, Write};

use serde::Serialize;

/// Maximum JSON payload size for a configure snapshot.
pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn encoded_size(value: &impl Serialize, limit: usize) -> Option<usize> {
    let mut sink = ByteCount { left: limit };
    serde_json::to_writer(&mut sink, value).ok()?;
    Some(limit - sink.left)
}

struct ByteCount {
    left: usize,
}

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.left = self
            .left
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("Configure journal exceeds its byte limit."))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn write_json(
    value: &impl Serialize,
    output: &mut Vec<u8>,
    limit: usize,
) -> Result<(), serde_json::Error> {
    serde_json::to_writer(
        BoundedOutput {
            output,
            left: limit,
        },
        value,
    )
}

struct BoundedOutput<'a> {
    output: &'a mut Vec<u8>,
    left: usize,
}

impl Write for BoundedOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.left = self
            .left
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("Configure journal exceeds its byte limit."))?;
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
