//! Build Debian packages from the bundled CPackDeb module's metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use rcmake_archive::{ArchiveFormat, CreateOptions, TarCompression, create_archive};
use rcmake_base::{BString, hash::HashAlgo, parse_source_date_epoch};
use rcmake_eval::Interp;
use rcmake_host::path::{from_native, from_native_cmake, to_native};
use rcmake_host::{CopyOptions, FileKind, Follow, Host, Stream, WriteMode};

use super::ScratchDir;
use super::linux::{passes, verify_path};
use crate::config::{CpackConfig, absolute};
use crate::{ComponentMembers, CpackError, PackageContext, PackageGenerator, error, safe_name};

pub(super) struct Deb;

impl PackageGenerator for Deb {
    fn name(&self) -> &'static str {
        "DEB"
    }
    fn extension(&self) -> &'static str {
        ".deb"
    }

    fn package(&self, _: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        Err(error("CPackDeb requires a CPack interpreter"))
    }

    fn package_with_interp(
        &self,
        cx: &PackageContext<'_>,
        _: &ComponentMembers,
        interp: &mut Interp,
    ) -> Result<Vec<PathBuf>, CpackError> {
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let mut packages = Vec::new();
        let mut names = BTreeSet::new();
        for pass in passes(&cfg, cx.grouping) {
            let vars = pass.module(
                interp,
                cx,
                "DEB",
                pass.name.as_deref().map(Vec::as_slice),
                &[],
            )?;
            for debug in [false, true] {
                if debug
                    && (!vars.on(b"GEN_CPACK_DEBIAN_DEBUGINFO_PACKAGE")
                        || vars.get(b"GEN_DBGSYMDIR").is_none())
                {
                    continue;
                }
                let (file, dir) = if debug {
                    (
                        b"GEN_CPACK_DBGSYM_OUTPUT_FILE_NAME".as_slice(),
                        b"GEN_DBGSYMDIR".as_slice(),
                    )
                } else {
                    (
                        b"GEN_CPACK_OUTPUT_FILE_NAME".as_slice(),
                        b"GEN_WDIR".as_slice(),
                    )
                };
                let name = safe_name(required(&vars, file)?)?;
                if !names.insert(name.clone()) {
                    return Err(error(format!(
                        "Duplicate Debian package: {}",
                        name.display()
                    )));
                }
                let path = cx.output_dir.join(name);
                let payload = to_native(required(&vars, dir)?);
                verify_path(cx.host, cx.output_dir, &payload, FileKind::Dir)?;
                build(cx, &vars, &payload, &path, debug)?;
                packages.push(path);
            }
        }
        Ok(packages)
    }
}

fn required<'a>(cfg: &'a CpackConfig, key: &[u8]) -> Result<&'a [u8], CpackError> {
    cfg.get(key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error(format!("CPackDeb did not set {}", BString::from(key))))
}

fn control(cfg: &CpackConfig, debug: bool) -> Result<BTreeMap<&'static str, BString>, CpackError> {
    let mut fields = BTreeMap::new();
    for (name, var) in [
        ("Package", "NAME"),
        ("Version", "VERSION"),
        ("Section", "SECTION"),
        ("Priority", "PRIORITY"),
        ("Architecture", "ARCHITECTURE"),
        ("Maintainer", "MAINTAINER"),
        ("Description", "DESCRIPTION"),
        ("Source", "SOURCE"),
        ("Depends", "DEPENDS"),
        ("Recommends", "RECOMMENDS"),
        ("Suggests", "SUGGESTS"),
        ("Homepage", "HOMEPAGE"),
        ("Pre-Depends", "PREDEPENDS"),
        ("Enhances", "ENHANCES"),
        ("Breaks", "BREAKS"),
        ("Conflicts", "CONFLICTS"),
        ("Provides", "PROVIDES"),
        ("Replaces", "REPLACES"),
        ("Multi-Arch", "MULTIARCH"),
    ] {
        if debug
            && !matches!(
                name,
                "Package" | "Version" | "Architecture" | "Maintainer" | "Source"
            )
        {
            continue;
        }
        let key = format!("GEN_CPACK_DEBIAN_PACKAGE_{var}");
        if let Some(value) = cfg.get(key.as_bytes()).filter(|value| !value.is_empty()) {
            if name == "Multi-Arch" && !matches!(value, b"same" | b"foreign" | b"allowed") {
                return Err(error(format!(
                    "Error: invalid value for Multi-Arch: {}. Valid values are: same, foreign, allowed",
                    BString::from(value)
                )));
            }
            fields.insert(name, BString::from(value));
        }
    }
    let raw_name = required(cfg, b"GEN_CPACK_DEBIAN_PACKAGE_NAME")?;
    let name = BString::from(raw_name.to_ascii_lowercase());
    if debug {
        let version = BString::from(required(cfg, b"GEN_CPACK_DEBIAN_PACKAGE_VERSION")?);
        fields.extend([
            ("Package", format!("{name}-dbgsym").into()),
            ("Package-Type", "ddeb".into()),
            ("Auto-Built-Package", "debug-symbols".into()),
            (
                "Depends",
                format!("{} (= {version})", BString::from(raw_name)).into(),
            ),
            ("Section", "debug".into()),
            ("Priority", "optional".into()),
            ("Description", format!("debug symbols for {name}").into()),
        ]);
        if let Some(ids) = cfg.get(b"GEN_BUILD_IDS").filter(|ids| !ids.is_empty()) {
            fields.insert("Build-Ids", ids.into());
        }
    } else {
        fields.insert("Package", name);
    }
    Ok(fields)
}

