//! Portable single-file JSON codec for cross-machine `rcmake diff-config` snapshots (ADR-051).
//!
//! Snapshots are always normalized before serialization (`${build}`, `${source}`, `${home}`,
//! forward-slash separators), intern [`CompileSettings`] into a deduplicated table, encode
//! arbitrary [`BString`] bytes losslessly without losing non-UTF-8 sequences, preserve
//! per-section [`Evidence`] verbatim, and emit canonical deterministic UTF-8 JSON with
//! sorted keys, LF line endings, and no wall-clock timestamps.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, ReadRange, WriteMode};
use rcmake_json::{Object, ParseBudget, ReadOptions, Value};

use crate::diff::{cache_set_includes, normalize};
use crate::snapshot::{
    Attribution, BuildSnapshot, CacheRecord, CacheSet, CompileSettings, DataSource, Evidence,
    IncludeDir, LinkFragment, PackageObservation, PackageSnapshot, Producer, Section, SnapshotHost,
    SourceInvocations, TargetSnapshot, ToolchainSnapshot,
};

/// Marker `"kind"` value identifying a portable `rcmake diff-config` snapshot file.
pub const SNAPSHOT_KIND: &str = "rcmake-config-snapshot";

/// Supported snapshot format major version.
pub const SNAPSHOT_VERSION_MAJOR: u32 = 1;

/// Current snapshot format minor version emitted by this binary.
pub const SNAPSHOT_VERSION_MINOR: u32 = 0;

/// Maximum accepted portable snapshot file size in bytes (64 MiB).
pub const MAX_SNAPSHOT_BYTES: usize = 64 << 20;

/// Maximum JSON container nesting depth accepted when decoding a portable snapshot.
pub const MAX_SNAPSHOT_JSON_DEPTH: usize = 32;

const MAX_COMPILE_SETTINGS: usize = 500_000;
const MAX_TARGETS: usize = 100_000;
const MAX_CONFIGS: usize = 1_024;
const MAX_SOURCES_PER_TARGET: usize = 200_000;
const MAX_TOTAL_SOURCES: usize = 1_000_000;
const MAX_INVOCATIONS_PER_SOURCE: usize = 64;
const MAX_LIST_ITEMS: usize = 200_000;
const MAX_TOOLCHAINS: usize = 256;
const MAX_PACKAGES: usize = 100_000;
const MAX_OBSERVATIONS_PER_PACKAGE: usize = 4_096;
const MAX_CACHE_ENTRIES: usize = 500_000;
const MAX_WARNINGS: usize = 100_000;
const MAX_HEX_BYTES: usize = 16 << 20;
const MAX_EXPANDED_COMPILE_BYTES: usize = 256 << 20;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Error returned when saving, loading, encoding, or decoding a portable snapshot fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// Filesystem read or write through [`Host`] failed.
    Io(String),
    /// The snapshot exceeds [`MAX_SNAPSHOT_BYTES`].
    TooLarge {
        /// Maximum allowed byte length.
        limit: usize,
    },
    /// The input is not valid JSON or exceeded the parse budget.
    Json(String),
    /// The input is valid JSON, but does not carry `"kind": "rcmake-config-snapshot"`.
    NotASnapshot,
    /// The snapshot uses an unsupported major format version.
    UnsupportedVersion {
        /// Snapshot major version.
        major: u32,
        /// Snapshot minor version.
        minor: u32,
    },
    /// The snapshot JSON failed schema validation or exceeded entry count limits.
    InvalidSchema(String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => f.write_str(message),
            Self::TooLarge { limit } => {
                write!(f, "Snapshot exceeds the maximum size of {limit} bytes.")
            }
            Self::Json(message) => write!(f, "Snapshot is not valid JSON: {message}"),
            Self::NotASnapshot => write!(
                f,
                "File is not a portable snapshot (expected \"kind\": \"{SNAPSHOT_KIND}\")."
            ),
            Self::UnsupportedVersion { major, minor } => write!(
                f,
                "Unsupported snapshot format version {major}.{minor}. This rcmake supports major version {SNAPSHOT_VERSION_MAJOR}."
            ),
            Self::InvalidSchema(message) => write!(f, "Invalid snapshot schema: {message}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Options controlling how a [`BuildSnapshot`] is prepared for portable serialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveOptions {
    /// Cache entries retained in the saved snapshot. Defaults to [`CacheSet::Key`] so
    /// secret-like cache variables are not persisted unless `--cache all` is chosen.
    pub cache: CacheSet,
}

fn snapshot_parse_budget(max_bytes: usize) -> ParseBudget {
    let mut budget = ParseBudget::artifact(max_bytes);
    budget.max_depth = MAX_SNAPSHOT_JSON_DEPTH;
    budget
}

fn snapshot_read_options() -> ReadOptions {
    ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: false,
    }
}

/// Normalize `snapshot` and apply `options` to produce the exact [`BuildSnapshot`] state
/// stored in a portable snapshot file.
#[must_use]
pub fn prepare_for_save(snapshot: &BuildSnapshot, options: &SaveOptions) -> BuildSnapshot {
    let mut copy = snapshot.clone();
    normalize(&mut copy);
    match options.cache {
        CacheSet::None => {
            copy.cache.clear();
            copy.saved_cache_set = Some(CacheSet::None);
            if copy.evidence(Section::Cache).comparable() {
                copy.evidence.insert(
                    Section::Cache,
                    Evidence::Unavailable(
                        "Cache entries were excluded when this snapshot was saved (--cache none)."
                            .into(),
                    ),
                );
            }
        }
        CacheSet::Key => {
            copy.cache
                .retain(|name, record| cache_set_includes(CacheSet::Key, name, record));
            if copy.saved_cache_set != Some(CacheSet::None) {
                copy.saved_cache_set = Some(CacheSet::Key);
            }
        }
        CacheSet::All => {
            copy.cache
                .retain(|name, record| cache_set_includes(CacheSet::All, name, record));
            copy.saved_cache_set = copy.saved_cache_set.or(Some(CacheSet::All));
        }
    }
    copy
}

fn hex_encode(bytes: &[u8]) -> BString {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        let hi = usize::from(byte >> 4);
        let lo = usize::from(byte & 0x0f);
        if let (Some(&h), Some(&l)) = (DIGITS.get(hi), DIGITS.get(lo)) {
            out.push(h);
            out.push(l);
        }
    }
    BString::from(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_decode(text: &str) -> Result<BString, String> {
    let raw = text.as_bytes();
    if !raw.len().is_multiple_of(2) {
        return Err("hex string must have an even number of digits.".to_owned());
    }
    if raw.len() / 2 > MAX_HEX_BYTES {
        return Err(format!(
            "encoded byte string exceeds the {MAX_HEX_BYTES}-byte limit."
        ));
    }
    let mut out = Vec::with_capacity(raw.len() / 2);
    for &[hi, lo] in raw.as_chunks::<2>().0 {
        let (Some(h), Some(l)) = (hex_nibble(hi), hex_nibble(lo)) else {
            return Err("non-hexadecimal character in bytes_hex.".to_owned());
        };
        out.push((h << 4) | l);
    }
    Ok(BString::from(out))
}

fn encode_bstring(bytes: &[u8]) -> Result<Value, SnapshotError> {
    if core::str::from_utf8(bytes).is_ok() {
        Ok(Value::String(BString::from(bytes)))
    } else {
        if bytes.len() > MAX_HEX_BYTES {
            return Err(SnapshotError::InvalidSchema(format!(
                "Non-UTF-8 byte string exceeds the {MAX_HEX_BYTES}-byte limit."
            )));
        }
        Ok(Value::object_from([(
            "bytes_hex",
            Value::String(hex_encode(bytes)),
        )]))
    }
}

fn encode_opt_bstring(value: Option<&BString>) -> Result<Value, SnapshotError> {
    match value {
        None => Ok(Value::Null),
        Some(bytes) => encode_bstring(bytes),
    }
}

fn encode_bstring_list(items: &[BString], field: &str) -> Result<Value, SnapshotError> {
    if items.len() > MAX_LIST_ITEMS {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field '{field}' has {} items, exceeding the limit of {MAX_LIST_ITEMS}.",
            items.len()
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(encode_bstring(item)?);
    }
    Ok(Value::Array(out))
}

fn compile_settings_size(settings: &CompileSettings) -> usize {
    let opt_len = |value: &Option<BString>| value.as_ref().map_or(0, |s| s.len());
    let vec_len = |items: &[BString]| items.iter().map(|s| s.len()).sum::<usize>();
    let inc_len: usize = settings
        .includes
        .iter()
        .map(|inc| inc.path.len().saturating_add(16))
        .sum();
    64_usize
        .saturating_add(opt_len(&settings.language))
        .saturating_add(opt_len(&settings.standard))
        .saturating_add(opt_len(&settings.compiler))
        .saturating_add(vec_len(&settings.defines))
        .saturating_add(inc_len)
        .saturating_add(vec_len(&settings.fragments))
        .saturating_add(vec_len(&settings.launcher))
        .saturating_add(
            settings
                .defines
                .len()
                .saturating_add(settings.fragments.len())
                .saturating_add(settings.launcher.len())
                .saturating_mul(16),
        )
}

fn encode_compile_settings(settings: &CompileSettings) -> Result<Value, SnapshotError> {
    if settings.includes.len() > MAX_LIST_ITEMS {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'includes' has {} items, exceeding the limit of {MAX_LIST_ITEMS}.",
            settings.includes.len()
        )));
    }
    let mut includes = Vec::with_capacity(settings.includes.len());
    for include in &settings.includes {
        includes.push(Value::object_from([
            ("path", encode_bstring(&include.path)?),
            ("system", Value::Bool(include.system)),
        ]));
    }
    Ok(Value::object_from([
        ("compiler", encode_opt_bstring(settings.compiler.as_ref())?),
        (
            "defines",
            encode_bstring_list(&settings.defines, "defines")?,
        ),
        (
            "fragments",
            encode_bstring_list(&settings.fragments, "fragments")?,
        ),
        ("includes", Value::Array(includes)),
        ("language", encode_opt_bstring(settings.language.as_ref())?),
        (
            "launcher",
            encode_bstring_list(&settings.launcher, "launcher")?,
        ),
        ("opaque", Value::Bool(settings.opaque)),
        ("standard", encode_opt_bstring(settings.standard.as_ref())?),
    ]))
}

