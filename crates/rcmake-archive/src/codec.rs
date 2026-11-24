use std::io::{self, Read, Write};
use std::path::Path;

use rcmake_host::{Host, ReadRange};

use bzip2::{Compression as BzLevel, read::MultiBzDecoder, write::BzEncoder};
use flate2::{Compression as GzLevel, read::MultiGzDecoder, write::GzEncoder};
use liblzma::{
    read::XzDecoder,
    stream::{CONCATENATED, Check, LzmaOptions, MtStreamBuilder, Stream},
    write::XzEncoder,
};

use crate::{ArchiveError, TarCompression, archive_error, compress};

const DECODER_MEMORY: u64 = 128 * 1024 * 1024;

pub(super) enum Encoder {
    Plain(Vec<u8>),
    Gzip(GzEncoder<Vec<u8>>),
    Bzip2(BzEncoder<Vec<u8>>),
    Lzma(Box<XzEncoder<Vec<u8>>>),
    Zstd(zstd::Encoder<'static, Vec<u8>>),
    Compress(compress::Encoder),
}

impl Encoder {
    pub(super) fn new(
        filter: Option<TarCompression>,
        level: Option<u32>,
        threads: u32,
    ) -> Result<Self, ArchiveError> {
        let level = level.unwrap_or(match filter {
            Some(TarCompression::Bzip2) => 9,
            Some(TarCompression::Zstd) => 3,
            _ => 6,
        });
        Ok(match filter {
            None => Self::Plain(Vec::new()),
            Some(TarCompression::Gzip) => {
                Self::Gzip(GzEncoder::new(Vec::new(), GzLevel::new(level)))
            }
            Some(TarCompression::Bzip2) => Self::Bzip2(BzEncoder::new(
                Vec::new(),
                if level == 0 {
                    BzLevel::best()
                } else {
                    BzLevel::new(level)
                },
            )),
            Some(TarCompression::Xz | TarCompression::Lzma) => {
                let stream = if filter == Some(TarCompression::Xz) {
                    if threads > 1 {
                        MtStreamBuilder::new()
                            .threads(threads.min(2))
                            .preset(level)
                            .check(Check::Crc64)
                            .encoder()
                    } else {
                        Stream::new_easy_encoder(level, Check::Crc64)
                    }
                } else {
                    Stream::new_lzma_encoder(
                        &LzmaOptions::new_preset(level).map_err(archive_error)?,
                    )
                }
                .map_err(archive_error)?;
                Self::Lzma(Box::new(XzEncoder::new_stream(Vec::new(), stream)))
            }
            Some(TarCompression::Zstd) => {
                let mut encoder =
                    zstd::Encoder::new(Vec::new(), level as i32).map_err(archive_error)?;
                if threads > 1 {
                    encoder.multithread(threads.min(2)).map_err(archive_error)?;
                }
                encoder.include_checksum(true).map_err(archive_error)?;
                Self::Zstd(encoder)
            }
            Some(TarCompression::Compress) => Self::Compress(compress::Encoder::new()),
        })
    }

    pub(super) fn finish(self) -> io::Result<Vec<u8>> {
        match self {
            Self::Plain(bytes) => Ok(bytes),
            Self::Gzip(encoder) => encoder.finish(),
            Self::Bzip2(encoder) => encoder.finish(),
            Self::Lzma(encoder) => encoder.finish(),
            Self::Zstd(encoder) => encoder.finish(),
            Self::Compress(encoder) => Ok(encoder.finish()),
        }
    }

    fn writer(&mut self) -> &mut dyn Write {
        match self {
            Self::Plain(bytes) => bytes,
            Self::Gzip(encoder) => encoder,
            Self::Bzip2(encoder) => encoder,
            Self::Lzma(encoder) => encoder.as_mut(),
            Self::Zstd(encoder) => encoder,
            Self::Compress(encoder) => encoder,
        }
    }
}

impl Write for Encoder {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer().flush()
    }
}

