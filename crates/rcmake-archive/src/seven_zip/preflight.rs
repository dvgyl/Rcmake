use std::io::{Cursor, SeekFrom};

use rcmake_host::ReadSeek;

use sevenz_rust2::{Archive, Block, BlockDecoder, EncoderMethod as Method, Password};

use super::{MAGIC, MAX_HEADER_MEMORY_KIB};
use crate::{ArchiveError, MAX_ENTRIES, archive_error, read_bounded};

const START_SIZE: usize = 32;
const MAX_HEADER_BYTES: usize = MAX_HEADER_MEMORY_KIB * 1024;

const END: u8 = 0x00;
const HEADER: u8 = 0x01;
const PROPERTIES: u8 = 0x02;
const MAIN_STREAMS: u8 = 0x04;
const FILES: u8 = 0x05;
const PACK_INFO: u8 = 0x06;
const UNPACK_INFO: u8 = 0x07;
const SUBSTREAMS: u8 = 0x08;
const SIZE: u8 = 0x09;
const CRC: u8 = 0x0a;
const FOLDER: u8 = 0x0b;
const CODER_SIZES: u8 = 0x0c;
const SUBSTREAM_COUNT: u8 = 0x0d;
const ENCODED_HEADER: u8 = 0x17;

pub(super) fn open(input: &mut dyn ReadSeek, len: u64) -> Result<Archive, ArchiveError> {
    let mut prefix = [0; START_SIZE];
    read_at(input, 0, &mut prefix)?;
    if !prefix.starts_with(MAGIC) || prefix.get(6) != Some(&0) {
        return Err(invalid());
    }
    let mut start = Header(prefix.get(8..).ok_or_else(invalid)?);
    let crc = u32::from_le_bytes(start.fixed()?);
    if crc32fast::hash(start.0) != crc {
        return Err(invalid());
    }
    let offset = u64::from_le_bytes(start.fixed()?);
    let size = u64::from_le_bytes(start.fixed()?);
    if size > MAX_HEADER_BYTES as u64 {
        return Err(limit());
    }
    let crc = u32::from_le_bytes(start.fixed()?);
    let position = offset.checked_add(START_SIZE as u64).ok_or_else(invalid)?;
    if position.checked_add(size).is_none_or(|end| end > len) {
        return Err(invalid());
    }
    let mut header = vec![0; size as usize];
    read_at(input, position, &mut header)?;
    if crc32fast::hash(&header) != crc {
        return Err(invalid());
    }

    if let [ENCODED_HEADER, stream_info @ ..] = header.as_slice() {
        let decoded = decode_header(input, offset, stream_info)?;
        validate_raw(&decoded, offset)?;
        read_raw(&decoded)
    } else {
        validate_raw(&header, offset)?;
        read_raw(&header)
    }
}

fn read_at(input: &mut dyn ReadSeek, position: u64, bytes: &mut [u8]) -> Result<(), ArchiveError> {
    input
        .seek(SeekFrom::Start(position))
        .map_err(archive_error)?;
    for chunk in bytes.chunks_mut(64 * 1024) {
        input.read_exact(chunk).map_err(archive_error)?;
    }
    Ok(())
}

fn validate_raw(header: &[u8], packed: u64) -> Result<(), ArchiveError> {
    let mut input = Header(header);
    input.marker(HEADER)?;
    let mut field = input.byte()?;
    if field == PROPERTIES {
        while input.byte()? != END {
            let bytes = usize::try_from(input.number()?).map_err(archive_error)?;
            input.take(bytes)?;
        }
        field = input.byte()?;
    }
    if field == MAIN_STREAMS {
        input.streams(false)?.packed_span(packed)?;
        field = input.byte()?;
    }
    match field {
        FILES => {
            input.count()?;
            Ok(())
        }
        END => Ok(()),
        _ => Err(invalid()),
    }
}

