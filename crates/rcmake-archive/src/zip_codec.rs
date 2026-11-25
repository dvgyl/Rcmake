use std::io::{BufRead, BufReader, Read, SeekFrom, Write};

use flate2::bufread::DeflateDecoder;
use jiff::tz::TimeZone;
use rcmake_host::{FileTime, ReadSeek};
use zip::{
    DateTime,
    extra_fields::{ExtendedTimestamp, Ntfs},
};

use crate::codec;
use crate::extract::stage;
use crate::{ArchiveError, TarCompression, ZipCompression, archive_error, timestamp};

mod directory;
pub(super) mod writer;

pub(super) use directory::open;

const LOCAL: &[u8; 4] = b"PK\x03\x04";
const CENTRAL: &[u8; 4] = b"PK\x01\x02";
const END: &[u8; 4] = b"PK\x05\x06";
const ZIP64_END: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const CENTRAL_HEADER_SIZE: usize = 46;
const END_RECORD_SIZE: usize = 22;

pub(super) struct Directory {
    records: Vec<u8>,
    pub(super) bytes_left: u64,
    count: usize,
    offset: u64,
    local_end: u64,
}

impl Directory {
    fn preflight(
        &self,
        input: &mut dyn ReadSeek,
        mut bytes_left: u64,
    ) -> Result<u64, ArchiveError> {
        let mut records = self.records.as_slice();
        for _ in 0..self.count {
            let entry = self.next(&mut records)?;
            entry.payload_start(input)?;
            bytes_left = bytes_left.checked_sub(entry.size).ok_or_else(size_error)?;
        }
        // A digital signature can follow the central directory entries.
        if let Some(signature) = records.strip_prefix(b"PK\x05\x05") {
            if signature.len() != 2 + usize::from(word(signature, 0)?) {
                return Err(invalid_header());
            }
        } else if !records.is_empty() {
            return Err(invalid_header());
        }
        Ok(bytes_left)
    }

    pub(super) fn metadata_bytes(&self) -> usize {
        self.records.len()
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = Result<Entry<'_>, ArchiveError>> + '_ {
        let mut records = self.records.as_slice();
        (0..self.count).map(move |_| self.next(&mut records))
    }

    fn next<'a>(&self, records: &mut &'a [u8]) -> Result<Entry<'a>, ArchiveError> {
        let mut reader = Fields(records.strip_prefix(CENTRAL).ok_or_else(invalid_header)?);
        let header = reader.take(CENTRAL_HEADER_SIZE - CENTRAL.len())?;
        let name = reader.take(usize::from(word(header, 24)?))?;
        let extra = reader.take(usize::from(word(header, 26)?))?;
        reader.take(usize::from(word(header, 28)?))?;
        *records = reader.0;

        let mut entry = Entry {
            name,
            extra,
            method: word(header, 6)?,
            flags: word(header, 4)?,
            date: word(header, 10)?,
            time: word(header, 8)?,
            crc: dword(header, 12)?,
            compressed: u64::from(dword(header, 16)?),
            size: u64::from(dword(header, 20)?),
            local_offset: u64::from(dword(header, 38)?),
            mode: unix_mode(word(header, 0)? >> 8, dword(header, 34)?),
            local_end: self.local_end,
        };
        if word(header, 30)? != 0 && word(header, 30)? != u16::MAX {
            return Err(invalid_header());
        }
        let mut fields = Fields(extra);
        while !fields.0.is_empty() {
            let (kind, field) = fields.extra()?;
            match kind {
                0x0001 => entry.zip64(field)?,
                0x7075 => {
                    let [1, crc @ ..] = field else {
                        return Err(invalid_header());
                    };
                    if dword(crc, 0)? != crc32fast::hash(entry.name) {
                        return Err(invalid_header());
                    }
                    entry.name = crc.get(4..).ok_or_else(invalid_header)?;
                    std::str::from_utf8(entry.name).map_err(archive_error)?;
                }
                _ => {}
            }
        }
        entry.local_offset = entry
            .local_offset
            .checked_add(self.offset)
            .ok_or_else(invalid_header)?;
        Ok(entry)
    }
}

pub(super) struct Entry<'a> {
    pub(super) name: &'a [u8],
    pub(super) size: u64,
    pub(super) mode: Option<u32>,
    local_end: u64,
    extra: &'a [u8],
    method: u16,
    flags: u16,
    date: u16,
    time: u16,
    crc: u32,
    compressed: u64,
    local_offset: u64,
}

