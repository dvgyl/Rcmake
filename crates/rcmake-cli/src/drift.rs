//! Stale `CMakeCache.txt` toolchain and prefix drift detection for Native `rcmake`.
//!
//! A native configure that completes generation writes a receipt to
//! `CMakeFiles/rcmake/toolchain-inputs-v1.json`. The receipt records the
//! toolchain file, the `CMAKE_PREFIX_PATH` inputs, and for each enabled
//! language the compiler identity that produced the saved detection results in
//! `CMake<LANG>Compiler.cmake`.
//!
//! A later configure compares the current inputs with the receipt and warns
//! about each changed field. The baseline advances only when the evidence is
//! new: a fresh configure, or a compiler identification during this configure.
//! `enable_language()` writes an identification record next to each compiler
//! file that it identifies. The tracker removes old records before the configure
//! runs, so a record at the end proves an identification in this configure. A
//! warm configure that loads saved detection results, or that only rewrites a
//! compiler file, keeps the old baseline. The drift stays visible until a
//! configure identifies the compiler again.

#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, ReadRange, path};
use rcmake_json::{
    FieldError, FromJson, ReadOptions, ToJson, Value, deny_unknown, object, opt, parse, req,
    write_styled,
};
use rcmake_model::Cache;
use rcmake_store::Digest;

/// Path of the receipt relative to the build directory.
pub(crate) const RECEIPT: &str = "CMakeFiles/rcmake/toolchain-inputs-v1.json";
const VERSION: u32 = 1;
const RECEIPT_LIMIT: u64 = 1024 * 1024;
const COMPILER_FILE_LIMIT: u64 = 1024 * 1024;
const IDENTIFICATION_LIMIT: u64 = 4096;
const READ_OPTIONS: ReadOptions = ReadOptions {
    allow_comments: false,
    dup_keys_last_wins: false,
};

/// A tool file named by a cache entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Tool {
    /// The value as given in the cache.
    supplied: BString,
    /// The absolute path without symbolic links, or `None` if the file is missing.
    resolved: Option<BString>,
    /// SHA-256 of the file content, or `None` if it could not be read.
    digest: Option<Digest>,
}

/// The compiler of one enabled language.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Compiler {
    tool: Tool,
    launcher: Option<BString>,
    id: BString,
    version: BString,
    /// SHA-256 of the `CMake<LANG>Compiler.cmake` file.
    evidence: Option<Digest>,
    /// Whether `tool.digest` was recorded when the compiler was identified.
    identified: bool,
}

/// Current or recorded toolchain inputs of one build tree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Facts {
    toolchain: Option<Tool>,
    prefix_cache: Option<Vec<BString>>,
    prefix_env: Option<Vec<BString>>,
    languages: BTreeMap<String, Compiler>,
}

/// A receipt that belongs to one build tree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Receipt {
    binary: BString,
    source: BString,
    facts: Facts,
}

/// State of the receipt file of a build tree.
#[derive(Debug)]
pub(crate) enum Baseline {
    /// No receipt exists. The tree is new or was configured without a native receipt.
    Missing,
    /// The receipt exists but cannot be used.
    Bad(String),
    /// A valid receipt for this build tree.
    Loaded(Receipt),
}

/// One changed field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Drift {
    /// Stable key that identifies the field.
    key: String,
    /// Variable and origin, for example `CMAKE_PREFIX_PATH (environment)`.
    field: String,
    /// Kind of change.
    change: &'static str,
    previous: String,
    current: String,
}

impl Drift {
    fn render(&self) -> String {
        format!(
            "{} {}.\n  Previous: {}\n  Current: {}",
            self.field, self.change, self.previous, self.current
        )
    }
}

// JSON encoding.

fn digest_json(digest: Option<Digest>) -> Value {
    digest.map_or(Value::Null, |digest| digest.to_json())
}

impl ToJson for Tool {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("supplied", self.supplied.to_json()),
            ("resolved", self.resolved.to_json()),
            ("sha256", digest_json(self.digest)),
        ])
    }
}

impl FromJson for Tool {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(obj, &["supplied", "resolved", "sha256"])?;
        Ok(Self {
            supplied: req(obj, "supplied")?,
            resolved: opt(obj, "resolved")?.flatten(),
            digest: opt(obj, "sha256")?.flatten(),
        })
    }
}