fn decode_header(
    mut source: &mut dyn ReadSeek,
    packed: u64,
    streams: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let mut input = Header(streams);
    let info = input.streams(true)?;
    if !input.0.is_empty() || info.folders != 1 || info.pack_streams != 1 || info.substreams != 1 {
        return Err(invalid());
    }
    let stream = info.packed_span(packed)?;

    // Present the validated header stream as one file. This reuses the bounded
    // decoder without letting it parse the decoded header as archive metadata.
    let mut header = vec![HEADER, MAIN_STREAMS];
    if info.has_substreams {
        header.extend_from_slice(streams);
    } else {
        let streams = streams.strip_suffix(&[END]).ok_or_else(invalid)?;
        header.extend_from_slice(streams);
        header.extend_from_slice(&[SUBSTREAMS, END, END]);
    }
    header.extend_from_slice(&[FILES, 1, END, END]);
    let archive = read_raw(&header)?;
    let [file] = archive.files.as_slice() else {
        return Err(invalid());
    };
    if file.size > MAX_HEADER_BYTES as u64 {
        return Err(limit());
    }
    let [block] = archive.blocks.as_slice() else {
        return Err(invalid());
    };
    check_decoder_memory(block, source, stream)?;

    source.seek(SeekFrom::Start(0)).map_err(archive_error)?;
    let mut decoded = None;
    BlockDecoder::new_with_memory_limit(
        1,
        MAX_HEADER_MEMORY_KIB,
        0,
        &archive,
        &Password::empty(),
        &mut source,
    )
    .for_each_entries(&mut |_, reader| {
        decoded = Some(read_bounded(reader, file.size));
        Ok(true)
    })
    .map_err(archive_error)?;
    let decoded = decoded.ok_or_else(invalid)??;
    if decoded.len() as u64 != file.size {
        return Err(invalid());
    }
    Ok(decoded)
}

fn check_decoder_memory(
    block: &Block,
    input: &mut dyn ReadSeek,
    stream: (u64, u64),
) -> Result<(), ArchiveError> {
    let mut remaining = MAX_HEADER_BYTES as u64;
    let mut packed_input = Some(stream);
    let mut count = 0;
    for (_, coder) in block.ordered_coder_iter() {
        count += 1;
        let method = coder.encoder_method_id();
        let work = match method {
            Method::ID_COPY | Method::ID_DELTA => 0,
            Method::ID_BCJ_X86
            | Method::ID_BCJ_ARM
            | Method::ID_BCJ_ARM_THUMB
            | Method::ID_BCJ_ARM64
            | Method::ID_BCJ_PPC
            | Method::ID_BCJ_IA64
            | Method::ID_BCJ_SPARC
            | Method::ID_BCJ_RISCV => 4 * 1024,
            Method::ID_LZMA => {
                let [props, a, b, c, d, ..] = coder.properties() else {
                    return Err(invalid());
                };
                if *props > 224 {
                    return Err(invalid());
                }
                let bytes = u64::from(u32::from_le_bytes([*a, *b, *c, *d])).max(4096);
                let literal_bits = (*props % 45) / 9 + *props % 9;
                // Include the literal probability tables as well as the dictionary.
                bytes.next_multiple_of(16) + (1536 << literal_bits) + 16 * 1024
            }
            Method::ID_LZMA2 => {
                let bits = u32::from(*coder.properties().first().ok_or_else(invalid)?);
                if bits >= 40 {
                    return Err(if bits == 40 { limit() } else { invalid() });
                }
                let dictionary = u64::from(2 | (bits & 1)) << (bits / 2 + 11);
                // Include the 64 KiB input buffer and the probability tables.
                dictionary + 104 * 1024
            }
            Method::ID_BZIP2 => 4 * 1024 * 1024,
            Method::ID_DEFLATE => 512 * 1024,
            Method::ID_ZSTD => {
                // A preceding transform can hide the frame from preflight.
                let span = packed_input.ok_or_else(|| {
                    ArchiveError::new("Zstandard must read packed bytes in a 7-Zip header")
                })?;
                zstd_window(input, span)? + 1024 * 1024
            }
            _ => return Err(ArchiveError::new("Unsupported 7-Zip header coder")),
        };
        // Include the reader and its fixed state in addition to codec workspace.
        remaining = remaining.checked_sub(work + 4096).ok_or_else(limit)?;
        if method != Method::ID_COPY {
            packed_input = None;
        }
    }
    if count != block.coders.len() {
        return Err(invalid());
    }
    Ok(())
}

