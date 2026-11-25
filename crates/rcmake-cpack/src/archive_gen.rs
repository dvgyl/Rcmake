//! Generators for tar, ZIP, 7-Zip, and self-extracting archives.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_archive::{
    ArchiveFormat, CreateOptions, SevenZipCompression, ZipCompression, create_archive,
    create_prefixed_gzip_tar,
};
use rcmake_base::BString;
use rcmake_base::parse_source_date_epoch;
use rcmake_host::path::to_native;
use rcmake_host::{FileKind, Follow, Host};

use crate::config::CpackConfig;

mod stgz;

use crate::{
    ComponentGrouping, ComponentMembers, CpackError, PackageContext, PackageGenerator, error,
    safe_name,
};

/// Archive format of an [`ArchiveGenerator`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `TGZ`: gzip-compressed tar.
    Tgz,
    /// `TXZ`: xz-compressed tar.
    Txz,
    /// `TZST`: zstd-compressed tar.
    Tzst,
    /// `TBZ2`: bzip2-compressed tar.
    Tbz2,
    /// `TZ`: LZW-compressed tar (`compress`).
    Tz,
    /// `TAR`: uncompressed tar.
    Tar,
    /// `ZIP`.
    Zip,
    /// ZIP with deflate compression.
    ZipDeflate,
    /// ZIP without compression.
    ZipStore,
    /// ZIP with bzip2 compression.
    ZipBzip2,
    /// ZIP with LZMA compression.
    ZipLzma,
    /// ZIP with LZMA2 compression.
    ZipLzma2,
    /// ZIP with Zstandard compression.
    ZipZstd,
    /// 7-Zip with the selected compression method.
    SevenZip(SevenZipCompression),
    /// The default 7-Zip generator.
    SevenZipDefault,
    /// Self-extracting gzip tar.
    Stgz,
}

impl ArchiveKind {
    /// All archive generators.
    pub const ALL: [Self; 22] = [
        Self::Tgz,
        Self::Txz,
        Self::Tzst,
        Self::Tbz2,
        Self::Tz,
        Self::Tar,
        Self::Zip,
        Self::ZipDeflate,
        Self::ZipStore,
        Self::ZipBzip2,
        Self::ZipLzma,
        Self::ZipLzma2,
        Self::ZipZstd,
        Self::SevenZipDefault,
        Self::SevenZip(SevenZipCompression::Store),
        Self::SevenZip(SevenZipCompression::Deflate),
        Self::SevenZip(SevenZipCompression::Bzip2),
        Self::SevenZip(SevenZipCompression::Lzma),
        Self::SevenZip(SevenZipCompression::Lzma2),
        Self::SevenZip(SevenZipCompression::Zstd),
        Self::SevenZip(SevenZipCompression::Ppmd),
        Self::Stgz,
    ];

