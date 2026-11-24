use std::io;

use crate::Digest;

const VERSION: u8 = 1;
pub(crate) const HEADER_LEN: usize = 8 + 1 + 8;
const FOOTER_LEN: usize = 32;

pub(crate) const ENTRY_MAGIC: &[u8; 8] = b"RCMENTRY";
pub(crate) const BLOB_MAGIC: &[u8; 8] = b"RCM_BLOB";
pub(crate) const ENTRY_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) fn disk_limit(limit: usize) -> u64 {
    (HEADER_LEN + zstd::zstd_safe::compress_bound(limit) + FOOTER_LEN) as u64
}

pub(crate) fn encode(magic: &[u8; 8], payload: &[u8], limit: usize) -> io::Result<Vec<u8>> {
    if payload.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "Cache payload exceeds its size limit.",
        ));
    }
    let compressed = zstd::bulk::compress(payload, 3)?;
    let mut bytes = Vec::with_capacity(HEADER_LEN + compressed.len() + FOOTER_LEN);
    bytes.extend_from_slice(magic);
    bytes.push(VERSION);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&compressed);
    let checksum = Digest::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    Ok(bytes)
}

pub(crate) fn decode(magic: &[u8; 8], bytes: &[u8], limit: usize) -> Option<Vec<u8>> {
    let (body, checksum) = bytes.split_at_checked(bytes.len().checked_sub(FOOTER_LEN)?)?;
    let len = payload_len(magic, body)?;
    let compressed = body.get(HEADER_LEN..)?;
    if len > limit
        || Digest::hash(body).as_bytes() != checksum
        || zstd::zstd_safe::find_frame_compressed_size(compressed).ok()? != compressed.len()
    {
        return None;
    }
    let payload = zstd::bulk::decompress(compressed, len).ok()?;
    (payload.len() == len).then_some(payload)
}

pub(crate) fn payload_len(magic: &[u8; 8], bytes: &[u8]) -> Option<usize> {
    let header = bytes
        .get(..HEADER_LEN)?
        .strip_prefix(magic)?
        .strip_prefix(&[VERSION])?;
    usize::try_from(u64::from_le_bytes(header.try_into().ok()?)).ok()
}
