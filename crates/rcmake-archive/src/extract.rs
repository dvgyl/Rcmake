use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{
    FileKind, FileTime, Follow, Host, Metadata, ReadRange, WriteMode, path::to_native,
};
use tar::{Archive, EntryType};

use crate::pattern::Patterns;
use crate::{
    ArchiveError, ExtractOptions, MAX_ENTRIES, absolute, archive_error, checked_name, emit_names,
    read_bounded, read_bytes, relative_path, size_limit,
};
use crate::{codec, external_filter, seven_zip, timestamp, zip_codec};

#[derive(Clone, Copy)]
pub(super) enum Listing<'a> {
    Shared,
    Names,
    Details(&'a dyn Fn(FileTime) -> String),
    SharedDetails(&'a dyn Fn(FileTime) -> String),
}

impl<'a> Listing<'a> {
    fn time_formatter(self) -> Option<&'a dyn Fn(FileTime) -> String> {
        match self {
            Self::Details(format) | Self::SharedDetails(format) => Some(format),
            Self::Shared | Self::Names => None,
        }
    }
}

pub(super) fn run(
    host: &dyn Host,
    archive_path: &Path,
    options: &ExtractOptions,
    listing: Listing<'_>,
) -> Result<Vec<BString>, ArchiveError> {
    absolute(archive_path)?;
    if !options.list_only {
        absolute(&options.destination)?;
    }
    let metadata = host.metadata(archive_path, Follow::Yes)?;
    if metadata.kind != FileKind::File {
        return Err(ArchiveError::new(format!(
            "The archive path must point to a regular file: {}",
            archive_path.display()
        )));
    }
    if metadata.len > size_limit() {
        return Err(ArchiveError::new(format!(
            "{}: File too large",
            archive_path.display()
        )));
    }
    let prefix = host.read(
        archive_path,
        ReadRange::Slice {
            offset: 0,
            len: Some(16),
        },
    )?;
    if options.excludes.iter().any(|pattern| pattern.is_empty()) {
        return Err(ArchiveError::new("Failed to add to exclusion list: "));
    }
    let mut scan = Scan {
        excludes: Patterns::new(&options.excludes)?,
        patterns: Patterns::new(&options.patterns)?,
        list_only: options.list_only,
        listing,
        touch: options.touch,
        names: Vec::new(),
        count: 0,
        bytes_left: size_limit(),
    };
    if prefix.starts_with(b"PK\x03\x04")
        || prefix.starts_with(b"PK\x05\x06")
        || prefix.starts_with(seven_zip::MAGIC)
    {
        let bytes = read_bytes(host, archive_path, size_limit())?;
        let entries = if prefix.starts_with(seven_zip::MAGIC) {
            scan.seven_zip(&bytes)?
        } else {
            scan.zip(&bytes)?
        };
        scan.patterns.finish()?;
        if !entries.is_empty() {
            let mut unpack = Unpack::new(host, &options.destination, scan.bytes_left)?;
            for (entry, payload) in entries {
                unpack.entry(entry, &payload)?;
            }
            unpack.finish()?;
        }
    } else if let Some(program) = external_filter::detect(&prefix) {
        let bytes = external_filter::decompress(host, archive_path, program)?;
        let entries = scan.tar_headers(bytes.as_slice())?;
        scan.patterns.finish()?;
        if !entries.is_empty() {
            Unpack::new(host, &options.destination, scan.bytes_left)?
                .tar(bytes.as_slice(), entries)?;
        }
    } else if let Some(filter) = codec::detect(&prefix) {
        let bytes = read_bytes(host, archive_path, size_limit())?;
        let entries = scan.tar_headers(codec::decoder(filter, &bytes)?)?;
        scan.patterns.finish()?;
        if !entries.is_empty() {
            let unpack = Unpack::new(host, &options.destination, scan.bytes_left)?;
            unpack.tar(codec::decoder(filter, &bytes)?, entries)?;
        }
    } else {
        let source = host.canonicalize(archive_path)?;
        let reader = || codec::FileReader::new(host, &source, &prefix);
        let entries = scan.tar_headers(reader())?;
        scan.patterns.finish()?;
        if !entries.is_empty() {
            let unpack = Unpack::new(host, &options.destination, scan.bytes_left)?;
            if unpack.replaces_source(&source, &metadata, &entries) {
                let bytes = read_bytes(host, &source, size_limit())?;
                unpack.tar(bytes.as_slice(), entries)?;
            } else {
                unpack.tar(reader(), entries)?;
            }
        }
    }
    emit_names(host, &scan.names, options.verbose);
    Ok(scan.names)
}

