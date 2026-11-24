//! Archive generators: `TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, and `ZIP`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_archive::{ArchiveFormat, CreateOptions, create_archive};
use rcmake_base::BString;
use rcmake_base::parse_source_date_epoch;
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
        let archive_name = cfg
            .get(b"CPACK_ARCHIVE_FILE_NAME")
            .filter(|name| !name.is_empty())
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
        // The archive API writes each archive with one codec worker.
        if let Some(value) = cfg
            .get(b"CPACK_ARCHIVE_THREADS")
            .or_else(|| cfg.get(b"CPACK_THREADS"))
        {
            level_number::<i32>(value, "CPACK_ARCHIVE_THREADS")?;
        }
        // CPack uses zero for times before the Unix epoch.
        let mtime = cx
            .host
            .env_get(b"SOURCE_DATE_EPOCH")
            .and_then(|value| parse_source_date_epoch(&value))
            .map(|epoch| u64::try_from(epoch).unwrap_or_default());
        let options = CreateOptions {
            format: self.kind.format(),
            mtime,
            compression_level,
            verbose: false,
        };
        let archives = component_archives(&cfg, archive_name, self.name());
        if archives.is_empty() {
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
            write_component_archive(cx, path, &prefix, &archive.components, &options)?;
        }
        Ok(paths)
    }
}

struct ComponentArchive {
    name: BString,
    components: Vec<BString>,
}

fn component_archives(cfg: &CpackConfig, base: &[u8], generator: &str) -> Vec<ComponentArchive> {
    let grouping = crate::stage::grouping(cfg);
    let mut seen = BTreeSet::new();
    let mut groups: BTreeMap<BString, Vec<BString>> = BTreeMap::new();
    let mut singles = BTreeSet::new();
    let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
    for [_, _, selection, _] in projects.as_chunks::<4>().0 {
        for component in crate::stage::package_components(cfg, selection) {
            if grouping == ComponentGrouping::OnePerGroup
                && let Some(group) = crate::stage::component_group(cfg, &component)
            {
                if seen.insert(component.clone()) {
                    groups.entry(group.into()).or_default().push(component);
                }
            } else {
                singles.insert(component);
            }
        }
    }
    if grouping == ComponentGrouping::AllComponentsInOne && !singles.is_empty() {
        return vec![ComponentArchive {
            name: base.into(),
            components: singles.into_iter().collect(),
        }];
    }
    groups
        .into_iter()
        .map(|(group, components)| ComponentArchive {
            name: component_archive_name(cfg, base, &group, generator, "COMPONENT_GROUP"),
            components,
        })
        .chain(singles.into_iter().map(|component| ComponentArchive {
            name: component_archive_name(cfg, base, &component, generator, "COMPONENT"),
            components: vec![component],
        }))
        .collect()
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
    if let Some(name) = cfg.get(variable.as_bytes()).filter(|name| !name.is_empty()) {
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
    if let Some(install_prefix) = cfg.get(b"CPACK_PACKAGING_INSTALL_PREFIX") {
        prefix.push(crate::stage::relative_install_path(install_prefix)?);
    }
    Ok(prefix)
}

fn write_component_archive(
    cx: &PackageContext<'_>,
    path: &Path,
    prefix: &Path,
    components: &[BString],
    options: &CreateOptions,
) -> Result<(), CpackError> {
    let mut roots = components
        .iter()
        .map(|name| cx.staging_dir.join(crate::stage::component_dir(name)));
    let first = roots
        .next()
        .ok_or_else(|| error("CPack component group is empty"))?;
    let component_inputs = |root: &Path| {
        children(cx.host, &root.join(prefix))
            .map(|files| files.into_iter().map(|file| prefix.join(file)))
    };
    if roots.len() == 0 {
        let inputs = component_inputs(&first)?.collect::<Vec<_>>();
        return write_archive(cx, path, &first, &inputs, options);
    }
    let merged = component_workspace(cx)?;
    let result: Result<(), CpackError> = (|| {
        let mut seen = BTreeSet::new();
        let mut inputs = Vec::new();
        for root in std::iter::once(first).chain(roots) {
            crate::source::merge_component(cx.host, &root, &merged)?;
            inputs.extend(component_inputs(&root)?.filter(|file| seen.insert(file.clone())));
        }
        write_archive(cx, path, &merged, &inputs, options)
    })();
    let cleanup = cx.host.remove_dir_all(&merged);
    result?;
    cleanup?;
    Ok(())
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
) -> Result<(), CpackError> {
    crate::stage::clear_output_file(cx.host, path)?;
    create_archive(cx.host, path, base, inputs, options).map_err(|err| error(err.message))?;
    Ok(())
}