impl ToJson for Compiler {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("compiler", self.tool.to_json()),
            ("launcher", self.launcher.to_json()),
            ("id", self.id.to_json()),
            ("version", self.version.to_json()),
            ("evidence_sha256", digest_json(self.evidence)),
            ("identified", self.identified.to_json()),
        ])
    }
}

impl FromJson for Compiler {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(
            obj,
            &[
                "compiler",
                "launcher",
                "id",
                "version",
                "evidence_sha256",
                "identified",
            ],
        )?;
        Ok(Self {
            tool: req(obj, "compiler")?,
            launcher: opt(obj, "launcher")?.flatten(),
            id: req(obj, "id")?,
            version: req(obj, "version")?,
            evidence: opt(obj, "evidence_sha256")?.flatten(),
            identified: req(obj, "identified")?,
        })
    }
}

impl ToJson for Receipt {
    fn to_json(&self) -> Value {
        let languages = self
            .facts
            .languages
            .iter()
            .map(|(name, compiler)| (name.as_str(), compiler.to_json()));
        Value::object_from([
            ("version", VERSION.to_json()),
            ("binary_dir", self.binary.to_json()),
            ("source_dir", self.source.to_json()),
            ("toolchain_file", self.facts.toolchain.to_json()),
            (
                "prefix_path",
                Value::object_from([
                    ("cache", self.facts.prefix_cache.to_json()),
                    ("environment", self.facts.prefix_env.to_json()),
                ]),
            ),
            ("languages", Value::object_from(languages)),
        ])
    }
}

impl FromJson for Receipt {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(
            obj,
            &[
                "version",
                "binary_dir",
                "source_dir",
                "toolchain_file",
                "prefix_path",
                "languages",
            ],
        )?;
        let version: u32 = req(obj, "version")?;
        if version != VERSION {
            return Err(FieldError::invalid(format!(
                "unsupported version {version}"
            )));
        }
        let prefix = object(rcmake_json::req_value(obj, "prefix_path")?)?;
        deny_unknown(prefix, &["cache", "environment"])?;
        Ok(Self {
            binary: req(obj, "binary_dir")?,
            source: req(obj, "source_dir")?,
            facts: Facts {
                toolchain: opt(obj, "toolchain_file")?.flatten(),
                prefix_cache: opt(prefix, "cache")?.flatten(),
                prefix_env: opt(prefix, "environment")?.flatten(),
                languages: req(obj, "languages")?,
            },
        })
    }
}

// Reading the build tree.

fn receipt_path(bin: &Path) -> PathBuf {
    bin.join(path::to_native(RECEIPT.as_bytes()))
}

fn read_bounded(host: &dyn Host, file: &Path, limit: u64) -> Option<Vec<u8>> {
    let meta = host.metadata(file, Follow::No).ok()?;
    if meta.kind != FileKind::File || meta.len > limit {
        return None;
    }
    host.read(file, ReadRange::All { limit }).ok()
}

/// Load the receipt of `bin` and check that it belongs to `bin` and `source`.
pub(crate) fn load(host: &dyn Host, bin: &Path, source: &Path) -> (Baseline, Option<Vec<u8>>) {
    let file = receipt_path(bin);
    match host.metadata(&file, Follow::No) {
        Err(error) if error.kind == std::io::ErrorKind::NotFound => {
            return (Baseline::Missing, None);
        }
        Err(error) => return (Baseline::Bad(error.to_string()), None),
        Ok(meta) if meta.kind != FileKind::File => {
            return (
                Baseline::Bad("The receipt is not a regular file.".into()),
                None,
            );
        }
        Ok(meta) if meta.len > RECEIPT_LIMIT => {
            return (Baseline::Bad("The receipt is too large.".into()), None);
        }
        Ok(_) => {}
    }
    let Some(bytes) = read_bounded(host, &file, RECEIPT_LIMIT) else {
        return (Baseline::Bad("The receipt cannot be read.".into()), None);
    };
    let receipt = parse(&bytes, &READ_OPTIONS)
        .map_err(|error| error.to_string())
        .and_then(|value| Receipt::from_json(&value).map_err(|error| error.to_string()));
    let same = |recorded: &BString, actual: &Path| {
        *recorded == path::from_native(actual) || {
            let recorded = host.canonicalize(&path::to_native(recorded)).ok();
            recorded.is_some() && recorded == host.canonicalize(actual).ok()
        }
    };
    let baseline = match receipt {
        Err(error) => Baseline::Bad(format!("The receipt is invalid: {error}")),
        Ok(receipt) if !same(&receipt.binary, bin) => {
            Baseline::Bad("The receipt belongs to another build directory.".into())
        }
        Ok(receipt) if !source.as_os_str().is_empty() && !same(&receipt.source, source) => {
            Baseline::Bad("The receipt belongs to another source directory.".into())
        }
        Ok(receipt) => Baseline::Loaded(receipt),
    };
    (baseline, Some(bytes))
}