struct CompileSettingsTable {
    index: HashMap<Arc<CompileSettings>, usize>,
    entries: Vec<Value>,
    sizes: Vec<usize>,
}

impl CompileSettingsTable {
    fn new() -> Self {
        Self {
            index: HashMap::new(),
            entries: Vec::new(),
            sizes: Vec::new(),
        }
    }

    fn intern(&mut self, settings: &Arc<CompileSettings>) -> Result<(usize, usize), SnapshotError> {
        if let Some(&idx) = self.index.get(settings) {
            let size = self.sizes.get(idx).copied().unwrap_or(0);
            return Ok((idx, size));
        }
        if self.entries.len() >= MAX_COMPILE_SETTINGS {
            return Err(SnapshotError::InvalidSchema(format!(
                "Snapshot exceeds the limit of {MAX_COMPILE_SETTINGS} unique compile settings."
            )));
        }
        let encoded = encode_compile_settings(settings)?;
        let size = compile_settings_size(settings);
        let idx = self.entries.len();
        self.entries.push(encoded);
        self.sizes.push(size);
        self.index.insert(Arc::clone(settings), idx);
        Ok((idx, size))
    }
}

/// Encode `snapshot` into canonical, deterministic, single-file UTF-8 JSON bytes.
///
/// The snapshot is normalized and cache-filtered according to `options` before encoding,
/// and validated against the same size, entry-count, and expansion budgets enforced during
/// decode so saved snapshots are always loadable. Repeated calls on the same build state
/// produce byte-identical output with sorted keys, LF line endings, and no wall-clock
/// timestamps.
///
/// # Errors
///
/// Returns [`SnapshotError`] if budget checks or UTF-8 JSON serialization fail.
pub fn encode_snapshot(
    snapshot: &BuildSnapshot,
    options: &SaveOptions,
) -> Result<BString, SnapshotError> {
    encode_snapshot_bounded(
        snapshot,
        options,
        MAX_SNAPSHOT_BYTES,
        MAX_EXPANDED_COMPILE_BYTES,
    )
}

fn encode_snapshot_bounded(
    snapshot: &BuildSnapshot,
    options: &SaveOptions,
    max_bytes: usize,
    max_expanded_compile_bytes: usize,
) -> Result<BString, SnapshotError> {
    let prepared = prepare_for_save(snapshot, options);

    if prepared.configs.len() > MAX_CONFIGS {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'configs' has {} items, exceeding the limit of {MAX_CONFIGS}.",
            prepared.configs.len()
        )));
    }
    if prepared.targets.len() > MAX_TARGETS {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'targets' has {} items, exceeding the limit of {MAX_TARGETS}.",
            prepared.targets.len()
        )));
    }
    if prepared.toolchains.len() > MAX_TOOLCHAINS {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'toolchains' has {} items, exceeding the limit of {MAX_TOOLCHAINS}.",
            prepared.toolchains.len()
        )));
    }
    if prepared.packages.len() > MAX_PACKAGES {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'packages' has {} items, exceeding the limit of {MAX_PACKAGES}.",
            prepared.packages.len()
        )));
    }
    if prepared.cache.len() > MAX_CACHE_ENTRIES {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'cache' has {} items, exceeding the limit of {MAX_CACHE_ENTRIES}.",
            prepared.cache.len()
        )));
    }
    if prepared.warnings.len() > MAX_WARNINGS {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field 'warnings' has {} items, exceeding the limit of {MAX_WARNINGS}.",
            prepared.warnings.len()
        )));
    }

    let mut compile_table = CompileSettingsTable::new();
    let mut total_sources: usize = 0;
    let mut total_expanded_compile_bytes: usize = 0;
    let mut targets_json: Vec<Value> = Vec::with_capacity(prepared.targets.len());

    for ((config, name), target) in &prepared.targets {
        if target.sources.len() > MAX_SOURCES_PER_TARGET {
            return Err(SnapshotError::InvalidSchema(format!(
                "Field 'targets[].sources' has {} items, exceeding the limit of {MAX_SOURCES_PER_TARGET}.",
                target.sources.len()
            )));
        }
        total_sources = total_sources.saturating_add(target.sources.len());
        if total_sources > MAX_TOTAL_SOURCES {
            return Err(SnapshotError::InvalidSchema(format!(
                "Total sources exceed the limit of {MAX_TOTAL_SOURCES}."
            )));
        }
        let mut sources_json: Vec<Value> = Vec::with_capacity(target.sources.len());
        for (source_path, invocations) in &target.sources {
            if invocations.len() > MAX_INVOCATIONS_PER_SOURCE {
                return Err(SnapshotError::InvalidSchema(format!(
                    "Field 'targets[].sources[].compileSettings' has {} items, exceeding the limit of {MAX_INVOCATIONS_PER_SOURCE}.",
                    invocations.len()
                )));
            }
            let mut indices: Vec<Value> = Vec::with_capacity(invocations.len());
            for settings in invocations {
                let (idx, size) = compile_table.intern(settings)?;
                total_expanded_compile_bytes = total_expanded_compile_bytes.saturating_add(size);
                if total_expanded_compile_bytes > max_expanded_compile_bytes {
                    return Err(SnapshotError::InvalidSchema(format!(
                        "Expanded compileSettings references exceed the {max_expanded_compile_bytes}-byte budget."
                    )));
                }
                indices.push(Value::UInt(idx as u64));
            }
            sources_json.push(Value::object_from([
                ("compileSettings", Value::Array(indices)),
                ("path", encode_bstring(source_path)?),
            ]));
        }

        let link_json = match &target.link {
            None => Value::Null,
            Some(fragments) => {
                if fragments.len() > MAX_LIST_ITEMS {
                    return Err(SnapshotError::InvalidSchema(format!(
                        "Field 'targets[].link' has {} items, exceeding the limit of {MAX_LIST_ITEMS}.",
                        fragments.len()
                    )));
                }
                let mut items = Vec::with_capacity(fragments.len());
                for item in fragments {
                    items.push(Value::object_from([
                        ("fragment", encode_bstring(&item.fragment)?),
                        ("role", encode_bstring(&item.role)?),
                    ]));
                }
                Value::Array(items)
            }
        };

        if target.dependencies.len() > MAX_LIST_ITEMS {
            return Err(SnapshotError::InvalidSchema(format!(
                "Field 'dependencies' has {} items, exceeding the limit of {MAX_LIST_ITEMS}.",
                target.dependencies.len()
            )));
        }
        let mut deps_json = Vec::with_capacity(target.dependencies.len());
        for dep in &target.dependencies {
            deps_json.push(encode_bstring(dep)?);
        }

        targets_json.push(Value::object_from([
            (
                "artifacts",
                encode_bstring_list(&target.artifacts, "artifacts")?,
            ),
            ("attribution", Value::string(target.attribution.label())),
            ("backtrace", encode_opt_bstring(target.backtrace.as_ref())?),
            ("config", encode_bstring(config)?),
            ("dependencies", Value::Array(deps_json)),
            ("kind", encode_opt_bstring(target.kind.as_ref())?),
            ("link", link_json),
            ("name", encode_bstring(name)?),
            ("sources", Value::Array(sources_json)),
        ]));
    }

    let mut sorted_toolchains: Vec<&ToolchainSnapshot> = prepared.toolchains.iter().collect();
    sorted_toolchains.sort_by(|a, b| {
        a.language
            .cmp(&b.language)
            .then_with(|| a.compiler_id.cmp(&b.compiler_id))
            .then_with(|| a.version.cmp(&b.version))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.target.cmp(&b.target))
            .then_with(|| a.frontend.cmp(&b.frontend))
    });
    let mut toolchains_json: Vec<Value> = Vec::with_capacity(sorted_toolchains.len());
    for tc in sorted_toolchains {
        toolchains_json.push(Value::object_from([
            ("compilerId", encode_opt_bstring(tc.compiler_id.as_ref())?),
            ("frontend", encode_opt_bstring(tc.frontend.as_ref())?),
            ("language", encode_bstring(&tc.language)?),
            ("path", encode_opt_bstring(tc.path.as_ref())?),
            ("target", encode_opt_bstring(tc.target.as_ref())?),
            ("version", encode_opt_bstring(tc.version.as_ref())?),
        ]));
    }

    let mut packages_json: Vec<Value> = Vec::with_capacity(prepared.packages.len());
    for (name, package) in &prepared.packages {
        if package.observed.len() > MAX_OBSERVATIONS_PER_PACKAGE {
            return Err(SnapshotError::InvalidSchema(format!(
                "Field 'packages[].observed' has {} items, exceeding the limit of {MAX_OBSERVATIONS_PER_PACKAGE}.",
                package.observed.len()
            )));
        }
        let mut observed = Vec::with_capacity(package.observed.len());
        for obs in &package.observed {
            observed.push(Value::object_from([
                ("outcome", encode_bstring(&obs.outcome)?),
                (
                    "packageCall",
                    obs.package_call
                        .map_or(Value::Null, |call| Value::UInt(u64::from(call))),
                ),
                ("selected", encode_opt_bstring(obs.selected.as_ref())?),
                ("version", encode_opt_bstring(obs.version.as_ref())?),
            ]));
        }
        packages_json.push(Value::object_from([
            (
                "cachedDir",
                encode_opt_bstring(package.cached_dir.as_ref())?,
            ),
            ("name", encode_bstring(name)?),
            ("observed", Value::Array(observed)),
            (
                "versionHint",
                encode_opt_bstring(package.version_hint.as_ref())?,
            ),
        ]));
    }

    let mut cache_json: Vec<Value> = Vec::with_capacity(prepared.cache.len());
    for (name, record) in &prepared.cache {
        cache_json.push(Value::object_from([
            ("help", encode_bstring(&record.help)?),
            ("name", encode_bstring(name)?),
            ("type", encode_bstring(&record.ty)?),
            ("value", encode_bstring(&record.value)?),
        ]));
    }

    let mut evidence_entries = Vec::with_capacity(Section::ALL.len());
    for section in Section::ALL {
        let ev = prepared.evidence(section);
        let (source, reason) = match &ev {
            Evidence::Complete(source) => (Some(source.label()), None),
            Evidence::Partial(source, reason) => (Some(source.label()), Some(reason)),
            Evidence::Unavailable(reason) => (None, Some(reason)),
        };
        evidence_entries.push((
            section.label(),
            Value::object_from([
                ("reason", encode_opt_bstring(reason)?),
                ("source", source.map_or(Value::Null, Value::string)),
                ("state", Value::string(ev.label())),
            ]),
        ));
    }
    let evidence_json = Value::object_from(evidence_entries);

    let host_json = match &prepared.host {
        None => Value::Null,
        Some(host) => Value::object_from([
            ("arch", encode_bstring(&host.arch)?),
            ("os", encode_bstring(&host.os)?),
        ]),
    };

    let mut configs_json = Vec::with_capacity(prepared.configs.len());
    for config in &prepared.configs {
        configs_json.push(encode_bstring(config)?);
    }

    let mut warnings_json = Vec::with_capacity(prepared.warnings.len());
    for warning in &prepared.warnings {
        warnings_json.push(encode_bstring(warning)?);
    }

    let root = Value::object_from([
        ("cache", Value::Array(cache_json)),
        (
            "cacheSet",
            prepared
                .saved_cache_set
                .map_or(Value::Null, |set| Value::string(set.label())),
        ),
        ("compileSettingsTable", Value::Array(compile_table.entries)),
        ("configs", Value::Array(configs_json)),
        ("evidence", evidence_json),
        (
            "generator",
            encode_opt_bstring(prepared.generator.as_ref())?,
        ),
        ("hasSourceRoot", Value::Bool(prepared.source_root.is_some())),
        ("host", host_json),
        ("kind", Value::string(SNAPSHOT_KIND)),
        ("packages", Value::Array(packages_json)),
        ("packagesUnknown", Value::Bool(prepared.packages_unknown)),
        (
            "producer",
            Value::object_from([
                ("tool", encode_bstring(&prepared.producer.tool)?),
                (
                    "version",
                    encode_opt_bstring(prepared.producer.version.as_ref())?,
                ),
            ]),
        ),
        ("targets", Value::Array(targets_json)),
        ("toolchains", Value::Array(toolchains_json)),
        (
            "version",
            Value::object_from([
                ("major", Value::UInt(u64::from(SNAPSHOT_VERSION_MAJOR))),
                ("minor", Value::UInt(u64::from(SNAPSHOT_VERSION_MINOR))),
            ]),
        ),
        ("warnings", Value::Array(warnings_json)),
    ]);

    let mut bytes = rcmake_json::write_pretty_utf8(&root)
        .map_err(|err| SnapshotError::InvalidSchema(err.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > max_bytes {
        return Err(SnapshotError::TooLarge { limit: max_bytes });
    }
    Ok(bytes)
}

fn req_field<'a>(obj: &'a Object, key: &str) -> Result<&'a Value, SnapshotError> {
    obj.get(key.as_bytes())
        .ok_or_else(|| SnapshotError::InvalidSchema(format!("Missing required field '{key}'.")))
}

