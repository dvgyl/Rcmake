use std::io::{self, Read, SeekFrom, Write};
use std::path::Path;

use rcmake_host::{FileKind, Follow, Host, ScratchFile};

use crate::{ArchiveError, archive_error};

pub(crate) const MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub(crate) const METADATA_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const BUFFER: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Interval {
    offset: u64,
    len: u64,
}

pub(super) struct Stage<'a> {
    host: &'a dyn Host,
    destination: &'a Path,
    file: Option<Box<dyn ScratchFile + 'a>>,
    size: u64,
}

impl<'a> Stage<'a> {
    pub(super) fn new(host: &'a dyn Host, destination: &'a Path) -> Self {
        Self {
            host,
            destination,
            file: None,
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
        if let Some(file) = &mut self.file {
            copy(file)?;
        } else if len == 0 {
            copy(&mut io::sink())?;
        } else {
            let mut file = self
                .host
                .scratch_file_in(scratch_directory(self.host, self.destination)?)?;
            copy(&mut file)?;
            self.file = Some(file);
        }
        self.size = end;
        Ok(Interval { offset: start, len })
    }

    pub(super) fn publish(&mut self, interval: Interval, dest: &Path) -> Result<(), ArchiveError> {
        let mut empty = io::empty();
        let input: &mut dyn Read = match &mut self.file {
            Some(file) => {
                file.seek(SeekFrom::Start(interval.offset))
                    .map_err(archive_error)?;
                file
            }
            None => &mut empty,
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
    let mut buffer = vec![0; BUFFER];
    let mut left = size;
    loop {
        let n = left.saturating_add(1).min(BUFFER as u64) as usize;
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
    use rcmake_host::PureHost;

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
}
