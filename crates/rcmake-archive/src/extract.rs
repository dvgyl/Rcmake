use std::collections::BTreeMap;
use std::io::{self, BufReader, ErrorKind, Read, SeekFrom};
use std::path::{Component, Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{
    FileKind, FileTime, Follow, Host, Metadata, ReadSeek, WriteMode, path::to_native,
};
use tar::{Archive, EntryType};

use crate::pattern::Patterns;
use crate::{
    ArchiveError, ExtractRequest, MAX_ENTRIES, absolute, archive_error, checked_name, emit_names,
    read_bounded, relative_path,
};
use crate::{codec, external_filter, seven_zip, tar_budget, timestamp, zip_codec};

pub(super) mod stage;

use stage::{Interval, Stage};

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

const RATIO: u64 = 200;
const FLOOR: u64 = 1024 * 1024 * 1024;
const TRAILER_BYTES: u64 = 16 * 1024 * 1024;
const MIN_TAR_OVERHEAD: u64 = 64 * 1024;

pub(super) fn size_limit_message(ratio_capped: bool) -> &'static str {
    if ratio_capped {
        "Archive exceeds the size limit (set RCMAKE_EXTRACT_MAX_BYTES to override)"
    } else {
        "Archive exceeds the size limit"
    }
}

struct StreamBudget<R> {
    inner: R,
    limit: u64,
    read: u64,
    ratio_capped: bool,
}

impl<R> StreamBudget<R> {
    fn new(inner: R, limit: u64, ratio_capped: bool) -> Self {
        Self {
            inner,
            limit,
            read: 0,
            ratio_capped,
        }
    }

    fn bytes_read(&self) -> u64 {
        self.read
    }
}

impl<R: Read> Read for StreamBudget<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.read);
        if remaining == 0 {
            let mut probe = [0_u8; 1];
            if self.inner.read(&mut probe)? == 0 {
                return Ok(0);
            }
            return Err(io::Error::other(size_limit_message(self.ratio_capped)));
        }
        let max = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let n = match buf.get_mut(..max) {
            Some(target) => self.inner.read(target)?,
            None => self.inner.read(buf)?,
        };
        self.read = self.read.saturating_add(n as u64);
        Ok(n)
    }
}

fn output_limit(host: &dyn Host, archive_len: u64, floor: u64, ratio: u64) -> (u64, bool) {
    if let Some(bytes) = host
        .env_get(b"RCMAKE_EXTRACT_MAX_BYTES")
        .and_then(|value| std::str::from_utf8(&value).ok()?.parse::<u64>().ok())
    {
        return (bytes.min(stage::MAX_BYTES), false);
    }
    let limit = archive_len
        .saturating_mul(ratio)
        .clamp(floor, stage::MAX_BYTES);
    (limit, limit < stage::MAX_BYTES)
}

pub(super) fn run(
    host: &dyn Host,
    archive_path: &Path,
    request: ExtractRequest<'_>,
    listing: Listing<'_>,
) -> Result<Vec<BString>, ArchiveError> {
    run_with_limits(host, archive_path, request, listing, FLOOR, RATIO)
}

pub(super) fn run_with_limits(
    host: &dyn Host,
    archive_path: &Path,
    request: ExtractRequest<'_>,
    listing: Listing<'_>,
    floor: u64,
    ratio: u64,
) -> Result<Vec<BString>, ArchiveError> {
    run_inner(host, archive_path, request, listing, |len| {
        output_limit(host, len, floor, ratio)
    })
}

#[cfg(test)]
pub(super) fn run_with_output_limit(
    host: &dyn Host,
    archive_path: &Path,
    request: ExtractRequest<'_>,
    listing: Listing<'_>,
    limit: u64,
) -> Result<Vec<BString>, ArchiveError> {
    run_inner(host, archive_path, request, listing, |_| (limit, false))
}