fn compression(name: &[u8]) -> Result<(Option<TarCompression>, &'static str), CpackError> {
    use TarCompression::*;
    Ok(match name {
        b"gzip" => (Some(Gzip), ".gz"),
        b"xz" => (Some(Xz), ".xz"),
        b"zstd" => (Some(Zstd), ".zst"),
        b"bzip2" => (Some(Bzip2), ".bz2"),
        b"lzma" => (Some(Lzma), ".lzma"),
        b"none" => (None, ""),
        _ => {
            return Err(error(format!(
                "Unsupported Debian compression: {}",
                BString::from(name)
            )));
        }
    })
}

fn build(
    cx: &PackageContext<'_>,
    vars: &CpackConfig,
    payload: &Path,
    output: &Path,
    debug: bool,
) -> Result<(), CpackError> {
    let host = cx.host;
    let work = ScratchDir::for_generator(host, cx.output_dir, "deb")?;
    let controls = work.path.join("control");
    host.create_dir_all(&controls)?;
    let (data_filter, data_suffix) =
        compression(required(vars, b"GEN_CPACK_DEBIAN_COMPRESSION_TYPE")?)?;
    let (control_filter, control_suffix) = if matches!(
        data_filter,
        Some(TarCompression::Lzma | TarCompression::Bzip2)
    ) {
        (Some(TarCompression::Gzip), ".gz")
    } else {
        (data_filter, data_suffix)
    };
    let epoch = host
        .env_get(b"SOURCE_DATE_EPOCH")
        .and_then(|raw| parse_source_date_epoch(&raw))
        .and_then(|n| u64::try_from(n).ok());
    let level = number::<u32>(vars, b"GEN_CPACK_DEBIAN_COMPRESSION_LEVEL", 0)?;
    let threads = number::<i32>(vars, b"CPACK_THREADS", 1)?.unsigned_abs();
    let tar = |file: &Path, root: &Path, paths: &[PathBuf], filter: Option<TarCompression>| {
        let options = CreateOptions {
            format: filter.map_or(ArchiveFormat::GnuTar, ArchiveFormat::GnuTarCompressed),
            mtime: epoch,
            compression_level: Some(level),
            verbose: false,
        };
        create_archive(
            host,
            file,
            root,
            paths,
            options
                .with_deterministic_order()
                .skipping_apple_double()
                .with_root_tar_entries()
                .with_threads(threads),
        )
        .map_err(|err| error(err.to_string()))
    };
    let paths = crate::stage::component_files::snapshot(host, payload)?;
    let data_inputs = paths
        .iter()
        .filter(|p| p.components().count() == 1)
        .cloned()
        .collect::<Vec<_>>();
    let data_name = format!("data.tar{data_suffix}");
    let data_tar = work.path.join(&data_name);
    tar(&data_tar, payload, &data_inputs, data_filter)?;
    let mut size = 0_u64;
    let mut sums = BString::default();
    for relative in paths {
        let path = payload.join(&relative);
        let metadata = host.metadata(&path, Follow::No)?;
        let len = if metadata.kind == FileKind::Symlink {
            host.metadata(&path, Follow::Yes)
                .map_or(0, |target| target.len)
        } else {
            metadata.len
        };
        size = size
            .checked_add(len)
            .ok_or_else(|| error("Debian installed size exceeds the size limit"))?;
        if metadata.kind == FileKind::File {
            let mut hasher = HashAlgo::Md5.hasher();
            host.read_chunks(&path, &mut |chunk| hasher.update(chunk))?;
            sums.extend_from_slice(hasher.finish_hex().as_bytes());
            sums.extend_from_slice(b"  ");
            sums.extend_from_slice(&from_native_cmake(&relative));
            sums.push(b'\n');
        }
    }
    let mut text = BString::default();
    for (key, value) in control(vars, debug)? {
        text.extend_from_slice(key.as_bytes());
        text.extend_from_slice(b": ");
        text.extend_from_slice(&value);
        text.push(b'\n');
    }
    text.extend_from_slice(format!("Installed-Size: {}\n", size.div_ceil(1024)).as_bytes());
    let mut control_inputs = Vec::new();
    for (name, contents) in [("md5sums", sums), ("control", text)] {
        write_control(host, &controls, &mut control_inputs, name, &contents, 0o644)?;
    }
    if !debug {
        extras(host, vars, &controls, &mut control_inputs)?;
    }
    let control_name = format!("control.tar{control_suffix}");
    let control_tar = work.path.join(&control_name);
    tar(&control_tar, &controls, &control_inputs, control_filter)?;
    let scratch = work.path.join("package");
    host.write(&scratch, b"!<arch>\n", WriteMode::CreateNew)?;
    let now = epoch.unwrap_or_else(|| u64::try_from(host.now().secs).unwrap_or_default());
    member(host, &scratch, "debian-binary", now, 4, &mut &b"2.0\n"[..])?;
    for (name, tar) in [(control_name, control_tar), (data_name, data_tar)] {
        let len = host.metadata(&tar, Follow::No)?.len;
        member(host, &scratch, &name, now, len, &mut host.open_read(&tar)?)?;
    }
    host.rename(&scratch, output)?;
    Ok(())
}

