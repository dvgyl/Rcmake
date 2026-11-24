use flate2::read::DeflateDecoder;
use jiff::tz::TimeZone;
use rcmake_host::FileTime;
use zip::{
    DateTime,
    extra_fields::{ExtendedTimestamp, Ntfs},
};

use crate::codec;
use crate::extract::read_payload;
use crate::{
    ArchiveError, MAX_ENTRIES, TarCompression, ZipCompression, archive_error, size_limit, timestamp,
};

pub(super) mod writer;

const LOCAL: &[u8; 4] = b"PK\x03\x04";
const CENTRAL: &[u8; 4] = b"PK\x01\x02";
const END: &[u8; 4] = b"PK\x05\x06";
const ZIP64_END: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const CENTRAL_HEADER_SIZE: usize = 46;
const END_RECORD_SIZE: usize = 22;

pub(super) struct Directory<'a> {
    records: &'a [u8],
    count: usize,
    offset: usize,
    local: &'a [u8],
}

pub(super) fn open(bytes: &[u8]) -> Result<Directory<'_>, ArchiveError> {
    let mut selected = None;
    let mut error = None;
    // ZIP permits trailing bytes after the end record and its comment.
    for (position, magic) in bytes.windows(END.len()).enumerate().rev() {
        let comment_start = position + END_RECORD_SIZE;
        if let Some((selected, _)) = &selected
            && comment_start + usize::from(u16::MAX) <= *selected
        {
            break;
        }
        if magic != END {
            continue;
        }
        let Some(footer) = footer_at(bytes, position) else {
            continue;
        };
        // A valid end record can still be part of an earlier record's comment.
        if let Some((selected, _)) = &selected
            && !(comment_start..footer.comment_end).contains(selected)
        {
            continue;
        }
        match open_directory(bytes, footer) {
            Ok(directory) => selected = Some((position, directory)),
            Err(failure) => {
                error.get_or_insert(failure);
            }
        }
    }
    selected
        .map(|(_, directory)| directory)
        .ok_or_else(|| error.unwrap_or_else(invalid_header))
}

fn open_directory<'a>(bytes: &'a [u8], footer: Footer<'a>) -> Result<Directory<'a>, ArchiveError> {
    let layout = if let Some(locator) = footer.locator {
        zip64_layout(bytes, locator)?
    } else {
        zip32_layout(bytes, footer)?
    };
    if layout.count > MAX_ENTRIES as u64 {
        return Err(entry_limit());
    }
    let end = layout
        .start
        .checked_add(layout.size)
        .ok_or_else(invalid_header)?;
    if end > layout.footer {
        return Err(invalid_header());
    }
    let records = bytes.get(layout.start..end).ok_or_else(invalid_header)?;
    let local = bytes.get(..layout.start).ok_or_else(invalid_header)?;
    if bytes
        .get(end..layout.footer)
        .is_some_and(|tail| tail.starts_with(CENTRAL))
    {
        return Err(invalid_header());
    }
    let directory = Directory {
        records,
        count: layout.count as usize,
        offset: layout.offset,
        local,
    };
    directory.preflight()?;
    Ok(directory)
}

struct Footer<'a> {
    position: usize,
    comment_end: usize,
    fields: &'a [u8],
    locator: Option<(usize, &'a [u8])>,
}

fn footer_at(bytes: &[u8], position: usize) -> Option<Footer<'_>> {
    let fields = bytes.get(position + END.len()..position + END_RECORD_SIZE)?;
    let comment_size = usize::from(word(fields, 16).ok()?);
    let comment_end = position + END_RECORD_SIZE + comment_size;
    bytes.get(..comment_end)?;
    let locator = position.checked_sub(20).and_then(|offset| {
        let locator = bytes.get(offset..position)?.strip_prefix(ZIP64_LOCATOR)?;
        (word(fields, 6).ok()? == u16::MAX || dword(fields, 12).ok()? == u32::MAX)
            .then_some((offset, locator))
    });
    Some(Footer {
        position,
        comment_end,
        fields,
        locator,
    })
}

struct Layout {
    start: usize,
    size: usize,
    offset: usize,
    count: u64,
    footer: usize,
}

fn zip32_layout(bytes: &[u8], footer: Footer<'_>) -> Result<Layout, ArchiveError> {
    let fields = footer.fields;
    let count = word(fields, 6)?;
    if word(fields, 0)? != 0 || word(fields, 2)? != 0 || word(fields, 4)? != count {
        return Err(invalid_header());
    }
    let relative = dword(fields, 12)? as usize;
    let size = dword(fields, 8)? as usize;
    // Check the declared offset even when the directory is empty.
    let available = bytes
        .get(relative..footer.position)
        .ok_or_else(invalid_header)?;
    if size > available.len() || size < usize::from(count) * CENTRAL_HEADER_SIZE {
        return Err(invalid_header());
    }
    let start = if count == 0 && size == 0 {
        footer.position
    } else {
        relative
            + available
                .windows(CENTRAL.len())
                .take(available.len() - size + 1)
                .position(|magic| magic == CENTRAL)
                .ok_or_else(invalid_header)?
    };
    let offset = start.checked_sub(relative).ok_or_else(invalid_header)?;
    Ok(Layout {
        start,
        size,
        offset,
        count: u64::from(count),
        footer: footer.position,
    })
}