    /// Generator name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tgz => "TGZ",
            Self::Txz => "TXZ",
            Self::Tzst => "TZST",
            Self::Tbz2 => "TBZ2",
            Self::Tz => "TZ",
            Self::Tar => "TAR",
            Self::Zip => "ZIP",
            Self::ZipDeflate => "ZIP_DEFLATE",
            Self::ZipStore => "ZIP_STORE",
            Self::ZipBzip2 => "ZIP_BZ2",
            Self::ZipLzma => "ZIP_LZMA",
            Self::ZipLzma2 => "ZIP_LZMA2",
            Self::ZipZstd => "ZIP_ZSTD",
            Self::SevenZipDefault => "7Z",
            Self::SevenZip(compression) => match compression {
                SevenZipCompression::Store => "7Z_STORE",
                SevenZipCompression::Deflate => "7Z_DEFLATE",
                SevenZipCompression::Bzip2 => "7Z_BZ2",
                SevenZipCompression::Lzma => "7Z_LZMA",
                SevenZipCompression::Lzma2 => "7Z_LZMA2",
                SevenZipCompression::Zstd => "7Z_ZSTD",
                SevenZipCompression::Ppmd => "7Z_PPMD",
            },
            Self::Stgz => "STGZ",
        }
    }

    /// Package file extension.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Tgz => ".tar.gz",
            Self::Txz => ".tar.xz",
            Self::Tzst => ".tar.zst",
            Self::Tbz2 => ".tar.bz2",
            Self::Tz => ".tar.Z",
            Self::Tar => ".tar",
            Self::Zip
            | Self::ZipDeflate
            | Self::ZipStore
            | Self::ZipBzip2
            | Self::ZipLzma
            | Self::ZipLzma2
            | Self::ZipZstd => ".zip",
            Self::SevenZipDefault | Self::SevenZip(_) => ".7z",
            Self::Stgz => ".sh",
        }
    }

    const fn format(self, threads: u32) -> ArchiveFormat {
        match self {
            Self::Tgz => ArchiveFormat::TarGz,
            Self::Txz => ArchiveFormat::TarXz,
            Self::Tzst => ArchiveFormat::TarZstd,
            Self::Tbz2 => ArchiveFormat::TarBz2,
            Self::Tz => ArchiveFormat::TarZ,
            Self::Tar => ArchiveFormat::Tar,
            Self::Zip | Self::ZipDeflate => ArchiveFormat::Zip,
            Self::ZipStore => ArchiveFormat::ZipStored,
            Self::ZipBzip2 => ArchiveFormat::ZipCompressed(ZipCompression::Bzip2),
            Self::ZipLzma => ArchiveFormat::ZipCompressed(ZipCompression::Lzma),
            Self::ZipLzma2 => ArchiveFormat::ZipCompressed(ZipCompression::Xz),
            Self::ZipZstd => ArchiveFormat::ZipCompressed(ZipCompression::Zstd),
            Self::SevenZipDefault => ArchiveFormat::SevenZip {
                compression: SevenZipCompression::Lzma,
                threads,
            },
            Self::SevenZip(compression) => ArchiveFormat::SevenZip {
                compression,
                threads,
            },
            Self::Stgz => ArchiveFormat::TarGz,
        }
    }
}

/// A CPack archive generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveGenerator {
    /// Archive format.
    pub kind: ArchiveKind,
}

impl PackageGenerator for ArchiveGenerator {
    fn name(&self) -> &'static str {
        self.kind.name()
    }

    fn extension(&self) -> &'static str {
        self.kind.extension()
    }

    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        self.package_with(cx, None)
    }

    fn package_from_stage(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
    ) -> Result<Vec<PathBuf>, CpackError> {
        self.package_with(cx, Some(members))
    }
}

