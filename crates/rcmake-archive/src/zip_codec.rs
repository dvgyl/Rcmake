use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, Write};

use flate2::Crc;
use zip::{CompressionMethod, ZipArchive};

use crate::codec::{self, Encoder};
use crate::extract::read_payload;
use crate::{
    ArchiveError, TarCompression, ZipCompression, archive_error, read_bounded, size_limit,
};

pub(super) struct Entry {
    codec: ZipCompression,
    size: u32,
    crc: u32,
}

impl ZipCompression {
    fn from_method(method: CompressionMethod) -> Option<Self> {
        match method {
            CompressionMethod::BZIP2 => Some(Self::Bzip2),
            CompressionMethod::LZMA => Some(Self::Lzma),
            CompressionMethod::XZ => Some(Self::Xz),
            CompressionMethod::ZSTD | CompressionMethod::ZSTD_DEPRECATED => Some(Self::Zstd),
            _ => None,
        }
    }

    fn method(self) -> u16 {
        match self {
            Self::Bzip2 => 12,
            Self::Lzma => 14,
            Self::Xz => 95,
            Self::Zstd => 93,
        }
    }

    fn stream(self) -> TarCompression {
        match self {
            Self::Bzip2 => TarCompression::Bzip2,
            Self::Lzma => TarCompression::Lzma,
            Self::Xz => TarCompression::Xz,
            Self::Zstd => TarCompression::Zstd,
        }
    }
}

pub(super) fn encode(
    codec: ZipCompression,
    level: Option<u32>,
    bytes: &[u8],
) -> Result<(Entry, Vec<u8>), ArchiveError> {
    let mut encoder = Encoder::new(Some(codec.stream()), level, 0)?;
    encoder.write_all(bytes).map_err(archive_error)?;
    let mut data = encoder.finish().map_err(archive_error)?;
    if codec == ZipCompression::Lzma {
        let properties = data.get(..5).ok_or_else(invalid_header)?;
        let mut zip = Vec::with_capacity(data.len());
        zip.extend_from_slice(&[9, 4, 5, 0]);
        zip.extend_from_slice(properties);
        zip.extend_from_slice(data.get(13..).ok_or_else(invalid_header)?);
        data = zip;
    }
    let entry = Entry {
        codec,
        size: u32::try_from(bytes.len()).map_err(archive_error)?,
        crc: checksum(bytes),
    };
    Ok((entry, data))
}

pub(super) fn update_headers(
    bytes: &mut [u8],
    compressed: &HashMap<String, Entry>,
) -> Result<(), ArchiveError> {
    if compressed.is_empty() {
        return Ok(());
    }
    let mut archive = ZipArchive::new(Cursor::new(&*bytes)).map_err(archive_error)?;
    let mut offsets = Vec::with_capacity(compressed.len());
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index).map_err(archive_error)?;
        if let Some(entry) = compressed.get(file.name()) {
            let local = usize::try_from(file.header_start()).map_err(archive_error)?;
            let central = usize::try_from(file.central_header_start()).map_err(archive_error)?;
            offsets.push((local, central, entry));
        }
    }
    for (local, central, entry) in offsets {
        update(
            bytes.get_mut(local..).ok_or_else(invalid_header)?,
            b"PK\x03\x04",
            entry,
        )?;
        update(
            bytes.get_mut(central..).ok_or_else(invalid_header)?,
            b"PK\x01\x02",
            entry,
        )?;
    }
    Ok(())
}

fn update(header: &mut [u8], signature: &[u8; 4], entry: &Entry) -> Result<(), ArchiveError> {
    if !header.starts_with(signature) {
        return Err(invalid_header());
    }
    let header = header.get_mut(4..).ok_or_else(invalid_header)?;
    let central_offset = if signature == b"PK\x01\x02" { 2 } else { 0 };
    let fields = header
        .get_mut(central_offset..central_offset + 22)
        .ok_or_else(invalid_header)?;
    let version = match entry.codec {
        ZipCompression::Bzip2 => 46_u16.to_le_bytes(),
        _ => 63_u16.to_le_bytes(),
    };
    set(fields, 0, &version)?;
    if entry.codec == ZipCompression::Lzma {
        let flags = fields.get_mut(2).ok_or_else(invalid_header)?;
        *flags |= 2;
    }
    set(fields, 4, &entry.codec.method().to_le_bytes())?;
    set(fields, 10, &entry.crc.to_le_bytes())?;
    set(fields, 18, &entry.size.to_le_bytes())
}

fn set(fields: &mut [u8], offset: usize, value: &[u8]) -> Result<(), ArchiveError> {
    fields
        .get_mut(offset..offset + value.len())
        .ok_or_else(invalid_header)?
        .copy_from_slice(value);
    Ok(())
}

pub(super) fn payload<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    index: usize,
) -> Result<Vec<u8>, ArchiveError> {
    let raw = archive.by_index_raw(index).map_err(archive_error)?;
    let size = raw.size();
    let Some(codec) = ZipCompression::from_method(raw.compression()) else {
        drop(raw);
        return read_payload(archive.by_index(index).map_err(archive_error)?, size);
    };
    if raw.encrypted() {
        return Err(ArchiveError::new("Encrypted ZIP entries are not supported"));
    }
    let crc = raw.crc32();
    let compressed = read_bounded(raw, size_limit())?;
    let lzma;
    let bytes = if codec == ZipCompression::Lzma {
        let [_, _, 5, 0, properties @ ..] = compressed.get(..9).ok_or_else(invalid_header)? else {
            return Err(invalid_header());
        };
        let tail = compressed.get(9..).ok_or_else(invalid_header)?;
        lzma = [properties, &size.to_le_bytes(), tail].concat();
        lzma.as_slice()
    } else {
        compressed.as_slice()
    };
    let data = read_payload(codec::decoder(codec.stream(), bytes)?, size)?;
    if checksum(&data) != crc {
        return Err(ArchiveError::new("ZIP entry checksum does not match"));
    }
    Ok(data)
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(bytes);
    crc.sum()
}

fn invalid_header() -> ArchiveError {
    ArchiveError::new("Invalid ZIP entry header")
}
