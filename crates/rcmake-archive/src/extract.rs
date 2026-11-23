use std::collections::BTreeMap;
use std::io::{Cursor, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use flate2::read::MultiGzDecoder;
use rcmake_base::BString;
use rcmake_host::{FileKind, FileTime, Follow, Host, Metadata, WriteMode, path::to_native};
use tar::Archive;
use zip::ZipArchive;

use crate::pattern::Patterns;
use crate::timestamp;
use crate::{
    ArchiveError, ExtractOptions, MAX_ENTRIES, absolute, archive_error, emit_names, read_bounded,
    read_bytes, relative_path, rooted, size_limit,
};

pub(super) fn run(
    host: &dyn Host,
    archive_path: &Path,
    options: &ExtractOptions,
) -> Result<Vec<BString>, ArchiveError> {
    absolute(archive_path)?;
    if !options.list_only {
        absolute(&options.destination)?;
    }
    if host.metadata(archive_path, Follow::Yes)?.kind != FileKind::File {
        return Err(ArchiveError::new(format!(
            "The archive path must point to a regular file: {}",
            archive_path.display()
        )));
    }
    let bytes = read_bytes(host, archive_path, size_limit())?;
    let mut scan = Scan {
        patterns: Patterns::new(&options.patterns)?,
        list_only: options.list_only,
        touch: options.touch,
        entries: Vec::new(),
        names: Vec::new(),
        count: 0,
        bytes_left: size_limit(),
    };
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        scan.zip(&bytes)?;
    } else if bytes.starts_with(b"\x1f\x8b") {
        let tar = read_bounded(MultiGzDecoder::new(bytes.as_slice()), size_limit())?;
        scan.tar(&tar)?;
    } else {
        scan.tar(&bytes)?;
    }
    scan.patterns.finish()?;
    if !scan.entries.is_empty() {
        unpack(host, &options.destination, scan.entries)?;
    }
    emit_names(host, &scan.names, options.verbose);
    Ok(scan.names)
}

struct Scan<'a> {
    patterns: Patterns<'a>,
    list_only: bool,
    touch: bool,
    entries: Vec<Entry>,
    names: Vec<BString>,
    count: usize,
    bytes_left: u64,
}

impl Scan<'_> {
    fn select(&mut self, name: &[u8], path: &Path) -> Result<bool, ArchiveError> {
        self.count += 1;
        self.bytes_left = self
            .bytes_left
            .checked_sub(name.len() as u64 + 512)
            .filter(|_| self.count <= MAX_ENTRIES)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the entry or size limit"))?;
        if !self.patterns.matches(path, name.ends_with(b"/"))? {
            return Ok(false);
        }
        self.names.push(BString::from(name));
        Ok(!self.list_only)
    }

    fn payload(&mut self, reader: impl Read, size: u64) -> Result<Vec<u8>, ArchiveError> {
        self.bytes_left = self
            .bytes_left
            .checked_sub(size)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
        let data = read_bounded(reader, size)?;
        if data.len() as u64 != size {
            return Err(ArchiveError::new(
                "Archive entry size does not match its header",
            ));
        }
        Ok(data)
    }

    fn tar(&mut self, bytes: &[u8]) -> Result<(), ArchiveError> {
        if bytes.len() < 512 {
            return Err(ArchiveError::new("Invalid tar archive"));
        }
        let mut archive = Archive::new(bytes);
        for item in archive.entries().map_err(archive_error)? {
            let mut item = item.map_err(archive_error)?;
            let kind = item.header().entry_type();
            if kind.is_pax_global_extensions() {
                continue;
            }
            let name = item.path_bytes();
            let path = relative_path(&name)?;
            if !self.select(&name, &path)? {
                continue;
            }
            let data = if kind.is_dir() {
                Data::Directory
            } else if kind.is_symlink() {
                let link = item
                    .link_name_bytes()
                    .ok_or_else(|| ArchiveError::new("The tar symbolic link has no target"))?;
                Data::Symlink(link_target(&path, &link)?)
            } else if kind.is_file() || kind.is_gnu_sparse() {
                let size = item.size();
                Data::File(self.payload(&mut item, size)?)
            } else {
                return Err(ArchiveError::new(format!(
                    "Unsupported tar entry: {}",
                    path.display()
                )));
            };
            let mode = Some(item.header().mode().map_err(archive_error)?);
            let mtime = if self.touch || matches!(data, Data::Symlink(_)) {
                None
            } else {
                Some(timestamp::tar_time(&mut item)?)
            };
            self.entries.push(Entry {
                path,
                data,
                mode,
                mtime,
            });
        }
        Ok(())
    }

    fn zip(&mut self, bytes: &[u8]) -> Result<(), ArchiveError> {
        let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(archive_error)?;
        for index in 0..archive.len() {
            let item = archive.by_index_raw(index).map_err(archive_error)?;
            let name: Vec<_> = item
                .name_raw()
                .iter()
                .map(|&byte| if byte == b'\\' { b'/' } else { byte })
                .collect();
            drop(item);
            let path = relative_path(&name)?;
            if !self.select(&name, &path)? {
                continue;
            }
            let mut item = archive.by_index(index).map_err(archive_error)?;
            let mode = item.unix_mode();
            let mtime = if self.touch {
                None
            } else {
                timestamp::zip_time(&item)
            };
            let data = if item.is_symlink() {
                let size = item.size();
                if size > crate::MAX_NAME_BYTES as u64 {
                    return Err(ArchiveError::new(
                        "The ZIP symbolic link target is too long",
                    ));
                }
                Data::Symlink(link_target(&path, &self.payload(&mut item, size)?)?)
            } else if item.is_dir() || mode.is_some_and(|mode| mode & 0o170000 == 0o40000) {
                Data::Directory
            } else {
                let size = item.size();
                Data::File(self.payload(&mut item, size)?)
            };
            self.entries.push(Entry {
                path,
                data,
                mode,
                mtime,
            });
        }
        Ok(())
    }
}