/// Directory with the saved compiler files for this rcmake version.
fn platform_dir(bin: &Path) -> PathBuf {
    bin.join("CMakeFiles").join(rcmake_base::CMAKE_VERSION_STR)
}

/// Return the language name of a `CMake<LANG>Compiler.cmake` file name.
fn compiler_language(name: &[u8]) -> Option<String> {
    let lang = name
        .strip_prefix(b"CMake")?
        .strip_suffix(b"Compiler.cmake")?;
    (!lang.is_empty()
        && lang
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_'))
    .then(|| String::from_utf8_lossy(lang).into_owned())
}

/// Return the language name of a `CMake<LANG>Compiler.identification-v1.json` file name.
fn identification_language(name: &[u8]) -> Option<String> {
    let base = name.strip_suffix(b".identification-v1.json")?;
    compiler_language(&[base, b".cmake"].concat())
}

/// One record that `enable_language()` wrote when it identified a compiler.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Identification {
    /// SHA-256 of the compiler file that the identification wrote.
    evidence: Digest,
}

impl FromJson for Identification {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(obj, &["version", "evidence_sha256"])?;
        let version: u32 = req(obj, "version")?;
        if version != 1 {
            return Err(FieldError::invalid(format!(
                "unsupported version {version}"
            )));
        }
        Ok(Self {
            evidence: req(obj, "evidence_sha256")?,
        })
    }
}

/// The identification record files of `bin` by language, with their bytes.
fn identification_files(host: &dyn Host, bin: &Path) -> BTreeMap<String, (PathBuf, Vec<u8>)> {
    let dir = platform_dir(bin);
    let Ok(entries) = host.read_dir(&dir) else {
        return BTreeMap::new();
    };
    entries
        .into_iter()
        .filter(|entry| entry.kind == FileKind::File)
        .filter_map(|entry| {
            let lang = identification_language(&entry.name)?;
            let file = dir.join(path::to_native(&entry.name));
            let bytes = read_bounded(host, &file, IDENTIFICATION_LIMIT)?;
            Some((lang, (file, bytes)))
        })
        .collect()
}

/// Remove the identification records of earlier configures.
///
/// Return the bytes of each record that could not be removed, so that it does
/// not count as new evidence.
fn clear_identifications(host: &dyn Host, bin: &Path) -> BTreeMap<String, Vec<u8>> {
    identification_files(host, bin)
        .into_iter()
        .filter(|(_, (file, _))| {
            host.remove_file(file)
                .is_err_and(|error| error.kind != std::io::ErrorKind::NotFound)
        })
        .map(|(lang, (_, bytes))| (lang, bytes))
        .collect()
}

/// Return the languages that this configure identified.
///
/// A language counts only if its record is not left from an earlier configure
/// and still describes the compiler file that `current` loaded.
fn identified_languages(
    host: &dyn Host,
    bin: &Path,
    stale: &BTreeMap<String, Vec<u8>>,
    current: &Facts,
) -> BTreeSet<String> {
    let records = identification_files(host, bin);
    current
        .languages
        .iter()
        .filter(|(lang, compiler)| {
            records.get(*lang).is_some_and(|(_, bytes)| {
                stale.get(*lang) != Some(bytes)
                    && parse(bytes, &READ_OPTIONS)
                        .ok()
                        .and_then(|value| Identification::from_json(&value).ok())
                        .is_some_and(|record| compiler.evidence == Some(record.evidence))
            })
        })
        .map(|(lang, _)| lang.clone())
        .collect()
}