#[cfg(test)]
pub(super) fn scan_tar_for_test(
    host: &dyn Host,
    destination: &Path,
    reader: impl Read,
) -> Result<Vec<BString>, ArchiveError> {
    let mut stage = Stage::new(host, destination);
    let mut scan = Scan {
        excludes: Patterns::new(&[])?,
        patterns: Patterns::new(&[])?,
        list_only: false,
        listing: Listing::Shared,
        touch: false,
        confined_links: false,
        names: Vec::new(),
        count: 0,
        bytes_left: stage::MAX_BYTES,
        ratio_capped: false,
        metadata_left: stage::METADATA_BYTES,
    };
    scan.tar_headers(reader, &mut stage)?;
    Ok(scan.names)
}

fn run_inner(
    host: &dyn Host,
    archive_path: &Path,
    request: ExtractRequest<'_>,
    listing: Listing<'_>,
    resolve_limit: impl FnOnce(u64) -> (u64, bool),
) -> Result<Vec<BString>, ArchiveError> {
    let options = request.options;
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
    if options.excludes.iter().any(|pattern| pattern.is_empty()) {
        return Err(ArchiveError::new("Failed to add to exclusion list: "));
    }
    let mut source = host.open_read(archive_path)?;
    let len = source.seek(SeekFrom::End(0)).map_err(archive_error)?;
    if len > stage::MAX_BYTES {
        return Err(ArchiveError::new(format!(
            "{}: File too large",
            archive_path.display()
        )));
    }
    let (limit, ratio_capped) = resolve_limit(len);
    source.seek(SeekFrom::Start(0)).map_err(archive_error)?;
    let mut prefix = [0; 16];
    let prefix = prefix
        .get_mut(..len.min(16) as usize)
        .ok_or_else(|| ArchiveError::new("Invalid archive prefix length"))?;
    source.read_exact(prefix).map_err(archive_error)?;
    source.seek(SeekFrom::Start(0)).map_err(archive_error)?;
    let mut staged = Stage::new(host, &options.destination);
    let mut scan = Scan {
        excludes: Patterns::new(&options.excludes)?,
        patterns: Patterns::new(&options.patterns)?,
        list_only: options.list_only,
        listing,
        touch: options.touch,
        confined_links: request.confined_links,
        names: Vec::new(),
        count: 0,
        bytes_left: limit,
        ratio_capped,
        metadata_left: stage::METADATA_BYTES,
    };
    let entries = if prefix.starts_with(seven_zip::MAGIC) {
        scan.seven_zip(&mut source, len, &mut staged)?
    } else if [b"PK\x03\x04", b"PK\x05\x06", b"PK\x06\x06"]
        .iter()
        .any(|magic| prefix.starts_with(*magic))
    {
        scan.zip(&mut source, len, &host.time_zone(), &mut staged)?
    } else if let Some(program) = external_filter::detect(prefix) {
        let bytes = external_filter::decompress(
            host,
            archive_path,
            program,
            scan.tar_stream_limit(),
            scan.ratio_capped,
        )?;
        scan.tar_headers(bytes.as_slice(), &mut staged)?
    } else {
        let input =
            BufReader::with_capacity(stage::BUFFER, (&mut source).take(stage::MAX_BYTES + 1));
        match codec::detect(prefix) {
            Some(filter) => scan.tar_headers(codec::decoder(filter, input)?, &mut staged)?,
            None => scan.tar_headers(input, &mut staged)?,
        }
    };
    drop(source);
    scan.patterns.finish()?;
    if !entries.is_empty() {
        let mut unpack = Unpack::new(
            host,
            &options.destination,
            scan.bytes_left,
            request.confined_links,
        )?;
        for (entry, payload) in entries {
            unpack.entry(entry, payload, &mut staged)?;
        }
        unpack.finish()?;
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
    confined_links: bool,
    names: Vec<BString>,
    count: usize,
    bytes_left: u64,
    ratio_capped: bool,
    metadata_left: usize,
}

type StagedEntry = (Entry, Option<Interval>);

impl Scan<'_> {
    fn size_limit_error(&self) -> ArchiveError {
        ArchiveError::new(size_limit_message(self.ratio_capped))
    }

    fn tar_stream_limit(&self) -> u64 {
        let overhead = (self.bytes_left / 8).clamp(MIN_TAR_OVERHEAD, TRAILER_BYTES);
        self.bytes_left
            .saturating_add(overhead)
            .min(stage::MAX_BYTES)
    }
    fn count_entry(&mut self, name_size: usize) -> Result<(), ArchiveError> {
        self.count += 1;
        if self.count > MAX_ENTRIES {
            return Err(ArchiveError::new("Archive exceeds the entry or size limit"));
        }
        self.reserve_metadata(name_size.saturating_mul(2).saturating_add(512))
    }

    fn reserve_metadata(&mut self, bytes: usize) -> Result<(), ArchiveError> {
        self.metadata_left = self
            .metadata_left
            .checked_sub(bytes)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the metadata size limit"))?;
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
            .ok_or_else(|| self.size_limit_error())?;
        Ok(())
    }

    fn tar_headers(
        &mut self,
        reader: impl Read,
        stage: &mut Stage<'_>,
    ) -> Result<Vec<StagedEntry>, ArchiveError> {
        let stream = StreamBudget::new(reader, self.tar_stream_limit(), self.ratio_capped);
        let mut archive = Archive::new(tar_budget::Reader::new(stream));
        let mut entries = Vec::new();
        for item in archive.entries().map_err(archive_error)? {
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
                self.reserve_metadata(line.len().saturating_sub(name.len()))?;
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
                    self.reserve_metadata(target.len())?;
                    Data::Symlink(symlink_target(&path, &target, self.confined_links)?)
                }
                EntryType::Link => {
                    let target = item
                        .link_name_bytes()
                        .ok_or_else(|| ArchiveError::new("The tar hard link has no target"))?;
                    self.reserve_metadata(target.len())?;
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
            let mode = if matches!(data, Data::Symlink(_) | Data::HardLink(_)) {
                None
            } else {
                Some(item.header().mode().map_err(archive_error)?)
            };
            let mtime = if self.touch || matches!(data, Data::HardLink(_)) {
                None
            } else {
                Some(timestamp::tar_time(&mut item)?)
            };
            let payload = if let Data::File(size) = data {
                Some(stage.payload(size, |output| stage::copy(&mut item, output, size))?)
            } else {
                None
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
        // Drain trailing padding and compression footers until EOF, bounded by both
        // TRAILER_BYTES and the decoded-stream budget.
        let mut remaining = archive.into_inner().into_inner();
        let consumed_before = remaining.bytes_read();
        if consumed_before < 512 {
            return Err(ArchiveError::new("Invalid tar archive"));
        }
        let mut trailer = (&mut remaining).take(TRAILER_BYTES);
        io::copy(&mut trailer, &mut io::sink()).map_err(archive_error)?;
        if trailer.limit() == 0 {
            let mut probe = [0_u8; 1];
            if remaining.read(&mut probe).map_err(archive_error)? != 0 {
                return Err(self.size_limit_error());
            }
        }
        Ok(entries)
    }

    fn seven_zip(
        &mut self,
        source: &mut dyn ReadSeek,
        len: u64,
        stage: &mut Stage<'_>,
    ) -> Result<Vec<StagedEntry>, ArchiveError> {
        let archive = seven_zip::open(source, len)?;
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
        seven_zip::read_entries(source, &archive, |item_index, item, reader| {
            let Some((index, mut entry, is_link)) =
                selected_entries.get_mut(item_index).and_then(Option::take)
            else {
                return stage::copy(reader, &mut io::sink(), item.size);
            };
            let payload = if is_link {
                let target = read_payload(reader, item.size)?;
                self.reserve_metadata(target.len().saturating_mul(2) + 4)?;
                if self.listing.time_formatter().is_some()
                    && let Some(name) = self.names.get_mut(index)
                {
                    name.extend_from_slice(b" -> ");
                    name.extend_from_slice(&target);
                }
                if !self.list_only {
                    entry.data =
                        Data::Symlink(symlink_target(&entry.path, &target, self.confined_links)?);
                }
                None
            } else if let Data::File(size) = entry.data {
                Some(stage.payload(size, |output| stage::copy(reader, output, size))?)
            } else {
                None
            };
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

    fn zip(
        &mut self,
        source: &mut dyn ReadSeek,
        len: u64,
        zone: &jiff::tz::TimeZone,
        stage: &mut Stage<'_>,
    ) -> Result<Vec<StagedEntry>, ArchiveError> {
        let archive = zip_codec::open(source, len, self.bytes_left).map_err(|error| {
            if self.ratio_capped && error.message == "Archive exceeds the size limit" {
                self.size_limit_error()
            } else {
                error
            }
        })?;
        self.bytes_left = archive.bytes_left;
        self.reserve_metadata(archive.metadata_bytes())?;
        let mut entries = Vec::new();
        for item in archive.entries() {
            let item = item?;
            let name: Vec<_> = item
                .name
                .iter()
                .map(|&byte| if byte == b'\\' { b'/' } else { byte })
                .collect();
            self.count_entry(name.len())?;
            let path = self.path(&name)?;
            if !self.select(&name, &path)? {
                continue;
            }

            let kind = item.mode.unwrap_or(0) & 0o170000;
            let is_link = kind == 0o120000;
            let is_dir = name.ends_with(b"/") || kind == 0o040000;
            let formatter = self.listing.time_formatter();
            let mtime = if formatter.is_some() || (!self.list_only && !self.touch) {
                item.time(zone)?
            } else {
                None
            };
            let needs_link = is_link && (!self.list_only || formatter.is_some());
            let link = if needs_link {
                if item.size > crate::MAX_NAME_BYTES as u64 {
                    return Err(ArchiveError::new(
                        "The ZIP symbolic link target is too long",
                    ));
                }
                let mut bytes = Vec::new();
                item.payload(source, &mut bytes)?;
                self.reserve_metadata(bytes.len().saturating_mul(2) + 4)?;
                bytes
            } else {
                Vec::new()
            };
            let display = if let Some(format_time) = formatter {
                let mode = item.mode.unwrap_or(if is_dir { 0o40755 } else { 0o100644 });
                let size = if is_link { 0 } else { item.size };
                let mut line = listing_line(
                    mode,
                    b"0 ",
                    b"0",
                    &size.to_string(),
                    &format_time(mtime.unwrap_or_default()),
                    &name,
                );
                if is_link {
                    line.extend_from_slice(b" -> ");
                    line.extend_from_slice(&link);
                }
                line
            } else {
                BString::from(name)
            };
            self.reserve_metadata(display.len())?;
            self.names.push(display);
            if self.list_only {
                continue;
            }
            let (data, payload) = if is_link {
                let target = symlink_target(&path, &link, self.confined_links)?;
                (Data::Symlink(target), None)
            } else if is_dir {
                (Data::Directory, None)
            } else {
                let interval = stage.payload(item.size, |output| item.payload(source, output))?;
                (Data::File(item.size), Some(interval))
            };
            entries.push((
                Entry {
                    path,
                    data,
                    mode: item.mode,
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

fn symlink_target(link: &Path, target: &[u8], confined: bool) -> Result<PathBuf, ArchiveError> {
    let error = || {
        ArchiveError::new(format!(
            "Invalid symbolic link target for {}",
            link.display()
        ))
    };
    if target.is_empty() || target.contains(&0) || target.len() > crate::MAX_NAME_BYTES {
        return Err(error());
    }
    let path = to_native(target);
    if confined {
        if crate::rooted(target) || path.components().count() > crate::MAX_DEPTH {
            return Err(error());
        }
        resolve_link(link, &path).ok_or_else(error)?;
    }
    Ok(path)
}

fn resolve_link(link: &Path, target: &Path) -> Option<PathBuf> {
    let mut path = link.parent()?.to_owned();
    let mut has_name = false;
    for part in target.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if !has_name && path.pop() => {}
            Component::Normal(name)
                if !cfg!(windows) || !crate::unsafe_windows_name(name.as_encoded_bytes()) =>
            {
                has_name = true;
                path.push(name);
            }
            _ => return None,
        }
    }
    Some(path)
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
    regular_members: Option<BTreeMap<PathBuf, (u64, u64)>>,
    bytes_left: u64,
}

impl<'a> Unpack<'a> {
    fn new(
        host: &'a dyn Host,
        destination: &Path,
        bytes_left: u64,
        confined_links: bool,
    ) -> Result<Self, ArchiveError> {
        host.create_dir_all(destination)?;
        Ok(Self {
            host,
            root: host.canonicalize(destination)?,
            directories: BTreeMap::new(),
            regular_members: confined_links.then(BTreeMap::new),
            bytes_left,
        })
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

    fn entry(
        &mut self,
        entry: Entry,
        payload: Option<Interval>,
        stage: &mut Stage<'_>,
    ) -> Result<(), ArchiveError> {
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
                stage.publish(
                    payload.ok_or_else(|| ArchiveError::new("Missing staged archive entry"))?,
                    &path,
                )?;
                let mode = entry.mode.filter(|&mode| {
                    cfg!(windows)
                        || !host
                            .metadata_fresh(&path, Follow::No)
                            .is_ok_and(|metadata| {
                                metadata.kind == FileKind::File && metadata.mode == mode & 0o777
                            })
                });
                restore(host, &path, mode, entry.mtime)?;
                if let Some(members) = &mut self.regular_members {
                    let metadata = host.metadata(&path, Follow::No)?;
                    members.insert(entry.path, (metadata.dev, metadata.ino));
                }
            }
            Data::Symlink(target) => {
                if let Some(members) = &mut self.regular_members {
                    let resolved =
                        resolve_link(&entry.path, &target).ok_or_else(|| conflict(&path))?;
                    let mut parent = self.root.clone();
                    for part in &resolved {
                        parent.push(part);
                        if existing(host, &parent)?
                            .is_some_and(|meta| meta.kind == FileKind::Symlink)
                        {
                            return Err(conflict(&parent));
                        }
                    }
                    members.remove(&entry.path);
                }
                remove_existing_file(host, &path)?;
                host.symlink(&target, &path)?;
                if let Some(mtime) = entry.mtime
                    && let Err(error) = host.set_symlink_times(&path, None, Some(mtime))
                    && error.kind != ErrorKind::Unsupported
                {
                    return Err(error.into());
                }
            }
            Data::HardLink(target) => {
                let source = self.checked_path(&target, false)?;
                let metadata = host.metadata(&source, Follow::No)?;
                if let Some(members) = &mut self.regular_members {
                    if members.get(&target) != Some(&(metadata.dev, metadata.ino)) {
                        return Err(ArchiveError::new(format!(
                            "The hard link target must be a regular member from this extraction: {}",
                            target.display()
                        )));
                    }
                    members.remove(&entry.path);
                }
                if metadata.kind != FileKind::File {
                    return Err(ArchiveError::new(format!(
                        "The hard link target must be a regular file: {}",
                        source.display()
                    )));
                }
                remove_existing_file(host, &path)?;
                if host.hard_link(&source, &path).is_err() {
                    self.bytes_left = self
                        .bytes_left
                        .checked_sub(metadata.len)
                        .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
                    let mut contents = host.open_read(&source)?;
                    host.write_from(&path, WriteMode::CreateNew, &mut contents, metadata.len)?;
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
            let metadata = existing(self.host, &path)?
                .filter(|metadata| metadata.kind == FileKind::Dir)
                .ok_or_else(|| conflict(&path))?;
            let mode = mode.filter(|&mode| metadata.mode != mode & 0o777);
            restore(self.host, &path, mode, mtime)?;
        }
        Ok(())
    }
}

fn existing(host: &dyn Host, path: &Path) -> Result<Option<Metadata>, ArchiveError> {
    match host.metadata_fresh(path, Follow::No) {
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
