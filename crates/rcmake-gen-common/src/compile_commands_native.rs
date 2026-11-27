//! Native multi-configuration `compile_commands.json` planning, rendering,
//! atomic publication, and ownership-tracked retirement (Phase 3 §3, P1-14).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, hash::HashAlgo, path_lex::Lexical};
use rcmake_host::{FileKind, Follow, Host, ReadRange, WriteMode, path::from_native};
use rcmake_json::{ParseBudget, ReadOptions, Value, write_styled};
use rcmake_model::TargetId;

use crate::GenError;
use crate::compile_commands::json_string;

const MANIFEST_REL_DIR: &str = "CMakeFiles/rcmake";
const MANIFEST_FILE: &str = "compile-commands-v1.json";
const LOCK_FILE: &str = "compile-commands.lock";
const MANIFEST_SCHEMA_VERSION: u64 = 1;
const MANIFEST_READ_LIMIT: u64 = 4 * 1024 * 1024;
const OWNED_FILE_READ_LIMIT: u64 = 128 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Reserved Windows device names (case-insensitive) that cannot appear as stems.
const RESERVED_WINDOWS_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// How a compile command is represented in a compile database entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandEncoding {
    /// Complete executable and argument vector (`"arguments": [...]`).
    Arguments(Vec<BString>),
    /// Raw dialect-escaped command string (`"command": "..."`).
    Command(BString),
}

/// One validated compile-database row belonging to a single configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompileRow {
    /// Configuration identity (for example `Debug` or `Release`).
    pub config: BString,
    /// Owning target identity.
    pub target: TargetId,
    /// Absolute working directory of the compilation step.
    pub directory: BString,
    /// Absolute source file path.
    pub file: BString,
    /// Absolute object file output path.
    pub output: BString,
    /// Executable plus argument vector when expansion is complete, or raw command string.
    pub invocation: CommandEncoding,
}

/// All compile-database rows belonging to one configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDatabase {
    /// Configuration name.
    pub config: BString,
    /// Compile rows in deterministic generation order.
    pub rows: Vec<CompileRow>,
}

/// Diagnostic describing a target/configuration unit that could not be exported
/// cleanly without guessing or scraping generated project files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncompleteExport {
    /// Affected configuration name.
    pub config: BString,
    /// Affected target name.
    pub target: BString,
    /// Actionable reason why the invocation was omitted.
    pub reason: BString,
}

/// Complete multi-configuration compile database plan produced by a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiConfigCompileDatabase {
    /// Per-configuration databases in declared configuration order.
    pub configs: Vec<ConfigDatabase>,
    /// Index into [`Self::configs`] of the generator's selected default configuration.
    pub default_index: usize,
    /// Incomplete export notices (for example unresolvable IDE macros or missing compilers).
    pub diagnostics: Vec<IncompleteExport>,
}

/// Render a single configuration's compile rows to JSON bytes matching LLVM's
/// compilation database specification.
#[must_use]
pub fn render_config(rows: &[CompileRow]) -> BString {
    if rows.is_empty() {
        return BString::from("[]\n");
    }
    let mut out = BString::from(Vec::with_capacity(rows.len().saturating_mul(256).max(64)));
    out.extend_from_slice(b"[\n");
    for (index, row) in rows.iter().enumerate() {
        if index != 0 {
            out.extend_from_slice(b",\n");
        }
        out.extend_from_slice(b"{\n  \"directory\": ");
        json_string(&mut out, &row.directory);
        match &row.invocation {
            CommandEncoding::Arguments(arguments) => {
                out.extend_from_slice(b",\n  \"arguments\": [");
                for (arg_index, arg) in arguments.iter().enumerate() {
                    if arg_index != 0 {
                        out.extend_from_slice(b", ");
                    }
                    json_string(&mut out, arg);
                }
                out.push(b']');
            }
            CommandEncoding::Command(command) => {
                out.extend_from_slice(b",\n  \"command\": ");
                json_string(&mut out, command);
            }
        }
        out.extend_from_slice(b",\n  \"file\": ");
        json_string(&mut out, &row.file);
        out.extend_from_slice(b",\n  \"output\": ");
        json_string(&mut out, &row.output);
        out.extend_from_slice(b"\n}");
    }
    out.extend_from_slice(b"\n]\n");
    out
}