fn as_object<'a>(value: &'a Value, field: &str) -> Result<&'a Object, SnapshotError> {
    value.as_object().ok_or_else(|| {
        SnapshotError::InvalidSchema(format!(
            "Expected object for '{field}', found {}.",
            value.type_name()
        ))
    })
}

fn as_array_bounded<'a>(
    value: &'a Value,
    field: &str,
    max: usize,
) -> Result<&'a [Value], SnapshotError> {
    let items = value.as_array().ok_or_else(|| {
        SnapshotError::InvalidSchema(format!(
            "Expected array for '{field}', found {}.",
            value.type_name()
        ))
    })?;
    if items.len() > max {
        return Err(SnapshotError::InvalidSchema(format!(
            "Field '{field}' has {} items, exceeding the limit of {max}.",
            items.len()
        )));
    }
    Ok(items)
}

fn decode_bstring(value: &Value, field: &str) -> Result<BString, SnapshotError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Object(map) => {
            if map.len() != 1 {
                return Err(SnapshotError::InvalidSchema(format!(
                    "Expected string or single-field {{\"bytes_hex\": ...}} object for '{field}'."
                )));
            }
            let Some(hex_val) = map.get(b"bytes_hex".as_slice()) else {
                return Err(SnapshotError::InvalidSchema(format!(
                    "Expected string or single-field {{\"bytes_hex\": ...}} object for '{field}'."
                )));
            };
            let Some(hex_str) = hex_val.as_str() else {
                return Err(SnapshotError::InvalidSchema(format!(
                    "Expected UTF-8 hex string in 'bytes_hex' for '{field}'."
                )));
            };
            hex_decode(hex_str).map_err(|msg| {
                SnapshotError::InvalidSchema(format!("Invalid 'bytes_hex' for '{field}': {msg}"))
            })
        }
        other => Err(SnapshotError::InvalidSchema(format!(
            "Expected string for '{field}', found {}.",
            other.type_name()
        ))),
    }
}

fn decode_opt_bstring(obj: &Object, key: &str) -> Result<Option<BString>, SnapshotError> {
    match obj.get(key.as_bytes()) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => decode_bstring(value, key).map(Some),
    }
}

fn decode_bool(value: &Value, field: &str) -> Result<bool, SnapshotError> {
    value.as_bool().ok_or_else(|| {
        SnapshotError::InvalidSchema(format!(
            "Expected boolean for '{field}', found {}.",
            value.type_name()
        ))
    })
}

fn decode_opt_bool(obj: &Object, key: &str) -> Result<Option<bool>, SnapshotError> {
    match obj.get(key.as_bytes()) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => decode_bool(value, key).map(Some),
    }
}

fn decode_u32(value: &Value, field: &str) -> Result<u32, SnapshotError> {
    let raw = value.as_u64().ok_or_else(|| {
        SnapshotError::InvalidSchema(format!(
            "Expected non-negative integer for '{field}', found {}.",
            value.type_name()
        ))
    })?;
    u32::try_from(raw)
        .map_err(|_| SnapshotError::InvalidSchema(format!("Integer out of range for '{field}'.")))
}

fn decode_usize(value: &Value, field: &str) -> Result<usize, SnapshotError> {
    let raw = value.as_u64().ok_or_else(|| {
        SnapshotError::InvalidSchema(format!(
            "Expected non-negative integer for '{field}', found {}.",
            value.type_name()
        ))
    })?;
    usize::try_from(raw)
        .map_err(|_| SnapshotError::InvalidSchema(format!("Integer out of range for '{field}'.")))
}

fn decode_bstring_list(obj: &Object, key: &str, max: usize) -> Result<Vec<BString>, SnapshotError> {
    let items = as_array_bounded(req_field(obj, key)?, key, max)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(decode_bstring(item, key)?);
    }
    Ok(out)
}