struct Scan<'a> {
    patterns: Patterns<'a>,
    excludes: Patterns<'a>,
    list_only: bool,
    listing: Listing<'a>,
    touch: bool,
    names: Vec<BString>,
    count: usize,
    bytes_left: u64,
}

impl Scan<'_> {
    fn count_entry(&mut self, name_size: usize) -> Result<(), ArchiveError> {
        self.count += 1;
        self.bytes_left = self
            .bytes_left
            .checked_sub(name_size as u64 + 512)
            .filter(|_| self.count <= MAX_ENTRIES)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the entry or size limit"))?;
        Ok(())
    }

    fn select(&mut self, name: &[u8], path: &Path) -> Result<bool, ArchiveError> {
        let directory = name.ends_with(b"/");
        let included = self.patterns.matches(path, directory)?;
        Ok(included && !self.excludes.excludes(path, directory)?)
    }

    fn path(&self, name: &[u8]) -> Result<PathBuf, ArchiveError> {
        match self.listing {
            Listing::Shared | Listing::SharedDetails(_) => relative_path(name),
            _ => checked_name(name),
        }
    }

    fn reserve_payload(&mut self, size: u64) -> Result<(), ArchiveError> {
        self.bytes_left = self
            .bytes_left
            .checked_sub(size)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
        Ok(())
    }

    fn tar_headers(&mut self, reader: impl Read) -> Result<Vec<(usize, Entry)>, ArchiveError> {
        let mut archive = Archive::new(reader.take(size_limit() + 1));
        let mut entries = Vec::new();
        for (index, item) in archive.entries().map_err(archive_error)?.enumerate() {
            let mut item = item.map_err(archive_error)?;
            let kind = item.header().entry_type();
            self.reserve_payload(item.size())?;
            let name = item.path_bytes();
            self.count_entry(name.len())?;
            if kind.is_pax_global_extensions() {
                continue;
            }
            let path = self.path(&name)?;
            if !self.select(&name, &path)? {
                continue;
            }
            let entry_name = if let Some(format_time) = self.listing.time_formatter() {
                let name = name.into_owned();
                let line = tar_listing(&mut item, &name, format_time)?;
                self.reserve_payload(line.len().saturating_sub(name.len() + 512) as u64)?;
                line
            } else {
                BString::from(name.into_owned())
            };
            self.names.push(entry_name);
            if self.list_only {
                continue;
            }
            let data = match kind {
                EntryType::Directory => Data::Directory,
                EntryType::Symlink => {
                    let target = item
                        .link_name_bytes()
                        .ok_or_else(|| ArchiveError::new("The tar symbolic link has no target"))?;
                    Data::Symlink(symlink_target(&path, &target)?)
                }
                EntryType::Link => {
                    let target = item
                        .link_name_bytes()
                        .ok_or_else(|| ArchiveError::new("The tar hard link has no target"))?;
                    let target = relative_path(&target)?;
                    if target.as_os_str().is_empty() || target == path {
                        return Err(ArchiveError::new(format!(
                            "Invalid hard link target for {}",
                            path.display()
                        )));
                    }
                    Data::HardLink(target)
                }
                EntryType::Regular | EntryType::GNUSparse => Data::File(item.size()),
                EntryType::Fifo | EntryType::Char | EntryType::Block => continue,
                _ => {
                    return Err(ArchiveError::new(format!(
                        "Unsupported tar entry: {}",
                        path.display()
                    )));
                }
            };
            let (mode, mtime) = if matches!(data, Data::Symlink(_) | Data::HardLink(_)) {
                (None, None)
            } else {
                let mode = item.header().mode().map_err(archive_error)?;
                let mtime = if self.touch {
                    None
                } else {
                    Some(timestamp::tar_time(&mut item)?)
                };
                (Some(mode), mtime)
            };
            entries.push((
                index,
                Entry {
                    path,
                    data,
                    mode,
                    mtime,
                },
            ));
        }
        // Read past the tar end marker to check the compression trailer and size limit.
        let mut remaining = archive.into_inner();
        io::copy(&mut remaining, &mut io::sink()).map_err(archive_error)?;
        if remaining.limit() == 0 {
            return Err(ArchiveError::new("Archive exceeds the size limit"));
        }
        if size_limit() + 1 - remaining.limit() < 512 {
            return Err(ArchiveError::new("Invalid tar archive"));
        }
        Ok(entries)
    }

    fn seven_zip(&mut self, bytes: &[u8]) -> Result<Vec<(Entry, Vec<u8>)>, ArchiveError> {
        let archive = seven_zip::open(bytes)?;
        let mut entries = Vec::new();
        let mut selected_entries = Vec::new();
        for item in &archive.files {
            self.reserve_payload(item.size)?;
            let mut name = item.name.replace('\\', "/").into_bytes();
            if item.is_directory && !name.ends_with(b"/") {
                name.push(b'/');
            }
            let path = self.path(&name)?;
            self.count_entry(name.len())?;
            let selected = self.select(&name, &path)? && !item.is_anti_item;
            let needs_payload = if selected {
                let mode = seven_zip::mode(item);
                let mtime = seven_zip::time(item);
                let is_link = mode & 0o170000 == 0o120000;
                let needs_payload =
                    !self.list_only || (is_link && self.listing.time_formatter().is_some());
                let data = if item.is_directory {
                    Data::Directory
                } else {
                    Data::File(item.size)
                };
                let display = match self.listing.time_formatter() {
                    Some(format_time) => listing_line(
                        mode,
                        b"0 ",
                        b"0",
                        &(if is_link { 0 } else { item.size }).to_string(),
                        &format_time(mtime.unwrap_or_default()),
                        &name,
                    ),
                    None => BString::from(name),
                };
                let index = self.names.len();
                self.names.push(display);
                if is_link && item.size > crate::MAX_NAME_BYTES as u64 {
                    return Err(ArchiveError::new(
                        "The 7-Zip symbolic link target is too long",
                    ));
                }
                needs_payload.then_some((
                    index,
                    Entry {
                        path,
                        data,
                        mode: Some(mode),
                        mtime: if self.touch { None } else { mtime },
                    },
                    is_link,
                ))
            } else {
                None
            };
            selected_entries.push(needs_payload);
        }
        if !selected_entries.iter().any(Option::is_some) {
            return Ok(Vec::new());
        }
        seven_zip::read_entries(bytes, &archive, |item_index, item, reader| {
            let Some((index, mut entry, is_link)) =
                selected_entries.get_mut(item_index).and_then(Option::take)
            else {
                return Ok(());
            };
            let mut payload = read_payload(reader, item.size)?;
            if is_link {
                if self.listing.time_formatter().is_some()
                    && let Some(name) = self.names.get_mut(index)
                {
                    name.extend_from_slice(b" -> ");
                    name.extend_from_slice(&payload);
                }
                if !self.list_only {
                    entry.data = Data::Symlink(symlink_target(&entry.path, &payload)?);
                }
                payload.clear();
            }
            if !self.list_only {
                entries.push((index, (entry, payload)));
            }
            Ok(())
        })?;
        if selected_entries.iter().any(Option::is_some) {
            return Err(ArchiveError::new("7-Zip archive is missing entry data"));
        }
        entries.sort_unstable_by_key(|(index, _)| *index);
        Ok(entries.into_iter().map(|(_, entry)| entry).collect())
    }

    fn zip(&mut self, bytes: &[u8]) -> Result<Vec<(Entry, Vec<u8>)>, ArchiveError> {
        let mut archive = zip_codec::open(bytes)?;
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let item = archive.by_index_raw(index).map_err(archive_error)?;
            let name: Vec<_> = item
                .name_raw()
                .iter()
                .map(|&byte| if byte == b'\\' { b'/' } else { byte })
                .collect();
            let path = self.path(&name)?;
            self.count_entry(name.len())?;
            self.reserve_payload(item.size())?;
            if !self.select(&name, &path)? {
                continue;
            }
            let entry_name = if let Some(format_time) = self.listing.time_formatter() {
                let is_link = item.is_symlink();
                let mode =
                    item.unix_mode()
                        .unwrap_or(if item.is_dir() { 0o40755 } else { 0o100644 });
                let mtime = timestamp::zip_time(&item).unwrap_or_default();
                let size = if is_link { 0 } else { item.size() };
                drop(item);
                let mut line = listing_line(
                    mode,
                    b"0 ",
                    b"0",
                    &size.to_string(),
                    &format_time(mtime),
                    &name,
                );
                if is_link {
                    let link = archive.by_index_raw(index).map_err(archive_error)?;
                    let size = link.size();
                    drop(link);
                    if size > crate::MAX_NAME_BYTES as u64 {
                        return Err(ArchiveError::new(
                            "The ZIP symbolic link target is too long",
                        ));
                    }
                    line.extend_from_slice(b" -> ");
                    line.extend_from_slice(&zip_codec::payload(&mut archive, index)?);
                }
                line
            } else {
                drop(item);
                BString::from(name)
            };
            self.names.push(entry_name);
            if self.list_only {
                continue;
            }
            let item = archive.by_index_raw(index).map_err(archive_error)?;
            let mode = item.unix_mode();
            let is_link = item.is_symlink();
            let is_dir = item.is_dir();
            let size = item.size();
            let mtime = if self.touch {
                None
            } else {
                timestamp::zip_time(&item)
            };
            drop(item);
            let (data, payload) = if is_link {
                if size > crate::MAX_NAME_BYTES as u64 {
                    return Err(ArchiveError::new(
                        "The ZIP symbolic link target is too long",
                    ));
                }
                let target = symlink_target(&path, &zip_codec::payload(&mut archive, index)?)?;
                (Data::Symlink(target), Vec::new())
            } else if is_dir || mode.is_some_and(|mode| mode & 0o170000 == 0o40000) {
                (Data::Directory, Vec::new())
            } else {
                (Data::File(size), zip_codec::payload(&mut archive, index)?)
            };
            entries.push((
                Entry {
                    path,
                    data,
                    mode,
                    mtime,
                },
                payload,
            ));
        }
        Ok(entries)
    }
}