fn zstd_window(
    input: &mut dyn ReadSeek,
    (mut offset, end): (u64, u64),
) -> Result<u64, ArchiveError> {
    let mut largest = 0;
    while offset < end {
        let mut bytes = [0; 18];
        let len = (end - offset).min(bytes.len() as u64) as usize;
        let bytes = bytes.get_mut(..len).ok_or_else(invalid)?;
        read_at(input, offset, bytes)?;
        let mut header = Header(bytes);
        let magic = u32::from_le_bytes(header.fixed()?);
        if (0x184d_2a50..=0x184d_2a5f).contains(&magic) {
            let size = u32::from_le_bytes(header.fixed()?);
            offset = advance(offset, 8 + u64::from(size), end)?;
            continue;
        }
        if magic != 0xfd2f_b528 {
            return Err(invalid());
        }
        let descriptor = header.byte()?;
        if descriptor & 0x08 != 0 {
            return Err(invalid());
        }
        let single = descriptor & 0x20 != 0;
        let window = if single {
            zstd::zstd_safe::get_frame_content_size(bytes)
                .map_err(|_| invalid())?
                .ok_or_else(invalid)?
        } else {
            let window = header.byte()?;
            let base = 1_u64 << (10 + (window >> 3));
            base + (base / 8) * u64::from(window & 7)
        };
        if window > MAX_HEADER_BYTES as u64 {
            return Err(limit());
        }
        largest = largest.max(window);
        let dictionary = [0, 1, 2, 4]
            .get(usize::from(descriptor & 3))
            .copied()
            .ok_or_else(invalid)?;
        let size = [usize::from(single), 2, 4, 8]
            .get(usize::from(descriptor >> 6))
            .copied()
            .ok_or_else(invalid)?;
        header.take(dictionary + size)?;
        offset = advance(offset, (len - header.0.len()) as u64, end)?;
        loop {
            if end - offset < 3 {
                return Err(invalid());
            }
            let mut block = [0; 3];
            read_at(input, offset, &mut block)?;
            let [a, b, c] = block;
            let block = u32::from_le_bytes([a, b, c, 0]);
            let bytes = match (block >> 1) & 3 {
                0 | 2 => u64::from(block >> 3),
                1 => 1,
                _ => return Err(invalid()),
            };
            offset = advance(offset, 3 + bytes, end)?;
            if block & 1 != 0 {
                break;
            }
        }
        if descriptor & 4 != 0 {
            offset = advance(offset, 4, end)?;
        }
    }
    Ok(largest)
}

fn advance(offset: u64, size: u64, end: u64) -> Result<u64, ArchiveError> {
    offset
        .checked_add(size)
        .filter(|&offset| offset <= end)
        .ok_or_else(invalid)
}

fn read_raw(header: &[u8]) -> Result<Archive, ArchiveError> {
    let mut start = Vec::with_capacity(20);
    start.extend_from_slice(&0_u64.to_le_bytes());
    start.extend_from_slice(&(header.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32fast::hash(header).to_le_bytes());
    let mut data = Vec::with_capacity(START_SIZE + header.len());
    data.extend_from_slice(MAGIC);
    data.extend_from_slice(&[0, 4]);
    data.extend_from_slice(&crc32fast::hash(&start).to_le_bytes());
    data.extend_from_slice(&start);
    data.extend_from_slice(header);
    Archive::read_with_memory_limit(
        &mut Cursor::new(data),
        &Password::empty(),
        MAX_HEADER_MEMORY_KIB,
    )
    .map_err(archive_error)
}

#[derive(Default)]
struct Streams {
    pack_position: u64,
    pack_size: u64,
    pack_streams: usize,
    folders: usize,
    substreams: usize,
    has_substreams: bool,
}

impl Streams {
    fn packed_span(&self, packed: u64) -> Result<(u64, u64), ArchiveError> {
        let end = advance(self.pack_position, self.pack_size, packed)?;
        Ok((
            START_SIZE as u64 + self.pack_position,
            START_SIZE as u64 + end,
        ))
    }
}

#[derive(Default)]
struct Folder {
    crc: bool,
}

struct Header<'a>(&'a [u8]);