/// Read `set(<name> "<value>")` lines from a saved compiler file.
fn compiler_file_values(text: &[u8]) -> BTreeMap<BString, BString> {
    let mut values = BTreeMap::new();
    for line in text.split(|&byte| byte == b'\n') {
        let Some(rest) = line.trim_ascii().strip_prefix(b"set(") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(b")") else {
            continue;
        };
        let Some(split) = rest.iter().position(|byte| byte.is_ascii_whitespace()) else {
            continue;
        };
        let (name, value) = rest.split_at(split);
        let value = value.trim_ascii();
        let value = value
            .strip_prefix(b"\"")
            .and_then(|value| value.strip_suffix(b"\""))
            .unwrap_or(value);
        values.insert(BString::from(name), BString::from(value));
    }
    values
}

/// Digests of tool files for one collection.
///
/// Each phase makes a new memo, so a digest never outlives the phase that read it.
#[derive(Default)]
pub(crate) struct DigestMemo(RefCell<BTreeMap<PathBuf, Option<Digest>>>);

impl DigestMemo {
    fn digest(&self, host: &dyn Host, file: &Path) -> Option<Digest> {
        if let Some(digest) = self.0.borrow().get(file) {
            return *digest;
        }
        let digest = rcmake_store::file_digest(host, file).ok();
        self.0.borrow_mut().insert(file.to_owned(), digest);
        digest
    }
}