fn tar_listing<R: Read>(
    item: &mut tar::Entry<'_, R>,
    name: &[u8],
    format_time: &dyn Fn(FileTime) -> String,
) -> Result<BString, ArchiveError> {
    let time = format_time(timestamp::tar_time(item)?);
    let header = item.header();
    let kind = header.entry_type();
    let file_type = match kind {
        EntryType::Directory => 0o40000,
        EntryType::Symlink => 0o120000,
        EntryType::Char => 0o20000,
        EntryType::Block => 0o60000,
        EntryType::Fifo => 0o10000,
        _ => 0o100000,
    };
    let mode = file_type | (header.mode().map_err(archive_error)? & 0o7777);
    let uid = format!("{} ", header.uid().unwrap_or_default());
    let gid = header.gid().unwrap_or_default().to_string();
    let owner = header
        .username_bytes()
        .filter(|name| !name.is_empty())
        .unwrap_or(uid.as_bytes());
    let group = header
        .groupname_bytes()
        .filter(|name| !name.is_empty())
        .unwrap_or(gid.as_bytes());
    let size = if matches!(kind, EntryType::Char | EntryType::Block) {
        let major = header
            .device_major()
            .map_err(archive_error)?
            .unwrap_or_default();
        let minor = header
            .device_minor()
            .map_err(archive_error)?
            .unwrap_or_default();
        format!("{major},{minor}")
    } else {
        item.size().to_string()
    };
    let mut line = listing_line(mode, owner, group, &size, &time, name);
    if let Some(target) = item
        .link_name_bytes()
        .filter(|_| matches!(kind, EntryType::Link | EntryType::Symlink))
    {
        if target.len() > crate::MAX_NAME_BYTES {
            return Err(ArchiveError::new("The tar link target is too long"));
        }
        line.extend_from_slice(if kind == EntryType::Link {
            b" link to "
        } else {
            b" -> "
        });
        line.extend_from_slice(&target);
    }
    Ok(line)
}