/// Strip `cmake -E __run_co_compile ... --` wrapper tokens from an argument vector.
#[must_use]
pub fn strip_co_compile_argv(argv: &[BString]) -> &[BString] {
    let mut i = 0;
    while i + 1 < argv.len() {
        let is_e = argv.get(i).is_some_and(|tok| tok.as_slice() == b"-E");
        let is_helper = argv
            .get(i + 1)
            .is_some_and(|tok| tok.as_slice() == b"__run_co_compile");
        if is_e && is_helper {
            if let Some(sep) = argv
                .iter()
                .enumerate()
                .skip(i + 2)
                .find_map(|(idx, tok)| (tok.as_slice() == b"--").then_some(idx))
            {
                return argv.get(sep + 1..).unwrap_or(argv);
            }
            break;
        }
        i += 1;
    }
    argv
}

/// Remove compiler dependency-file generation flags (`-MD`, `-MMD`, `-MT <arg>`,
/// `-MF <arg>`) from an argument vector while keeping all other arguments intact.
#[must_use]
pub fn strip_depfile_argv(argv: &[BString]) -> Vec<BString> {
    let mut out = Vec::with_capacity(argv.len());
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_slice() {
            b"-MD" | b"-MMD" => {}
            b"-MT" | b"-MF" => skip_next = true,
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// Validate and map every configuration in `configs` to its destination filename
/// (`compile_commands_<Config>.json`).
///
/// Rejects empty names, non-UTF-8 names, path separators, traversal components,
/// control or Windows-forbidden characters, reserved device names, and
/// case-insensitive filename collisions before any file is written.
///
/// # Errors
///
/// Returns [`GenError::Invalid`] describing the invalid or colliding configuration name.
pub fn preflight_config_filenames(configs: &[ConfigDatabase]) -> Result<Vec<String>, GenError> {
    if configs.is_empty() {
        return Err(GenError::Invalid(
            "Cannot export multi-configuration compile_commands.json with no configurations."
                .into(),
        ));
    }
    let mut filenames = Vec::with_capacity(configs.len());
    let mut seen_lower: BTreeMap<String, String> = BTreeMap::new();
    for entry in configs {
        let name = std::str::from_utf8(&entry.config).map_err(|_| {
            GenError::Invalid(
                format!(
                    "Cannot export compile_commands.json: configuration name {:?} is not valid UTF-8.",
                    entry.config
                )
                .into(),
            )
        })?;
        validate_config_name(name)?;
        let filename = format!("compile_commands_{name}.json");
        let lower = filename.to_ascii_lowercase();
        if lower == "compile_commands.json" {
            return Err(GenError::Invalid(
                format!(
                    "Cannot export compile_commands.json: configuration name {name:?} collides with compile_commands.json."
                )
                .into(),
            ));
        }
        if let Some(previous) = seen_lower.insert(lower, name.to_owned()) {
            return Err(GenError::Invalid(
                format!(
                    "Cannot export compile_commands.json: configurations {previous:?} and {name:?} collide on case-insensitive filename {filename:?}."
                )
                .into(),
            ));
        }
        filenames.push(filename);
    }
    Ok(filenames)
}

fn validate_config_name(name: &str) -> Result<(), GenError> {
    if name.is_empty() || matches!(name, "." | "..") {
        return Err(GenError::Invalid(
            format!("Cannot export compile_commands.json: invalid configuration name {name:?}.")
                .into(),
        ));
    }
    if name.ends_with(['.', ' ']) || name.starts_with(' ') {
        return Err(GenError::Invalid(
            format!(
                "Cannot export compile_commands.json: configuration name {name:?} has leading/trailing space or trailing dot."
            )
            .into(),
        ));
    }
    if name.bytes().any(|byte| {
        byte < 0x20
            || byte == 0x7f
            || matches!(
                byte,
                b'/' | b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|'
            )
    }) {
        return Err(GenError::Invalid(
            format!(
                "Cannot export compile_commands.json: configuration name {name:?} contains forbidden path or filename characters."
            )
            .into(),
        ));
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if RESERVED_WINDOWS_STEMS.contains(&stem.as_str()) {
        return Err(GenError::Invalid(
            format!(
                "Cannot export compile_commands.json: configuration name {name:?} uses a reserved device name."
            )
            .into(),
        ));
    }
    Ok(())
}

/// Publish all per-configuration compile databases (`compile_commands_<Config>.json`)
/// and replace root `compile_commands.json` with identical bytes to the selected
/// default configuration.
///
/// Preflights all filenames and serializes all payloads before modifying the build tree,
/// tracks owned files and SHA-256 digests under `CMakeFiles/rcmake/compile-commands-v1.json`,
/// retires only unmodified owned files from removed configurations, and supports clean
/// recovery after an interrupted publication.
///
/// # Errors
///
/// Returns [`GenError::Invalid`] if configuration names are invalid or collide, or
/// [`GenError::Write`] if locking, staging, or atomic rename fails.
pub fn publish(
    host: &dyn Host,
    build_dir: &Path,
    db: &MultiConfigCompileDatabase,
) -> Result<(), GenError> {
    let filenames = preflight_config_filenames(&db.configs)?;
    let default_index = if db.default_index < db.configs.len() {
        db.default_index
    } else {
        0
    };
    let default_config = db
        .configs
        .get(default_index)
        .ok_or_else(|| {
            GenError::Invalid("Missing default configuration for compile_commands.json.".into())
        })?
        .config
        .clone();

    // Serialize all per-config databases and compute their digests before any write.
    let mut payloads: Vec<(String, BString, String)> = Vec::with_capacity(db.configs.len() + 1);
    let mut default_bytes = BString::default();
    let mut default_hash = String::new();
    for (index, (entry, filename)) in db.configs.iter().zip(filenames).enumerate() {
        let bytes = render_config(&entry.rows);
        let digest = sha256_hex(&bytes);
        if index == default_index {
            default_bytes = bytes.clone();
            default_hash = digest.clone();
        }
        payloads.push((filename, bytes, digest));
    }

    let mut new_files: BTreeMap<String, String> = BTreeMap::new();
    for (filename, _, digest) in &payloads {
        new_files.insert(filename.clone(), digest.clone());
    }
    new_files.insert("compile_commands.json".to_owned(), default_hash.clone());

    let state_dir = build_dir.join(MANIFEST_REL_DIR);
    host.create_dir_all(&state_dir)
        .map_err(|err| write_error(&state_dir, err))?;
    let _lock = acquire_lock(host, &state_dir)?;

    let manifest_path = state_dir.join(MANIFEST_FILE);
    let prior_owned = read_manifest_owned(host, &manifest_path);

    // Record `state: "publishing"` with both new target hashes and prior owned hashes
    // BEFORE modifying any output file so an interrupted publish can recover cleanly.
    let publishing = Manifest {
        state: ManifestState::Publishing,
        default_config: String::from_utf8_lossy(&default_config).into_owned(),
        files: new_files.clone(),
        previous: prior_owned.clone(),
    };
    write_manifest_atomic(host, &state_dir, &manifest_path, &publishing)?;

    // Write changed per-config files atomically first.
    for (index, (filename, bytes, _)) in payloads.iter().enumerate() {
        let dest = build_dir.join(filename);
        write_atomic_if_changed(host, build_dir, &dest, bytes, index)?;
    }

    // Atomically replace root `compile_commands.json` last with identical bytes to default.
    let root_dest = build_dir.join("compile_commands.json");
    write_atomic_if_changed(host, build_dir, &root_dest, &default_bytes, payloads.len())?;

    // Retire owned files from removed configurations only if unmodified on disk.
    for (old_name, allowed_hashes) in &prior_owned {
        if !new_files.contains_key(old_name) {
            retire_if_unmodified(host, build_dir, old_name, allowed_hashes)?;
        }
    }

    // Commit final `state: "complete"` manifest.
    let complete = Manifest {
        state: ManifestState::Complete,
        default_config: String::from_utf8_lossy(&default_config).into_owned(),
        files: new_files,
        previous: BTreeMap::new(),
    };
    write_manifest_atomic(host, &state_dir, &manifest_path, &complete)?;
    Ok(())
}

/// Retire unmodified native multi-config compile database files owned by `rcmake`
/// when export is explicitly disabled.
///
/// Never removes files that were modified externally (or overwritten by compatibility mode),
/// and never follows symbolic links.
///
/// # Errors
///
/// Returns [`GenError::Write`] if locking or file removal fails unexpectedly.
pub fn retire(host: &dyn Host, build_dir: &Path) -> Result<(), GenError> {
    let state_dir = build_dir.join(MANIFEST_REL_DIR);
    let manifest_path = state_dir.join(MANIFEST_FILE);
    match host.metadata(&manifest_path, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File => {}
        Ok(_) => return Ok(()),
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(write_error(&manifest_path, err)),
    }

    let _lock = acquire_lock(host, &state_dir)?;
    let owned = read_manifest_owned(host, &manifest_path);
    for (filename, allowed_hashes) in &owned {
        retire_if_unmodified(host, build_dir, filename, allowed_hashes)?;
    }
    match host.remove_file(&manifest_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(write_error(&manifest_path, err)),
    }
}

/// Normalize a path relative to `base` into an absolute lexical CMake-style path.
#[must_use]
pub fn absolute_path(base: &[u8], path: &[u8]) -> BString {
    if path.is_empty() {
        return Lexical::new(base).normalize();
    }
    if Lexical::new(path).absolute() {
        return Lexical::new(path).normalize();
    }
    Lexical::new(&Lexical::new(base).append(path)).normalize()
}

/// Check whether `text` still contains an unexpanded IDE macro reference (`$(...)`).
#[must_use]
pub fn contains_unresolved_ide_macro(text: &[u8]) -> bool {
    let mut rest = text;
    while let Some(pos) = rest.windows(2).position(|w| w == b"$(") {
        let after = rest.get(pos + 2..).unwrap_or_default();
        if let Some(close) = after.iter().position(|&b| b == b')') {
            let name = after.get(..close).unwrap_or_default();
            if !name.is_empty()
                && name
                    .iter()
                    .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'))
            {
                return true;
            }
        }
        rest = after;
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManifestState {
    Publishing,
    Complete,
}

struct Manifest {
    state: ManifestState,
    default_config: String,
    files: BTreeMap<String, String>,
    previous: BTreeMap<String, BTreeSet<String>>,
}

impl Manifest {
    fn to_json(&self) -> BString {
        let mut root = BTreeMap::new();
        root.insert(
            BString::from("schema_version"),
            Value::UInt(MANIFEST_SCHEMA_VERSION),
        );
        root.insert(
            BString::from("state"),
            Value::string(match self.state {
                ManifestState::Publishing => "publishing",
                ManifestState::Complete => "complete",
            }),
        );
        root.insert(
            BString::from("default_config"),
            Value::string(self.default_config.as_str()),
        );
        let files = self
            .files
            .iter()
            .map(|(k, v)| (BString::from(k.as_str()), Value::string(v.as_str())))
            .collect();
        root.insert(BString::from("files"), Value::Object(files));
        if !self.previous.is_empty() {
            let previous = self
                .previous
                .iter()
                .map(|(k, digests)| {
                    (
                        BString::from(k.as_str()),
                        Value::Array(
                            digests
                                .iter()
                                .map(|digest| Value::string(digest.as_str()))
                                .collect(),
                        ),
                    )
                })
                .collect();
            root.insert(BString::from("previous"), Value::Object(previous));
        }
        let mut rendered = write_styled(&Value::Object(root), "  ");
        rendered.push(b'\n');
        rendered
    }
}

fn read_manifest_owned(host: &dyn Host, path: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let Ok(meta) = host.metadata(path, Follow::No) else {
        return BTreeMap::new();
    };
    if meta.kind != FileKind::File || meta.len > MANIFEST_READ_LIMIT {
        return BTreeMap::new();
    }
    let Ok(bytes) = host.read(
        path,
        ReadRange::All {
            limit: MANIFEST_READ_LIMIT,
        },
    ) else {
        return BTreeMap::new();
    };
    let budget = ParseBudget::artifact(MANIFEST_READ_LIMIT as usize);
    let Ok(value) = rcmake_json::parse_bounded(&bytes, &ReadOptions::default(), &budget) else {
        return BTreeMap::new();
    };
    let mut owned: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(files) = value.get("files").and_then(Value::as_object) {
        for (name, digest) in files {
            if let (Ok(name), Some(digest)) = (std::str::from_utf8(name), digest.as_str())
                && is_plain_compdb_filename(name)
            {
                owned
                    .entry(name.to_owned())
                    .or_default()
                    .insert(digest.to_owned());
            }
        }
    }
    if let Some(previous) = value.get("previous").and_then(Value::as_object) {
        for (name, digests) in previous {
            if let Ok(name) = std::str::from_utf8(name)
                && is_plain_compdb_filename(name)
            {
                let entry = owned.entry(name.to_owned()).or_default();
                if let Some(single) = digests.as_str() {
                    entry.insert(single.to_owned());
                } else if let Some(array) = digests.as_array() {
                    for item in array {
                        if let Some(digest) = item.as_str() {
                            entry.insert(digest.to_owned());
                        }
                    }
                }
            }
        }
    }
    owned
}

fn is_plain_compdb_filename(name: &str) -> bool {
    if name == "compile_commands.json" {
        return true;
    }
    let Some(config) = name
        .strip_prefix("compile_commands_")
        .and_then(|rest| rest.strip_suffix(".json"))
    else {
        return false;
    };
    validate_config_name(config).is_ok()
}

fn retire_if_unmodified(
    host: &dyn Host,
    build_dir: &Path,
    filename: &str,
    allowed_hashes: &BTreeSet<String>,
) -> Result<(), GenError> {
    if !is_plain_compdb_filename(filename) || allowed_hashes.is_empty() {
        return Ok(());
    }
    let path = build_dir.join(filename);
    let meta = match host.metadata_fresh(&path, Follow::No) {
        Ok(meta) => meta,
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(write_error(&path, err)),
    };
    if meta.kind != FileKind::File || meta.len > OWNED_FILE_READ_LIMIT {
        return Ok(());
    }
    let bytes = match host.read(
        &path,
        ReadRange::All {
            limit: OWNED_FILE_READ_LIMIT,
        },
    ) {
        Ok(bytes) => bytes,
        Err(err) if err.kind == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(write_error(&path, err)),
    };
    let digest = sha256_hex(&bytes);
    if allowed_hashes.contains(&digest) {
        match host.remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind == ErrorKind::NotFound => {}
            Err(err) => return Err(write_error(&path, err)),
        }
    }
    Ok(())
}

fn acquire_lock(
    host: &dyn Host,
    state_dir: &Path,
) -> Result<Option<Box<dyn rcmake_host::FileLockGuard>>, GenError> {
    let lock_path = state_dir.join(LOCK_FILE);
    match host.lock_file(&lock_path, Some(LOCK_TIMEOUT)) {
        Ok(guard) => Ok(Some(guard)),
        Err(err) if err.kind == ErrorKind::Unsupported => Ok(None),
        Err(err) => Err(write_error(&lock_path, err)),
    }
}

fn write_manifest_atomic(
    host: &dyn Host,
    state_dir: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
) -> Result<(), GenError> {
    let bytes = manifest.to_json();
    write_atomic_replace(host, state_dir, manifest_path, &bytes, "manifest")
}

fn write_atomic_if_changed(
    host: &dyn Host,
    staging_dir: &Path,
    dest: &Path,
    content: &[u8],
    ordinal: usize,
) -> Result<(), GenError> {
    let limit = u64::try_from(content.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if host
        .metadata(dest, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::File && meta.len == content.len() as u64)
        && host
            .read(dest, ReadRange::All { limit })
            .is_ok_and(|old| old == content)
    {
        return Ok(());
    }
    write_atomic_replace(host, staging_dir, dest, content, &format!("db-{ordinal}"))
}

fn write_atomic_replace(
    host: &dyn Host,
    staging_dir: &Path,
    dest: &Path,
    content: &[u8],
    tag: &str,
) -> Result<(), GenError> {
    let pid = host.system().pid;
    let stamp = host.monotonic().as_nanos();
    let mut temp_path: Option<PathBuf> = None;
    for attempt in 0..64u32 {
        let candidate =
            staging_dir.join(format!(".rcmake-compdb-{tag}-{pid}-{stamp}-{attempt}.tmp"));
        match host.write(&candidate, content, WriteMode::CreateNew) {
            Ok(()) => {
                temp_path = Some(candidate);
                break;
            }
            Err(err) if err.kind == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(write_error(&candidate, err)),
        }
    }
    let Some(temp) = temp_path else {
        return Err(GenError::Write {
            path: from_native(dest),
            message: "Could not allocate a unique temporary file for compile_commands.json.".into(),
        });
    };
    if let Err(err) = host.rename(&temp, dest) {
        let _ = host.remove_file(&temp);
        return Err(write_error(dest, err));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    HashAlgo::Sha256.hex_digest(bytes).to_string()
}

fn write_error(path: &Path, err: rcmake_host::HostError) -> GenError {
    GenError::Write {
        path: from_native(path),
        message: err.to_string().into(),
    }
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    fn test_target_id() -> TargetId {
        let mut model = rcmake_model::ProjectModel::default();
        model
            .add_target(rcmake_model::Target::new(
                "app".into(),
                rcmake_model::TargetKind::Executable,
                rcmake_model::TargetOrigin::Normal,
                rcmake_model::DirId::ROOT,
                rcmake_diag::Backtrace::default(),
                Default::default(),
            ))
            .expect("target id")
    }

    fn row(config: &str, file: &str, object: &str, args: &[&str]) -> CompileRow {
        CompileRow {
            config: config.into(),
            target: test_target_id(),
            directory: "/build".into(),
            file: file.into(),
            output: object.into(),
            invocation: CommandEncoding::Arguments(
                args.iter().copied().map(BString::from).collect(),
            ),
        }
    }

    #[test]
    fn preflight_rejects_invalid_and_colliding_config_names() {
        for bad in [
            "", ".", "..", "a/b", "a\\b", "a:b", "Debug ", "Debug.", "CON", "nul.txt",
        ] {
            let db = vec![ConfigDatabase {
                config: bad.into(),
                rows: Vec::new(),
            }];
            assert!(
                preflight_config_filenames(&db).is_err(),
                "expected {bad:?} to fail"
            );
        }
        let colliding = vec![
            ConfigDatabase {
                config: "Debug".into(),
                rows: Vec::new(),
            },
            ConfigDatabase {
                config: "debug".into(),
                rows: Vec::new(),
            },
        ];
        let err = preflight_config_filenames(&colliding).expect_err("case collision");
        assert!(err.to_string().contains("collide"), "{err}");
    }

    #[test]
    fn publish_switch_default_remove_config_and_retire_respects_user_edits()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::default();
        let build = Path::new("/build");
        host.create_dir_all(build)?;

        let db = MultiConfigCompileDatabase {
            configs: vec![
                ConfigDatabase {
                    config: "Debug".into(),
                    rows: vec![row(
                        "Debug",
                        "/src/a.c",
                        "/build/Debug/a.o",
                        &["cc", "-O0", "-c", "/src/a.c"],
                    )],
                },
                ConfigDatabase {
                    config: "Release".into(),
                    rows: vec![row(
                        "Release",
                        "/src/a.c",
                        "/build/Release/a.o",
                        &["cc", "-O2", "-c", "/src/a.c"],
                    )],
                },
                ConfigDatabase {
                    config: "EmptyCfg".into(),
                    rows: Vec::new(),
                },
            ],
            default_index: 1,
            diagnostics: Vec::new(),
        };
        publish(&host, build, &db)?;

        let root = host.read(
            &build.join("compile_commands.json"),
            ReadRange::All { limit: 65536 },
        )?;
        let release = host.read(
            &build.join("compile_commands_Release.json"),
            ReadRange::All { limit: 65536 },
        )?;
        let debug = host.read(
            &build.join("compile_commands_Debug.json"),
            ReadRange::All { limit: 65536 },
        )?;
        let empty = host.read(
            &build.join("compile_commands_EmptyCfg.json"),
            ReadRange::All { limit: 65536 },
        )?;
        assert_eq!(root, release);
        assert!(String::from_utf8_lossy(&root).contains("\"-O2\""));
        assert!(String::from_utf8_lossy(&debug).contains("\"-O0\""));
        assert_eq!(empty, b"[]\n");

        // User edits EmptyCfg manually; Release stays unmodified; Debug is removed.
        host.write(
            &build.join("compile_commands_EmptyCfg.json"),
            b"/* custom */\n[]\n",
            WriteMode::Truncate,
        )?;

        // Re-publish with only Release (removing Debug and EmptyCfg).
        let db2 = MultiConfigCompileDatabase {
            configs: vec![db.configs[1].clone()],
            default_index: 0,
            diagnostics: Vec::new(),
        };
        publish(&host, build, &db2)?;
        assert!(
            host.metadata(&build.join("compile_commands_Debug.json"), Follow::No)
                .is_err()
        );
        // Modified EmptyCfg MUST remain untouched!
        assert_eq!(
            host.read(
                &build.join("compile_commands_EmptyCfg.json"),
                ReadRange::All { limit: 65536 }
            )?,
            b"/* custom */\n[]\n"
        );

        // Now simulate disable transition while root was modified by compat mode.
        host.write(
            &build.join("compile_commands.json"),
            b"[{\"compat\":true}]\n",
            WriteMode::Truncate,
        )?;
        retire(&host, build)?;
        // Unmodified Release file is retired; modified root file is preserved!
        assert!(
            host.metadata(&build.join("compile_commands_Release.json"), Follow::No)
                .is_err()
        );
        assert_eq!(
            host.read(
                &build.join("compile_commands.json"),
                ReadRange::All { limit: 65536 }
            )?,
            b"[{\"compat\":true}]\n"
        );
        assert!(
            host.metadata(
                &build.join("CMakeFiles/rcmake/compile-commands-v1.json"),
                Follow::No
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn interrupted_publish_recovers_on_retry_and_retire() -> Result<(), Box<dyn std::error::Error>>
    {
        let host = PureHost::default();
        let build = Path::new("/build");
        let state_dir = build.join(MANIFEST_REL_DIR);
        host.create_dir_all(&state_dir)?;

        let old_debug =
            render_config(&[row("Debug", "/src/a.c", "/build/Debug/a.o", &["cc", "-O0"])]);
        let new_rel = render_config(&[row(
            "Release",
            "/src/a.c",
            "/build/Release/a.o",
            &["cc", "-O2"],
        )]);
        host.write(
            &build.join("compile_commands_Debug.json"),
            &old_debug,
            WriteMode::Truncate,
        )?;
        host.write(
            &build.join("compile_commands_Release.json"),
            &new_rel,
            WriteMode::Truncate,
        )?;

        // Simulate crash mid-publish (state == "publishing" with both previous and files).
        let interrupted = Manifest {
            state: ManifestState::Publishing,
            default_config: "Release".into(),
            files: BTreeMap::from([
                ("compile_commands_Release.json".into(), sha256_hex(&new_rel)),
                ("compile_commands.json".into(), sha256_hex(&new_rel)),
            ]),
            previous: BTreeMap::from([(
                "compile_commands_Debug.json".into(),
                BTreeSet::from([sha256_hex(&old_debug)]),
            )]),
        };
        host.write(
            &state_dir.join(MANIFEST_FILE),
            &interrupted.to_json(),
            WriteMode::Truncate,
        )?;

        // Disable transition after interrupted publish must retire BOTH previous and partial new files.
        retire(&host, build)?;
        assert!(
            host.metadata(&build.join("compile_commands_Debug.json"), Follow::No)
                .is_err()
        );
        assert!(
            host.metadata(&build.join("compile_commands_Release.json"), Follow::No)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn detects_unresolved_ide_macros() {
        assert!(contains_unresolved_ide_macro(b"-DVAL=$(CUSTOM_MACRO)"));
        assert!(contains_unresolved_ide_macro(b"$(SDKROOT)/usr/include"));
        assert!(!contains_unresolved_ide_macro(b"-DFOO=1 -I/opt/include"));
        assert!(!contains_unresolved_ide_macro(b"$<CONFIG:Debug>"));
    }
}
