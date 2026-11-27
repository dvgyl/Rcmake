//! Package evidence: cached `<Pkg>_DIR` entries, literal version hints, and the
//! observed outcomes of a current native `rcmake` find trace.

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_host::{FileKind, Follow, Host};
use rcmake_json::Value;

use crate::reader::{ArtifactReader, ReadLimits};
use crate::snapshot::{CacheRecord, PackageObservation, PackageSnapshot};

const HELP_PREFIX: &[u8] = b"The directory containing a CMake configuration file for ";
const TRACE: &str = "CMakeFiles/rcmake/find-trace.v1.json";
const VERSION_FILE_LIMIT: u64 = 256 << 10;

/// Collect packages from cache entries and literal version files.
pub(crate) fn from_cache(
    host: &dyn Host,
    cache: &BTreeMap<BString, CacheRecord>,
) -> BTreeMap<BString, PackageSnapshot> {
    let mut packages = BTreeMap::new();
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
        let version_hint = (!missing)
            .then(|| version_hint(host, &record.value, package))
            .flatten();
        packages.insert(
            BString::from(package),
            PackageSnapshot {
                cached_dir: Some(record.value.clone()),
                version_hint,
                observed: Vec::new(),
            },
        );
    }
    packages
}

fn version_hint(host: &dyn Host, dir: &[u8], package: &[u8]) -> Option<BString> {
    let root = rcmake_host::path::to_native(dir);
    let mut reader = ArtifactReader::new(
        host,
        &root,
        ReadLimits {
            max_file_bytes: VERSION_FILE_LIMIT,
            max_total_bytes: 3 * VERSION_FILE_LIMIT,
        },
    )?;
    let lower = package.to_ascii_lowercase();
    let names = [
        [package, b"ConfigVersion.cmake"].concat(),
        [package, b"-config-version.cmake"].concat(),
        [&lower[..], b"-config-version.cmake"].concat(),
    ];
    names.iter().find_map(|name| {
        let bytes = reader
            .read(Path::new(&String::from_utf8_lossy(name).into_owned()))
            .ok()?;
        literal_version(&bytes)
    })
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

/// Observations from a native trace whose identity and stamps match the tree.
pub(crate) struct TraceEvidence {
    pub(crate) calls: Vec<(BString, PackageObservation)>,
    pub(crate) complete: bool,
}

fn stamp_matches(host: &dyn Host, stamp: &Value) -> bool {
    let (Some(path), Some(size), Some(mtime)) = (
        stamp.get("path").and_then(Value::as_bytes),
        stamp.get("size").and_then(Value::as_u64),
        stamp.get("mtime_ns").and_then(Value::as_i64),
    ) else {
        return false;
    };
    host.metadata(&rcmake_host::path::to_native(path), Follow::Yes)
        .is_ok_and(|meta| {
            meta.kind == FileKind::File
                && meta.len == size
                && meta
                    .mtime
                    .secs
                    .saturating_mul(1_000_000_000)
                    .saturating_add(i64::from(meta.mtime.nanos))
                    == mtime
        })
}

/// Read the native trace. Return `Err` with a note when it exists but is not current.
pub(crate) fn trace(
    reader: &mut ArtifactReader<'_>,
    build: &[u8],
) -> Option<Result<TraceEvidence, BString>> {
    let value = reader.json(Path::new(TRACE)).ok()?;
    if value.get("schema").and_then(Value::as_str) != Some("rcmake-find-trace-v1") {
        return None;
    }
    let host = reader.host();
    let same_build = value
        .get("build")
        .and_then(Value::as_bytes)
        .is_some_and(|recorded| {
            Lexical::new(recorded).normalize() == Lexical::new(build).normalize()
        });
    let current = same_build
        && value
            .get("cache")
            .is_some_and(|cache| stamp_matches(host, cache))
        && value
            .get("inputs")
            .and_then(Value::as_array)
            .is_some_and(|inputs| inputs.iter().all(|input| stamp_matches(host, input)));
    if !current {
        return Some(Err(
            "The rcmake find trace is from a past run. Package outcomes are unknown.".into(),
        ));
    }
    let complete = value.get("completed").and_then(Value::as_bool) == Some(true)
        && value.get("omitted_calls").and_then(Value::as_u64) == Some(0);
    let calls = value
        .get("calls")
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|call| {
            let bytes = |value: Option<&Value>| value.and_then(Value::as_bytes).map(BString::from);
            Some((
                bytes(call.get("name"))?,
                PackageObservation {
                    outcome: bytes(call.get("outcome"))?,
                    version: bytes(call.get("version")),
                    selected: bytes(
                        call.get("selected")
                            .and_then(|selected| selected.get("file")),
                    ),
                },
            ))
        })
        .collect();
    Some(Ok(TraceEvidence { calls, complete }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