/// Resolve a tool path from the cache without running it.
fn tool(host: &dyn Host, memo: &DigestMemo, value: &[u8], bases: &[&Path]) -> Tool {
    let native = path::to_native(value);
    let found = if native.is_absolute() {
        Some(native)
    } else if native.components().count() == 1
        && let Some(found) = std::str::from_utf8(value)
            .ok()
            .and_then(|name| path::executable_on_absolute_path(host, name))
    {
        Some(found)
    } else {
        bases
            .iter()
            .map(|base| base.join(&native))
            .find(|candidate| host.metadata(candidate, Follow::Yes).is_ok())
    };
    let resolved = found
        .and_then(|file| host.canonicalize(&file).ok())
        .filter(|file| {
            host.metadata(file, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        });
    Tool {
        supplied: value.into(),
        digest: resolved.as_deref().and_then(|file| memo.digest(host, file)),
        resolved: resolved.as_deref().map(path::from_native),
    }
}

fn split_list(value: &[u8], separator: u8) -> Vec<BString> {
    if value.is_empty() {
        return Vec::new();
    }
    value
        .split(|&byte| byte == separator)
        .map(BString::from)
        .collect()
}

/// Collect the current facts of `bin` from `cache`, the environment, and the saved
/// compiler files. Tools are read and hashed. No program is run.
pub(crate) fn collect(host: &dyn Host, memo: &DigestMemo, bin: &Path, cache: &Cache) -> Facts {
    let value = |name: &[u8]| cache.get(name).map(|entry| entry.value.clone());
    let source = value(b"CMAKE_HOME_DIRECTORY")
        .map(|dir| path::to_native(&dir))
        .unwrap_or_default();
    let toolchain = value(b"CMAKE_TOOLCHAIN_FILE")
        .filter(|file| !file.is_empty())
        .map(|file| tool(host, memo, &file, &[bin, &source]));
    let env_separator = if host.system().sysname == "Windows" {
        b';'
    } else {
        b':'
    };
    let mut languages = BTreeMap::new();
    let dir = platform_dir(bin);
    for entry in host.read_dir(&dir).unwrap_or_default() {
        let Some(lang) = compiler_language(&entry.name).filter(|_| entry.kind == FileKind::File)
        else {
            continue;
        };
        let Some(text) = read_bounded(
            host,
            &dir.join(path::to_native(&entry.name)),
            COMPILER_FILE_LIMIT,
        ) else {
            continue;
        };
        let values = compiler_file_values(&text);
        let var = |suffix: &str| BString::from(format!("CMAKE_{lang}_COMPILER{suffix}"));
        let from_file = |suffix: &str| values.get(&var(suffix)).cloned().unwrap_or_default();
        let selected = value(&var("")).unwrap_or_else(|| from_file(""));
        if selected.is_empty() {
            continue;
        }
        let launcher = value(format!("CMAKE_{lang}_COMPILER_LAUNCHER").as_bytes())
            .filter(|launcher| !launcher.is_empty());
        languages.insert(
            lang.clone(),
            Compiler {
                tool: tool(host, memo, &selected, &[]),
                launcher,
                id: from_file("_ID"),
                version: from_file("_VERSION"),
                evidence: Some(Digest::hash(&text)),
                identified: false,
            },
        );
    }
    Facts {
        toolchain,
        prefix_cache: value(b"CMAKE_PREFIX_PATH").map(|list| split_list(&list, b';')),
        prefix_env: host
            .env_get(b"CMAKE_PREFIX_PATH")
            .map(|list| split_list(&list, env_separator)),
        languages,
    }
}

// Comparison.

fn short(digest: Option<Digest>) -> String {
    digest.map_or_else(
        || "unreadable".to_owned(),
        |digest| {
            let hex = digest.to_hex();
            format!("sha256 {}", hex.get(..12).unwrap_or(&hex))
        },
    )
}

fn shown(value: Option<&BString>) -> String {
    value.map_or_else(|| "missing".to_owned(), ToString::to_string)
}

fn list(value: Option<&Vec<BString>>) -> String {
    match value {
        None => "not set".to_owned(),
        Some(items) if items.is_empty() => "empty".to_owned(),
        Some(items) => items
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(";"),
    }
}

fn compare_tool(drifts: &mut Vec<Drift>, key: &str, field: &str, previous: &Tool, current: &Tool) {
    let mut push = |suffix: &str, change: &'static str, before: String, after: String| {
        drifts.push(Drift {
            key: format!("{key}.{suffix}"),
            field: field.to_owned(),
            change,
            previous: before,
            current: after,
        });
    };
    if previous.supplied != current.supplied {
        push(
            "supplied",
            "selection changed",
            previous.supplied.to_string(),
            current.supplied.to_string(),
        );
    } else if current.resolved.is_none() {
        push(
            "missing",
            "file is missing",
            shown(previous.resolved.as_ref()),
            "missing".to_owned(),
        );
    } else if previous.resolved != current.resolved {
        push(
            "resolved",
            "now resolves to another file",
            shown(previous.resolved.as_ref()),
            shown(current.resolved.as_ref()),
        );
    } else if previous.digest.is_some() && previous.digest != current.digest {
        push(
            "content",
            "content changed",
            format!(
                "{} {}",
                shown(previous.resolved.as_ref()),
                short(previous.digest)
            ),
            format!(
                "{} {}",
                shown(current.resolved.as_ref()),
                short(current.digest)
            ),
        );
    }
}