impl Entry<'_> {
    fn zip64(&mut self, bytes: &[u8]) -> Result<(), ArchiveError> {
        let mut fields = Fields(bytes);
        let use_all = bytes.len() >= 24;
        let sentinel = u64::from(u32::MAX);
        for value in [&mut self.size, &mut self.compressed, &mut self.local_offset] {
            if use_all || *value == sentinel {
                *value = qword(fields.take(8)?, 0)?;
            }
        }
        Ok(())
    }

    pub(super) fn time(&self, zone: &TimeZone) -> Result<Option<FileTime>, ArchiveError> {
        let mut unix_time = None;
        let mut fields = Fields(self.extra);
        while !fields.0.is_empty() {
            let (id, mut data) = fields.extra()?;
            let len = data.len() as u16;
            match id {
                0x000a => {
                    let time = Ntfs::try_from_reader(&mut data, len).map_err(archive_error)?;
                    return Ok(Some(timestamp::ntfs_time(time.mtime())));
                }
                0x5455 => {
                    let time = ExtendedTimestamp::try_from_reader(&mut data, len)
                        .map_err(archive_error)?;
                    if let Some(secs) = time.mod_time() {
                        unix_time = Some(timestamp::seconds(i64::from(secs as i32)));
                    }
                }
                _ => {}
            }
        }
        Ok(unix_time.or_else(|| {
            timestamp::dos_to_unix(DateTime::try_from_msdos(self.date, self.time).ok()?, zone)
        }))
    }

    fn payload_start(&self, input: &mut dyn ReadSeek) -> Result<u64, ArchiveError> {
        let header_end = self
            .local_offset
            .checked_add(30)
            .filter(|&end| end <= self.local_end)
            .ok_or_else(invalid_header)?;
        let header = directory::read_at::<30>(input, self.local_offset)?;
        if !header.starts_with(LOCAL) {
            return Err(invalid_header());
        }
        let start = header_end + u64::from(word(&header, 26)?) + u64::from(word(&header, 28)?);
        if start
            .checked_add(self.compressed)
            .is_none_or(|end| end > self.local_end)
        {
            return Err(invalid_header());
        }
        Ok(start)
    }

    pub(super) fn payload(
        &self,
        input: &mut dyn ReadSeek,
        output: &mut dyn Write,
    ) -> Result<(), ArchiveError> {
        if self.flags & 1 != 0 {
            return Err(ArchiveError::new("Encrypted ZIP entries are not supported"));
        }
        let start = self.payload_start(input)?;
        input.seek(SeekFrom::Start(start)).map_err(archive_error)?;
        let mut packed = BufReader::with_capacity(stage::BUFFER, input.take(self.compressed));
        let mut checksum = Checksum {
            output,
            crc: crc32fast::Hasher::new(),
        };
        let size = self.size;
        match self.method {
            0 => stage::copy(&mut packed, &mut checksum, size)?,
            8 => stage::copy(DeflateDecoder::new(&mut packed), &mut checksum, size)?,
            14 => {
                let mut header = [0; 9];
                packed.read_exact(&mut header).map_err(archive_error)?;
                let [_, _, 5, 0, properties @ ..] = header else {
                    return Err(invalid_header());
                };
                let len = size.to_le_bytes();
                let stream = properties
                    .as_slice()
                    .chain(len.as_slice())
                    .chain(&mut packed);
                stage::copy(
                    codec::decoder(TarCompression::Lzma, stream)?,
                    &mut checksum,
                    size,
                )?;
            }
            method => {
                let filter = match method {
                    12 => TarCompression::Bzip2,
                    93 | 20 => TarCompression::Zstd,
                    95 => TarCompression::Xz,
                    _ => {
                        return Err(ArchiveError::new(format!(
                            "Unsupported ZIP compression method {method}"
                        )));
                    }
                };
                stage::copy(codec::decoder(filter, &mut packed)?, &mut checksum, size)?;
            }
        }
        if checksum.crc.finalize() != self.crc {
            return Err(ArchiveError::new("ZIP entry checksum does not match"));
        }
        if !packed.fill_buf().map_err(archive_error)?.is_empty() || packed.get_ref().limit() != 0 {
            return Err(invalid_header());
        }
        Ok(())
    }
}

struct Checksum<'a> {
    output: &'a mut dyn Write,
    crc: crc32fast::Hasher,
}

impl Write for Checksum<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.crc.update(
            bytes
                .get(..written)
                .ok_or_else(|| std::io::Error::other("Invalid ZIP output length"))?,
        );
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

fn unix_mode(system: u16, attrs: u32) -> Option<u32> {
    match (system, attrs) {
        (_, 0) => None,
        (3, _) => Some(attrs >> 16),
        (0, _) => {
            let mode = if attrs & 0x10 != 0 { 0o40775 } else { 0o100664 };
            Some(if attrs & 1 == 0 { mode } else { mode & 0o555 })
        }
        _ => None,
    }
}

impl ZipCompression {
    fn method(self) -> u16 {
        match self {
            Self::Bzip2 => 12,
            Self::Lzma => 14,
            Self::Xz => 95,
            Self::Zstd => 93,
            Self::Ppmd => 98,
        }
    }

    fn stream(self) -> Result<TarCompression, ArchiveError> {
        Ok(match self {
            Self::Bzip2 => TarCompression::Bzip2,
            Self::Lzma => TarCompression::Lzma,
            Self::Xz => TarCompression::Xz,
            Self::Zstd => TarCompression::Zstd,
            Self::Ppmd => return Err(ppmd_error()),
        })
    }
}

pub(super) fn ppmd_error() -> ArchiveError {
    ArchiveError::new("PPMd is not supported for zip")
}

struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], ArchiveError> {
        let (value, rest) = self.0.split_at_checked(size).ok_or_else(invalid_header)?;
        self.0 = rest;
        Ok(value)
    }

    fn extra(&mut self) -> Result<(u16, &'a [u8]), ArchiveError> {
        let header = self.take(4)?;
        Ok((word(header, 0)?, self.take(usize::from(word(header, 2)?))?))
    }
}

fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], ArchiveError> {
    bytes
        .get(offset..offset + N)
        .ok_or_else(invalid_header)?
        .try_into()
        .map_err(archive_error)
}

fn word(bytes: &[u8], offset: usize) -> Result<u16, ArchiveError> {
    Ok(u16::from_le_bytes(field(bytes, offset)?))
}

fn dword(bytes: &[u8], offset: usize) -> Result<u32, ArchiveError> {
    Ok(u32::from_le_bytes(field(bytes, offset)?))
}

fn qword(bytes: &[u8], offset: usize) -> Result<u64, ArchiveError> {
    Ok(u64::from_le_bytes(field(bytes, offset)?))
}

fn invalid_header() -> ArchiveError {
    ArchiveError::new("Invalid ZIP entry header")
}

fn size_error() -> ArchiveError {
    ArchiveError::new("Archive exceeds the size limit")
}

fn entry_limit() -> ArchiveError {
    ArchiveError::new("Archive exceeds the entry or size limit")
}
