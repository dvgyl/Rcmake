use std::cmp::Reverse;
use std::io::{Cursor, Read};

use rcmake_host::{FileKind, FileTime, Metadata};
use sevenz_rust2::encoder_options::{
    Bzip2Options, DeflateOptions, EncoderOptions, Lzma2Options, LzmaOptions, ZstandardOptions,
};
use sevenz_rust2::{
    Archive, ArchiveEntry, ArchiveWriter, BlockDecoder, EncoderConfiguration, EncoderMethod,
    NtTime, Password,
};

use crate::{ArchiveError, MAX_ENTRIES, SevenZipCompression, archive_error, size_limit};

pub(super) const MAGIC: &[u8] = b"\x37\x7a\xbc\xaf\x27\x1c";

const TICKS_PER_SECOND: u64 = 10_000_000;
const NTFS_EPOCH: i64 = 11_644_473_600;
const UNIX_ATTRIBUTES: u32 = 0x8000;
const MAX_HEADER_MEMORY_KIB: usize = 16 * 1024;
const MAX_DECODE_MEMORY_KIB: usize = 256 * 1024;

mod ppmd;

pub(super) struct Writer {
    output: Output,
    empty: Vec<ArchiveEntry>,
}

enum Output {
    Generic(ArchiveWriter<Cursor<Vec<u8>>>),
    Ppmd(ppmd::Writer),
}

impl Writer {
    pub(super) fn finish(mut self) -> Result<Vec<u8>, ArchiveError> {
        // CMake writes entries with data first, then empty files in input order.
        // Directories follow, in descending order by UTF-16LE name length and bytes.
        self.empty.sort_by_cached_key(|entry| {
            entry.is_directory.then(|| {
                let name: Vec<_> = entry
                    .name
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                Reverse((name.len(), name))
            })
        });
        for entry in self.empty {
            self.output.add(entry, &[])?;
        }
        match self.output {
            Output::Generic(writer) => writer
                .finish()
                .map(Cursor::into_inner)
                .map_err(archive_error),
            Output::Ppmd(writer) => Ok(writer.finish()),
        }
    }
}

impl Output {
    fn add(&mut self, entry: ArchiveEntry, bytes: &[u8]) -> Result<(), ArchiveError> {
        match self {
            Self::Generic(writer) => writer
                .push_archive_entry(entry, (!bytes.is_empty()).then_some(bytes))
                .map(|_| ())
                .map_err(archive_error),
            Self::Ppmd(writer) => writer.add(entry, bytes),
        }
    }
}

pub(super) fn writer(
    compression: SevenZipCompression,
    level: Option<u32>,
    threads: u32,
) -> Result<Writer, ArchiveError> {
    let default_level = level.unwrap_or(5);
    let (method, options) = match compression {
        SevenZipCompression::Store => (EncoderMethod::COPY, None),
        SevenZipCompression::Lzma => (
            EncoderMethod::LZMA,
            Some(EncoderOptions::Lzma(LzmaOptions::from_level(default_level))),
        ),
        SevenZipCompression::Lzma2 => {
            let options = if threads > 1 {
                // Each encoder thread needs its own dictionary.
                Lzma2Options::from_level_mt(default_level, threads.min(2), 0)
            } else {
                Lzma2Options::from_level(default_level)
            };
            (EncoderMethod::LZMA2, Some(options.into()))
        }
        SevenZipCompression::Deflate => (
            EncoderMethod::DEFLATE,
            Some(DeflateOptions::from_level(level.unwrap_or(6)).into()),
        ),
        SevenZipCompression::Bzip2 => (
            EncoderMethod::BZIP2,
            Some(Bzip2Options::from_level(level.filter(|&value| value > 0).unwrap_or(6)).into()),
        ),
        SevenZipCompression::Ppmd => {
            return Ok(Writer {
                output: Output::Ppmd(ppmd::Writer::new(level)?),
                empty: Vec::new(),
            });
        }
        SevenZipCompression::Zstd => (
            EncoderMethod::ZSTD,
            Some(
                ZstandardOptions::from_level(level.filter(|&value| value > 0).unwrap_or(3)).into(),
            ),
        ),
    };
    let mut method = EncoderConfiguration::new(method);
    if let Some(options) = options {
        method = method.with_options(options);
    }
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).map_err(archive_error)?;
    writer.set_content_methods(vec![method]);
    Ok(Writer {
        output: Output::Generic(writer),
        empty: Vec::new(),
    })
}