/// Compare the recorded baseline with the current facts. Skip the languages in `skip`.
pub(crate) fn compare(previous: &Facts, current: &Facts, skip: &BTreeSet<String>) -> Vec<Drift> {
    let mut drifts = Vec::new();
    match (&previous.toolchain, &current.toolchain) {
        (Some(before), Some(after)) => {
            compare_tool(
                &mut drifts,
                "toolchain",
                "CMAKE_TOOLCHAIN_FILE (cache)",
                before,
                after,
            );
        }
        (None, None) => {}
        (before, after) => drifts.push(Drift {
            key: "toolchain.supplied".into(),
            field: "CMAKE_TOOLCHAIN_FILE (cache)".into(),
            change: "selection changed",
            previous: before
                .as_ref()
                .map_or_else(|| "not set".into(), |tool| tool.supplied.to_string()),
            current: after
                .as_ref()
                .map_or_else(|| "not set".into(), |tool| tool.supplied.to_string()),
        }),
    }
    for (key, origin, before, after) in [
        (
            "prefix.cache",
            "cache",
            &previous.prefix_cache,
            &current.prefix_cache,
        ),
        (
            "prefix.environment",
            "environment",
            &previous.prefix_env,
            &current.prefix_env,
        ),
    ] {
        if before != after {
            let reordered = match (before, after) {
                (Some(before), Some(after)) => {
                    let mut left = before.clone();
                    let mut right = after.clone();
                    left.sort();
                    right.sort();
                    left == right
                }
                _ => false,
            };
            drifts.push(Drift {
                key: key.into(),
                field: format!("CMAKE_PREFIX_PATH ({origin})"),
                change: if reordered {
                    "order changed"
                } else {
                    "changed"
                },
                previous: list(before.as_ref()),
                current: list(after.as_ref()),
            });
        }
    }
    for (lang, before) in &previous.languages {
        let Some(after) = current.languages.get(lang).filter(|_| !skip.contains(lang)) else {
            continue;
        };
        let field = format!("CMAKE_{lang}_COMPILER (cache)");
        let count = drifts.len();
        compare_tool(
            &mut drifts,
            &format!("lang.{lang}"),
            &field,
            &before.tool,
            &after.tool,
        );
        if let Some(drift) = drifts.get_mut(count..).and_then(<[Drift]>::first_mut) {
            if drift.key.ends_with(".content") {
                drift.change = "content changed after compiler detection";
                let _ = write!(
                    drift.current,
                    "; the saved detection results still describe {} {}",
                    before.id, before.version
                );
            }
        } else if before.version != after.version || before.id != after.id {
            drifts.push(Drift {
                key: format!("lang.{lang}.version"),
                field: format!("CMAKE_{lang}_COMPILER_VERSION (CMake{lang}Compiler.cmake)"),
                change: "detected compiler changed",
                previous: format!("{} {}", before.id, before.version),
                current: format!("{} {}", after.id, after.version),
            });
        }
        if before.launcher != after.launcher {
            drifts.push(Drift {
                key: format!("lang.{lang}.launcher"),
                field: format!("CMAKE_{lang}_COMPILER_LAUNCHER (cache)"),
                change: "changed",
                previous: shown(before.launcher.as_ref()),
                current: shown(after.launcher.as_ref()),
            });
        }
    }
    drifts
}

/// Build the next baseline.
///
/// `fresh` means the configure started without a cache. Each language in
/// `identified` has an identification record from this configure. Other fields
/// keep the previous baseline, so unresolved drift stays visible.
fn advance(
    previous: Option<&Facts>,
    mut current: Facts,
    fresh: bool,
    identified: &BTreeSet<String>,
) -> Facts {
    let Some(previous) = previous.filter(|_| !fresh) else {
        for (lang, compiler) in &mut current.languages {
            compiler.identified = identified.contains(lang);
        }
        return current;
    };
    current.toolchain.clone_from(&previous.toolchain);
    current.prefix_cache.clone_from(&previous.prefix_cache);
    current.prefix_env.clone_from(&previous.prefix_env);
    for (lang, compiler) in &mut current.languages {
        if identified.contains(lang) {
            compiler.identified = true;
        } else if let Some(before) = previous.languages.get(lang) {
            *compiler = before.clone();
        }
    }
    current
}

fn recovery(bin: &Path) -> String {
    format!(
        "Run 'rcmake --fresh -B {}' or configure a new build directory.",
        path::from_native_cmake(bin)
    )
}

// Configure integration.

/// Drift state for one native configure, from directory resolution to the end.
pub(crate) struct Tracker {
    host: Option<Arc<dyn Host>>,
    bin: PathBuf,
    source: PathBuf,
    baseline: Baseline,
    prior_bytes: Option<Vec<u8>>,
    /// Identification records of earlier configures that could not be removed.
    stale_identifications: BTreeMap<String, Vec<u8>>,
    warned: RefCell<BTreeSet<String>>,
    fresh: Cell<bool>,
    pending: RefCell<Option<Receipt>>,
    finished: Cell<bool>,
}

impl Tracker {
    /// A tracker that does nothing.
    pub(crate) fn disabled() -> Self {
        Self::new(
            None,
            Path::new(""),
            Path::new(""),
            (Baseline::Missing, None),
        )
    }

    /// Whether drift tracking is enabled for this configure.
    pub(crate) fn is_enabled(&self) -> bool {
        self.host.is_some()
    }

