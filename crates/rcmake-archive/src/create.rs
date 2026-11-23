use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use flate2::{Compression, write::GzEncoder};
use rcmake_base::BString;
use rcmake_host::{FileKind, FileTime, Follow, Host, WriteMode, path::from_native};
use tar::{Builder, EntryType, Header};
use zip::{CompressionMethod, ZipWriter, write::FullFileOptions};

use crate::timestamp;
use crate::{
    ArchiveError, ArchiveFormat, CreateOptions, MAX_ENTRIES, absolute, archive_error, archive_name,
    checked_name, emit_names, read_bytes, relative_path, size_limit,
};

pub(super) fn run(
    host: &dyn Host,
    output: &Path,
    base_dir: &Path,
    inputs: &[PathBuf],
    options: &CreateOptions,
    command: bool,
) -> Result<Vec<BString>, ArchiveError> {
    absolute(output)?;
    absolute(base_dir)?;
    let mtime = options
        .mtime
        .map(i64::try_from)
        .transpose()
        .map_err(archive_error)?
        .map(timestamp::seconds);
    let mut archive = Creation {
        host,
        writer: Writer::new(options)?,
        mtime,
        names: Vec::new(),
        bytes_left: size_limit(),
        command,
    };
    for input in inputs {
        let name = if input.is_absolute() {
            if let Ok(relative) = input.strip_prefix(base_dir) {
                archive_name(relative)
            } else if command {
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
        archive.add(&base_dir.join(input), name)?;
    }

    let bytes = archive.writer.finish()?;
    host.write(output, &bytes, WriteMode::Truncate)?;
    emit_names(host, &archive.names, options.verbose);
    Ok(archive.names)
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
    mtime: Option<FileTime>,
    names: Vec<BString>,
    bytes_left: u64,
    command: bool,
}

impl Creation<'_> {
    fn add(&mut self, path: &Path, mut name: BString) -> Result<(), ArchiveError> {
        if self.command {
            checked_name(&name)?;
        } else {
            relative_path(&name)?;
        }
        let metadata = self.host.metadata(path, Follow::No)?;
        if metadata.kind == FileKind::Dir {
            name.push(b'/');
        }
        self.bytes_left = self
            .bytes_left
            .checked_sub(name.len() as u64 + 512)
            .filter(|_| self.names.len() < MAX_ENTRIES)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the entry or size limit"))?;

        let contents = match metadata.kind {
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
            .checked_sub(contents.len() as u64)
            .ok_or_else(|| ArchiveError::new("Archive exceeds the size limit"))?;
        self.writer.add(
            &name,
            metadata.kind,
            metadata.mode,
            self.mtime.unwrap_or(metadata.mtime),
            &contents,
        )?;

        if metadata.kind == FileKind::Dir {
            let mut children = self.host.read_dir(path)?;
            children.sort_unstable_by(|left, right| left.name.cmp(&right.name));
            self.names.push(name.clone());
            for child in children {
                let child_path = relative_path(&child.name)?;
                if child_path.components().count() != 1 || child.name.contains(&b'/') {
                    return Err(ArchiveError::new(
                        "The host returned an invalid directory entry",
                    ));
                }
                let mut child_name = name.clone();
                child_name.extend_from_slice(&child.name);
                self.add(&path.join(child_path), child_name)?;
            }
        } else {
            self.names.push(name);
        }
        Ok(())
    }
}

enum Writer {
    Tar(Builder<Vec<u8>>, bool),
    TarGz(Builder<GzEncoder<Vec<u8>>>, bool),
    Zip {
        writer: Box<ZipWriter<Cursor<Vec<u8>>>>,
        method: CompressionMethod,
        level: Option<i64>,
    },
}

impl Writer {
    fn new(options: &CreateOptions) -> Result<Self, ArchiveError> {
        let level = options.compression_level;
        if level.is_some_and(|level| level > 9) {
            return Err(ArchiveError::new(
                "The compression level must be between 0 and 9",
            ));
        }
        let gnu = matches!(
            options.format,
            ArchiveFormat::GnuTar | ArchiveFormat::GnuTarGz
        );
        Ok(match options.format {
            ArchiveFormat::Tar | ArchiveFormat::GnuTar => Self::Tar(Builder::new(Vec::new()), gnu),
            ArchiveFormat::TarGz | ArchiveFormat::GnuTarGz => Self::TarGz(
                Builder::new(GzEncoder::new(
                    Vec::new(),
                    level.map_or_else(Compression::default, Compression::new),
                )),
                gnu,
            ),
            ArchiveFormat::Zip | ArchiveFormat::ZipStored => Self::Zip {
                writer: Box::new(ZipWriter::new(Cursor::new(Vec::new()))),
                method: if options.format == ArchiveFormat::ZipStored {
                    CompressionMethod::Stored
                } else {
                    CompressionMethod::Deflated
                },
                level: level
                    .filter(|_| options.format == ArchiveFormat::Zip)
                    .map(i64::from),
            },
        })
    }

    fn add(
        &mut self,
        name: &[u8],
        kind: FileKind,
        mode: u32,
        mtime: FileTime,
        contents: &[u8],
    ) -> Result<(), ArchiveError> {
        match self {
            Self::Tar(writer, gnu) => add_tar(writer, *gnu, name, kind, mode, mtime, contents),
            Self::TarGz(writer, gnu) => add_tar(writer, *gnu, name, kind, mode, mtime, contents),
            Self::Zip {
                writer,
                method,
                level,
            } => {
                if name.contains(&b'\\') {
                    return Err(ArchiveError::new(
                        "ZIP input paths must use slash separators",
                    ));
                }
                let name = std::str::from_utf8(name).map_err(archive_error)?;
                let mut options = FullFileOptions::default()
                    .compression_method(*method)
                    .compression_level(*level)
                    .unix_permissions(mode)
                    .last_modified_time(timestamp::zip_dos_time(mtime));
                timestamp::add_zip_time(&mut options, mtime)?;
                match kind {
                    FileKind::Dir => writer.add_directory(name, options).map_err(archive_error),
                    FileKind::Symlink => {
                        let target = std::str::from_utf8(contents).map_err(archive_error)?;
                        writer
                            .add_symlink(name, target, options)
                            .map_err(archive_error)
                    }
                    _ => {
                        writer.start_file(name, options).map_err(archive_error)?;
                        writer.write_all(contents).map_err(archive_error)
                    }
                }
            }
        }
    }

    fn finish(self) -> Result<Vec<u8>, ArchiveError> {
        match self {
            Self::Tar(writer, _) => writer.into_inner().map_err(archive_error),
            Self::TarGz(writer, _) => writer
                .into_inner()
                .and_then(GzEncoder::finish)
                .map_err(archive_error),
            Self::Zip { writer, .. } => writer
                .finish()
                .map(Cursor::into_inner)
                .map_err(archive_error),
        }
    }
}

fn add_tar(
    writer: &mut Builder<impl Write>,
    gnu: bool,
    name: &[u8],
    kind: FileKind,
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
    header.set_entry_type(match kind {
        FileKind::Dir => EntryType::Directory,
        FileKind::Symlink => EntryType::Symlink,
        _ => EntryType::Regular,
    });
    header.set_size(if kind == FileKind::File {
        contents.len() as u64
    } else {
        0
    });

    let long_link = kind == FileKind::Symlink && header.set_link_name_literal(contents).is_err();
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
    let data = if kind == FileKind::File {
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
