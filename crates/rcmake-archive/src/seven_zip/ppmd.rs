use ppmd_turbo::{Params, SevenZDecoder, SevenZEncoder, SevenZStatus};
use sevenz_rust2::{Archive, ArchiveEntry, EncoderMethod};

use super::{MAGIC, MAX_DECODE_MEMORY_KIB};
use crate::{ArchiveError, archive_error, size_limit};

const SIGNATURE_SIZE: usize = 32;
const END: u8 = 0x00;
const HEADER: u8 = 0x01;
const STREAMS: u8 = 0x04;
const FILES: u8 = 0x05;
const PACK_INFO: u8 = 0x06;
const UNPACK_INFO: u8 = 0x07;
const SUBSTREAMS: u8 = 0x08;
const SIZE: u8 = 0x09;
const CRC: u8 = 0x0a;
const FOLDER: u8 = 0x0b;
const CODER_UNPACK_SIZE: u8 = 0x0c;
const EMPTY_STREAM: u8 = 0x0e;
const EMPTY_FILE: u8 = 0x0f;
const NAME: u8 = 0x11;
const MTIME: u8 = 0x14;
const ATTRIBUTES: u8 = 0x15;

pub(crate) struct Writer {
    params: Params,
    output: Vec<u8>,
    files: Vec<ArchiveEntry>,
}

impl Writer {
    pub(super) fn new(level: Option<u32>) -> Result<Self, ArchiveError> {
        let level = level.unwrap_or(6);
        let params = Params::new(3 + level, 1 << (19 + level)).map_err(archive_error)?;
        Ok(Self {
            params,
            output: vec![0; SIGNATURE_SIZE],
            files: Vec::new(),
        })
    }

    pub(super) fn add(
        &mut self,
        mut entry: ArchiveEntry,
        mut data: &[u8],
    ) -> Result<(), ArchiveError> {
        if !data.is_empty() {
            entry.has_stream = true;
            entry.size = data.len() as u64;
            entry.crc = u64::from(crc32fast::hash(data));
            let offset = self.output.len();
            let mut encoder = SevenZEncoder::new(self.params).map_err(archive_error)?;
            let mut buffer = [0; 8192];
            while !data.is_empty() {
                let step = encoder.encode(data, &mut buffer).map_err(archive_error)?;
                let invalid = || ArchiveError::new("Invalid PPMd encoder progress");
                self.output
                    .extend_from_slice(buffer.get(..step.produced).ok_or_else(invalid)?);
                data = data.get(step.consumed..).ok_or_else(invalid)?;
            }
            loop {
                let step = encoder.finish(&mut buffer, false).map_err(archive_error)?;
                let remaining = buffer
                    .get(..step.produced)
                    .ok_or_else(|| ArchiveError::new("Invalid PPMd finish length"))?;
                self.output.extend_from_slice(remaining);
                if step.done {
                    break;
                }
            }
            entry.compressed_size = (self.output.len() - offset) as u64;
        } else {
            entry.has_stream = false;
        }
        self.files.push(entry);
        Ok(())
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        let mut header = vec![HEADER];
        self.write_streams(&mut header);
        self.write_files(&mut header);
        header.push(END);

        let mut start = Vec::with_capacity(20);
        start.extend_from_slice(&((self.output.len() - SIGNATURE_SIZE) as u64).to_le_bytes());
        start.extend_from_slice(&(header.len() as u64).to_le_bytes());
        start.extend_from_slice(&crc32fast::hash(&header).to_le_bytes());
        let mut signature = Vec::with_capacity(SIGNATURE_SIZE);
        signature.extend_from_slice(MAGIC);
        signature.extend_from_slice(&[0, 4]);
        signature.extend_from_slice(&crc32fast::hash(&start).to_le_bytes());
        signature.extend_from_slice(&start);
        for (byte, value) in self.output.iter_mut().zip(signature) {
            *byte = value;
        }
        self.output.extend_from_slice(&header);
        self.output
    }

    fn write_streams(&self, header: &mut Vec<u8>) {
        let streams = || self.files.iter().filter(|entry| entry.has_stream);
        let count = streams().count() as u64;
        if count == 0 {
            return;
        }
        header.extend_from_slice(&[STREAMS, PACK_INFO, 0]);
        number(header, count);
        header.push(SIZE);
        for file in streams() {
            number(header, file.compressed_size);
        }
        header.extend_from_slice(&[END, UNPACK_INFO, FOLDER]);
        number(header, count);
        header.push(0);
        for _ in streams() {
            header.extend_from_slice(&[1, 0x20 | EncoderMethod::ID_PPMD.len() as u8]);
            header.extend_from_slice(EncoderMethod::ID_PPMD);
            let props = self.params.to_7z_props();
            number(header, props.len() as u64);
            header.extend_from_slice(&props);
        }
        header.push(CODER_UNPACK_SIZE);
        for file in streams() {
            number(header, file.size);
        }
        header.extend_from_slice(&[CRC, 1]);
        for file in streams() {
            header.extend_from_slice(&(file.crc as u32).to_le_bytes());
        }
        header.extend_from_slice(&[END, SUBSTREAMS, END, END]);
    }