    fn new(
        host: Option<Arc<dyn Host>>,
        bin: &Path,
        source: &Path,
        (baseline, prior_bytes): (Baseline, Option<Vec<u8>>),
    ) -> Self {
        let stale_identifications = host
            .as_deref()
            .map(|host| clear_identifications(host, bin))
            .unwrap_or_default();
        Self {
            finished: Cell::new(host.is_none()),
            host,
            bin: bin.to_owned(),
            source: source.to_owned(),
            baseline,
            prior_bytes,
            stale_identifications,
            warned: RefCell::new(BTreeSet::new()),
            fresh: Cell::new(false),
            pending: RefCell::new(None),
        }
    }

    /// Read the receipt before any cache cleanup, and remove the identification
    /// records of earlier configures.
    ///
    /// Return a disabled tracker for the compatibility persona, oracle mode, and
    /// cache views.
    pub(crate) fn begin(
        host: &Arc<dyn Host>,
        native: bool,
        view: bool,
        bin: &Path,
        source: &Path,
    ) -> Self {
        if !native || view || crate::persona::oracle_mode(host.as_ref()) {
            return Self::disabled();
        }
        let loaded = load(host.as_ref(), bin, source);
        Self::new(Some(Arc::clone(host)), bin, source, loaded)
    }

    fn baseline(&self) -> Option<&Facts> {
        match &self.baseline {
            Baseline::Loaded(receipt) => Some(&receipt.facts),
            Baseline::Missing | Baseline::Bad(_) => None,
        }
    }

    fn warn(&self, host: &dyn Host, drifts: Vec<Drift>) {
        let mut warned = self.warned.borrow_mut();
        for drift in drifts {
            if warned.insert(drift.key.clone()) {
                crate::options::warn(
                    host,
                    &format!(
                        "{}\n  The build tree was configured with the previous value. {}",
                        drift.render(),
                        recovery(&self.bin)
                    ),
                );
            }
        }
    }

    /// Compare the incoming cache with the baseline before the configure runs.
    ///
    /// `fresh` is true when the configure starts without a saved cache. A fresh
    /// configure resolves all drift, so it prints no warning.
    pub(crate) fn check(&self, cache: &Cache, fresh: bool) {
        let Some(host) = self.host.as_deref() else {
            return;
        };
        self.fresh.set(fresh);
        if fresh {
            return;
        }
        if let Baseline::Bad(reason) = &self.baseline
            && self.warned.borrow_mut().insert("receipt".into())
        {
            crate::options::warn(
                host,
                &format!("{reason} rcmake replaces {RECEIPT} after this configure."),
            );
        }
        if let Some(baseline) = self.baseline() {
            let current = collect(host, &DigestMemo::default(), &self.bin, cache);
            self.warn(host, compare(baseline, &current, &BTreeSet::new()));
        }
    }

    /// Record the facts after generation completed. Check the final selections again.
    pub(crate) fn generated(&self, cache: &Cache) {
        let Some(host) = self.host.as_deref() else {
            return;
        };
        let current = collect(host, &DigestMemo::default(), &self.bin, cache);
        let identified =
            identified_languages(host, &self.bin, &self.stale_identifications, &current);
        let fresh = self.fresh.get();
        if !fresh && let Some(baseline) = self.baseline() {
            self.warn(host, compare(baseline, &current, &identified));
        }
        let facts = advance(self.baseline(), current, fresh, &identified);
        *self.pending.borrow_mut() = Some(Receipt {
            binary: path::from_native(&self.bin),
            source: path::from_native(&self.source),
            facts,
        });
    }

