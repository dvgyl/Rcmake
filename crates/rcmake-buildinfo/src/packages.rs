//! Package evidence: cached `<Pkg>_DIR` entries and literal version hints.
//!
//! The observed outcomes come from a native `rcmake` find trace that the caller
//! decodes and checks, as [`crate::TraceInput`].

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_host::Host;

use crate::reader::{ArtifactReader, ReadLimits};
use crate::snapshot::{CacheRecord, PackageSnapshot};

const HELP_PREFIX: &[u8] = b"The directory containing a CMake configuration file for ";
const VERSION_FILE_LIMIT: u64 = 256 << 10;
/// Most bytes that version hints read across all package directories together.
const HINT_TOTAL_LIMIT: u64 = 16 << 20;
/// Most package directories that version hints read.
const MAX_HINT_PACKAGES: usize = 4096;

/// Collect packages from cache entries and literal version files.
///
/// All directories share one read budget. Return the packages and whether every
/// version hint fit in the budget.
pub(crate) fn from_cache(
    host: &dyn Host,
    cache: &BTreeMap<BString, CacheRecord>,
) -> (BTreeMap<BString, PackageSnapshot>, bool) {
    let mut packages = BTreeMap::new();
    let mut remaining = HINT_TOTAL_LIMIT;
    let mut hinted = 0_usize;
    let mut complete = true;
    for (name, record) in cache {
        let Some(package) = record
            .help
            .strip_prefix(HELP_PREFIX)
            .and_then(|rest| rest.strip_suffix(b"."))
        else {
            continue;
        };
        if name.as_slice() != [package, b"_DIR"].concat().as_slice() {
            continue;
        }
        let missing =
            record.value.ends_with(b"-NOTFOUND") || record.value.as_slice() == b"NOTFOUND";
        let version_hint = if missing {
            None
        } else if hinted >= MAX_HINT_PACKAGES || remaining == 0 {
            complete = false;
            None
        } else {
            hinted += 1;
            version_hint(host, &record.value, package, &mut remaining, &mut complete)
        };
        packages.insert(
            BString::from(package),
            PackageSnapshot {
                cached_dir: Some(record.value.clone()),
                version_hint,
                observed: Vec::new(),
            },
        );
    }
    (packages, complete)
}

fn version_hint(
    host: &dyn Host,
    dir: &[u8],
    package: &[u8],
    remaining: &mut u64,
    complete: &mut bool,
) -> Option<BString> {
    let root = rcmake_host::path::to_native(dir);
    let mut reader = ArtifactReader::new(
        host,
        &root,
        ReadLimits {
            max_file_bytes: VERSION_FILE_LIMIT.min(*remaining),
            max_total_bytes: (3 * VERSION_FILE_LIMIT).min(*remaining),
        },
    )?;
    let lower = package.to_ascii_lowercase();
    let names = [
        [package, b"ConfigVersion.cmake"].concat(),
        [package, b"-config-version.cmake"].concat(),
        [&lower[..], b"-config-version.cmake"].concat(),
    ];
    let mut found = None;
    for name in &names {
        match reader.read(&rcmake_host::path::to_native(name)) {
            Ok(bytes) => {
                found = literal_version(&bytes);
                break;
            }
            Err(crate::ReadError::TooLarge) => {
                *complete = false;
                break;
            }
            Err(_) => {}
        }
    }
    *remaining = remaining.saturating_sub(reader.used());
    found
}

/// Return the version that a version file sets with one top-level literal
/// `set(PACKAGE_VERSION ...)` before any conditional block.
///
/// A conditional or computed first value gives `None`. Later conditional changes, such
/// as an architecture suffix, are ignored, so the result is a hint and not the version
/// that a search accepted.
pub(crate) fn literal_version(text: &[u8]) -> Option<BString> {
    let mut found = None;
    let mut depth = 0_u32;
    let mut seen_block = false;
    for line in text.split(|byte| *byte == b'\n') {
        let line = line.trim_ascii();
        let lower = line.to_ascii_lowercase();
        let command = |name: &[u8]| {
            lower.starts_with(name)
                && lower
                    .get(name.len()..)
                    .is_some_and(|rest| rest.trim_ascii_start().starts_with(b"("))
        };
        if command(b"if") {
            depth += 1;
            seen_block = true;
            continue;
        }
        if command(b"endif") {
            depth = depth.saturating_sub(1);
            continue;
        }
        let Some(rest) = lower
            .strip_prefix(b"set(package_version")
            .and_then(|_| line.get(19..))
        else {
            continue;
        };
        if !rest.first().is_some_and(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        if depth > 0 {
            found.as_ref()?;
            continue;
        }
        if found.is_some() || seen_block {
            return None;
        }
        let value = rest.trim_ascii().strip_suffix(b")")?.trim_ascii();
        let value = value
            .strip_prefix(b"\"")
            .and_then(|value| value.strip_suffix(b"\""))
            .unwrap_or(value);
        if value.is_empty() || value.contains(&b'$') || value.contains(&b' ') {
            return None;
        }
        found = Some(BString::from(value));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    #[test]
    fn version_hints_share_one_read_budget() {
        let host = rcmake_host::PureHost::default();
        let mut cache = BTreeMap::new();
        let version = format!("set(PACKAGE_VERSION 1.0)\n#{}\n", "x".repeat(200 << 10));
        for index in 0..100 {
            let dir = format!("/p/{index}");
            host.seed_file(
                Path::new(&format!("{dir}/Pkg{index}ConfigVersion.cmake")),
                version.as_bytes(),
            )
            .expect("seed");
            cache.insert(
                BString::from(format!("Pkg{index}_DIR")),
                CacheRecord {
                    ty: "PATH".into(),
                    value: dir.into(),
                    help: format!("{}Pkg{index}.", String::from_utf8_lossy(HELP_PREFIX)).into(),
                },
            );
        }
        let (packages, complete) = from_cache(&host, &cache);
        assert_eq!(packages.len(), 100);
        assert!(!complete, "100 files of 200 KiB exceed the 16 MiB budget");
        let hinted = packages
            .values()
            .filter(|package| package.version_hint.is_some())
            .count();
        assert!(hinted > 0 && hinted < 100, "{hinted}");
    }

    #[test]
    fn literal_version_rejects_conditional_and_computed_values() {
        assert_eq!(
            literal_version(b"set(PACKAGE_VERSION \"1.2.3\")\nif(PACKAGE_FIND_VERSION)\nendif()\n"),
            Some("1.2.3".into())
        );
        assert_eq!(literal_version(b"set(PACKAGE_VERSION ${X})\n"), None);
        assert_eq!(
            literal_version(b"if(WIN32)\n  set(PACKAGE_VERSION 1.0)\nelse()\n  set(PACKAGE_VERSION 2.0)\nendif()\n"),
            None
        );
        assert_eq!(
            literal_version(b"set(PACKAGE_VERSION 1.0)\nset(PACKAGE_VERSION 2.0)\n"),
            None
        );
        assert_eq!(
            literal_version(b"set(PACKAGE_VERSION_COMPATIBLE TRUE)\n"),
            None
        );
        let basic = b"set(PACKAGE_VERSION \"3.1.0\")\nif(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION)\n  set(PACKAGE_VERSION_COMPATIBLE FALSE)\nendif()\nif(NOT CMAKE_SIZEOF_VOID_P STREQUAL \"8\")\n  set(PACKAGE_VERSION \"${PACKAGE_VERSION} (64bit)\")\nendif()\n";
        assert_eq!(literal_version(basic), Some("3.1.0".into()));
    }
}