fn listing_line(
    mode: u32,
    owner: &[u8],
    group: &[u8],
    size: &str,
    time: &str,
    name: &[u8],
) -> BString {
    let file_type = match mode & 0o170000 {
        0o40000 => b'd',
        0o120000 => b'l',
        0o20000 => b'c',
        0o60000 => b'b',
        0o10000 => b'p',
        0o140000 => b's',
        _ => b'-',
    };
    let mut line = BString::from([file_type].as_slice());
    for (shift, special) in [(6, b's'), (3, b's'), (0, b't')] {
        line.push(if mode & (0o4 << shift) != 0 {
            b'r'
        } else {
            b'-'
        });
        line.push(if mode & (0o2 << shift) != 0 {
            b'w'
        } else {
            b'-'
        });
        let execute = mode & (0o1 << shift) != 0;
        let special_bit = 0o1000 << (shift / 3);
        line.push(match (execute, mode & special_bit != 0) {
            (true, true) => special,
            (false, true) => special.to_ascii_uppercase(),
            (true, false) => b'x',
            (false, false) => b'-',
        });
    }
    line.extend_from_slice(b"  0 ");
    line.extend_from_slice(owner);
    line.extend(std::iter::repeat_n(
        b' ',
        6_usize.saturating_sub(owner.len()) + 1,
    ));
    line.extend_from_slice(group);
    let width = (13_usize.saturating_sub(group.len())).max(size.len() + 1);
    line.extend_from_slice(format!("{size:>width$} {time} ").as_bytes());
    line.extend_from_slice(name);
    line
}