    /// Publish the new receipt after a successful configure and generation.
    /// Otherwise keep the previous receipt, and restore it if a cache cleanup removed it.
    pub(crate) fn finish(&self, success: bool) {
        let Some(host) = self.host.as_deref() else {
            return;
        };
        if self.finished.replace(true) {
            return;
        }
        let pending = self.pending.borrow_mut().take();
        let file = receipt_path(&self.bin);
        let (bytes, action) = match (success, pending) {
            (true, Some(receipt)) => {
                let mut bytes: Vec<u8> = write_styled(&receipt.to_json(), "  ").into();
                bytes.push(b'\n');
                (bytes, "write")
            }
            _ => match &self.prior_bytes {
                Some(bytes) if host.metadata(&file, Follow::No).is_err() => {
                    (bytes.clone(), "restore")
                }
                _ => return,
            },
        };
        let written = file
            .parent()
            .map_or(Ok(()), |dir| host.create_dir_all(dir))
            .and_then(|()| crate::probe_cache::replace_file(host, &file, &bytes));
        if let Err(error) = written {
            crate::options::warn(
                host,
                &format!(
                    "Cannot {action} the drift receipt {}: {error}",
                    path::from_native_cmake(&file)
                ),
            );
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.finish(false);
    }
}

// Doctor report.

/// Write the drift report for `rcmake doctor -B <bin>`. Return the exit code.
///
/// The report reads the cache, the receipt, and the tool files through the host.
/// It runs no program and no project code.
pub(crate) fn doctor(host: &dyn Host, bin: &Path, text: &mut String) -> i32 {
    let shown_bin = path::from_native_cmake(bin);
    let _ = writeln!(text, "Build directory: {shown_bin}");
    if !host
        .metadata(&bin.join("CMakeCache.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        let _ = writeln!(
            text,
            "Build status: no CMakeCache.txt found. This is not a configured build directory."
        );
        return 1;
    }
    let cache = match crate::cache::load(host, bin) {
        Ok(cache) => cache,
        Err(error) => {
            let _ = writeln!(text, "Build status: CMakeCache.txt cannot be read: {error}");
            return 1;
        }
    };
    let source = cache
        .get(b"CMAKE_HOME_DIRECTORY")
        .map(|entry| path::to_native(&entry.value))
        .unwrap_or_default();
    let memo = DigestMemo::default();
    let current = collect(host, &memo, bin, &cache);
    let mut missing_tools = 0;
    let mut tool_lines = |label: &str, tool: &Tool| {
        let _ = writeln!(
            text,
            "{label}: {} ({})",
            tool.supplied,
            tool.resolved.as_ref().map_or_else(
                || {
                    missing_tools += 1;
                    "missing".to_owned()
                },
                |resolved| format!("{resolved}, {}", short(tool.digest))
            )
        );
    };
    if let Some(toolchain) = &current.toolchain {
        tool_lines("CMAKE_TOOLCHAIN_FILE", toolchain);
    }
    for (lang, compiler) in &current.languages {
        tool_lines(&format!("CMAKE_{lang}_COMPILER"), &compiler.tool);
    }
    let receipt = match load(host, bin, &source).0 {
        Baseline::Missing => {
            let _ = writeln!(
                text,
                "Drift receipt: unavailable. The build directory has no {RECEIPT}.\nBaseline: unavailable for compiler and toolchain content. Paths come from CMakeCache.txt only.\nThe next successful native rcmake configure records a baseline."
            );
            return i32::from(missing_tools > 0);
        }
        Baseline::Bad(reason) => {
            let _ = writeln!(text, "Drift receipt: invalid. {reason}\n{}", recovery(bin));
            return 1;
        }
        Baseline::Loaded(receipt) => receipt,
    };
    let _ = writeln!(text, "Drift receipt: {RECEIPT}");
    for (lang, compiler) in &receipt.facts.languages {
        if !compiler.identified {
            let _ = writeln!(
                text,
                "CMAKE_{lang}_COMPILER: unverified. The baseline was recorded without a compiler identification."
            );
        }
    }
    let drifts = compare(&receipt.facts, &current, &BTreeSet::new());
    for drift in &drifts {
        let _ = writeln!(text, "Drift: {}", drift.render());
    }
    if drifts.is_empty() && missing_tools == 0 {
        text.push_str("Drift: none found.\n");
        return 0;
    }
    if drifts.is_empty() {
        let _ = writeln!(text, "Drift: {missing_tools} tool file(s) are missing.");
    } else {
        let _ = writeln!(
            text,
            "Drift: {} changed field(s) since the last successful configure.",
            drifts.len()
        );
    }
    let _ = writeln!(text, "{}", recovery(bin));
    1
}

#[cfg(test)]
mod tests;
