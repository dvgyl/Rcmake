//! Size limits for configure journal records.

use rcmake_json::{ToJson, write_compact};

/// Maximum JSON payload size for a configure snapshot.
pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Get the compact JSON size of `value`, or `None` when it exceeds `limit` bytes.
pub(super) fn encoded_size(value: &impl ToJson, limit: usize) -> Option<usize> {
    let len = write_compact(&value.to_json()).len();
    (len <= limit).then_some(len)
}

/// Append the compact JSON of `value` to `output`, or return `None` when it
/// exceeds `limit` bytes.
pub(super) fn write_json(value: &impl ToJson, output: &mut Vec<u8>, limit: usize) -> Option<()> {
    let json = write_compact(&value.to_json());
    (json.len() <= limit).then(|| output.extend_from_slice(&json))
}