fn decode_compile_settings(value: &Value) -> Result<Arc<CompileSettings>, SnapshotError> {
    let obj = as_object(value, "compileSettingsTable[]")?;
    let language = decode_opt_bstring(obj, "language")?;
    let standard = decode_opt_bstring(obj, "standard")?;
    let defines = decode_bstring_list(obj, "defines", MAX_LIST_ITEMS)?;
    let includes_raw = as_array_bounded(req_field(obj, "includes")?, "includes", MAX_LIST_ITEMS)?;
    let mut includes = Vec::with_capacity(includes_raw.len());
    for entry in includes_raw {
        let entry_obj = as_object(entry, "includes[]")?;
        let path = decode_bstring(req_field(entry_obj, "path")?, "includes[].path")?;
        let system = decode_bool(req_field(entry_obj, "system")?, "includes[].system")?;
        includes.push(IncludeDir { path, system });
    }
    let fragments = decode_bstring_list(obj, "fragments", MAX_LIST_ITEMS)?;
    let opaque = decode_bool(req_field(obj, "opaque")?, "opaque")?;
    let compiler = decode_opt_bstring(obj, "compiler")?;
    let launcher = decode_bstring_list(obj, "launcher", MAX_LIST_ITEMS)?;
    Ok(Arc::new(CompileSettings {
        language,
        standard,
        defines,
        includes,
        fragments,
        opaque,
        compiler,
        launcher,
    }))
}

fn decode_evidence(obj: &Object) -> Result<BTreeMap<Section, Evidence>, SnapshotError> {
    let mut out = BTreeMap::new();
    for (key, value) in obj {
        let Some(section) = Section::parse(key) else {
            // Forward compatibility: ignore unknown sections from newer minor versions.
            continue;
        };
        let entry = as_object(value, "evidence[]")?;
        let state = req_field(entry, "state")?.as_str().ok_or_else(|| {
            SnapshotError::InvalidSchema("Expected UTF-8 string for 'evidence[].state'.".to_owned())
        })?;
        let source_val = entry.get(b"source".as_slice());
        let parsed_source = match source_val {
            None | Some(Value::Null) => None,
            Some(val) => {
                let bytes = val.as_bytes().ok_or_else(|| {
                    SnapshotError::InvalidSchema(
                        "Expected string or null for 'evidence[].source'.".to_owned(),
                    )
                })?;
                let source = DataSource::parse(bytes).ok_or_else(|| {
                    SnapshotError::InvalidSchema(format!(
                        "Unknown evidence source '{}'.",
                        String::from_utf8_lossy(bytes)
                    ))
                })?;
                Some(source)
            }
        };
        let reason = decode_opt_bstring(entry, "reason")?;
        let evidence = match state {
            "complete" => {
                let source = parsed_source.ok_or_else(|| {
                    SnapshotError::InvalidSchema(
                        "Complete evidence requires a non-null 'source'.".to_owned(),
                    )
                })?;
                Evidence::Complete(source)
            }
            "partial" => {
                let source = parsed_source.ok_or_else(|| {
                    SnapshotError::InvalidSchema(
                        "Partial evidence requires a non-null 'source'.".to_owned(),
                    )
                })?;
                Evidence::Partial(source, reason.unwrap_or_default())
            }
            "unavailable" => Evidence::Unavailable(reason.unwrap_or_default()),
            other => {
                return Err(SnapshotError::InvalidSchema(format!(
                    "Unknown evidence state '{other}'."
                )));
            }
        };
        out.insert(section, evidence);
    }
    Ok(out)
}

/// Return whether `bytes` is a JSON object carrying the portable snapshot `"kind"` marker.
///
/// Enforces [`MAX_SNAPSHOT_BYTES`] and [`MAX_SNAPSHOT_JSON_DEPTH`].
#[must_use]
pub fn is_snapshot_bytes(bytes: &[u8]) -> bool {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return false;
    }
    let Ok(value) = rcmake_json::parse_bounded(
        bytes,
        &snapshot_read_options(),
        &snapshot_parse_budget(MAX_SNAPSHOT_BYTES),
    ) else {
        return false;
    };
    value
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == SNAPSHOT_KIND)
}

/// Decode a portable single-file JSON snapshot from `bytes`.
///
/// Rejects inputs larger than [`MAX_SNAPSHOT_BYTES`], nesting deeper than
/// [`MAX_SNAPSHOT_JSON_DEPTH`], unknown major format versions, and malformed schemas.
/// Unknown additive fields in the same major version are ignored for forward compatibility.
///
/// # Errors
///
/// Returns [`SnapshotError`] when the input fails budget, JSON, marker, version, or schema checks.
pub fn decode_snapshot(
    bytes: &[u8],
    display_path: BString,
) -> Result<BuildSnapshot, SnapshotError> {
    decode_snapshot_bounded(
        bytes,
        display_path,
        MAX_SNAPSHOT_BYTES,
        MAX_EXPANDED_COMPILE_BYTES,
    )
}