pub(super) fn detect(bytes: &[u8]) -> Option<TarCompression> {
    if bytes.starts_with(b"\x1f\x8b") {
        Some(TarCompression::Gzip)
    } else if matches!(bytes, [b'B', b'Z', b'h', b'1'..=b'9', ..]) {
        Some(TarCompression::Bzip2)
    } else if bytes.starts_with(b"\xfd7zXZ\0") {
        Some(TarCompression::Xz)
    } else if bytes.starts_with(b"\x28\xb5\x2f\xfd")
        || matches!(bytes, [0x50..=0x5f, 0x2a, 0x4d, 0x18, ..])
    {
        Some(TarCompression::Zstd)
    } else if bytes.starts_with(b"\x1f\x9d") {
        Some(TarCompression::Compress)
    } else if lzma_header(bytes) {
        Some(TarCompression::Lzma)
    } else {
        None
    }
}

fn lzma_header(bytes: &[u8]) -> bool {
    // LZMA-alone has no magic number. Check its properties, dictionary size,
    // and first range-coder byte. Encoders use 2^n or 3 * 2^n dictionary sizes.
    let [properties, a, b, c, d, _, _, _, _, _, _, _, _, 0, ..] = bytes else {
        return false;
    };
    let size = u32::from_le_bytes([*a, *b, *c, *d]);
    *properties < 225
        && size >= 4096
        && (size.is_power_of_two() || (size.is_multiple_of(3) && (size / 3).is_power_of_two()))
}

pub(super) fn decoder(
    filter: TarCompression,
    bytes: &[u8],
) -> Result<Box<dyn Read + '_>, ArchiveError> {
    Ok(match filter {
        TarCompression::Gzip => Box::new(MultiGzDecoder::new(bytes)),
        TarCompression::Bzip2 => Box::new(MultiBzDecoder::new(bytes)),
        TarCompression::Xz | TarCompression::Lzma => {
            let stream = if filter == TarCompression::Xz {
                Stream::new_stream_decoder(DECODER_MEMORY, CONCATENATED)
            } else {
                Stream::new_lzma_decoder(DECODER_MEMORY)
            }
            .map_err(archive_error)?;
            Box::new(XzDecoder::new_stream(bytes, stream))
        }
        TarCompression::Zstd => {
            let mut decoder = zstd::Decoder::with_buffer(bytes).map_err(archive_error)?;
            decoder.window_log_max(27).map_err(archive_error)?;
            Box::new(decoder)
        }
        TarCompression::Compress => Box::new(compress::Decoder::new(bytes).map_err(archive_error)?),
    })
}

/// Read regular archive files in bounded chunks through the host.
pub(super) struct FileReader<'a> {
    host: &'a dyn Host,
    path: &'a Path,
    offset: u64,
    chunk: &'a [u8],
    buffer: Vec<u8>,
    cursor: usize,
    eof: bool,
}

impl<'a> FileReader<'a> {
    pub(super) fn new(host: &'a dyn Host, path: &'a Path, prefix: &'a [u8]) -> Self {
        Self {
            host,
            path,
            offset: prefix.len() as u64,
            chunk: prefix,
            buffer: Vec::new(),
            cursor: 0,
            eof: false,
        }
    }
}

impl Read for FileReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        const CHUNK: u64 = 256 * 1024;
        if !self.chunk.is_empty() {
            return self.chunk.read(out);
        }
        if out.is_empty() {
            return Ok(0);
        }
        if self.cursor == self.buffer.len() && !self.eof {
            self.buffer = self
                .host
                .read(
                    self.path,
                    ReadRange::Slice {
                        offset: self.offset,
                        len: Some(CHUNK),
                    },
                )
                .map_err(|error| io::Error::new(error.kind, ArchiveError::from(error)))?;
            self.offset += self.buffer.len() as u64;
            self.eof = (self.buffer.len() as u64) < CHUNK;
            self.cursor = 0;
        }
        let read = self
            .buffer
            .get(self.cursor..)
            .unwrap_or_default()
            .read(out)?;
        self.cursor += read;
        Ok(read)
    }
}