pub(super) fn add(
    writer: &mut Writer,
    name: &[u8],
    meta: &Metadata,
    mtime: FileTime,
    bytes: &[u8],
) -> Result<(), ArchiveError> {
    let name = std::str::from_utf8(name).map_err(archive_error)?;
    let (mut entry, kind, attributes) = match meta.kind {
        FileKind::Dir => (
            ArchiveEntry::new_directory(name.trim_end_matches('/')),
            0o040000,
            0x10,
        ),
        FileKind::Symlink => (ArchiveEntry::new_file(name), 0o120000, 0),
        _ => (ArchiveEntry::new_file(name), 0o100000, 0x20),
    };
    entry.has_windows_attributes = true;
    entry.windows_attributes = ((kind | (meta.mode & 0o7777)) << 16) | UNIX_ATTRIBUTES | attributes;
    entry.has_last_modified_date = true;
    let ticks = (i128::from(mtime.secs) + i128::from(NTFS_EPOCH)) * i128::from(TICKS_PER_SECOND)
        + i128::from(mtime.nanos / 100);
    entry.last_modified_date = NtTime::from(u64::try_from(ticks).map_err(archive_error)?);
    if bytes.is_empty() {
        writer.empty.push(entry);
        Ok(())
    } else {
        writer.output.add(entry, bytes)
    }
}

pub(super) fn open(bytes: &[u8]) -> Result<Archive, ArchiveError> {
    let archive = Archive::read_with_memory_limit(
        &mut Cursor::new(bytes),
        &Password::empty(),
        MAX_HEADER_MEMORY_KIB,
    )
    .map_err(archive_error)?;
    let limit_error = || ArchiveError::new("Archive exceeds the entry or size limit");
    if archive.files.len() > MAX_ENTRIES || archive.blocks.len() > MAX_ENTRIES {
        return Err(limit_error());
    }
    let mut bytes_left = size_limit();
    for block in &archive.blocks {
        bytes_left = bytes_left
            .checked_sub(block.get_unpack_size())
            .ok_or_else(limit_error)?;
        if block
            .coders
            .iter()
            .enumerate()
            .any(|(index, _)| block.get_unpack_size_at_index(index) > size_limit())
        {
            return Err(limit_error());
        }
    }
    Ok(archive)
}

pub(super) fn read_entries(
    bytes: &[u8],
    archive: &Archive,
    mut read: impl FnMut(usize, &ArchiveEntry, &mut dyn Read) -> Result<(), ArchiveError>,
) -> Result<(), ArchiveError> {
    let mut source = Cursor::new(bytes);
    let password = Password::empty();
    for (block_index, block) in archive.blocks.iter().enumerate() {
        let first = archive
            .stream_map
            .block_first_file_index
            .get(block_index)
            .copied()
            .ok_or_else(|| ArchiveError::new("Invalid 7-Zip block index"))?;
        if let [coder] = block.coders.as_slice()
            && coder.encoder_method_id() == EncoderMethod::ID_PPMD
        {
            let data = ppmd::decode(bytes, archive, block_index, coder.properties())?;
            let decoder = BlockDecoder::new(1, block_index, archive, &password, &mut source);
            let mut remaining = data.as_slice();
            for (index, entry) in decoder.entries().iter().enumerate() {
                let size = usize::try_from(entry.size).map_err(archive_error)?;
                let (payload, rest) = remaining
                    .split_at_checked(size)
                    .ok_or_else(|| ArchiveError::new("Invalid 7-Zip PPMd stream size"))?;
                remaining = rest;
                if entry.has_crc && u64::from(crc32fast::hash(payload)) != entry.crc {
                    return Err(ArchiveError::new("7-Zip PPMd entry checksum mismatch"));
                }
                read(first + index, entry, &mut &*payload)?;
            }
            if !remaining.is_empty() {
                return Err(ArchiveError::new("Invalid 7-Zip PPMd block size"));
            }
        } else {
            let mut index = first;
            let mut error = None;
            let result = BlockDecoder::new_with_memory_limit(
                1,
                MAX_DECODE_MEMORY_KIB,
                block_index,
                archive,
                &password,
                &mut source,
            )
            .for_each_entries(&mut |entry, data| {
                let result = read(index, entry, data);
                index += 1;
                if let Err(err) = result {
                    error = Some(err);
                    return Ok(false);
                }
                Ok(true)
            });
            if let Some(error) = error {
                return Err(error);
            }
            result.map_err(archive_error)?;
        }
    }
    for (index, entry) in archive.files.iter().enumerate() {
        if archive.stream_map.file_block_index.get(index) == Some(&None) {
            read(index, entry, &mut &[][..])?;
        }
    }
    Ok(())
}

pub(super) fn time(entry: &ArchiveEntry) -> Option<FileTime> {
    entry.has_last_modified_date.then(|| {
        let ticks = u64::from(entry.last_modified_date);
        FileTime {
            secs: (ticks / TICKS_PER_SECOND) as i64 - NTFS_EPOCH,
            nanos: ((ticks % TICKS_PER_SECOND) * 100) as u32,
        }
    })
}

pub(super) fn mode(entry: &ArchiveEntry) -> u32 {
    if entry.windows_attributes & UNIX_ATTRIBUTES != 0 {
        entry.windows_attributes >> 16
    } else if entry.is_directory {
        0o40755
    } else if entry.windows_attributes & 1 != 0 {
        0o100444
    } else {
        0o100644
    }
}