impl ArchiveGenerator {
    fn package_with(
        &self,
        cx: &PackageContext<'_>,
        members: Option<&ComponentMembers>,
    ) -> Result<Vec<PathBuf>, CpackError> {
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let archive_name = cfg
            .get(b"CPACK_ARCHIVE_FILE_NAME")
            .filter(|name| archive_name_is_set(name))
            .unwrap_or(cx.file_name);
        let extension = cfg
            .get(b"CPACK_ARCHIVE_FILE_EXTENSION")
            .filter(|ext| !ext.is_empty())
            .map_or_else(
                || self.extension().into(),
                |ext| {
                    if ext.starts_with(b".") {
                        BString::from(ext)
                    } else {
                        format!(".{}", BString::from(ext)).into()
                    }
                },
            );
        let compression_level = cfg
            .get(b"CPACK_ARCHIVE_COMPRESSION_LEVEL")
            .or_else(|| cfg.get(b"CPACK_COMPRESSION_LEVEL"))
            .map(|level| level_number(level, "CPACK_ARCHIVE_COMPRESSION_LEVEL"))
            .transpose()?;
        let threads = cfg
            .get(b"CPACK_ARCHIVE_THREADS")
            .or_else(|| cfg.get(b"CPACK_THREADS"))
            .map(|value| level_number::<i32>(value, "CPACK_ARCHIVE_THREADS"))
            .transpose()?
            .unwrap_or(1)
            .unsigned_abs();
        // CPack uses zero for times before the Unix epoch.
        let mtime = cx
            .host
            .env_get(b"SOURCE_DATE_EPOCH")
            .and_then(|value| parse_source_date_epoch(&value))
            .map(|epoch| u64::try_from(epoch).unwrap_or_default());
        let options = CreateOptions {
            format: self.kind.format(threads),
            mtime,
            compression_level,
            verbose: false,
        };
        let header = (self.kind == ArchiveKind::Stgz)
            .then(|| stgz::header(cx, &cfg))
            .transpose()?;
        let header = header.as_ref().map(|header| header.as_slice());
        let archives = component_archives(&cfg, archive_name, self.name())?;
        if archives.is_empty() {
            let (base, inputs) = if header.is_none()
                && (cfg.get(b"CPACK_INCLUDE_TOPLEVEL_DIRECTORY").is_none()
                    || cfg.on(b"CPACK_INCLUDE_TOPLEVEL_DIRECTORY"))
            {
                let parent = cx
                    .staging_dir
                    .parent()
                    .ok_or_else(|| error("Missing parent directory for the staging directory"))?;
                let prefix = safe_name(cx.file_name)?;
                let inputs = children(cx.host, cx.staging_dir)?
                    .into_iter()
                    .map(|child| prefix.join(child))
                    .collect();
                (parent.to_owned(), inputs)
            } else {
                (
                    cx.staging_dir.to_owned(),
                    children(cx.host, cx.staging_dir)?,
                )
            };
            let path = archive_path(cx, archive_name, &extension)?;
            write_archive(cx, &path, &base, &inputs, &options, header)?;
            return Ok(vec![path]);
        }

        let mut used_paths = BTreeSet::new();
        let paths = archives
            .iter()
            .map(|archive| {
                let path = archive_path(cx, &archive.name, &extension)?;
                if !used_paths.insert(path.clone()) {
                    return Err(error(format!(
                        "Duplicate CPack archive name: {}",
                        path.display()
                    )));
                }
                Ok(path)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let prefix = component_prefix(cx, &cfg)?;
        for (archive, path) in archives.iter().zip(&paths) {
            let roots = archive
                .roots
                .iter()
                .map(|root| members.and_then(|members| members.get(root)))
                .collect::<Option<Vec<_>>>();
            let output = ArchiveOutput {
                path,
                options: &options,
                header,
            };
            match roots {
                Some(recorded) => {
                    write_recorded_archive(cx, &output, &archive.roots, &recorded)?;
                }
                None => write_component_archive(cx, &output, &prefix, &archive.roots)?,
            }
        }
        Ok(paths)
    }
}

struct ArchiveOutput<'a> {
    path: &'a Path,
    options: &'a CreateOptions,
    header: Option<&'a [u8]>,
}

struct ComponentArchive {
    name: BString,
    roots: Vec<PathBuf>,
}

fn component_archives(
    cfg: &CpackConfig,
    base: &[u8],
    generator: &str,
) -> Result<Vec<ComponentArchive>, CpackError> {
    let grouping = crate::stage::grouping(cfg);
    let mut seen = BTreeSet::new();
    let mut groups: BTreeMap<BString, Vec<PathBuf>> = BTreeMap::new();
    let mut singles: BTreeMap<BString, Vec<PathBuf>> = BTreeMap::new();
    let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
    for [_, _, selection, subdir] in projects.as_chunks::<4>().0 {
        let components = crate::stage::package_components(cfg, selection);
        if components.is_empty() {
            continue;
        }
        let subdir = crate::stage::relative_install_path(subdir)?;
        for component in components {
            let root = subdir.join(crate::stage::component_dir(&component));
            if !seen.insert((component.clone(), root.clone())) {
                continue;
            }
            if grouping == ComponentGrouping::OnePerGroup
                && let Some(group) = crate::stage::component_group(cfg, &component)
            {
                groups.entry(group.into()).or_default().push(root);
            } else {
                singles.entry(component).or_default().push(root);
            }
        }
    }
    if grouping == ComponentGrouping::AllComponentsInOne && !singles.is_empty() {
        return Ok(vec![ComponentArchive {
            name: base.into(),
            roots: singles.into_values().flatten().collect(),
        }]);
    }
    Ok(groups
        .into_iter()
        .map(|(group, roots)| ComponentArchive {
            name: component_archive_name(cfg, base, &group, generator, "COMPONENT_GROUP"),
            roots,
        })
        .chain(
            singles
                .into_iter()
                .map(|(component, roots)| ComponentArchive {
                    name: component_archive_name(cfg, base, &component, generator, "COMPONENT"),
                    roots,
                }),
        )
        .collect())
}

fn component_archive_name(
    cfg: &CpackConfig,
    base: &[u8],
    name: &[u8],
    generator: &str,
    scope: &str,
) -> BString {
    let upper = BString::from(name.to_ascii_uppercase());
    let variable = format!("CPACK_ARCHIVE_{upper}_FILE_NAME");
    if let Some(name) = cfg
        .get(variable.as_bytes())
        .filter(|name| archive_name_is_set(name))
    {
        return name.into();
    }
    let display = format!("CPACK_{generator}_USE_DISPLAY_NAME_IN_FILENAME");
    let suffix = if cfg.on(display.as_bytes()) {
        let variable = format!("CPACK_{scope}_{upper}_DISPLAY_NAME");
        cfg.get(variable.as_bytes()).unwrap_or(name)
    } else {
        name
    };
    [base, b"-", suffix].concat().into()
}

fn archive_name_is_set(name: &[u8]) -> bool {
    !name.is_empty() && name != b"NOTFOUND" && !name.ends_with(b"-NOTFOUND")
}

fn component_workspace(cx: &PackageContext<'_>) -> Result<PathBuf, CpackError> {
    for index in 0..100 {
        let path = cx.staging_dir.join(format!(".rcmake-components-{index}"));
        match cx.host.metadata(&path, Follow::No) {
            Ok(_) => {}
            Err(err) if err.kind == ErrorKind::NotFound => {
                cx.host.create_dir_all(&path)?;
                return Ok(path);
            }
            Err(err) => return Err(err.into()),
        }
    }
    Err(error("Cannot create a CPack component workspace"))
}

fn component_prefix(cx: &PackageContext<'_>, cfg: &CpackConfig) -> Result<PathBuf, CpackError> {
    let mut prefix = if cfg.on(b"CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY") {
        safe_name(cx.file_name)?
    } else {
        PathBuf::new()
    };
    let install_prefix = crate::stage::project_install_prefix(cfg);
    prefix.extend(crate::stage::relative_install_path(install_prefix)?.iter());
    Ok(prefix)
}

fn component_root(cx: &PackageContext<'_>, root: &Path) -> Result<(), CpackError> {
    let metadata = cx.host.metadata(root, Follow::No).map_err(|err| {
        error(format!(
            "Failed to change working directory to {}: {err}",
            root.display()
        ))
    })?;
    if metadata.kind != FileKind::Dir {
        return Err(error(format!(
            "Failed to change working directory to {}",
            root.display()
        )));
    }
    Ok(())
}

fn write_component_archive(
    cx: &PackageContext<'_>,
    output: &ArchiveOutput<'_>,
    prefix: &Path,
    roots: &[PathBuf],
) -> Result<(), CpackError> {
    let ArchiveOutput {
        path,
        options,
        header,
    } = *output;
    let mut roots = roots.iter().map(|root| cx.staging_dir.join(root));
    let first = roots
        .next()
        .ok_or_else(|| error("CPack component group is empty"))?;
    let component_inputs = |root: &Path| {
        component_root(cx, root)?;
        children(cx.host, &root.join(prefix))
            .map(|files| files.into_iter().map(|file| prefix.join(file)))
    };
    if roots.len() == 0 {
        let inputs = component_inputs(&first)?.collect::<Vec<_>>();
        return write_archive(cx, path, &first, &inputs, options, header);
    }
    let merged = component_workspace(cx)?;
    let result: Result<(), CpackError> = (|| {
        let mut seen = BTreeSet::new();
        let mut inputs = Vec::new();
        for root in std::iter::once(first).chain(roots) {
            inputs.extend(component_inputs(&root)?.filter(|file| seen.insert(file.clone())));
            crate::source::merge_component(cx.host, &root, &merged)?;
        }
        write_archive(cx, path, &merged, &inputs, options, header)
    })();
    let cleanup = cx.host.remove_dir_all(&merged);
    result?;
    cleanup?;
    Ok(())
}

// Archive only the paths that the component installs recorded, with their current data.
fn write_recorded_archive(
    cx: &PackageContext<'_>,
    output: &ArchiveOutput<'_>,
    roots: &[PathBuf],
    recorded: &[&BTreeSet<PathBuf>],
) -> Result<(), CpackError> {
    let roots = roots
        .iter()
        .map(|root| cx.staging_dir.join(root))
        .collect::<Vec<_>>();
    let mut members = BTreeSet::new();
    let mut complete = true;
    for (root, recorded) in roots.iter().zip(recorded) {
        component_root(cx, root)?;
        complete &= check_recorded(cx.host, root, recorded)?;
        members.extend(recorded.iter().cloned());
    }
    let inputs = top_members(&members, recorded);
    if complete && let [root] = roots.as_slice() {
        return write_archive(
            cx,
            output.path,
            root,
            &inputs,
            output.options,
            output.header,
        );
    }
    let merged = component_workspace(cx)?;
    let result: Result<(), CpackError> = (|| {
        for (root, recorded) in roots.iter().zip(recorded) {
            crate::source::merge_recorded(cx.host, root, recorded, &merged)?;
        }
        write_archive(
            cx,
            output.path,
            &merged,
            &inputs,
            output.options,
            output.header,
        )
    })();
    let cleanup = cx.host.remove_dir_all(&merged);
    result?;
    cleanup?;
    Ok(())
}

// Check that each recorded path exists below real directories. Returns whether every
// child of each recorded directory is also recorded.
fn check_recorded(
    host: &dyn Host,
    root: &Path,
    recorded: &BTreeSet<PathBuf>,
) -> Result<bool, CpackError> {
    let mut complete = true;
    for member in recorded {
        let mut path = root.to_owned();
        let mut parts = member.components().peekable();
        while let Some(part) = parts.next() {
            path.push(part);
            let missing = |err: rcmake_host::HostError| {
                error(format!(
                    "CPack cannot read the installed component file {}: {err}",
                    path.display()
                ))
            };
            let kind = host.metadata(&path, Follow::No).map_err(missing)?.kind;
            if parts.peek().is_some() && kind != FileKind::Dir {
                return Err(error(format!(
                    "CPack expected a directory at {}",
                    path.display()
                )));
            }
            if parts.peek().is_none() && kind == FileKind::Dir {
                complete &= children(host, &path)?
                    .iter()
                    .all(|child| recorded.contains(&member.join(child)));
            }
        }
    }
    Ok(complete)
}

// List the recorded paths without a recorded ancestor, one root after another. The
// archive adds each recorded directory with its recorded children.
fn top_members(members: &BTreeSet<PathBuf>, recorded: &[&BTreeSet<PathBuf>]) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    recorded
        .iter()
        .flat_map(|paths| paths.iter())
        .filter(|path| {
            path.ancestors()
                .skip(1)
                .all(|ancestor| !members.contains(ancestor))
                && seen.insert(*path)
        })
        .cloned()
        .collect()
}