    fn write_files(&self, header: &mut Vec<u8>) {
        header.push(FILES);
        number(header, self.files.len() as u64);
        if self.files.iter().any(|entry| !entry.has_stream) {
            property(
                header,
                EMPTY_STREAM,
                &bits(self.files.iter().map(|entry| !entry.has_stream)),
            );
            property(
                header,
                EMPTY_FILE,
                &bits(
                    self.files
                        .iter()
                        .filter(|entry| !entry.has_stream)
                        .map(|entry| !entry.is_directory),
                ),
            );
        }
        let mut names = vec![0];
        let mut mtimes = vec![1, 0];
        let mut attrs = vec![1, 0];
        for entry in &self.files {
            for code in entry.name.encode_utf16().chain([0]) {
                names.extend_from_slice(&code.to_le_bytes());
            }
            mtimes.extend_from_slice(&u64::from(entry.last_modified_date).to_le_bytes());
            attrs.extend_from_slice(&entry.windows_attributes.to_le_bytes());
        }
        property(header, NAME, &names);
        property(header, MTIME, &mtimes);
        property(header, ATTRIBUTES, &attrs);
        header.push(END);
    }
}

fn property(header: &mut Vec<u8>, id: u8, content: &[u8]) {
    header.push(id);
    number(header, content.len() as u64);
    header.extend_from_slice(content);
}

fn bits(values: impl Iterator<Item = bool>) -> Vec<u8> {
    let mut result = Vec::new();
    for (index, value) in values.enumerate() {
        let bit = index % 8;
        if bit == 0 {
            result.push(0);
        }
        if value && let Some(byte) = result.last_mut() {
            *byte |= 0x80 >> bit;
        }
    }
    result
}

fn number(header: &mut Vec<u8>, value: u64) {
    let extra = ((64 - value.leading_zeros()).saturating_sub(1) / 7).min(8) as usize;
    let prefix = 0xff_u8.checked_shl((8 - extra) as u32).unwrap_or(0);
    let high = value.checked_shr((8 * extra) as u32).unwrap_or(0) as u8;
    header.push(prefix | high);
    header.extend(value.to_le_bytes().into_iter().take(extra));
}

pub(super) fn decode(
    bytes: &[u8],
    archive: &Archive,
    block_index: usize,
    props: &[u8],
) -> Result<Vec<u8>, ArchiveError> {
    let invalid = || ArchiveError::new("Invalid 7-Zip PPMd block");
    let params = Params::from_7z_props(props).map_err(archive_error)?;
    if params.mem_size() as usize / 1024 > MAX_DECODE_MEMORY_KIB {
        return Err(ArchiveError::new("7-Zip PPMd memory limit exceeded"));
    }
    let block = archive.blocks.get(block_index).ok_or_else(invalid)?;
    let size = block.get_unpack_size();
    if size > size_limit() {
        return Err(ArchiveError::new("Archive exceeds the size limit"));
    }
    let map = &archive.stream_map;
    let pack = *map
        .block_first_pack_stream_index()
        .get(block_index)
        .ok_or_else(invalid)?;
    let offset = (SIGNATURE_SIZE as u64)
        .checked_add(archive.pack_pos())
        .and_then(|base| base.checked_add(*map.pack_stream_offsets().get(pack)?))
        .ok_or_else(invalid)?;
    let end = offset
        .checked_add(*archive.pack_sizes().get(pack).ok_or_else(invalid)?)
        .ok_or_else(invalid)?;
    let start = usize::try_from(offset).map_err(archive_error)?;
    let end = usize::try_from(end).map_err(archive_error)?;
    let data = bytes.get(start..end).ok_or_else(invalid)?;
    const CHUNK: usize = 64 * 1024;
    let size_usize = usize::try_from(size).map_err(archive_error)?;
    let mut output = Vec::with_capacity(size_usize.min(CHUNK));
    let mut decoder = SevenZDecoder::new(params, Some(size)).map_err(archive_error)?;
    let mut input = data;
    let mut chunk = vec![0; size_usize.min(CHUNK)];
    loop {
        let remaining = size_usize - output.len();
        let limit = remaining.min(chunk.len());
        let target = chunk.get_mut(..limit).ok_or_else(invalid)?;
        let step = decoder.decode(input, true, target).map_err(archive_error)?;
        output.extend_from_slice(target.get(..step.produced).ok_or_else(invalid)?);
        if output.len() == size_usize
            && matches!(
                step.status,
                SevenZStatus::ReachedSize | SevenZStatus::OutputFull
            )
        {
            break;
        }
        if step.produced == 0 || step.status != SevenZStatus::OutputFull {
            return Err(invalid());
        }
        input = input.get(step.consumed..).ok_or_else(invalid)?;
    }
    if block.has_crc && u64::from(crc32fast::hash(&output)) != block.crc {
        return Err(ArchiveError::new("7-Zip PPMd block checksum mismatch"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use sevenz_rust2::Password;

    use super::*;

    #[test]
    fn decodes_a_block_across_chunks_and_rejects_a_false_size()
    -> Result<(), Box<dyn std::error::Error>> {
        let data: Vec<u8> = (0..200_000_u32).map(|value| (value % 251) as u8).collect();
        let mut writer = Writer::new(Some(1))?;
        writer.add(ArchiveEntry::new_file("payload"), &data)?;
        let props = writer.params.to_7z_props();
        let bytes = writer.finish();
        let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty())?;
        assert_eq!(decode(&bytes, &archive, 0, &props)?, data);

        let mut writer = Writer::new(Some(1))?;
        writer.add(ArchiveEntry::new_file("payload"), b"abc")?;
        let [entry] = writer.files.as_mut_slice() else {
            return Err("Expected one file".into());
        };
        entry.size = size_limit();
        let bytes = writer.finish();
        let archive = Archive::read(&mut Cursor::new(&bytes), &Password::empty())?;
        assert!(bytes.len() < 512);
        assert!(decode(&bytes, &archive, 0, &props).is_err());
        Ok(())
    }
}
