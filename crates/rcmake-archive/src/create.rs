use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{
    FileKind, FileTime, Follow, Host, Metadata, ReadRange, WriteMode, path::from_native,
};
use tar::{Builder, EntryType, Header};

use crate::pattern::Patterns;
use crate::{
    ArchiveError, ArchiveFormat, CreateOptions, CreateRequest, MAX_ENTRIES, absolute,
    archive_error, archive_name, checked_name, emit_names, read_bytes, relative_path, size_limit,
};
use crate::{TarCompression, ZipCompression, codec::Encoder, seven_zip, timestamp, zip_codec};

#[derive(Default)]
pub(super) struct Selection<'a> {
    pub(super) excludes: &'a [BString],
    pub(super) threads: u32,
    pub(super) allow_outside: bool,
    pub(super) command: bool,
}

pub(super) fn run(
    host: &dyn Host,
    output: &Path,
    base_dir: &Path,
    inputs: &[PathBuf],
    request: CreateRequest<'_>,
    selection: Selection<'_>,
) -> Result<Vec<BString>, ArchiveError> {
    absolute(output)?;
    absolute(base_dir)?;
    let options = request.options;
    let mtime = options
        .mtime
        .map(i64::try_from)
        .transpose()
        .map_err(archive_error)?
        .map(timestamp::seconds);
    let mut archive = Creation {
        host,
        writer: Writer::new(options, selection.threads)?,
        zone: host.time_zone(),
        mtime,
        names: Vec::new(),
        links: HashMap::new(),
        bytes_left: size_limit(),
        allow_outside: selection.allow_outside,
        command: selection.command,
        deterministic_order: request.deterministic_order,
        skip_apple_double: request.skip_apple_double,
        excludes: Patterns::new(selection.excludes)?,
    };
    for input in inputs {
        let name = if input.has_root() {
            if let Ok(relative) = input.strip_prefix(base_dir) {
                archive_name(relative)
            } else if let Some(relative) = relative_from_canonical_parent(host, base_dir, input) {
                archive_name(&relative)
            } else if selection.allow_outside {
                command_name(base_dir, input)
            } else {
                return Err(ArchiveError::new(format!(
                    "Input is outside the base directory: {}",
                    input.display()
                )));
            }
        } else {
            archive_name(input)
        };
        let name = if name.is_empty() {
            BString::from(".")
        } else {
            name
        };
        archive.add(&base_dir.join(input), name, None)?;
    }

    let bytes = archive.writer.finish()?;
    host.write(output, &bytes, WriteMode::Truncate)?;
    emit_names(host, &archive.names, options.verbose);
    Ok(archive.names)
}

fn relative_from_canonical_parent(host: &dyn Host, base: &Path, input: &Path) -> Option<PathBuf> {
    let base = host.canonicalize(base).ok()?;
    let parent = host.canonicalize(input.parent()?).ok()?;
    let relative = parent.strip_prefix(base).ok()?;
    Some(relative.join(input.file_name()?))
}

fn command_name(base_dir: &Path, input: &Path) -> BString {
    let common = base_dir
        .components()
        .zip(input.components())
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return from_native(input);
    }
    let mut relative = PathBuf::new();
    for _ in base_dir.components().skip(common) {
        relative.push("..");
    }
    for part in input.components().skip(common) {
        relative.push(part);
    }
    archive_name(&relative)
}

struct Creation<'a> {
    host: &'a dyn Host,
    writer: Writer,
    zone: jiff::tz::TimeZone,
    mtime: Option<FileTime>,
    names: Vec<BString>,
    links: HashMap<(u64, u64), (BString, PathBuf)>,
    bytes_left: u64,
    allow_outside: bool,
    command: bool,
    deterministic_order: bool,
    skip_apple_double: bool,
    excludes: Patterns<'a>,
}