fn number<T: std::str::FromStr>(
    cfg: &CpackConfig,
    key: &[u8],
    default: T,
) -> Result<T, CpackError> {
    cfg.get(key)
        .filter(|value| !value.is_empty())
        .map_or(Ok(default), |value| {
            let value = BString::from(value);
            value
                .to_string()
                .parse()
                .map_err(|_| error(format!("Invalid {}: {value}", BString::from(key))))
        })
}

fn write_control(
    host: &dyn Host,
    root: &Path,
    paths: &mut Vec<PathBuf>,
    name: &str,
    contents: &[u8],
    mode: u32,
) -> Result<(), CpackError> {
    let path = root.join(name);
    host.write(&path, contents, WriteMode::CreateNew)?;
    host.set_permissions(&path, mode)?;
    paths.push(name.into());
    Ok(())
}

fn extras(
    host: &dyn Host,
    cfg: &CpackConfig,
    root: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), CpackError> {
    if cfg.on(b"CPACK_DEBIAN_PACKAGE_GENERATE_SHLIBS")
        && let Some(shlibs) = cfg
            .get(b"GEN_CPACK_DEBIAN_PACKAGE_SHLIBS")
            .filter(|value| !value.is_empty())
    {
        write_control(
            host,
            root,
            paths,
            "shlibs",
            &[shlibs, b"\n"].concat(),
            0o644,
        )?;
    }
    for (key, name, action) in [
        (
            b"GEN_CPACK_DEBIAN_GENERATE_POSTINST".as_slice(),
            "postinst",
            "configure",
        ),
        (
            b"GEN_CPACK_DEBIAN_GENERATE_POSTRM".as_slice(),
            "postrm",
            "remove",
        ),
    ] {
        if cfg.on(key) {
            let script = format!(
                "#!/bin/sh\n\nset -e\n\nif [ \"$1\" = \"{action}\" ]; then\n\tldconfig\nfi\n"
            );
            write_control(host, root, paths, name, script.as_bytes(), 0o755)?;
        }
    }
    if cfg.on(b"GEN_CPACK_DEBIAN_GENERATE_LDCONFIG_TRIGGERS") {
        write_control(
            host,
            root,
            paths,
            "triggers",
            b"activate-noawait ldconfig\n",
            0o644,
        )?;
    }
    let strict = cfg
        .get(b"GEN_CPACK_DEBIAN_PACKAGE_CONTROL_STRICT_PERMISSION")
        .is_some_and(|value| {
            !value.is_empty() && value != b"NOTFOUND" && !value.ends_with(b"-NOTFOUND")
        });
    for file in cfg.list(b"GEN_CPACK_DEBIAN_PACKAGE_CONTROL_EXTRA") {
        let source = absolute(&host.initial_cwd(), &file);
        if !host
            .metadata(&source, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            host.write_out(
                Stream::Stderr,
                format!("CPack Warning: Cannot copy Debian control file: {file}\n").as_bytes(),
            );
            continue;
        }
        let name = source
            .file_name()
            .ok_or_else(|| error("Missing Debian control file name"))?;
        let relative = safe_name(&from_native(Path::new(name)))?;
        let dest = root.join(&relative);
        host.copy_file(
            &source,
            &dest,
            CopyOptions {
                preserve_permissions: true,
                preserve_mtime: true,
            },
        )?;
        if strict {
            let executable = matches!(
                name.to_str(),
                Some("config" | "postinst" | "postrm" | "preinst" | "prerm" | "triggers")
            );
            host.set_permissions(&dest, if executable { 0o755 } else { 0o644 })?;
        }
        if !paths.contains(&relative) {
            paths.push(relative);
        }
    }
    Ok(())
}

fn member(
    host: &dyn Host,
    ar: &Path,
    name: &str,
    time: u64,
    len: u64,
    data: &mut dyn Read,
) -> Result<(), CpackError> {
    let header = format!(
        "{name:<16}{time:<12}{:<6}{:<6}{:<8}{len:<10}`\n",
        0, 0, "100644"
    );
    if header.len() != 60 {
        return Err(error("Debian ar member exceeds a header field limit"));
    }
    host.write(ar, header.as_bytes(), WriteMode::Append)?;
    host.write_from(ar, WriteMode::Append, data, len)?;
    if !len.is_multiple_of(2) {
        host.write(ar, b"\n", WriteMode::Append)?;
    }
    Ok(())
}