struct Entry {
    path: PathBuf,
    data: Data,
    mode: Option<u32>,
    mtime: Option<FileTime>,
}

enum Data {
    Directory,
    File(u64),
    Symlink(PathBuf),
    HardLink(PathBuf),
}

fn symlink_target(link: &Path, target: &[u8]) -> Result<PathBuf, ArchiveError> {
    if target.is_empty() || target.contains(&0) || target.len() > crate::MAX_NAME_BYTES {
        return Err(ArchiveError::new(format!(
            "Invalid symbolic link target for {}",
            link.display()
        )));
    }
    Ok(to_native(target))
}

pub(super) fn read_payload(reader: impl Read, size: u64) -> Result<Vec<u8>, ArchiveError> {
    let data = read_bounded(reader, size)?;
    check_payload_size(&data, size)?;
    Ok(data)
}

fn check_payload_size(data: &[u8], size: u64) -> Result<(), ArchiveError> {
    if data.len() as u64 != size {
        return Err(ArchiveError::new(
            "Archive entry size does not match its header",
        ));
    }
    Ok(())
}

struct Unpack<'a> {
    host: &'a dyn Host,
    root: PathBuf,
    directories: BTreeMap<PathBuf, (Option<u32>, Option<FileTime>)>,
    bytes_left: u64,
}

impl<'a> Unpack<'a> {
    fn new(host: &'a dyn Host, destination: &Path, bytes_left: u64) -> Result<Self, ArchiveError> {
        host.create_dir_all(destination)?;
        Ok(Self {
            host,
            root: host.canonicalize(destination)?,
            directories: BTreeMap::new(),
            bytes_left,
        })
    }

    fn replaces_source(
        &self,
        source: &Path,
        metadata: &Metadata,
        entries: &[(usize, Entry)],
    ) -> bool {
        // The reader opens the input again for each chunk. Keep a snapshot if
        // an entry can replace the input before the reader reaches the end.
        let root_metadata = self.host.metadata(&self.root, Follow::Yes).ok();
        let Some(depth) = source
            .ancestors()
            .skip(1)
            .position(|parent| {
                parent == self.root
                    || root_metadata.as_ref().is_some_and(|root| {
                        self.host
                            .metadata(parent, Follow::Yes)
                            .is_ok_and(|candidate| {
                                (root.dev, root.ino) == (candidate.dev, candidate.ino)
                            })
                    })
            })
            .map(|depth| depth + 1)
        else {
            return false;
        };
        entries.iter().any(|(_, entry)| {
            !matches!(entry.data, Data::Directory)
                && entry.path.components().count() == depth
                && self
                    .host
                    .metadata(&self.root.join(&entry.path), Follow::No)
                    .is_ok_and(|target| (target.dev, target.ino) == (metadata.dev, metadata.ino))
        })
    }

