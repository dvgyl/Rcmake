//! Archive generators: `TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, and `ZIP`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_archive::{ArchiveFormat, CreateOptions, create_archive};
use rcmake_base::BString;
use rcmake_host::path::to_native;
use rcmake_host::{Follow, Host};

use crate::config::CpackConfig;

use crate::{ComponentGrouping, CpackError, PackageContext, PackageGenerator, error, safe_name};

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
}

impl ArchiveKind {
    /// All archive generators.
    pub const ALL: [Self; 9] = [
        Self::Tgz,
        Self::Txz,
        Self::Tzst,
        Self::Tbz2,
        Self::Tz,
        Self::Tar,
        Self::Zip,
        Self::ZipDeflate,
        Self::ZipStore,
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
            Self::Zip | Self::ZipDeflate | Self::ZipStore => ".zip",
        }
    }

    const fn format(self) -> ArchiveFormat {
        match self {
            Self::Tgz => ArchiveFormat::TarGz,
            Self::Txz => ArchiveFormat::TarXz,
            Self::Tzst => ArchiveFormat::TarZstd,
            Self::Tbz2 => ArchiveFormat::TarBz2,
            Self::Tz => ArchiveFormat::TarZ,
            Self::Tar => ArchiveFormat::Tar,
            Self::Zip | Self::ZipDeflate => ArchiveFormat::Zip,
            Self::ZipStore => ArchiveFormat::ZipStored,
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
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let archive_name = cfg.get(b"CPACK_ARCHIVE_FILE_NAME").unwrap_or(cx.file_name);
        let extension = cfg.get(b"CPACK_ARCHIVE_FILE_EXTENSION").map_or_else(
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
        // The archive API writes each archive with one codec worker.
        if let Some(value) = cfg
            .get(b"CPACK_ARCHIVE_THREADS")
            .or_else(|| cfg.get(b"CPACK_THREADS"))
        {
            level_number::<i32>(value, "CPACK_ARCHIVE_THREADS")?;
        }
        let mtime = cx
            .host
            .env_get(b"SOURCE_DATE_EPOCH")
            .and_then(|value| source_date_epoch(&value));
        let options = CreateOptions {
            format: self.kind.format(),
            mtime,
            compression_level,
            verbose: false,
        };
        let component_groups = component_groups(&cfg);
        let mut outputs = BTreeSet::new();
        if component_groups.is_empty() {
            let (base, inputs) = if cfg.get(b"CPACK_INCLUDE_TOPLEVEL_DIRECTORY").is_none()
                || cfg.on(b"CPACK_INCLUDE_TOPLEVEL_DIRECTORY")
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
            write_archive(cx, &path, &base, &inputs, &options)?;
            outputs.insert(path);
        } else {
            for (group, components) in component_groups {
                let name = component_archive_name(&cfg, archive_name, &group);
                let path = archive_path(cx, &name, &extension)?;
                if !outputs.insert(path.clone()) {
                    return Err(error(format!(
                        "Duplicate CPack archive name: {}",
                        path.display()
                    )));
                }
                let mut roots = components.iter().map(|name| cx.staging_dir.join(name));
                let first = roots
                    .next()
                    .ok_or_else(|| error("CPack component group is empty"))?;
                if components.len() == 1 {
                    write_component_archive(cx, &cfg, &path, &first, &options)?;
                } else {
                    let merged = component_workspace(cx)?;
                    let result = (|| {
                        for root in std::iter::once(first).chain(roots) {
                            crate::source::merge_component(cx.host, &root, &merged)?;
                        }
                        write_component_archive(cx, &cfg, &path, &merged, &options)
                    })();
                    let cleanup = cx.host.remove_dir_all(&merged);
                    result?;
                    cleanup?;
                }
            }
        }
        Ok(outputs.into_iter().collect())
    }
}

fn component_groups(cfg: &CpackConfig) -> BTreeMap<BString, BTreeSet<PathBuf>> {
    let mut groups: BTreeMap<BString, BTreeSet<PathBuf>> = BTreeMap::new();
    let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
    for [_, _, selection, _] in projects.as_chunks::<4>().0 {
        for component in crate::stage::package_components(cfg, selection) {
            let group = crate::stage::package_group(cfg, &component);
            groups
                .entry(group)
                .or_default()
                .insert(crate::stage::component_dir(&component));
        }
    }
    groups
}

fn component_archive_name(cfg: &CpackConfig, base: &[u8], group: &[u8]) -> BString {
    if crate::stage::grouping(cfg) == ComponentGrouping::AllComponentsInOne {
        return base.into();
    }
    let group = BString::from(group);
    let variable = format!(
        "CPACK_ARCHIVE_{}_FILE_NAME",
        BString::from(group.to_ascii_uppercase())
    );
    cfg.get(variable.as_bytes()).map_or_else(
        || {
            format!("{}-{group}", BString::from(base))
                .replace(' ', "-")
                .into()
        },
        BString::from,
    )
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

fn write_component_archive(
    cx: &PackageContext<'_>,
    cfg: &CpackConfig,
    path: &Path,
    root: &Path,
    options: &CreateOptions,
) -> Result<(), CpackError> {
    let inputs = if cfg.on(b"CPACK_COMPONENT_INCLUDE_TOPLEVEL_DIRECTORY") {
        let prefix = safe_name(cx.file_name)?;
        children(cx.host, &root.join(&prefix))?
            .into_iter()
            .map(|name| prefix.join(name))
            .collect()
    } else {
        children(cx.host, root)?
    };
    write_archive(cx, path, root, &inputs, options)
}

fn source_date_epoch(value: &[u8]) -> Option<u64> {
    let epoch: i64 = std::str::from_utf8(value)
        .ok()?
        .trim_start_matches(|ch: char| ch.is_ascii_whitespace() || ch == '\u{b}')
        .parse()
        .ok()?;
    u64::try_from(epoch).ok()
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
    let mut paths: Vec<_> = host
        .read_dir(path)?
        .into_iter()
        .filter(|entry| !entry.name.starts_with(b"._"))
        .map(|entry| to_native(&entry.name))
        .collect();
    paths.sort();
    Ok(paths)
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
) -> Result<(), CpackError> {
    crate::stage::clear_output_file(cx.host, path)?;
    create_archive(cx.host, path, base, inputs, options).map_err(|err| error(err.message))?;
    Ok(())
}