fn decode_snapshot_bounded(
    bytes: &[u8],
    display_path: BString,
    max_bytes: usize,
    max_expanded_compile_bytes: usize,
) -> Result<BuildSnapshot, SnapshotError> {
    if bytes.len() > max_bytes {
        return Err(SnapshotError::TooLarge { limit: max_bytes });
    }
    let value = rcmake_json::parse_bounded(
        bytes,
        &snapshot_read_options(),
        &snapshot_parse_budget(max_bytes),
    )
    .map_err(|err| SnapshotError::Json(err.message))?;
    let Some(root) = value.as_object() else {
        return Err(SnapshotError::NotASnapshot);
    };
    let Some(kind) = root.get(b"kind".as_slice()).and_then(Value::as_str) else {
        return Err(SnapshotError::NotASnapshot);
    };
    if kind != SNAPSHOT_KIND {
        return Err(SnapshotError::NotASnapshot);
    }

    let version_obj = as_object(req_field(root, "version")?, "version")?;
    let major = decode_u32(req_field(version_obj, "major")?, "version.major")?;
    let minor = decode_u32(req_field(version_obj, "minor")?, "version.minor")?;
    if major != SNAPSHOT_VERSION_MAJOR {
        return Err(SnapshotError::UnsupportedVersion { major, minor });
    }

    let producer_obj = as_object(req_field(root, "producer")?, "producer")?;
    let producer = Producer {
        tool: decode_bstring(req_field(producer_obj, "tool")?, "producer.tool")?,
        version: decode_opt_bstring(producer_obj, "version")?,
    };

    let host = match root.get(b"host".as_slice()) {
        None | Some(Value::Null) => None,
        Some(host_val) => {
            let host_obj = as_object(host_val, "host")?;
            Some(SnapshotHost {
                os: decode_bstring(req_field(host_obj, "os")?, "host.os")?,
                arch: decode_bstring(req_field(host_obj, "arch")?, "host.arch")?,
            })
        }
    };

    let generator = decode_opt_bstring(root, "generator")?;
    let has_source_root = decode_opt_bool(root, "hasSourceRoot")?.unwrap_or(false);
    let saved_cache_set = match root.get(b"cacheSet".as_slice()) {
        None | Some(Value::Null) => None,
        Some(val) => {
            let raw = val.as_bytes().ok_or_else(|| {
                SnapshotError::InvalidSchema("Expected string or null for 'cacheSet'.".to_owned())
            })?;
            let set = CacheSet::parse(raw).ok_or_else(|| {
                SnapshotError::InvalidSchema(format!(
                    "Unknown cacheSet '{}'.",
                    String::from_utf8_lossy(raw)
                ))
            })?;
            Some(set)
        }
    };

    let configs = decode_bstring_list(root, "configs", MAX_CONFIGS)?;

    let table_raw = as_array_bounded(
        req_field(root, "compileSettingsTable")?,
        "compileSettingsTable",
        MAX_COMPILE_SETTINGS,
    )?;
    let mut compile_table: Vec<Arc<CompileSettings>> = Vec::with_capacity(table_raw.len());
    let mut compile_sizes: Vec<usize> = Vec::with_capacity(table_raw.len());
    for entry in table_raw {
        let decoded = decode_compile_settings(entry)?;
        compile_sizes.push(compile_settings_size(&decoded));
        compile_table.push(decoded);
    }

    let targets_raw = as_array_bounded(req_field(root, "targets")?, "targets", MAX_TARGETS)?;
    let mut targets: BTreeMap<(BString, BString), TargetSnapshot> = BTreeMap::new();
    let mut total_sources: usize = 0;
    let mut total_expanded_compile_bytes: usize = 0;
    for target_val in targets_raw {
        let target_obj = as_object(target_val, "targets[]")?;
        let config = decode_bstring(req_field(target_obj, "config")?, "targets[].config")?;
        let name = decode_bstring(req_field(target_obj, "name")?, "targets[].name")?;
        let kind = decode_opt_bstring(target_obj, "kind")?;
        let attr_bytes = req_field(target_obj, "attribution")?
            .as_bytes()
            .ok_or_else(|| {
                SnapshotError::InvalidSchema(
                    "Expected string for 'targets[].attribution'.".to_owned(),
                )
            })?;
        let attribution = Attribution::parse(attr_bytes).ok_or_else(|| {
            SnapshotError::InvalidSchema(format!(
                "Unknown attribution '{}'.",
                String::from_utf8_lossy(attr_bytes)
            ))
        })?;

        let sources_raw = as_array_bounded(
            req_field(target_obj, "sources")?,
            "targets[].sources",
            MAX_SOURCES_PER_TARGET,
        )?;
        total_sources = total_sources.saturating_add(sources_raw.len());
        if total_sources > MAX_TOTAL_SOURCES {
            return Err(SnapshotError::InvalidSchema(format!(
                "Total sources exceed the limit of {MAX_TOTAL_SOURCES}."
            )));
        }
        let mut sources: BTreeMap<BString, SourceInvocations> = BTreeMap::new();
        for src_val in sources_raw {
            let src_obj = as_object(src_val, "targets[].sources[]")?;
            let path = decode_bstring(req_field(src_obj, "path")?, "targets[].sources[].path")?;
            let indices_raw = as_array_bounded(
                req_field(src_obj, "compileSettings")?,
                "targets[].sources[].compileSettings",
                MAX_INVOCATIONS_PER_SOURCE,
            )?;
            let mut invocations = Vec::with_capacity(indices_raw.len());
            for idx_val in indices_raw {
                let idx = decode_usize(idx_val, "compileSettings[]")?;
                let settings = compile_table.get(idx).ok_or_else(|| {
                    SnapshotError::InvalidSchema(format!(
                        "compileSettings index {idx} out of bounds (table length {}).",
                        compile_table.len()
                    ))
                })?;
                let size = compile_sizes.get(idx).copied().unwrap_or(0);
                total_expanded_compile_bytes = total_expanded_compile_bytes.saturating_add(size);
                if total_expanded_compile_bytes > max_expanded_compile_bytes {
                    return Err(SnapshotError::InvalidSchema(format!(
                        "Expanded compileSettings references exceed the {max_expanded_compile_bytes}-byte budget."
                    )));
                }
                invocations.push(Arc::clone(settings));
            }
            if sources.contains_key(&path) {
                return Err(SnapshotError::InvalidSchema(format!(
                    "Duplicate source path '{}' in target '{}'.",
                    String::from_utf8_lossy(&path),
                    String::from_utf8_lossy(&name)
                )));
            }
            sources.insert(path, invocations);
        }

        let link = match target_obj.get(b"link".as_slice()) {
            None | Some(Value::Null) => None,
            Some(link_val) => {
                let items = as_array_bounded(link_val, "targets[].link", MAX_LIST_ITEMS)?;
                let mut fragments = Vec::with_capacity(items.len());
                for item in items {
                    let item_obj = as_object(item, "targets[].link[]")?;
                    fragments.push(LinkFragment {
                        role: decode_bstring(req_field(item_obj, "role")?, "link[].role")?,
                        fragment: decode_bstring(
                            req_field(item_obj, "fragment")?,
                            "link[].fragment",
                        )?,
                    });
                }
                Some(fragments)
            }
        };

        let deps_list = decode_bstring_list(target_obj, "dependencies", MAX_LIST_ITEMS)?;
        let dependencies: BTreeSet<BString> = deps_list.into_iter().collect();
        let artifacts = decode_bstring_list(target_obj, "artifacts", MAX_LIST_ITEMS)?;
        let backtrace = decode_opt_bstring(target_obj, "backtrace")?;

        let target_key = (config, name);
        if targets.contains_key(&target_key) {
            return Err(SnapshotError::InvalidSchema(format!(
                "Duplicate target '{}' in configuration '{}'.",
                String::from_utf8_lossy(&target_key.1),
                String::from_utf8_lossy(&target_key.0)
            )));
        }
        targets.insert(
            target_key,
            TargetSnapshot {
                kind,
                attribution,
                sources,
                link,
                dependencies,
                artifacts,
                backtrace,
            },
        );
    }

    let toolchains_raw =
        as_array_bounded(req_field(root, "toolchains")?, "toolchains", MAX_TOOLCHAINS)?;
    let mut toolchains = Vec::with_capacity(toolchains_raw.len());
    for tc_val in toolchains_raw {
        let tc_obj = as_object(tc_val, "toolchains[]")?;
        toolchains.push(ToolchainSnapshot {
            language: decode_bstring(req_field(tc_obj, "language")?, "toolchains[].language")?,
            compiler_id: decode_opt_bstring(tc_obj, "compilerId")?,
            version: decode_opt_bstring(tc_obj, "version")?,
            path: decode_opt_bstring(tc_obj, "path")?,
            target: decode_opt_bstring(tc_obj, "target")?,
            frontend: decode_opt_bstring(tc_obj, "frontend")?,
        });
    }

    let packages_raw = as_array_bounded(req_field(root, "packages")?, "packages", MAX_PACKAGES)?;
    let mut packages: BTreeMap<BString, PackageSnapshot> = BTreeMap::new();
    for pkg_val in packages_raw {
        let pkg_obj = as_object(pkg_val, "packages[]")?;
        let name = decode_bstring(req_field(pkg_obj, "name")?, "packages[].name")?;
        let cached_dir = decode_opt_bstring(pkg_obj, "cachedDir")?;
        let version_hint = decode_opt_bstring(pkg_obj, "versionHint")?;
        let obs_raw = as_array_bounded(
            req_field(pkg_obj, "observed")?,
            "packages[].observed",
            MAX_OBSERVATIONS_PER_PACKAGE,
        )?;
        let mut observed = Vec::with_capacity(obs_raw.len());
        for obs_val in obs_raw {
            let obs_obj = as_object(obs_val, "packages[].observed[]")?;
            let package_call = match obs_obj.get(b"packageCall".as_slice()) {
                None | Some(Value::Null) => None,
                Some(val) => Some(decode_u32(val, "observed[].packageCall")?),
            };
            observed.push(PackageObservation {
                package_call,
                outcome: decode_bstring(req_field(obs_obj, "outcome")?, "observed[].outcome")?,
                version: decode_opt_bstring(obs_obj, "version")?,
                selected: decode_opt_bstring(obs_obj, "selected")?,
            });
        }
        if packages.contains_key(&name) {
            return Err(SnapshotError::InvalidSchema(format!(
                "Duplicate package '{}'.",
                String::from_utf8_lossy(&name)
            )));
        }
        packages.insert(
            name,
            PackageSnapshot {
                cached_dir,
                version_hint,
                observed,
            },
        );
    }

    let packages_unknown = decode_opt_bool(root, "packagesUnknown")?.unwrap_or(false);

    let cache_raw = as_array_bounded(req_field(root, "cache")?, "cache", MAX_CACHE_ENTRIES)?;
    let mut cache: BTreeMap<BString, CacheRecord> = BTreeMap::new();
    for entry_val in cache_raw {
        let entry_obj = as_object(entry_val, "cache[]")?;
        let name = decode_bstring(req_field(entry_obj, "name")?, "cache[].name")?;
        let ty = decode_bstring(req_field(entry_obj, "type")?, "cache[].type")?;
        let value = decode_bstring(req_field(entry_obj, "value")?, "cache[].value")?;
        let help = decode_bstring(req_field(entry_obj, "help")?, "cache[].help")?;
        if cache.contains_key(&name) {
            return Err(SnapshotError::InvalidSchema(format!(
                "Duplicate cache entry '{}'.",
                String::from_utf8_lossy(&name)
            )));
        }
        cache.insert(name, CacheRecord { ty, value, help });
    }

    let evidence_obj = as_object(req_field(root, "evidence")?, "evidence")?;
    let evidence = decode_evidence(evidence_obj)?;

    let warnings = match root.get(b"warnings".as_slice()) {
        None | Some(Value::Null) => Vec::new(),
        Some(val) => {
            let items = as_array_bounded(val, "warnings", MAX_WARNINGS)?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(decode_bstring(item, "warnings[]")?);
            }
            out
        }
    };

    Ok(BuildSnapshot {
        path: display_path,
        build_root: BString::from("${build}"),
        producer,
        host,
        generator,
        source_root: has_source_root.then(|| BString::from("${source}")),
        home_root: None,
        is_normalized: true,
        saved_cache_set,
        configs,
        targets,
        toolchains,
        packages,
        packages_unknown,
        cache,
        evidence,
        warnings,
    })
}

/// Encode `snapshot` and write it atomically to `file` through `host`.
///
/// Missing parent directories are created automatically. The snapshot is written to a
/// sibling temporary file with [`WriteMode::CreateNew`] and renamed over `file` so
/// readers never observe a half-written file.
///
/// # Errors
///
/// Returns [`SnapshotError`] if encoding, directory creation, writing, or renaming fails.
pub fn save_snapshot_file(
    host: &dyn Host,
    file: &Path,
    snapshot: &BuildSnapshot,
    options: &SaveOptions,
) -> Result<(), SnapshotError> {
    if let Ok(meta) = host.metadata(file, Follow::No)
        && meta.kind == FileKind::Dir
    {
        return Err(SnapshotError::Io(format!(
            "{} is a directory.",
            file.display()
        )));
    }
    let bytes = encode_snapshot(snapshot, options)?;
    let parent = file
        .parent()
        .ok_or_else(|| SnapshotError::Io(format!("{} has no parent directory.", file.display())))?;
    if !parent.as_os_str().is_empty() {
        host.create_dir_all(parent)
            .map_err(|err| SnapshotError::Io(err.to_string()))?;
    }
    let pid = host.system().pid;
    for _ in 0..32 {
        let seq = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".rcmake-snapshot.{pid}.{seq}.tmp"));
        match host.write(&temp, &bytes, WriteMode::CreateNew) {
            Ok(()) => {
                if let Err(err) = host.rename(&temp, file) {
                    let _ = host.remove_file(&temp);
                    return Err(SnapshotError::Io(err.to_string()));
                }
                return Ok(());
            }
            Err(err) if err.kind == ErrorKind::AlreadyExists => {}
            Err(err) => {
                let _ = host.remove_file(&temp);
                return Err(SnapshotError::Io(err.to_string()));
            }
        }
    }
    Err(SnapshotError::Io(
        "Cannot reserve a temporary file for atomic snapshot write.".to_owned(),
    ))
}