    fn tar(mut self, reader: impl Read, entries: Vec<(usize, Entry)>) -> Result<(), ArchiveError> {
        let mut payload = Vec::with_capacity(64 * 1024);
        let mut entries = entries.into_iter().peekable();
        let mut archive = Archive::new(reader.take(size_limit() + 1));
        for (index, item) in archive.entries().map_err(archive_error)?.enumerate() {
            let mut item = item.map_err(archive_error)?;
            let Some((_, entry)) = entries.next_if(|(selected, _)| *selected == index) else {
                continue;
            };
            payload.clear();
            if let Data::File(size) = entry.data {
                if item.size() != size {
                    return Err(ArchiveError::new("The archive changed during extraction"));
                }
                (&mut item)
                    .take(size.saturating_add(1))
                    .read_to_end(&mut payload)
                    .map_err(archive_error)?;
                check_payload_size(&payload, size)?;
            }
            self.entry(entry, &payload)?;
            if entries.peek().is_none() {
                break;
            }
        }
        if entries.peek().is_some() {
            return Err(ArchiveError::new("The archive changed during extraction"));
        }
        self.finish()
    }

    fn checked_path(&self, relative: &Path, create_parents: bool) -> Result<PathBuf, ArchiveError> {
        let relative_parent = relative.parent().unwrap_or(Path::new(""));
        let mut parent = self.root.clone();
        ensure_directory(self.host, &parent, false)?;
        for part in relative_parent {
            parent.push(part);
            ensure_directory(self.host, &parent, create_parents)?;
        }
        Ok(self.root.join(relative))
    }

    fn entry(&mut self, entry: Entry, payload: &[u8]) -> Result<(), ArchiveError> {
        let host = self.host;
        let path = self.checked_path(&entry.path, true)?;
        match entry.data {
            Data::Directory => {
                ensure_directory(host, &path, true)?;
                self.directories
                    .insert(entry.path, (entry.mode, entry.mtime));
            }
            Data::File(_) => {
                remove_existing_file(host, &path)?;
                host.write(&path, payload, WriteMode::CreateNew)?;
                restore(host, &path, entry.mode, entry.mtime)?;
            }
            Data::Symlink(target) => {
                remove_existing_file(host, &path)?;
                host.symlink(&target, &path)?;
            }
            Data::HardLink(target) => {
                let target = self.checked_path(&target, false)?;
                let metadata = host.metadata(&target, Follow::No)?;
                if metadata.kind != FileKind::File {
                    return Err(ArchiveError::new(format!(
                        "The hard link target must be a regular file: {}",
                        target.display()
                    )));
                }
                remove_existing_file(host, &path)?;
                if host.hard_link(&target, &path).is_err() {
                    let contents = read_bytes(host, &target, self.bytes_left)?;
                    self.bytes_left = self
                        .bytes_left
                        .checked_sub(contents.len() as u64)
                        .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
                    host.write(&path, &contents, WriteMode::CreateNew)?;
                    restore(host, &path, Some(metadata.mode), Some(metadata.mtime))?;
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<(), ArchiveError> {
        // Restore children first so a directory mode cannot prevent child writes.
        for (relative, &(mode, mtime)) in self.directories.iter().rev() {
            let path = self.checked_path(relative, false)?;
            ensure_directory(self.host, &path, false)?;
            restore(self.host, &path, mode, mtime)?;
        }
        Ok(())
    }
}

fn existing(host: &dyn Host, path: &Path) -> Result<Option<Metadata>, ArchiveError> {
    match host.metadata(path, Follow::No) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn ensure_directory(host: &dyn Host, path: &Path, create: bool) -> Result<(), ArchiveError> {
    match existing(host, path)? {
        None if create => Ok(host.create_dir_all(path)?),
        Some(metadata) if metadata.kind == FileKind::Dir => Ok(()),
        _ => Err(conflict(path)),
    }
}

fn remove_existing_file(host: &dyn Host, path: &Path) -> Result<(), ArchiveError> {
    match existing(host, path)? {
        None => Ok(()),
        Some(metadata) if metadata.kind == FileKind::Dir => Err(conflict(path)),
        Some(_) => Ok(host.remove_file(path)?),
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
        host.set_permissions(path, mode & 0o777)?;
    }
    if let Some(mtime) = mtime {
        host.set_times(path, None, Some(mtime))?;
    }
    Ok(())
}