fn level_number<T: std::str::FromStr>(value: &[u8], variable: &str) -> Result<T, CpackError> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| {
            error(format!(
                "Invalid numeric value for {variable}: {}",
                BString::from(value)
            ))
        })
}

fn children(host: &dyn Host, path: &Path) -> Result<Vec<PathBuf>, CpackError> {
    let mut entries = host.read_dir(path)?;
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.name.starts_with(b"._"))
        .map(|entry| to_native(&entry.name))
        .collect())
}

fn archive_path(
    cx: &PackageContext<'_>,
    name: &[u8],
    extension: &[u8],
) -> Result<PathBuf, CpackError> {
    let name = [name, extension].concat();
    Ok(cx.output_dir.join(safe_name(&name)?))
}

fn write_archive(
    cx: &PackageContext<'_>,
    path: &Path,
    base: &Path,
    inputs: &[PathBuf],
    options: &CreateOptions,
    header: Option<&[u8]>,
) -> Result<(), CpackError> {
    crate::stage::clear_output_file(cx.host, path)?;
    if let Some(header) = header {
        create_prefixed_gzip_tar(cx.host, path, base, inputs, options, header)
            .map_err(|err| error(err.message))?;
        cx.host.set_permissions(path, 0o755)?;
    } else {
        create_archive(cx.host, path, base, inputs, options).map_err(|err| error(err.message))?;
    }
    Ok(())
}
