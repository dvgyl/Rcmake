use std::io::Cursor;

use sevenz_rust2::{Archive, BlockDecoder, Password};

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

pub(super) fn open(bytes: &[u8]) -> Result<Archive, ArchiveError> {
    let (prefix, rest) = bytes.split_at_checked(START_SIZE).ok_or_else(invalid)?;
    if !prefix.starts_with(MAGIC) || prefix.get(6) != Some(&0) {
        return Err(invalid());
    }
    let mut start = Header(prefix.get(8..).ok_or_else(invalid)?);
    let crc = u32::from_le_bytes(start.fixed()?);
    if crc32fast::hash(start.0) != crc {
        return Err(invalid());
    }
    let offset = usize::try_from(u64::from_le_bytes(start.fixed()?)).map_err(archive_error)?;
    let size = usize::try_from(u64::from_le_bytes(start.fixed()?)).map_err(archive_error)?;
    if size > MAX_HEADER_BYTES {
        return Err(limit());
    }
    let crc = u32::from_le_bytes(start.fixed()?);
    let packed = rest.get(..offset).ok_or_else(invalid)?;
    let mut tail = Header(rest.get(offset..).ok_or_else(invalid)?);
    let header = tail.take(size)?;
    if crc32fast::hash(header) != crc {
        return Err(invalid());
    }

    if let [ENCODED_HEADER, stream_info @ ..] = header {
        let decoded = decode_header(bytes, packed.len(), stream_info)?;
        validate_raw(&decoded)?;
        read_raw(&decoded)
    } else {
        validate_raw(header)?;
        Archive::read_with_memory_limit(
            &mut Cursor::new(bytes),
            &Password::empty(),
            MAX_HEADER_MEMORY_KIB,
        )
        .map_err(archive_error)
    }
}

fn validate_raw(header: &[u8]) -> Result<(), ArchiveError> {
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
        input.streams(false)?;
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
    bytes: &[u8],
    packed_size: usize,
    streams: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let mut input = Header(streams);
    let info = input.streams(true)?;
    if !input.0.is_empty()
        || info.folders != 1
        || info.pack_streams != 1
        || info.substreams != 1
        || info.pack_position.saturating_add(info.pack_size) > packed_size as u64
    {
        return Err(invalid());
    }

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

    let mut source = Cursor::new(bytes);
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