/// Read and decode a portable snapshot file from `file` through `host` with strict size
/// and JSON parse budgets.
///
/// # Errors
///
/// Returns [`SnapshotError`] if the file is unreadable, not a regular file, exceeds
/// [`MAX_SNAPSHOT_BYTES`], is not a portable snapshot, or fails validation.
pub fn load_snapshot_file(host: &dyn Host, file: &Path) -> Result<BuildSnapshot, SnapshotError> {
    let meta = host
        .metadata(file, Follow::Yes)
        .map_err(|err| SnapshotError::Io(err.to_string()))?;
    if meta.kind != FileKind::File {
        return Err(SnapshotError::Io(format!(
            "{} is not a regular file.",
            file.display()
        )));
    }
    let limit_u64 = MAX_SNAPSHOT_BYTES as u64;
    if meta.len > limit_u64 {
        return Err(SnapshotError::TooLarge {
            limit: MAX_SNAPSHOT_BYTES,
        });
    }
    let bytes = host
        .read(
            file,
            ReadRange::All {
                limit: limit_u64.saturating_add(1),
            },
        )
        .map_err(|err| SnapshotError::Io(err.to_string()))?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(SnapshotError::TooLarge {
            limit: MAX_SNAPSHOT_BYTES,
        });
    }
    decode_snapshot(&bytes, rcmake_host::path::from_native_cmake(file))
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;
    use crate::diff::{DiffOptions, compare};

    fn rich_snapshot() -> BuildSnapshot {
        let shared = Arc::new(CompileSettings {
            language: Some("CXX".into()),
            standard: Some("20".into()),
            defines: vec![
                "DEBUG=1".into(),
                BString::from(b"RAW_DEF=\xff\xfe".as_slice()),
                "SRC_DIR=\"/Users/alice/proj/data\"".into(),
            ],
            includes: vec![
                IncludeDir {
                    path: "/Users/alice/proj/include".into(),
                    system: false,
                },
                IncludeDir {
                    path: "/Users/alice/sdk/inc".into(),
                    system: true,
                },
            ],
            fragments: vec!["-O2".into(), "-I/Users/alice/proj/build/gen".into()],
            opaque: false,
            compiler: Some("/Users/alice/llvm/bin/clang++".into()),
            launcher: vec!["ccache".into()],
        });

        let mut sources = BTreeMap::new();
        sources.insert(
            BString::from("/Users/alice/proj/src/a.cpp"),
            vec![Arc::clone(&shared)],
        );
        sources.insert(
            BString::from(b"/Users/alice/proj/src/\xffb.cpp".as_slice()),
            vec![Arc::clone(&shared)],
        );

        let mut targets = BTreeMap::new();
        targets.insert(
            (BString::from("Debug"), BString::from("core")),
            TargetSnapshot {
                kind: Some("STATIC_LIBRARY".into()),
                attribution: Attribution::Exact,
                sources,
                link: Some(vec![LinkFragment {
                    role: "libraries".into(),
                    fragment: "/Users/alice/proj/build/libdep.a".into(),
                }]),
                dependencies: [BString::from("dep")].into_iter().collect(),
                artifacts: vec!["/Users/alice/proj/build/libcore.a".into()],
                backtrace: Some("/Users/alice/proj/CMakeLists.txt:14".into()),
            },
        );

        let mut packages = BTreeMap::new();
        packages.insert(
            BString::from("ZLIB"),
            PackageSnapshot {
                cached_dir: Some("/Users/alice/vcpkg/zlib".into()),
                version_hint: Some("1.3.1".into()),
                observed: vec![PackageObservation {
                    package_call: Some(1),
                    outcome: "found".into(),
                    version: Some("1.3.1".into()),
                    selected: Some("/Users/alice/vcpkg/zlib/ZLIBConfig.cmake".into()),
                }],
            },
        );

        let mut cache = BTreeMap::new();
        cache.insert(
            BString::from("CMAKE_BUILD_TYPE"),
            CacheRecord {
                ty: "STRING".into(),
                value: "Debug".into(),
                help: "Build type".into(),
            },
        );
        cache.insert(
            BString::from("API_KEY"),
            CacheRecord {
                ty: "STRING".into(),
                value: "super-secret-value".into(),
                help: "Secret API key".into(),
            },
        );
        cache.insert(
            BString::from(b"RAW_\x80KEY".as_slice()),
            CacheRecord {
                ty: "STRING".into(),
                value: BString::from(b"val\xff".as_slice()),
                help: "Non-UTF8 entry".into(),
            },
        );

        let mut evidence = BTreeMap::new();
        evidence.insert(Section::Targets, Evidence::Complete(DataSource::FileApi));
        evidence.insert(Section::Compile, Evidence::Complete(DataSource::FileApi));
        evidence.insert(
            Section::Link,
            Evidence::Unavailable("compile_commands.json has no link data.".into()),
        );
        evidence.insert(
            Section::Toolchain,
            Evidence::Complete(DataSource::CompilerFiles),
        );
        evidence.insert(
            Section::Packages,
            Evidence::Partial(
                DataSource::FindTrace,
                "Partial trace under /Users/alice/proj/build.".into(),
            ),
        );
        evidence.insert(Section::Cache, Evidence::Complete(DataSource::Cache));

        BuildSnapshot {
            path: "/Users/alice/proj/build".into(),
            build_root: "/Users/alice/proj/build".into(),
            producer: Producer {
                tool: "rcmake".into(),
                version: Some("4.5.1".into()),
            },
            host: Some(SnapshotHost {
                os: "Darwin".into(),
                arch: "arm64".into(),
            }),
            generator: Some("Ninja".into()),
            source_root: Some("/Users/alice/proj".into()),
            home_root: Some("/Users/alice".into()),
            is_normalized: false,
            saved_cache_set: None,
            configs: vec!["Debug".into()],
            targets,
            toolchains: vec![ToolchainSnapshot {
                language: "CXX".into(),
                compiler_id: Some("Clang".into()),
                version: Some("16.0.0".into()),
                path: Some("/Users/alice/llvm/bin/clang++".into()),
                target: Some("arm64-apple-darwin".into()),
                frontend: Some("GNU".into()),
            }],
            packages,
            packages_unknown: true,
            cache,
            evidence,
            warnings: vec!["note under /Users/alice/proj".into()],
        }
    }

    #[test]
    fn round_trip_preserves_all_sections_non_utf8_interning_and_evidence() {
        let original = rich_snapshot();
        let encoded = encode_snapshot(&original, &SaveOptions::default()).expect("encode");
        let text = core::str::from_utf8(&encoded).expect("valid UTF-8 JSON output");
        assert!(!text.contains("/Users/alice"), "home path leaked:\n{text}");
        assert!(
            !text.contains("super-secret-value"),
            "API_KEY leaked under default CacheSet::Key"
        );
        assert!(text.contains("\"bytes_hex\""));
        assert!(is_snapshot_bytes(&encoded));

        let parsed = rcmake_json::parse(&encoded, &ReadOptions::default()).expect("json");
        assert_eq!(
            parsed
                .get("compileSettingsTable")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(1),
            "identical CompileSettings must be deduplicated into 1 table entry"
        );

        let decoded = decode_snapshot(&encoded, "snap.json".into()).expect("decode");
        assert!(decoded.is_normalized);
        assert_eq!(decoded.saved_cache_set, Some(CacheSet::Key));
        assert_eq!(decoded.producer.tool, "rcmake");
        assert_eq!(decoded.producer.version, Some("4.5.1".into()));
        assert_eq!(
            decoded.host,
            Some(SnapshotHost {
                os: "Darwin".into(),
                arch: "arm64".into(),
            })
        );
        assert_eq!(
            decoded.evidence(Section::Link),
            Evidence::Unavailable("compile_commands.json has no link data.".into())
        );
        assert_eq!(
            decoded.evidence(Section::Packages),
            Evidence::Partial(
                DataSource::FindTrace,
                "Partial trace under ${build}.".into()
            )
        );
        assert!(decoded.cache.contains_key(b"CMAKE_BUILD_TYPE".as_slice()));
        assert!(!decoded.cache.contains_key(b"API_KEY".as_slice()));
        assert_eq!(
            decoded
                .cache
                .get(b"RAW_\x80KEY".as_slice())
                .map(|r| r.value.as_slice()),
            Some(b"val\xff".as_slice())
        );

        let target = &decoded.targets[&(BString::from("Debug"), BString::from("core"))];
        assert_eq!(target.backtrace, Some("${source}/CMakeLists.txt:14".into()));
        let a_inv = &target.sources[b"${source}/src/a.cpp".as_slice()];
        let b_inv = &target.sources[b"${source}/src/\xffb.cpp".as_slice()];
        assert!(
            Arc::ptr_eq(&a_inv[0], &b_inv[0]),
            "decoded sources must share interned Arc<CompileSettings>"
        );
        assert_eq!(
            a_inv[0].defines[1],
            BString::from(b"RAW_DEF=\xff\xfe".as_slice())
        );
        assert_eq!(
            decoded.toolchains[0].path,
            Some("${home}/llvm/bin/clang++".into())
        );
        assert_eq!(
            decoded.packages[b"ZLIB".as_slice()].cached_dir,
            Some("${home}/vcpkg/zlib".into())
        );
        assert_eq!(
            decoded.packages[b"ZLIB".as_slice()].observed[0].selected,
            Some("${home}/vcpkg/zlib/ZLIBConfig.cmake".into())
        );

        // Comparing decoded snapshot against live unnormalized original with default CacheSet::Key
        // should report zero differences and preserve exit code 2 for unavailable Link.
        let diff = compare(&decoded, &original, &DiffOptions::default());
        assert!(!diff.has_changes(), "{diff:?}");
        assert_eq!(diff.exit_code(), 2);

        // Re-encoding a decoded snapshot produces byte-identical JSON.
        let reencoded = encode_snapshot(&decoded, &SaveOptions::default()).expect("reencode");
        assert_eq!(encoded, reencoded);
    }

    #[test]
    fn repeated_saves_are_byte_identical_and_lf_only() {
        let mut snap1 = rich_snapshot();
        let mut snap2 = rich_snapshot();
        snap1.toolchains.push(ToolchainSnapshot {
            language: "C".into(),
            path: Some("/usr/bin/cc".into()),
            ..ToolchainSnapshot::default()
        });
        snap2.toolchains.insert(
            0,
            ToolchainSnapshot {
                language: "C".into(),
                path: Some("/usr/bin/cc".into()),
                ..ToolchainSnapshot::default()
            },
        );
        let a = encode_snapshot(&snap1, &SaveOptions::default()).expect("a");
        let b = encode_snapshot(&snap2, &SaveOptions::default()).expect("b");
        assert_eq!(a, b);
        assert!(!a.contains(&b'\r'));
        assert!(a.ends_with(b"\n"));
    }

    #[test]
    fn cache_options_control_secret_retention_and_none_evidence() {
        let snap = rich_snapshot();

        let all_bytes = encode_snapshot(
            &snap,
            &SaveOptions {
                cache: CacheSet::All,
            },
        )
        .expect("all");
        let all = decode_snapshot(&all_bytes, "all.json".into()).expect("decode all");
        assert!(all.cache.contains_key(b"API_KEY".as_slice()));
        assert_eq!(all.saved_cache_set, Some(CacheSet::All));

        let none_bytes = encode_snapshot(
            &snap,
            &SaveOptions {
                cache: CacheSet::None,
            },
        )
        .expect("none");
        let none = decode_snapshot(&none_bytes, "none.json".into()).expect("decode none");
        assert!(none.cache.is_empty());
        assert_eq!(none.saved_cache_set, Some(CacheSet::None));
        assert!(!none.evidence(Section::Cache).comparable());
    }

    #[test]
    fn version_rejection_and_minor_forward_compatibility() {
        let snap = rich_snapshot();
        let bytes = encode_snapshot(&snap, &SaveOptions::default()).expect("encode");
        let mut value = rcmake_json::parse(&bytes, &ReadOptions::default()).expect("parse");

        // Major 2 must be rejected cleanly.
        let mut v2 = value.clone();
        if let Value::Object(ref mut map) = v2 {
            map.insert(
                BString::from("version"),
                Value::object_from([("major", Value::UInt(2)), ("minor", Value::UInt(0))]),
            );
        }
        let v2_bytes = rcmake_json::write_pretty_utf8(&v2).expect("write");
        assert_eq!(
            decode_snapshot(&v2_bytes, "v2.json".into()),
            Err(SnapshotError::UnsupportedVersion { major: 2, minor: 0 })
        );

        // Minor 7 with additive unknown fields at root and nested levels must decode cleanly.
        if let Value::Object(ref mut map) = value {
            map.insert(
                BString::from("version"),
                Value::object_from([
                    ("major", Value::UInt(1)),
                    ("minor", Value::UInt(7)),
                    ("futureTag", Value::string("beta")),
                ]),
            );
            map.insert(
                BString::from("futureTopLevel"),
                Value::object_from([("nested", Value::Bool(true))]),
            );
            if let Some(Value::Object(producer)) = map.get_mut(b"producer".as_slice()) {
                producer.insert(BString::from("commit"), Value::string("abc123"));
            }
            if let Some(Value::Object(evidence)) = map.get_mut(b"evidence".as_slice()) {
                evidence.insert(
                    BString::from("futureSection"),
                    Value::object_from([("state", Value::string("complete"))]),
                );
            }
        }
        let v17_bytes = rcmake_json::write_pretty_utf8(&value).expect("write");
        let decoded = decode_snapshot(&v17_bytes, "v17.json".into()).expect("minor forward compat");
        assert_eq!(decoded.producer.tool, "rcmake");
    }

    #[test]
    fn rejects_malformed_oversized_deep_and_out_of_bounds_untrusted_inputs() {
        // Empty / truncated / non-JSON / wrong kind.
        assert!(matches!(
            decode_snapshot(b"", "empty".into()),
            Err(SnapshotError::Json(_))
        ));
        assert!(matches!(
            decode_snapshot(b"{\"kind\":", "trunc".into()),
            Err(SnapshotError::Json(_))
        ));
        assert_eq!(
            decode_snapshot(b"[]", "arr".into()),
            Err(SnapshotError::NotASnapshot)
        );
        assert_eq!(
            decode_snapshot(br#"{"kind":"other"}"#, "other".into()),
            Err(SnapshotError::NotASnapshot)
        );

        // Deep nesting beyond MAX_SNAPSHOT_JSON_DEPTH (32).
        let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
        assert!(matches!(
            decode_snapshot(deep.as_bytes(), "deep".into()),
            Err(SnapshotError::Json(_))
        ));

        // Out-of-bounds compileSettings index.
        let bad_index = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": "cmake", "version": null},
            "configs": ["Debug"],
            "compileSettingsTable": [],
            "targets": [{
                "config": "Debug",
                "name": "app",
                "kind": "EXECUTABLE",
                "attribution": "exact",
                "sources": [{"path": "${source}/main.cpp", "compileSettings": [0]}],
                "link": null,
                "dependencies": [],
                "artifacts": [],
                "backtrace": null
            }],
            "toolchains": [],
            "packages": [],
            "cache": [],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(bad_index, "oob".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));

        // Bad hex encoding in bytes_hex.
        let bad_hex = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": {"bytes_hex": "zzz"}, "version": null},
            "configs": [],
            "compileSettingsTable": [],
            "targets": [],
            "toolchains": [],
            "packages": [],
            "cache": [],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(bad_hex, "badhex".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));

        // Oversized input rejected by budget without panicking.
        assert_eq!(
            decode_snapshot_bounded(b"{}", "tiny".into(), 1, MAX_EXPANDED_COMPILE_BYTES),
            Err(SnapshotError::TooLarge { limit: 1 })
        );

        // Save-side size and expansion budgets reject snapshots that load would reject.
        let sample = rich_snapshot();
        assert_eq!(
            encode_snapshot_bounded(
                &sample,
                &SaveOptions::default(),
                16,
                MAX_EXPANDED_COMPILE_BYTES
            ),
            Err(SnapshotError::TooLarge { limit: 16 })
        );
        assert!(matches!(
            encode_snapshot_bounded(&sample, &SaveOptions::default(), MAX_SNAPSHOT_BYTES, 16),
            Err(SnapshotError::InvalidSchema(_))
        ));

        // Decode-side expanded CompileSettings amplification budget blocks small JSON bombs.
        let encoded = encode_snapshot(&sample, &SaveOptions::default()).expect("encode");
        assert!(matches!(
            decode_snapshot_bounded(&encoded, "amp.json".into(), MAX_SNAPSHOT_BYTES, 64),
            Err(SnapshotError::InvalidSchema(_))
        ));

        // Strict bytes_hex wrapper must reject extra fields alongside bytes_hex.
        let extra_hex_field = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": {"bytes_hex": "6162", "extra": 1}, "version": null},
            "configs": [],
            "compileSettingsTable": [],
            "targets": [],
            "toolchains": [],
            "packages": [],
            "cache": [],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(extra_hex_field, "extra_hex".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));

        // Duplicate target / source / package / cache keys must be rejected.
        let dup_cache = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": "cmake", "version": null},
            "configs": [],
            "compileSettingsTable": [],
            "targets": [],
            "toolchains": [],
            "packages": [],
            "cache": [
                {"name": "A", "type": "STRING", "value": "1", "help": ""},
                {"name": "A", "type": "STRING", "value": "2", "help": ""}
            ],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(dup_cache, "dup_cache".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));
        let dup_pkg = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": "cmake", "version": null},
            "configs": [],
            "compileSettingsTable": [],
            "targets": [],
            "toolchains": [],
            "packages": [
                {"name": "Z", "cachedDir": null, "versionHint": null, "observed": []},
                {"name": "Z", "cachedDir": null, "versionHint": null, "observed": []}
            ],
            "cache": [],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(dup_pkg, "dup_pkg".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));
        let dup_source = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": "cmake", "version": null},
            "configs": ["Debug"],
            "compileSettingsTable": [],
            "targets": [{
                "config": "Debug",
                "name": "app",
                "kind": "EXECUTABLE",
                "attribution": "exact",
                "sources": [
                    {"path": "${source}/a.cpp", "compileSettings": []},
                    {"path": "${source}/a.cpp", "compileSettings": []}
                ],
                "link": null,
                "dependencies": [],
                "artifacts": [],
                "backtrace": null
            }],
            "toolchains": [],
            "packages": [],
            "cache": [],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(dup_source, "dup_source".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));
        let dup_target = br#"{
            "kind": "rcmake-config-snapshot",
            "version": {"major": 1, "minor": 0},
            "producer": {"tool": "cmake", "version": null},
            "configs": ["Debug"],
            "compileSettingsTable": [],
            "targets": [
                {
                    "config": "Debug", "name": "app", "kind": "EXECUTABLE", "attribution": "exact",
                    "sources": [], "link": null, "dependencies": [], "artifacts": [], "backtrace": null
                },
                {
                    "config": "Debug", "name": "app", "kind": "EXECUTABLE", "attribution": "exact",
                    "sources": [], "link": null, "dependencies": [], "artifacts": [], "backtrace": null
                }
            ],
            "toolchains": [],
            "packages": [],
            "cache": [],
            "evidence": {}
        }"#;
        assert!(matches!(
            decode_snapshot(dup_target, "dup_target".into()),
            Err(SnapshotError::InvalidSchema(_))
        ));

        // Duplicate keys and JSON comments are rejected under strict snapshot parsing.
        assert!(matches!(
            decode_snapshot(
                br#"{"kind":"rcmake-config-snapshot","kind":"rcmake-config-snapshot"}"#,
                "dup".into()
            ),
            Err(SnapshotError::Json(_))
        ));
        assert!(matches!(
            decode_snapshot(b"// comment\n{}", "comment".into()),
            Err(SnapshotError::Json(_))
        ));
    }

    #[test]
    fn cross_os_windows_and_unix_snapshots_compare_equal() {
        let mut win = rich_snapshot();
        win.build_root = r"C:\Users\winuser\repo\build".into();
        win.source_root = Some(r"C:\Users\winuser\repo".into());
        win.home_root = Some(r"C:\Users\winuser".into());
        win.host = Some(SnapshotHost {
            os: "Windows".into(),
            arch: "x86_64".into(),
        });
        win.targets.clear();
        let win_settings = Arc::new(CompileSettings {
            language: Some("CXX".into()),
            standard: Some("20".into()),
            defines: vec![r#"SRC_DIR="C:\Users\winuser\repo\data""#.into()],
            includes: vec![
                IncludeDir {
                    path: r"C:\Users\winuser\repo\include".into(),
                    system: false,
                },
                IncludeDir {
                    path: r"C:\Users\winuser\sdk\inc".into(),
                    system: true,
                },
            ],
            fragments: vec!["-O2".into(), r"-IC:\Users\winuser\repo\build\gen".into()],
            opaque: false,
            compiler: Some(r"C:\Users\winuser\llvm\bin\clang++".into()),
            launcher: Vec::new(),
        });
        let mut win_sources = BTreeMap::new();
        win_sources.insert(
            BString::from(r"C:\Users\winuser\repo\src\a.cpp"),
            vec![win_settings],
        );
        win.targets.insert(
            (BString::from("Debug"), BString::from("core")),
            TargetSnapshot {
                kind: Some("STATIC_LIBRARY".into()),
                attribution: Attribution::Exact,
                sources: win_sources,
                link: Some(vec![LinkFragment {
                    role: "libraries".into(),
                    fragment: r"C:\Users\winuser\repo\build\lib\dep.a".into(),
                }]),
                dependencies: BTreeSet::new(),
                artifacts: vec![r"C:\Users\winuser\repo\build\lib\core.a".into()],
                backtrace: Some(r"C:\Users\winuser\repo\CMakeLists.txt:14".into()),
            },
        );
        win.toolchains = vec![ToolchainSnapshot {
            language: "CXX".into(),
            compiler_id: Some("Clang".into()),
            version: Some("16.0.0".into()),
            path: Some(r"C:\Users\winuser\llvm\bin\clang++".into()),
            target: Some("x86_64-pc-windows".into()),
            frontend: Some("GNU".into()),
        }];
        win.packages.clear();
        win.packages.insert(
            BString::from("ZLIB"),
            PackageSnapshot {
                cached_dir: Some(r"C:\Users\winuser\vcpkg\zlib".into()),
                version_hint: Some("1.3.1".into()),
                observed: vec![PackageObservation {
                    package_call: Some(1),
                    outcome: "found".into(),
                    version: Some("1.3.1".into()),
                    selected: Some(r"C:\Users\winuser\vcpkg\zlib\ZLIBConfig.cmake".into()),
                }],
            },
        );
        win.cache.clear();
        win.cache.insert(
            BString::from("CMAKE_TOOLCHAIN_FILE"),
            CacheRecord {
                ty: "FILEPATH".into(),
                value: r"C:\Users\winuser\vcpkg\toolchain.cmake".into(),
                help: "Toolchain".into(),
            },
        );
        win.evidence
            .insert(Section::Link, Evidence::Complete(DataSource::FileApi));
        win.evidence
            .insert(Section::Packages, Evidence::Complete(DataSource::FindTrace));
        win.packages_unknown = false;
        win.warnings.clear();

        let mut unix = win.clone();
        unix.build_root = "/Users/macuser/repo/build".into();
        unix.source_root = Some("/Users/macuser/repo".into());
        unix.home_root = Some("/Users/macuser".into());
        unix.host = Some(SnapshotHost {
            os: "Darwin".into(),
            arch: "arm64".into(),
        });
        unix.targets.clear();
        let unix_settings = Arc::new(CompileSettings {
            language: Some("CXX".into()),
            standard: Some("20".into()),
            defines: vec![r#"SRC_DIR="/Users/macuser/repo/data""#.into()],
            includes: vec![
                IncludeDir {
                    path: "/Users/macuser/repo/include".into(),
                    system: false,
                },
                IncludeDir {
                    path: "/Users/macuser/sdk/inc".into(),
                    system: true,
                },
            ],
            fragments: vec!["-O2".into(), "-I/Users/macuser/repo/build/gen".into()],
            opaque: false,
            compiler: Some("/Users/macuser/llvm/bin/clang++".into()),
            launcher: Vec::new(),
        });
        let mut unix_sources = BTreeMap::new();
        unix_sources.insert(
            BString::from("/Users/macuser/repo/src/a.cpp"),
            vec![unix_settings],
        );
        unix.targets.insert(
            (BString::from("Debug"), BString::from("core")),
            TargetSnapshot {
                kind: Some("STATIC_LIBRARY".into()),
                attribution: Attribution::Exact,
                sources: unix_sources,
                link: Some(vec![LinkFragment {
                    role: "libraries".into(),
                    fragment: "/Users/macuser/repo/build/lib/dep.a".into(),
                }]),
                dependencies: BTreeSet::new(),
                artifacts: vec!["/Users/macuser/repo/build/lib/core.a".into()],
                backtrace: Some("/Users/macuser/repo/CMakeLists.txt:14".into()),
            },
        );
        unix.toolchains[0].path = Some("/Users/macuser/llvm/bin/clang++".into());
        unix.toolchains[0].target = Some("x86_64-pc-windows".into());
        unix.packages
            .get_mut(b"ZLIB".as_slice())
            .expect("pkg")
            .cached_dir = Some("/Users/macuser/vcpkg/zlib".into());
        unix.packages
            .get_mut(b"ZLIB".as_slice())
            .expect("pkg")
            .observed[0]
            .selected = Some("/Users/macuser/vcpkg/zlib/ZLIBConfig.cmake".into());
        unix.cache
            .get_mut(b"CMAKE_TOOLCHAIN_FILE".as_slice())
            .expect("cache")
            .value = "/Users/macuser/vcpkg/toolchain.cmake".into();

        let win_bytes = encode_snapshot(&win, &SaveOptions::default()).expect("win encode");
        let unix_bytes = encode_snapshot(&unix, &SaveOptions::default()).expect("unix encode");
        let win_loaded = decode_snapshot(&win_bytes, "win.snap".into()).expect("win decode");
        let unix_loaded = decode_snapshot(&unix_bytes, "unix.snap".into()).expect("unix decode");

        let diff = compare(&win_loaded, &unix_loaded, &DiffOptions::default());
        assert!(!diff.has_changes(), "{diff:?}");
        assert_eq!(diff.exit_code(), 0);
    }

    #[test]
    fn atomic_save_and_load_via_host_creates_parents_and_overwrites() {
        let host = PureHost::default();
        let snap = rich_snapshot();
        let path = Path::new("/out/nested/dir/baseline");
        save_snapshot_file(&host, path, &snap, &SaveOptions::default()).expect("save first");
        let first_bytes = host
            .read(path, ReadRange::All { limit: 1 << 20 })
            .expect("read");
        save_snapshot_file(&host, path, &snap, &SaveOptions::default()).expect("overwrite");
        let second_bytes = host
            .read(path, ReadRange::All { limit: 1 << 20 })
            .expect("read2");
        assert_eq!(first_bytes, second_bytes);

        let loaded = load_snapshot_file(&host, path).expect("load");
        assert_eq!(loaded.path, "/out/nested/dir/baseline");
        assert!(loaded.is_normalized);
    }
}