fn zip64_layout(bytes: &[u8], locator: (usize, &[u8])) -> Result<Layout, ArchiveError> {
    let (position, fields) = locator;
    let relative = usize::try_from(qword(fields, 4)?).map_err(archive_error)?;
    if dword(fields, 0)? != 0 || dword(fields, 12)? > 1 {
        return Err(invalid_header());
    }
    let search = bytes.get(relative..position).ok_or_else(invalid_header)?;
    for (offset, signature) in search.windows(ZIP64_END.len()).enumerate() {
        if signature != ZIP64_END {
            continue;
        }
        let Some(tail) = search.get(offset + 4..) else {
            continue;
        };
        let Ok(record_size) = qword(tail, 0) else {
            continue;
        };
        // The locator can follow a gap after the ZIP64 end record.
        if record_size < 44 || record_size.saturating_add(8) > tail.len() as u64 {
            continue;
        }
        if dword(tail, 12)? != 0 || dword(tail, 16)? != 0 || word(tail, 10)? > word(tail, 8)? {
            continue;
        }
        let count = qword(tail, 28)?;
        if qword(tail, 20)? != count {
            continue;
        }
        if count > MAX_ENTRIES as u64 {
            return Err(entry_limit());
        }
        let start = usize::try_from(qword(tail, 44)?)
            .map_err(archive_error)?
            .checked_add(offset)
            .ok_or_else(invalid_header)?;
        let size = usize::try_from(qword(tail, 36)?).map_err(archive_error)?;
        return Ok(Layout {
            start,
            size,
            offset,
            count,
            footer: relative + offset,
        });
    }
    Err(invalid_header())
}

impl<'a> Directory<'a> {
    pub(super) fn preflight(&self) -> Result<u64, ArchiveError> {
        let mut bytes_left = size_limit();
        let mut records = self.records;
        for _ in 0..self.count {
            let entry = self.next(&mut records)?;
            bytes_left = bytes_left
                .checked_sub(entry.name.len() as u64 + 512)
                .ok_or_else(entry_limit)?
                .checked_sub(entry.size)
                .ok_or_else(size_error)?;
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

    pub(super) fn entries(&self) -> impl Iterator<Item = Result<Entry<'a>, ArchiveError>> + '_ {
        let mut records = self.records;
        (0..self.count).map(move |_| self.next(&mut records))
    }

    fn next(&self, records: &mut &'a [u8]) -> Result<Entry<'a>, ArchiveError> {
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
            local: self.local,
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
            .checked_add(self.offset as u64)
            .ok_or_else(invalid_header)?;
        Ok(entry)
    }
}

pub(super) struct Entry<'a> {
    pub(super) name: &'a [u8],
    pub(super) size: u64,
    pub(super) mode: Option<u32>,
    local: &'a [u8],
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

    pub(super) fn payload(&self) -> Result<Vec<u8>, ArchiveError> {
        if self.flags & 1 != 0 {
            return Err(ArchiveError::new("Encrypted ZIP entries are not supported"));
        }
        let offset = usize::try_from(self.local_offset).map_err(archive_error)?;
        let local = self
            .local
            .get(offset..)
            .and_then(|bytes| bytes.strip_prefix(LOCAL))
            .ok_or_else(invalid_header)?;
        let start = 26 + usize::from(word(local, 22)?) + usize::from(word(local, 24)?);
        let mut local = Fields(local.get(start..).ok_or_else(invalid_header)?);
        let compressed = local.take(usize::try_from(self.compressed).map_err(archive_error)?)?;

        let data = match self.method {
            0 => read_payload(compressed, self.size)?,
            8 => read_payload(DeflateDecoder::new(compressed), self.size)?,
            14 => {
                let Some([_, _, 5, 0, properties @ ..]) = compressed.get(..9) else {
                    return Err(invalid_header());
                };
                let tail = compressed.get(9..).ok_or_else(invalid_header)?;
                let lzma = [properties, &self.size.to_le_bytes(), tail].concat();
                read_payload(codec::decoder(TarCompression::Lzma, &lzma)?, self.size)?
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
                read_payload(codec::decoder(filter, compressed)?, self.size)?
            }
        };
        if crc32fast::hash(&data) != self.crc {
            return Err(ArchiveError::new("ZIP entry checksum does not match"));
        }
        Ok(data)
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