struct Entry {
    path: PathBuf,
    data: Data,
    mode: Option<u32>,
    mtime: Option<FileTime>,
}

enum Data {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn link_target(link: &Path, target: &[u8]) -> Result<PathBuf, ArchiveError> {
    let path = to_native(target);
    let invalid = || {
        ArchiveError::new(format!(
            "Invalid symbolic link target for {}",
            link.display()
        ))
    };
    if target.is_empty() || target.contains(&0) || rooted(target) {
        return Err(invalid());
    }
    let mut depth = link.components().count().saturating_sub(1);
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if cfg!(windows) && part.as_encoded_bytes().contains(&b':') {
                    return Err(invalid());
                }
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => depth = depth.checked_sub(1).ok_or_else(invalid)?,
            _ => return Err(invalid()),
        }
    }
    Ok(path)
}

fn unpack(host: &dyn Host, destination: &Path, entries: Vec<Entry>) -> Result<(), ArchiveError> {
    host.create_dir_all(destination)?;
    let root = host.canonicalize(destination)?;
    let mut directories = BTreeMap::new();
    for entry in entries {
        let relative_parent = entry.path.parent().unwrap_or(Path::new(""));
        let mut parent = root.clone();
        for part in relative_parent {
            parent.push(part);
            ensure_directory(host, &parent)?;
        }
        let path = root.join(entry.path);
        match entry.data {
            Data::Directory => {
                ensure_directory(host, &path)?;
                directories.insert(path, (entry.mode, entry.mtime));
            }
            Data::File(contents) => {
                let mode = match existing(host, &path)? {
                    None => WriteMode::CreateNew,
                    Some(metadata) if metadata.kind == FileKind::File && metadata.nlink == 1 => {
                        WriteMode::Truncate
                    }
                    Some(_) => return Err(conflict(&path)),
                };
                host.write(&path, &contents, mode)?;
                restore(host, &path, entry.mode, entry.mtime)?;
            }
            Data::Symlink(target) => {
                if existing(host, &path)?.is_some() {
                    return Err(conflict(&path));
                }
                host.symlink(&target, &path)?;
            }
        }
    }
    // Restore children first so a directory mode cannot prevent child writes.
    for (path, (mode, mtime)) in directories.into_iter().rev() {
        restore(host, &path, mode, mtime)?;
    }
    Ok(())
}

fn existing(host: &dyn Host, path: &Path) -> Result<Option<Metadata>, ArchiveError> {
    match host.metadata(path, Follow::No) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn ensure_directory(host: &dyn Host, path: &Path) -> Result<(), ArchiveError> {
    match existing(host, path)? {
        None => Ok(host.create_dir_all(path)?),
        Some(metadata) if metadata.kind == FileKind::Dir => Ok(()),
        Some(_) => Err(conflict(path)),
    }
}

fn conflict(path: &Path) -> ArchiveError {
    ArchiveError::new(format!(
        "Cannot extract over this filesystem entry: {}",
        path.display()
    ))
}

fn restore(
    host: &dyn Host,
    path: &Path,
    mode: Option<u32>,
    mtime: Option<FileTime>,
) -> Result<(), ArchiveError> {
    if let Some(mode) = mode {
        host.set_permissions(path, mode & 0o7777)?;
    }
    if let Some(mtime) = mtime {
        host.set_times(path, None, Some(mtime))?;
    }
    Ok(())
}