impl Creation<'_> {
    fn add(
        &mut self,
        path: &Path,
        mut name: BString,
        siblings: Option<&HashSet<&[u8]>>,
    ) -> Result<(), ArchiveError> {
        if self.allow_outside {
            checked_name(&name)?;
        } else {
            relative_path(&name)?;
        }
        let metadata = self.host.metadata(path, Follow::No)?;
        let directory = metadata.kind == FileKind::Dir;
        if self
            .excludes
            .excludes(&crate::to_native(&name), directory)?
        {
            return Ok(());
        }
        if let Some(siblings) = siblings
            && metadata.kind == FileKind::File
            && let Some(child) = name.rsplit(|&byte| byte == b'/').next()
            && is_apple_double(self.host, path, child, siblings, metadata.len)?
        {
            return Ok(());
        }
        if directory {
            name.push(b'/');
        }
        self.bytes_left = self
            .bytes_left
            .checked_sub(name.len() as u64 + 512)
            .filter(|_| self.names.len() < MAX_ENTRIES)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the entry or size limit"))?;

        let identity = (metadata.dev, metadata.ino);
        let link_name = if !self.command
            && matches!(self.writer, Writer::Tar(..))
            && metadata.kind == FileKind::File
            && metadata.nlink > 1
            && let Ok(relative) = relative_path(&name)
        {
            let (target, target_path) = self
                .links
                .entry(identity)
                .or_insert_with(|| (name.clone(), relative.clone()));
            (*target_path != relative).then_some(target.as_slice())
        } else {
            None
        };
        let contents = match metadata.kind {
            FileKind::File if link_name.is_some() => Vec::new(),
            FileKind::File => read_bytes(self.host, path, self.bytes_left)?,
            FileKind::Symlink => from_native(&self.host.read_link(path)?).into(),
            FileKind::Dir => Vec::new(),
            FileKind::Other => {
                return Err(ArchiveError::new(format!(
                    "Unsupported archive input: {}",
                    path.display()
                )));
            }
        };
        self.bytes_left = self
            .bytes_left
            .checked_sub(link_name.unwrap_or(&contents).len() as u64)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
        let mut mtime = self.mtime.unwrap_or(metadata.mtime);
        if self.command && !matches!(self.writer, Writer::SevenZip(_)) {
            mtime.nanos = 0;
        }
        self.writer
            .add(&name, &metadata, mtime, &contents, link_name, &self.zone)?;

        if metadata.kind == FileKind::Dir {
            let mut children = self.host.read_dir(path)?;
            if self.deterministic_order {
                children.sort_unstable_by(|left, right| left.name.cmp(&right.name));
            }
            let siblings = self
                .skip_apple_double
                .then(|| children.iter().map(|entry| entry.name.as_slice()).collect());
            self.names.push(name.clone());
            for child in &children {
                let child_path = relative_path(&child.name)?;
                if child_path.components().count() != 1 || child.name.contains(&b'/') {
                    return Err(ArchiveError::new(
                        "The host returned an invalid directory entry",
                    ));
                }
                let mut child_name = name.clone();
                child_name.extend_from_slice(&child.name);
                self.add(&path.join(child_path), child_name, siblings.as_ref())?;
            }
        } else {
            self.names.push(name);
        }
        Ok(())
    }
}

fn is_apple_double(
    host: &dyn Host,
    path: &Path,
    name: &[u8],
    siblings: &HashSet<&[u8]>,
    size: u64,
) -> Result<bool, ArchiveError> {
    let Some(original) = name.strip_prefix(b"._") else {
        return Ok(false);
    };
    if !siblings.contains(original) {
        return Ok(false);
    }

    // macOS sidecars have a Finder info entry, a resource fork entry, or both.
    const HEADER_SIZE: usize = 26;
    const DESCRIPTOR_SIZE: usize = 12;
    let bytes = host.read(
        path,
        ReadRange::Slice {
            offset: 0,
            len: Some((HEADER_SIZE + 2 * DESCRIPTOR_SIZE) as u64),
        },
    )?;
    let Some(header @ [.., high, low]) = bytes.get(..HEADER_SIZE) else {
        return Ok(false);
    };
    let count = usize::from(u16::from_be_bytes([*high, *low]));
    if !header.starts_with(b"\0\x05\x16\x07\0\x02\0\0") || !(1..=2).contains(&count) {
        return Ok(false);
    }
    let end = HEADER_SIZE + count * DESCRIPTOR_SIZE;
    let Some(descriptors) = bytes.get(HEADER_SIZE..end) else {
        return Ok(false);
    };
    let mut ids = 0_u16;
    let fields = descriptors.as_chunks::<4>().0;
    for [id, offset, length] in fields.as_chunks::<3>().0 {
        let id = u32::from_be_bytes(*id);
        let offset = u64::from(u32::from_be_bytes(*offset));
        let length = u64::from(u32::from_be_bytes(*length));
        if !matches!(id, 2 | 9)
            || ids & (1 << id) != 0
            || offset < end as u64
            || offset + length > size
            || (id == 9 && length < 32)
        {
            return Ok(false);
        }
        ids |= 1 << id;
    }
    Ok(true)
}

enum Writer {
    Tar(Box<Builder<Encoder>>, bool),
    SevenZip(Box<seven_zip::Writer>),
    Zip(zip_codec::writer::Writer),
}

