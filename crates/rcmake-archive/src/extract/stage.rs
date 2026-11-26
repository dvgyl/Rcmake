use std::io::{self, Read, SeekFrom, Write};
use std::path::Path;

use rcmake_host::{FileKind, Follow, Host, ScratchFile};

use crate::{ArchiveError, archive_error};

pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub(crate) const METADATA_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const BUFFER: usize = 64 * 1024;
// Keep small archives in memory until validation completes.
const MEMORY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Interval {
    offset: u64,
    len: u64,
}

pub(super) struct Stage<'a> {
    host: &'a dyn Host,
    destination: &'a Path,
    storage: Storage<'a>,
    size: u64,
}

enum Storage<'a> {
    Memory(Vec<u8>),
    File(Box<dyn ScratchFile + 'a>),
}

impl<'a> Stage<'a> {
    pub(super) fn new(host: &'a dyn Host, destination: &'a Path) -> Self {
        Self {
            host,
            destination,
            storage: Storage::Memory(Vec::new()),
            size: 0,
        }
    }

    pub(super) fn payload(
        &mut self,
        len: u64,
        copy: impl FnOnce(&mut dyn Write) -> Result<(), ArchiveError>,
    ) -> Result<Interval, ArchiveError> {
        let start = self.size;
        let end = start
            .checked_add(len)
            .filter(|&end| end <= MAX_BYTES)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
        match &mut self.storage {
            Storage::Memory(bytes) if end <= MEMORY_BYTES as u64 => {
                let end = end as usize;
                if end > bytes.capacity() {
                    let capacity = end.max(bytes.capacity() * 2).min(MEMORY_BYTES);
                    bytes
                        .try_reserve_exact(capacity - bytes.len())
                        .map_err(archive_error)?;
                }
                if start == 0 && len != 0 {
                    scratch_directory(self.host, self.destination)?;
                }
                bytes.resize(end, 0);
                let mut output = bytes
                    .get_mut(start as usize..end)
                    .ok_or_else(|| ArchiveError::new("Invalid staged archive entry"))?;
                copy(&mut output)?;
            }
            Storage::Memory(bytes) => {
                let mut file = self
                    .host
                    .scratch_file_in(scratch_directory(self.host, self.destination)?)?;
                file.write_all(bytes).map_err(archive_error)?;
                copy(&mut file)?;
                self.storage = Storage::File(file);
            }
            Storage::File(file) => copy(file)?,
        }
        self.size = end;
        Ok(Interval { offset: start, len })
    }

    pub(super) fn publish(&mut self, interval: Interval, dest: &Path) -> Result<(), ArchiveError> {
        let mut contents;
        let input: &mut dyn Read = match &mut self.storage {
            Storage::Memory(bytes) => {
                let start = interval.offset as usize;
                let end = start + interval.len as usize;
                contents = bytes
                    .get(start..end)
                    .ok_or_else(|| ArchiveError::new("Invalid staged archive entry"))?;
                &mut contents
            }
            Storage::File(file) => {
                file.seek(SeekFrom::Start(interval.offset))
                    .map_err(archive_error)?;
                file
            }
        };
        self.host
            .write_from(dest, rcmake_host::WriteMode::CreateNew, input, interval.len)?;
        Ok(())
    }
}

fn scratch_directory<'a>(host: &dyn Host, destination: &'a Path) -> Result<&'a Path, ArchiveError> {
    for directory in destination.ancestors() {
        match host.metadata(directory, Follow::Yes) {
            Ok(metadata) if metadata.kind == FileKind::Dir => return Ok(directory),
            Err(error) if error.kind == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
            Ok(_) => {
                return Err(ArchiveError::new(format!(
                    "The extraction path must point to a directory: {}",
                    directory.display()
                )));
            }
        }
    }
    Err(ArchiveError::new("Cannot find the extraction directory"))
}

pub(crate) fn copy(
    mut reader: impl Read,
    output: &mut dyn Write,
    size: u64,
) -> Result<(), ArchiveError> {
    let mut buffer = vec![0; size.saturating_add(1).min(BUFFER as u64) as usize];
    let mut left = size;
    loop {
        let n = left.saturating_add(1).min(buffer.len() as u64) as usize;
        let chunk = buffer
            .get_mut(..n)
            .ok_or_else(|| ArchiveError::new("Invalid archive copy length"))?;
        let read = match reader.read(chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(archive_error(error)),
        };
        if read == 0 && left == 0 {
            return Ok(());
        }
        if read == 0 || read as u64 > left {
            return Err(ArchiveError::new(
                "Archive entry size does not match its header",
            ));
        }
        let data = chunk
            .get(..read)
            .ok_or_else(|| ArchiveError::new("Invalid archive read length"))?;
        output.write_all(data).map_err(archive_error)?;
        left -= read as u64;
    }
}

#[cfg(test)]
mod tests {
    use rcmake_host::{PureHost, ReadRange};

    use super::*;

    #[test]
    fn stages_in_the_nearest_existing_destination_directory() -> Result<(), ArchiveError> {
        let host = PureHost::default();
        let parent = Path::new("/work/extract");
        let destination = parent.join("new/tree");
        host.create_dir_all(parent)?;
        assert_eq!(scratch_directory(&host, &destination)?, parent);
        host.create_dir_all(&destination)?;
        assert_eq!(scratch_directory(&host, &destination)?, destination);
        Ok(())
    }

    #[test]
    fn preserves_payloads_when_staging_moves_to_disk() -> Result<(), ArchiveError> {
        let host = PureHost::default();
        let destination = Path::new("/work");
        host.create_dir_all(destination)?;
        let mut stage = Stage::new(&host, destination);
        let first = stage.payload(3, |output| output.write_all(b"one").map_err(archive_error))?;
        let second = stage.payload(MEMORY_BYTES as u64, |output| {
            copy(
                io::repeat(b'x').take(MEMORY_BYTES as u64),
                output,
                MEMORY_BYTES as u64,
            )
        })?;
        assert!(matches!(stage.storage, Storage::File(_)));
        let second_path = destination.join("second");
        stage.publish(second, &second_path)?;
        let first_path = destination.join("first");
        stage.publish(first, &first_path)?;
        assert_eq!(host.read(&first_path, ReadRange::All { limit: 3 })?, b"one");
        assert_eq!(
            host.metadata(&second_path, Follow::No)?.len,
            MEMORY_BYTES as u64
        );
        assert_eq!(
            host.read(
                &second_path,
                ReadRange::Slice {
                    offset: MEMORY_BYTES as u64 - 1,
                    len: Some(1),
                }
            )?,
            b"x"
        );
        Ok(())
    }
}