impl<'a> Header<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], ArchiveError> {
        let (bytes, rest) = self.0.split_at_checked(len).ok_or_else(invalid)?;
        self.0 = rest;
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], ArchiveError> {
        self.take(N)?.try_into().map_err(archive_error)
    }

    fn byte(&mut self) -> Result<u8, ArchiveError> {
        let [byte] = self.fixed()?;
        Ok(byte)
    }

    fn marker(&mut self, byte: u8) -> Result<(), ArchiveError> {
        if self.byte()? == byte {
            Ok(())
        } else {
            Err(invalid())
        }
    }

    fn number(&mut self) -> Result<u64, ArchiveError> {
        let high = self.byte()?;
        let extra = high.leading_ones() as usize;
        let mut low = [0; 8];
        for (target, &byte) in low.iter_mut().zip(self.take(extra)?) {
            *target = byte;
        }
        if let Some(target) = low.get_mut(extra) {
            *target = high & (0x7f >> extra);
        }
        Ok(u64::from_le_bytes(low))
    }

    fn count(&mut self) -> Result<usize, ArchiveError> {
        let count = self.number()?;
        if count > MAX_ENTRIES as u64 {
            return Err(limit());
        }
        Ok(count as usize)
    }

    fn numbers(&mut self, count: usize) -> Result<(), ArchiveError> {
        for _ in 0..count {
            self.number()?;
        }
        Ok(())
    }

    fn streams(&mut self, encoded: bool) -> Result<Streams, ArchiveError> {
        let mut info = Streams::default();
        let mut field = self.byte()?;
        if field == PACK_INFO {
            info.pack_position = self.number()?;
            info.pack_streams = self.count()?;
            field = self.byte()?;
            if field == SIZE {
                for _ in 0..info.pack_streams {
                    info.pack_size = info
                        .pack_size
                        .checked_add(self.number()?)
                        .ok_or_else(invalid)?;
                }
                field = self.byte()?;
            }
            if field == CRC {
                self.crcs(info.pack_streams, |_| ())?;
                field = self.byte()?;
            }
            if field != END {
                return Err(invalid());
            }
            field = self.byte()?;
        }
        let folders = if field == UNPACK_INFO {
            let folders = self.folders(encoded)?;
            field = self.byte()?;
            folders
        } else {
            Vec::new()
        };
        info.folders = folders.len();
        info.substreams = folders.len();
        if field == SUBSTREAMS {
            info.has_substreams = true;
            info.substreams = self.substreams(&folders)?;
            field = self.byte()?;
        }
        if field != END {
            return Err(invalid());
        }
        Ok(info)
    }

    fn folders(&mut self, encoded: bool) -> Result<Vec<Folder>, ArchiveError> {
        self.marker(FOLDER)?;
        let count = self.count()?;
        self.marker(0)?; // External folder definitions are unsupported.
        let mut folders = Vec::new();
        let mut inputs = 0;
        let mut outputs = 0;
        for _ in 0..count {
            let coders = self.count()?;
            let mut folder_inputs = 0;
            for _ in 0..coders {
                let flags = self.byte()?;
                self.take(usize::from(flags & 15))?;
                let input = if flags & 0x10 == 0 {
                    1
                } else {
                    let input = self.count()?;
                    if self.number()? != 1 {
                        return Err(invalid());
                    }
                    input
                };
                folder_inputs = add_count(folder_inputs, input)?;
                if flags & 0x20 != 0 {
                    let size = usize::try_from(self.number()?).map_err(archive_error)?;
                    self.take(size)?;
                }
                if flags & 0x80 != 0 || (encoded && input != 1) {
                    return Err(invalid());
                }
            }
            inputs = add_count(inputs, folder_inputs)?;
            outputs = add_count(outputs, coders)?;
            let pairs = coders.checked_sub(1).ok_or_else(invalid)?;
            self.numbers(pairs * 2)?;
            let packed = folder_inputs.checked_sub(pairs).ok_or_else(invalid)?;
            match packed {
                0 => return Err(invalid()),
                1 => {}
                _ => self.numbers(packed)?,
            }
            folders.push(Folder::default());
        }
        self.marker(CODER_SIZES)?;
        for _ in 0..outputs {
            let size = self.number()?;
            if encoded && size > MAX_HEADER_BYTES as u64 {
                return Err(limit());
            }
        }
        let mut field = self.byte()?;
        if field == CRC {
            let mut folders = folders.iter_mut();
            self.crcs(count, |present| {
                if let Some(folder) = folders.next() {
                    folder.crc = present;
                }
            })?;
            field = self.byte()?;
        }
        if field != END {
            return Err(invalid());
        }
        Ok(folders)
    }

    fn substreams(&mut self, folders: &[Folder]) -> Result<usize, ArchiveError> {
        let mut field = self.byte()?;
        let mut streams = folders.len();
        let mut digests = folders.iter().filter(|folder| !folder.crc).count();
        let mut sizes = 0;
        if field == SUBSTREAM_COUNT {
            streams = 0;
            digests = 0;
            for folder in folders {
                let count = self.count()?;
                streams = add_count(streams, count)?;
                sizes += count.saturating_sub(1);
                if count != 1 || !folder.crc {
                    digests += count;
                }
            }
            field = self.byte()?;
        }
        if field == SIZE {
            self.numbers(sizes)?;
            field = self.byte()?;
        }
        if field == CRC {
            self.crcs(digests, |_| ())?;
            field = self.byte()?;
        }
        if field != END {
            return Err(invalid());
        }
        Ok(streams)
    }

    fn crcs(&mut self, count: usize, mut record: impl FnMut(bool)) -> Result<(), ArchiveError> {
        let all = self.byte()? != 0;
        let bits = if all {
            &[]
        } else {
            self.take(count.div_ceil(8))?
        };
        let mut digests = 0;
        for index in 0..count {
            let present = all
                || bits
                    .get(index / 8)
                    .is_some_and(|byte| byte & (0x80 >> (index % 8)) != 0);
            digests += usize::from(present);
            record(present);
        }
        self.take(digests * 4)?;
        Ok(())
    }
}

fn add_count(total: usize, count: usize) -> Result<usize, ArchiveError> {
    let total = total + count;
    if total > MAX_ENTRIES {
        return Err(limit());
    }
    Ok(total)
}

fn limit() -> ArchiveError {
    ArchiveError::new("7-Zip header exceeds the entry or size limit")
}

fn invalid() -> ArchiveError {
    ArchiveError::new("Invalid 7-Zip header")
}