impl Writer {
    fn new(options: &CreateOptions, threads: u32) -> Result<Self, ArchiveError> {
        use ArchiveFormat::*;
        let level = options.compression_level;
        let (gnu, filter) = match options.format {
            Tar => (false, None),
            TarGz => (false, Some(TarCompression::Gzip)),
            TarBz2 => (false, Some(TarCompression::Bzip2)),
            TarXz => (false, Some(TarCompression::Xz)),
            TarLzma => (false, Some(TarCompression::Lzma)),
            TarZstd => (false, Some(TarCompression::Zstd)),
            TarZ => (false, Some(TarCompression::Compress)),
            GnuTar => (true, None),
            GnuTarGz => (true, Some(TarCompression::Gzip)),
            GnuTarCompressed(filter) => (true, Some(filter)),
            SevenZip { .. } | Zip | ZipStored | ZipCompressed(_) => (false, None),
        };
        let max_level = if filter == Some(TarCompression::Zstd)
            || matches!(
                options.format,
                SevenZip {
                    compression: crate::SevenZipCompression::Zstd,
                    ..
                }
            ) {
            19
        } else {
            9
        };
        if let Some(level) = level.filter(|&level| level > max_level) {
            return Err(ArchiveError::new(format!(
                "Compression level must be between 0 and {max_level}. Got {level}"
            )));
        }
        if let SevenZip {
            compression,
            threads,
        } = options.format
        {
            return Ok(Self::SevenZip(Box::new(seven_zip::writer(
                compression,
                level,
                threads,
            )?)));
        }
        if matches!(options.format, Zip | ZipStored | ZipCompressed(_)) {
            if options.format == ZipCompressed(ZipCompression::Ppmd) {
                return Err(zip_codec::ppmd_error());
            }
            return Ok(Self::Zip(zip_codec::writer::Writer::new(
                options.format,
                level,
            )));
        }
        let encoder = Encoder::new(filter, level, threads)?;
        Ok(Self::Tar(Box::new(Builder::new(encoder)), gnu))
    }

    fn add(
        &mut self,
        name: &[u8],
        metadata: &Metadata,
        mtime: FileTime,
        contents: &[u8],
        link_name: Option<&[u8]>,
        zone: &jiff::tz::TimeZone,
    ) -> Result<(), ArchiveError> {
        match self {
            Self::Tar(writer, gnu) => {
                let kind = match (metadata.kind, link_name) {
                    (_, Some(_)) => EntryType::Link,
                    (FileKind::Dir, None) => EntryType::Directory,
                    (FileKind::Symlink, None) => EntryType::Symlink,
                    _ => EntryType::Regular,
                };
                add_tar(
                    writer,
                    *gnu,
                    name,
                    kind,
                    metadata.mode,
                    mtime,
                    link_name.unwrap_or(contents),
                )
            }
            Self::SevenZip(writer) => seven_zip::add(writer, name, metadata, mtime, contents),
            Self::Zip(writer) => writer.add(name, metadata, mtime, contents, zone),
        }
    }

    fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        match self {
            Self::Tar(writer, _) => writer
                .into_inner()
                .and_then(Encoder::finish)
                .map_err(archive_error),
            Self::SevenZip(writer) => writer.finish(),
            Self::Zip(writer) => writer.finish(),
        }
    }
}

fn add_tar(
    writer: &mut Builder<impl Write>,
    gnu: bool,
    name: &[u8],
    kind: EntryType,
    mode: u32,
    mtime: FileTime,
    contents: &[u8],
) -> Result<(), ArchiveError> {
    let mut header = if gnu {
        Header::new_gnu()
    } else {
        Header::new_ustar()
    };
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(u64::try_from(mtime.secs).unwrap_or_default());
    header.set_entry_type(kind);
    header.set_size(if kind == EntryType::Regular {
        contents.len() as u64
    } else {
        0
    });

    let long_link = matches!(kind, EntryType::Link | EntryType::Symlink)
        && header.set_link_name_literal(contents).is_err();
    if gnu {
        if !copy_name(&mut header, name) {
            gnu_long(writer, EntryType::GNULongName, name)?;
            let field = &mut header.as_old_mut().name;
            for (byte, &value) in field.iter_mut().zip(name) {
                *byte = value;
            }
        }
        if long_link {
            gnu_long(writer, EntryType::GNULongLink, contents)?;
        }
    } else {
        let mut pax = Vec::new();
        let time = timestamp::pax_time(mtime);
        if let Some(time) = &time {
            pax.push(("mtime", time.as_bytes()));
        }
        if !copy_name(&mut header, name)
            && (header.set_path(crate::to_native(name)).is_err() || header.path_bytes() != name)
        {
            header.set_path("PaxEntry").map_err(archive_error)?;
            pax.push(("path", name));
        }
        if long_link {
            pax.push(("linkpath", contents));
        }
        writer.append_pax_extensions(pax).map_err(archive_error)?;
    }
    let data = if kind == EntryType::Regular {
        contents
    } else {
        &[]
    };
    header.set_cksum();
    writer.append(&header, data).map_err(archive_error)
}

fn copy_name(header: &mut Header, name: &[u8]) -> bool {
    let Some(field) = header.as_old_mut().name.get_mut(..name.len()) else {
        return false;
    };
    field.copy_from_slice(name);
    true
}

fn gnu_long(
    writer: &mut Builder<impl Write>,
    kind: EntryType,
    value: &[u8],
) -> Result<(), ArchiveError> {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(value.len() as u64 + 1);
    header.set_path("././@LongLink").map_err(archive_error)?;
    header.set_cksum();
    writer
        .append(&header, value.chain(b"\0".as_slice()))
        .map_err(archive_error)
}
