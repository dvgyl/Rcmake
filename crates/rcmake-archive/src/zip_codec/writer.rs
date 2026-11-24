use std::io::Write;

use flate2::{Compression, write::DeflateEncoder};
use jiff::tz::TimeZone;
use rcmake_host::{FileKind, FileTime, Metadata};

use super::{CENTRAL, END, LOCAL, ZIP64_END, ZIP64_LOCATOR, invalid_header};
use crate::{
    ArchiveError, ArchiveFormat, ZipCompression, archive_error, codec::Encoder, timestamp,
};

pub(crate) struct Writer {
    format: ArchiveFormat,
    level: Option<u32>,
    data: Vec<u8>,
    directory: Vec<u8>,
    count: u64,
}

impl Writer {
    pub(crate) fn new(format: ArchiveFormat, level: Option<u32>) -> Self {
        Self {
            format,
            level,
            data: Vec::new(),
            directory: Vec::new(),
            count: 0,
        }
    }

    pub(crate) fn add(
        &mut self,
        name: &[u8],
        meta: &Metadata,
        mtime: FileTime,
        contents: &[u8],
        zone: &TimeZone,
    ) -> Result<(), ArchiveError> {
        if name.contains(&b'\\') {
            return Err(ArchiveError::new(
                "ZIP input paths must use slash separators",
            ));
        }
        std::str::from_utf8(name).map_err(archive_error)?;
        if meta.kind == FileKind::Symlink {
            std::str::from_utf8(contents).map_err(archive_error)?;
        }
        let payload;
        let (method, version, flags, compressed) = match (meta.kind, self.format) {
            (FileKind::File, ArchiveFormat::Zip) => {
                let level = self.level.filter(|&level| level != 0).unwrap_or(6);
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
                encoder.write_all(contents).map_err(archive_error)?;
                payload = encoder.finish().map_err(archive_error)?;
                (8, 20, 0, payload.as_slice())
            }
            (FileKind::File, ArchiveFormat::ZipCompressed(codec)) => {
                payload = encode(codec, self.level, contents)?;
                let version = if codec == ZipCompression::Bzip2 {
                    46
                } else {
                    63
                };
                let flags = if codec == ZipCompression::Lzma { 2 } else { 0 };
                (codec.method(), version, flags, payload.as_slice())
            }
            _ => (0, 20, 0, contents),
        };

        let extra = timestamp::zip_time_fields(mtime)?;
        let dos = timestamp::zip_dos_time(mtime, zone);
        let name_len = u16::try_from(name.len()).map_err(archive_error)?;
        let extra_len = u16::try_from(extra.len()).map_err(archive_error)?;
        let offset = u32::try_from(self.data.len()).map_err(archive_error)?;
        let size = u32::try_from(contents.len()).map_err(archive_error)?;
        let compressed_size = u32::try_from(compressed.len()).map_err(archive_error)?;

        let mut fields = Vec::with_capacity(26);
        words(
            &mut fields,
            &[
                version,
                flags | (1 << 11),
                method,
                dos.timepart(),
                dos.datepart(),
            ],
        );
        dwords(
            &mut fields,
            &[crc32fast::hash(contents), compressed_size, size],
        );
        words(&mut fields, &[name_len, extra_len]);

        self.data.extend_from_slice(LOCAL);
        self.data.extend_from_slice(&fields);
        self.data.extend_from_slice(name);
        self.data.extend_from_slice(&extra);
        self.data.extend_from_slice(compressed);

        let kind = match meta.kind {
            FileKind::Dir => 0o040000,
            FileKind::Symlink => 0o120000,
            _ => 0o100000,
        };
        let attrs = ((kind | (meta.mode & 0o777)) << 16)
            | if meta.kind == FileKind::Dir { 0x10 } else { 0 };
        self.directory.extend_from_slice(CENTRAL);
        words(&mut self.directory, &[3 << 8 | version]);
        self.directory.extend_from_slice(&fields);
        words(&mut self.directory, &[0, 0, 0]); // Comment length, disk number, internal attributes.
        dwords(&mut self.directory, &[attrs, offset]);
        self.directory.extend_from_slice(name);
        self.directory.extend_from_slice(&extra);
        self.count += 1;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<Vec<u8>, ArchiveError> {
        let offset = u32::try_from(self.data.len()).map_err(archive_error)?;
        let size = u32::try_from(self.directory.len()).map_err(archive_error)?;
        self.data.append(&mut self.directory);
        if self.count >= u64::from(u16::MAX) {
            let end_offset = self.data.len() as u64;
            self.data.extend_from_slice(ZIP64_END);
            qwords(&mut self.data, &[44]);
            words(&mut self.data, &[45, 45]);
            dwords(&mut self.data, &[0, 0]);
            qwords(
                &mut self.data,
                &[self.count, self.count, u64::from(size), u64::from(offset)],
            );
            self.data.extend_from_slice(ZIP64_LOCATOR);
            dwords(&mut self.data, &[0]);
            qwords(&mut self.data, &[end_offset]);
            dwords(&mut self.data, &[1]);
        }
        let count = u16::try_from(self.count).unwrap_or(u16::MAX);
        self.data.extend_from_slice(END);
        words(&mut self.data, &[0, 0, count, count]);
        dwords(&mut self.data, &[size, offset]);
        words(&mut self.data, &[0]);
        Ok(self.data)
    }
}

fn encode(
    codec: ZipCompression,
    level: Option<u32>,
    bytes: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let mut encoder = Encoder::new(Some(codec.stream()?), level, 0)?;
    encoder.write_all(bytes).map_err(archive_error)?;
    let data = encoder.finish().map_err(archive_error)?;
    if codec != ZipCompression::Lzma {
        return Ok(data);
    }
    let mut zip = Vec::with_capacity(data.len());
    zip.extend_from_slice(&[9, 4, 5, 0]);
    zip.extend_from_slice(data.get(..5).ok_or_else(invalid_header)?);
    zip.extend_from_slice(data.get(13..).ok_or_else(invalid_header)?);
    Ok(zip)
}

fn words(bytes: &mut Vec<u8>, values: &[u16]) {
    bytes.extend(values.iter().flat_map(|value| value.to_le_bytes()));
}

fn dwords(bytes: &mut Vec<u8>, values: &[u32]) {
    bytes.extend(values.iter().flat_map(|value| value.to_le_bytes()));
}

fn qwords(bytes: &mut Vec<u8>, values: &[u64]) {
    bytes.extend(values.iter().flat_map(|value| value.to_le_bytes()));
}
