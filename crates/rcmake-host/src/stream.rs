//! Bounded stream adapters for host files.

use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{FileKind, Follow, Host, HostError, HostResult, ReadRange, WriteMode};

const BUFFER: usize = 64 * 1024;
const MEMORY_LIMIT: u64 = 512 * 1024 * 1024;

/// A readable file that supports random access.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// A private temporary file that supports reading, writing, and random access.
pub trait ScratchFile: Read + Write + Seek {}
impl<T: Read + Write + Seek> ScratchFile for T {}

pub(crate) fn open_read<'a, H: Host + ?Sized>(
    host: &'a H,
    path: &Path,
) -> HostResult<Box<dyn ReadSeek + 'a>> {
    let metadata = host.metadata(path, Follow::Yes)?;
    if metadata.kind != FileKind::File {
        return Err(HostError::unsupported(
            "Streaming reads require a regular file.",
        ));
    }
    Ok(Box::new(Reader {
        host,
        path: path.to_owned(),
        len: metadata.len,
        offset: 0,
    }))
}

struct Reader<'a, H: Host + ?Sized> {
    host: &'a H,
    path: PathBuf,
    len: u64,
    offset: u64,
}

impl<H: Host + ?Sized> Read for Reader<'_, H> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let len = self
            .len
            .saturating_sub(self.offset)
            .min(buffer.len().min(BUFFER) as u64);
        if len == 0 {
            return Ok(0);
        }
        let data = self
            .host
            .read(
                &self.path,
                ReadRange::Slice {
                    offset: self.offset,
                    len: Some(len),
                },
            )
            .map_err(|error| io::Error::new(error.kind, error.to_string()))?;
        if data.len() as u64 > len {
            return Err(io::Error::other(
                "The host returned more bytes than requested.",
            ));
        }
        buffer
            .get_mut(..data.len())
            .ok_or_else(|| io::Error::other("Invalid host read length."))?
            .copy_from_slice(&data);
        self.offset += data.len() as u64;
        Ok(data.len())
    }
}

impl<H: Host + ?Sized> Seek for Reader<'_, H> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.offset = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.offset.checked_add_signed(delta),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid file offset."))?;
        Ok(self.offset)
    }
}

pub(crate) fn scratch() -> Box<dyn ScratchFile> {
    Box::new(MemoryScratch(Cursor::new(Vec::new())))
}

struct MemoryScratch(Cursor<Vec<u8>>);

impl Read for MemoryScratch {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.read(bytes)
    }
}

impl Write for MemoryScratch {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let size = self.0.position().saturating_add(bytes.len() as u64);
        if size > MEMORY_LIMIT {
            return Err(io::Error::other(
                "The scratch file exceeds the memory limit.",
            ));
        }
        let data = self.0.get_mut();
        if size as usize > data.capacity() {
            let capacity = (size as usize)
                .max(data.capacity().saturating_mul(2))
                .min(MEMORY_LIMIT as usize);
            data.try_reserve_exact(capacity - data.len())
                .map_err(io::Error::other)?;
        }
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for MemoryScratch {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0.seek(position)
    }
}

pub(crate) fn write_from<H: Host + ?Sized>(
    host: &H,
    path: &Path,
    mode: WriteMode,
    input: &mut dyn Read,
    len: u64,
) -> HostResult<()> {
    if len > MEMORY_LIMIT {
        return Err(crate::error::SyntheticError::FileTooLarge.at(Some(path)));
    }
    let mut data = Vec::new();
    copy_exact(input, &mut data, len)
        .map_err(|error| HostError::from_io(error, Some(path.to_owned())))?;
    host.write(path, &data, mode)
}

pub(crate) fn copy_exact(
    input: &mut dyn Read,
    output: &mut dyn Write,
    mut len: u64,
) -> io::Result<()> {
    // Do not allocate more than the copy needs.
    let mut buffer = vec![0; len.min(BUFFER as u64) as usize];
    while len != 0 {
        let count = len.min(buffer.len() as u64) as usize;
        let chunk = buffer
            .get_mut(..count)
            .ok_or_else(|| io::Error::other("Invalid stream copy length."))?;
        input.read_exact(chunk)?;
        output.write_all(chunk)?;
        len -= count as u64;
    }
    Ok(())
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use crate::NativeHost;

    #[test]
    fn exclusive_stream_write_cleans_up_only_new_files() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let host = NativeHost::new()?;
        let existing = directory.path().join("existing");
        host.write(&existing, b"original", WriteMode::CreateNew)?;
        let conflict = host.write_from(&existing, WriteMode::CreateNew, &mut &b"other"[..], 5);
        assert!(conflict.is_err_and(|error| error.kind == io::ErrorKind::AlreadyExists));
        assert_eq!(
            host.read(&existing, ReadRange::All { limit: 16 })?,
            b"original"
        );

        let missing = directory.path().join("missing");
        let short = host.write_from(&missing, WriteMode::CreateNew, &mut &b"part"[..], 9);
        assert!(short.is_err_and(|error| error.kind == io::ErrorKind::UnexpectedEof));
        assert!(host.metadata(&missing, Follow::No).is_err());
        Ok(())
    }
}
