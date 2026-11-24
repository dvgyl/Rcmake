use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_host::{FileTime, path::from_native_cmake as from_native};

use super::{
    DATA_LIMIT, ITEM_LIMIT, Result, ToolCtx, info, parse, read, resolve, timestamp, touch,
    write_changed,
};

pub(super) fn settings(cx: &ToolCtx<'_>, path: &Path) -> BTreeMap<BString, String> {
    let contents = read(cx, path, DATA_LIMIT).unwrap_or_default();
    lines(&contents)
        .filter_map(|line| {
            let (key, digest) = split(line, b':')?;
            let digest = std::str::from_utf8(digest).ok()?;
            (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| (key.into(), digest.to_owned()))
        })
        .collect()
}

pub(super) fn settings_hash<'a>(fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hash = HashAlgo::Sha256.hasher();
    for field in fields {
        hash.update(field);
        hash.update(b";");
    }
    hash.finish_hex()
}

pub(super) fn write_settings<const N: usize>(
    cx: &ToolCtx<'_>,
    path: &Path,
    settings: [(&str, Option<String>); N],
) -> Result<()> {
    let mut contents = String::new();
    for (name, hash) in settings {
        if let Some(hash) = hash {
            contents.push_str(&format!("{name}:{hash}\n"));
        }
    }
    write_changed(cx, path, contents.as_bytes())?;
    Ok(())
}

#[derive(Default)]
pub(super) struct MocScan {
    pub(super) macro_name: BString,
    pub(super) underscore: Vec<BString>,
    pub(super) dot: Vec<BString>,
    pub(super) dependencies: Vec<BString>,
}

#[derive(Default)]
pub(super) struct UicScan {
    pub(super) includes: Vec<BString>,
    pub(super) dependencies: Vec<BString>,
}

#[derive(Default)]
pub(super) struct Scan {
    pub(super) moc: MocScan,
    pub(super) uic: UicScan,
}

pub(super) struct ScanTimes {
    pub(super) moc: Option<FileTime>,
    pub(super) uic: Option<FileTime>,
    pub(super) metadata: FileTime,
}

pub(super) struct ParseCache {
    cached: BTreeMap<PathBuf, Scan>,
    active: BTreeMap<PathBuf, Scan>,
    time: Option<FileTime>,
    rescanned: bool,
}

impl ParseCache {
    pub(super) fn read(cx: &ToolCtx<'_>, path: &Path) -> Self {
        let contents = read(cx, path, DATA_LIMIT);
        let time = contents.as_ref().ok().and_then(|_| timestamp(cx, path));
        let mut cached = BTreeMap::<PathBuf, Scan>::new();
        if let Ok(contents) = contents {
            let mut current = None;
            for line in lines(&contents).take(ITEM_LIMIT) {
                if line.is_empty() || line.starts_with(b"#") {
                    continue;
                }
                if !line.starts_with(b" ") {
                    current = if line.contains(&0) {
                        None
                    } else {
                        Some(resolve(cx.cwd, line))
                    };
                    if let Some(file) = &current {
                        cached.entry(file.clone()).or_default();
                    }
                    continue;
                }
                let Some(scan) = current.as_ref().and_then(|file| cached.get_mut(file)) else {
                    continue;
                };
                let Some((key, value)) = split(line, b':') else {
                    continue;
                };
                if value.contains(&0) {
                    continue;
                }
                match key {
                    b" mmc" => scan.moc.macro_name = value.into(),
                    b" miu" => scan.moc.underscore.push(value.into()),
                    b" mid" => scan.moc.dot.push(value.into()),
                    b" mdp" => scan.moc.dependencies.push(value.into()),
                    b" uic" => scan.uic.includes.push(value.into()),
                    b" udp" => scan.uic.dependencies.push(value.into()),
                    _ => {}
                }
            }
        }
        Self {
            cached,
            active: BTreeMap::new(),
            time,
            rescanned: false,
        }
    }

    pub(super) fn scan(
        &mut self,
        cx: &ToolCtx<'_>,
        row: &info::ScanRow,
        time: FileTime,
        info: &info::Autogen,
        stamps: &ScanTimes,
    ) -> Result<()> {
        if self.active.contains_key(&row.path) {
            return Ok(());
        }
        let old = self.cached.remove(&row.path);
        let reusable = self.time.filter(|&cache_time| {
            old.is_some() && cache_time >= stamps.metadata && cache_time >= time
        });
        let moc = info
            .moc
            .as_ref()
            .filter(|moc| row.moc && !moc.skip.contains(&row.path));
        let uic = info
            .uic
            .as_ref()
            .filter(|uic| row.uic && !uic.skip.contains(&row.path));
        let refresh_moc = moc.is_some() && !current(reusable, stamps.moc);
        let refresh_uic = uic.is_some() && !current(reusable, stamps.uic);
        let new = if refresh_moc || refresh_uic {
            self.rescanned = true;
            let contents = read(cx, &row.path, DATA_LIMIT)?;
            parse::scan(
                &contents,
                moc.filter(|_| refresh_moc),
                refresh_uic,
                row.kind,
                info.qt_version,
            )?
        } else {
            Scan::default()
        };
        let mut scan = old.unwrap_or_default();
        if refresh_moc || moc.is_none() {
            scan.moc = new.moc;
        }
        if refresh_uic || uic.is_none() {
            scan.uic = new.uic;
        }
        self.active.insert(row.path.clone(), scan);
        Ok(())
    }

    pub(super) fn get(&self, path: &Path) -> Option<&Scan> {
        self.active.get(path)
    }

    pub(super) fn get_or_default(&mut self, path: &Path) -> &mut Scan {
        self.active.entry(path.to_path_buf()).or_default()
    }

    pub(super) fn write(&self, cx: &ToolCtx<'_>, path: &Path) -> Result<()> {
        let mut contents = b"# Generated by CMake. Changes will be overwritten.\n".to_vec();
        for (file, scan) in &self.active {
            let name = from_native(file);
            contents.extend_from_slice(&name);
            contents.push(b'\n');
            if !scan.moc.macro_name.is_empty() {
                append(&mut contents, b" mmc:", &scan.moc.macro_name)?;
            }
            for (prefix, entries) in [
                (b" miu:", &scan.moc.underscore),
                (b" mid:", &scan.moc.dot),
                (b" mdp:", &scan.moc.dependencies),
                (b" uic:", &scan.uic.includes),
                (b" udp:", &scan.uic.dependencies),
            ] {
                for entry in entries {
                    append(&mut contents, prefix, entry)?;
                }
            }
            if contents.len() as u64 > DATA_LIMIT {
                return Err("The parse cache exceeds the size limit.".into());
            }
        }
        if !write_changed(cx, path, &contents)? && self.rescanned {
            touch(cx, path)?;
        }
        Ok(())
    }
}

fn current(cache: Option<FileTime>, dependency: Option<FileTime>) -> bool {
    matches!((cache, dependency), (Some(cache), Some(dependency)) if cache >= dependency)
}

fn lines(contents: &[u8]) -> impl Iterator<Item = &[u8]> {
    contents
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

pub(super) fn split(value: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
    let index = value.iter().position(|&byte| byte == delimiter)?;
    Some((value.get(..index)?, value.get(index + 1..)?))
}

fn append(output: &mut Vec<u8>, prefix: &[u8], value: &[u8]) -> Result<()> {
    if (output.len() + prefix.len() + value.len() + 1) as u64 > DATA_LIMIT {
        return Err("The parse cache exceeds the size limit.".into());
    }
    output.extend_from_slice(prefix);
    output.extend_from_slice(value);
    output.push(b'\n');
    Ok(())
}
