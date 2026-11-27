//! Native multi-configuration `compile_commands.json` planning, rendering,
//! atomic publication, and ownership-tracked retirement.

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

/// Return whether `argv` invokes an MSVC-style compiler driver (where `-MD` and `-MT`
/// select the C runtime library rather than GNU dependency-file generation).
#[must_use]
pub fn is_msvc_style_argv(argv: &[BString]) -> bool {
    for tok in argv.iter().take(2) {
        let base = tok
            .rsplit(|&b| b == b'/' || b == b'\\')
            .next()
            .unwrap_or(tok.as_slice());
        let stem = base
            .strip_suffix(b".exe")
            .or_else(|| base.strip_suffix(b".EXE"))
            .unwrap_or(base);
        if stem.eq_ignore_ascii_case(b"cl")
            || stem.eq_ignore_ascii_case(b"clang-cl")
            || stem.eq_ignore_ascii_case(b"icl")
            || stem.eq_ignore_ascii_case(b"icx-cl")
            || stem.eq_ignore_ascii_case(b"intel-cl")
        {
            return true;
        }
    }
    argv.iter().any(|arg| {
        let s = arg.as_slice();
        s.eq_ignore_ascii_case(b"/showIncludes")
            || s.eq_ignore_ascii_case(b"-showIncludes")
            || matches!(
                s,
                b"/nologo" | b"-nologo" | b"/MD" | b"/MDd" | b"/MT" | b"/MTd" | b"-MDd" | b"-MTd"
            )
            || s.starts_with(b"/Fo")
            || s.starts_with(b"-Fo")
            || s.starts_with(b"/Fd")
            || s.starts_with(b"-Fd")
            || s.starts_with(b"/Fe")
            || s.starts_with(b"/Zc:")
            || s.starts_with(b"/EH")
    })
}

/// Remove GNU/Clang compiler dependency-file generation flags (`-MD`, `-MMD`,
/// `-MT <arg>`, `-MF <arg>`, `-MQ <arg>`) from an argument vector when the
/// invocation uses a GNU/Clang-style compiler dialect.
///
/// MSVC-style compiler invocations are returned unchanged so `-MD`, `-MDd`,
/// `-MT`, and `-MTd` runtime library flags and their following arguments are
/// never stripped.
#[must_use]
pub fn strip_depfile_argv(argv: &[BString]) -> Vec<BString> {
    strip_depfile_argv_if(argv, true)
}

/// Remove GNU/Clang dependency-file generation flags when `strip_gnu_depfiles`
/// is `true` and `argv` is not an MSVC-style compiler invocation.
#[must_use]
pub fn strip_depfile_argv_if(argv: &[BString], strip_gnu_depfiles: bool) -> Vec<BString> {
    if !strip_gnu_depfiles || is_msvc_style_argv(argv) {
        return argv.to_vec();
    }
    let mut out = Vec::with_capacity(argv.len());
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_slice() {
            b"-MD" | b"-MMD" => {}
            b"-MT" | b"-MF" | b"-MQ" => skip_next = true,
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// Compute a filesystem-safe canonical name key that folds Unicode case and
/// common NFC/NFD canonical decompositions (Latin, Greek, Cyrillic, Hangul) so
/// case-insensitive and normalization-insensitive volumes (APFS, HFS+, NTFS)
/// detect collisions and spelling-only renames consistently.
#[must_use]
pub fn fs_name_key(name: &str) -> String {
    let mut decomposed = Vec::with_capacity(name.len() * 2);
    for ch in name.chars() {
        decompose_and_fold_char(ch, &mut decomposed);
    }
    canonical_order_combining(&mut decomposed);
    decomposed.into_iter().collect()
}

fn decompose_and_fold_char(ch: char, out: &mut Vec<char>) {
    match ch {
        'ß' | 'ẞ' => {
            out.push('s');
            out.push('s');
        }
        'ς' => out.push('σ'),
        'ſ' => out.push('s'),
        'K' => out.push('k'),
        'Å' => {
            out.push('a');
            out.push('\u{030a}');
        }
        'İ' => {
            out.push('i');
            out.push('\u{0307}');
        }
        // Algorithmic Hangul syllable NFD decomposition (Unicode §3.12).
        '\u{AC00}'..='\u{D7A3}' => {
            let s_index = (ch as u32) - 0xAC00;
            let l = 0x1100 + s_index / 588;
            let v = 0x1161 + (s_index % 588) / 28;
            let t = s_index % 28;
            if let Some(c) = char::from_u32(l) {
                out.push(c);
            }
            if let Some(c) = char::from_u32(v) {
                out.push(c);
            }
            if t != 0
                && let Some(c) = char::from_u32(0x11A7 + t)
            {
                out.push(c);
            }
        }
        _ => {
            // Apply uppercase -> lowercase so pairs like `µ` (U+00B5) and `μ` (U+03BC)
            // fold identically, then apply canonical NFD decomposition for precomposed chars.
            for folded in ch.to_uppercase().flat_map(char::to_lowercase) {
                if let Some(parts) = canonical_decompose_folded(folded) {
                    out.extend_from_slice(parts);
                } else {
                    out.push(folded);
                }
            }
        }
    }
}

fn canonical_decompose_folded(ch: char) -> Option<&'static [char]> {
    Some(match ch {
        // Latin-1 Supplement (lowercase precomposed forms after case folding).
        'à' => &['a', '\u{0300}'],
        'á' => &['a', '\u{0301}'],
        'â' => &['a', '\u{0302}'],
        'ã' => &['a', '\u{0303}'],
        'ä' => &['a', '\u{0308}'],
        'å' => &['a', '\u{030a}'],
        'ç' => &['c', '\u{0327}'],
        'è' => &['e', '\u{0300}'],
        'é' => &['e', '\u{0301}'],
        'ê' => &['e', '\u{0302}'],
        'ë' => &['e', '\u{0308}'],
        'ì' => &['i', '\u{0300}'],
        'í' => &['i', '\u{0301}'],
        'î' => &['i', '\u{0302}'],
        'ï' => &['i', '\u{0308}'],
        'ñ' => &['n', '\u{0303}'],
        'ò' => &['o', '\u{0300}'],
        'ó' => &['o', '\u{0301}'],
        'ô' => &['o', '\u{0302}'],
        'õ' => &['o', '\u{0303}'],
        'ö' => &['o', '\u{0308}'],
        'ù' => &['u', '\u{0300}'],
        'ú' => &['u', '\u{0301}'],
        'û' => &['u', '\u{0302}'],
        'ü' => &['u', '\u{0308}'],
        'ý' => &['y', '\u{0301}'],
        'ÿ' => &['y', '\u{0308}'],
        // Latin Extended-A common precomposed forms.
        'ā' => &['a', '\u{0304}'],
        'ă' => &['a', '\u{0306}'],
        'ą' => &['a', '\u{0328}'],
        'ć' => &['c', '\u{0301}'],
        'ĉ' => &['c', '\u{0302}'],
        'ċ' => &['c', '\u{0307}'],
        'č' => &['c', '\u{030c}'],
        'ď' => &['d', '\u{030c}'],
        'ē' => &['e', '\u{0304}'],
        'ĕ' => &['e', '\u{0306}'],
        'ė' => &['e', '\u{0307}'],
        'ę' => &['e', '\u{0328}'],
        'ě' => &['e', '\u{030c}'],
        'ĝ' => &['g', '\u{0302}'],
        'ğ' => &['g', '\u{0306}'],
        'ġ' => &['g', '\u{0307}'],
        'ģ' => &['g', '\u{0327}'],
        'ĥ' => &['h', '\u{0302}'],
        'ĩ' => &['i', '\u{0303}'],
        'ī' => &['i', '\u{0304}'],
        'ĭ' => &['i', '\u{0306}'],
        'į' => &['i', '\u{0328}'],
        'ĵ' => &['j', '\u{0302}'],
        'ķ' => &['k', '\u{0327}'],
        'ĺ' => &['l', '\u{0301}'],
        'ļ' => &['l', '\u{0327}'],
        'ľ' => &['l', '\u{030c}'],
        'ń' => &['n', '\u{0301}'],
        'ņ' => &['n', '\u{0327}'],
        'ň' => &['n', '\u{030c}'],
        'ō' => &['o', '\u{0304}'],
        'ŏ' => &['o', '\u{0306}'],
        'ő' => &['o', '\u{030b}'],
        'ŕ' => &['r', '\u{0301}'],
        'ŗ' => &['r', '\u{0327}'],
        'ř' => &['r', '\u{030c}'],
        'ś' => &['s', '\u{0301}'],
        'ŝ' => &['s', '\u{0302}'],
        'ş' => &['s', '\u{0327}'],
        'š' => &['s', '\u{030c}'],
        'ţ' => &['t', '\u{0327}'],
        'ť' => &['t', '\u{030c}'],
        'ũ' => &['u', '\u{0303}'],
        'ū' => &['u', '\u{0304}'],
        'ŭ' => &['u', '\u{0306}'],
        'ů' => &['u', '\u{030a}'],
        'ű' => &['u', '\u{030b}'],
        'ų' => &['u', '\u{0328}'],
        'ŵ' => &['w', '\u{0302}'],
        'ŷ' => &['y', '\u{0302}'],
        'ź' => &['z', '\u{0301}'],
        'ż' => &['z', '\u{0307}'],
        'ž' => &['z', '\u{030c}'],
        // Common Greek & Cyrillic precomposed forms.
        'ά' => &['α', '\u{0301}'],
        'έ' => &['ε', '\u{0301}'],
        'ή' => &['η', '\u{0301}'],
        'ί' => &['ι', '\u{0301}'],
        'ό' => &['ο', '\u{0301}'],
        'ύ' => &['υ', '\u{0301}'],
        'ώ' => &['ω', '\u{0301}'],
        'ϊ' => &['ι', '\u{0308}'],
        'ϋ' => &['υ', '\u{0308}'],
        'ё' => &['е', '\u{0308}'],
        'й' => &['и', '\u{0306}'],
        'ї' => &['і', '\u{0308}'],
        'ў' => &['у', '\u{0306}'],
        _ => return None,
    })
}

fn combining_class(ch: char) -> u8 {
    match ch {
        '\u{0300}'..='\u{0314}'
        | '\u{033d}'..='\u{0344}'
        | '\u{0346}'
        | '\u{034a}'..='\u{034c}' => 230,
        '\u{0316}'..='\u{0319}'
        | '\u{031c}'..='\u{0320}'
        | '\u{0323}'..='\u{0326}'
        | '\u{0329}'..='\u{0333}'
        | '\u{0339}'..='\u{033c}' => 220,
        '\u{0327}' | '\u{0328}' => 202,
        '\u{0334}'..='\u{0338}' => 1,
        _ => 0,
    }
}

fn canonical_order_combining(chars: &mut [char]) {
    let mut i = 0;
    while i < chars.len() {
        let Some(&ch) = chars.get(i) else { break };
        if combining_class(ch) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars.get(i).is_some_and(|&c| combining_class(c) != 0) {
            i += 1;
        }
        if let Some(run) = chars.get_mut(start..i) {
            run.sort_by_key(|&c| combining_class(c));
        }
    }
}

/// Snapshot of prior `rcmake`-owned native compile database files captured
/// before cache cleanup (such as `--fresh`, environment refresh, toolchain
/// refresh, or platform restart) can remove `CMakeFiles`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedSnapshot {
    owned: BTreeMap<String, BTreeSet<String>>,
}

impl OwnedSnapshot {
    /// Return whether any prior owned compile-database entries were recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owned.is_empty()
    }

    /// Merge another ownership snapshot into `self`.
    pub fn merge(&mut self, other: Self) {
        for (name, digests) in other.owned {
            self.owned.entry(name).or_default().extend(digests);
        }
    }
}

/// Capture the current native compile-database ownership manifest before cache
/// cleanup can delete `CMakeFiles`.
#[must_use]
pub fn snapshot_owned(host: &dyn Host, build_dir: &Path) -> OwnedSnapshot {
    let manifest_path = build_dir.join(MANIFEST_REL_DIR).join(MANIFEST_FILE);
    OwnedSnapshot {
        owned: read_manifest_owned(host, &manifest_path),
    }
}

/// Validate and map every configuration in `configs` to its destination filename
/// (`compile_commands_<Config>.json`).
///
/// Rejects empty names, non-UTF-8 names, path separators, traversal components,
/// control or Windows-forbidden characters, reserved device names, and
/// case/normalization filename collisions before any file is written.
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
    let root_key = fs_name_key("compile_commands.json");
    let mut filenames = Vec::with_capacity(configs.len());
    let mut seen_keys: BTreeMap<String, String> = BTreeMap::new();
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
        let key = fs_name_key(&filename);
        if key == root_key {
            return Err(GenError::Invalid(
                format!(
                    "Cannot export compile_commands.json: configuration name {name:?} collides with compile_commands.json."
                )
                .into(),
            ));
        }
        if let Some(previous) = seen_keys.insert(key, name.to_owned()) {
            return Err(GenError::Invalid(
                format!(
                    "Cannot export compile_commands.json: configurations {previous:?} and {name:?} collide on case/normalization-insensitive filename {filename:?}."
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
    publish_with_prior(host, build_dir, db, None)
}

/// Like [`publish`], merging a pre-cleanup [`OwnedSnapshot`] so files owned
/// before `--fresh` or a platform restart are still tracked for retirement.
///
/// # Errors
///
/// Returns [`GenError::Invalid`] if configuration names are invalid or collide, or
/// [`GenError::Write`] if locking, staging, or atomic rename fails.
pub fn publish_with_prior(
    host: &dyn Host,
    build_dir: &Path,
    db: &MultiConfigCompileDatabase,
    prior: Option<&OwnedSnapshot>,
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
    let mut new_by_key: BTreeMap<String, String> = BTreeMap::new();
    for (filename, _, digest) in &payloads {
        new_files.insert(filename.clone(), digest.clone());
        new_by_key.insert(fs_name_key(filename), filename.clone());
    }
    new_files.insert("compile_commands.json".to_owned(), default_hash.clone());
    new_by_key.insert(
        fs_name_key("compile_commands.json"),
        "compile_commands.json".to_owned(),
    );

    let state_dir = build_dir.join(MANIFEST_REL_DIR);
    host.create_dir_all(&state_dir)
        .map_err(|err| write_error(&state_dir, err))?;
    let _lock = acquire_lock(host, &state_dir)?;

    let manifest_path = state_dir.join(MANIFEST_FILE);
    let mut prior_owned = read_manifest_owned(host, &manifest_path);
    if let Some(snapshot) = prior {
        for (name, digests) in &snapshot.owned {
            prior_owned
                .entry(name.clone())
                .or_default()
                .extend(digests.iter().cloned());
        }
    }

    // Group prior spellings by their filesystem-safe key so spelling-only renames
    // (for example `Debug` -> `debug` or NFC <-> NFD) are resolved before retirement.
    let mut prior_by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for old_name in prior_owned.keys() {
        prior_by_key
            .entry(fs_name_key(old_name))
            .or_default()
            .push(old_name.clone());
    }

    // Record `state: "publishing"` with both new target hashes and prior owned hashes
    // before modifying any output file so an interrupted publish can recover cleanly.
    let publishing = Manifest {
        state: ManifestState::Publishing,
        default_config: String::from_utf8_lossy(&default_config).into_owned(),
        files: new_files.clone(),
        previous: prior_owned.clone(),
    };
    write_manifest_atomic(host, &state_dir, &manifest_path, &publishing)?;

    let existing_entries = dir_entry_names(host, build_dir);
    let mut renamed_in_place: BTreeSet<String> = BTreeSet::new();

    // Publish per-config databases before updating root `compile_commands.json` so
    // readers never observe a root pointing to a missing or stale per-config file.
    for (index, (filename, bytes, _)) in payloads.iter().enumerate() {
        let dest = build_dir.join(filename);
        let key = fs_name_key(filename);
        let alias_old = prior_by_key.get(&key).and_then(|olds| {
            olds.iter().find(|old| {
                old.as_str() != filename.as_str()
                    && is_spelling_only_alias(host, build_dir, &existing_entries, old, filename)
            })
        });
        if let Some(old_name) = alias_old {
            replace_spelling_alias(
                host,
                build_dir,
                &build_dir.join(old_name),
                &dest,
                bytes,
                index,
            )?;
            renamed_in_place.insert(old_name.clone());
        } else {
            write_atomic_if_changed(host, build_dir, &dest, bytes, index)?;
        }
    }

    let root_dest = build_dir.join("compile_commands.json");
    write_atomic_if_changed(host, build_dir, &root_dest, &default_bytes, payloads.len())?;

    // Collect active file identities and exact directory spellings so retirement can
    // never delete a newly published file on a case- or normalization-insensitive volume.
    let current_entries = dir_entry_names(host, build_dir);
    let active_inodes: BTreeSet<(u64, u64)> = new_files
        .keys()
        .filter_map(|name| {
            let meta = host
                .metadata_fresh(&build_dir.join(name), Follow::No)
                .ok()?;
            (meta.ino != 0).then_some((meta.dev, meta.ino))
        })
        .collect();

    for (old_name, allowed_hashes) in &prior_owned {
        if new_files.contains_key(old_name) || renamed_in_place.contains(old_name) {
            continue;
        }
        let old_key = fs_name_key(old_name);
        if new_by_key.contains_key(&old_key) && !current_entries.contains(old_name.as_bytes()) {
            continue;
        }
        retire_if_unmodified(host, build_dir, old_name, allowed_hashes, &active_inodes)?;
    }

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
    retire_with_prior(host, build_dir, None)
}

/// Like [`retire`], merging a pre-cleanup [`OwnedSnapshot`] so disabling export
/// after `--fresh` or a platform restart still removes unmodified owned databases.
///
/// # Errors
///
/// Returns [`GenError::Write`] if locking or file removal fails unexpectedly.
pub fn retire_with_prior(
    host: &dyn Host,
    build_dir: &Path,
    prior: Option<&OwnedSnapshot>,
) -> Result<(), GenError> {
    let state_dir = build_dir.join(MANIFEST_REL_DIR);
    let manifest_path = state_dir.join(MANIFEST_FILE);
    let has_manifest = match host.metadata(&manifest_path, Follow::No) {
        Ok(meta) => meta.kind == FileKind::File,
        Err(err) if err.kind == ErrorKind::NotFound => false,
        Err(err) => return Err(write_error(&manifest_path, err)),
    };
    let has_prior = prior.is_some_and(|snap| !snap.is_empty());
    if !has_manifest && !has_prior {
        return Ok(());
    }

    let _lock = if host
        .metadata(&state_dir, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        acquire_lock(host, &state_dir)?
    } else {
        None
    };

    let mut owned = if has_manifest {
        read_manifest_owned(host, &manifest_path)
    } else {
        BTreeMap::new()
    };
    if let Some(snapshot) = prior {
        for (name, digests) in &snapshot.owned {
            owned
                .entry(name.clone())
                .or_default()
                .extend(digests.iter().cloned());
        }
    }

    let empty_active = BTreeSet::new();
    for (filename, allowed_hashes) in &owned {
        retire_if_unmodified(host, build_dir, filename, allowed_hashes, &empty_active)?;
    }
    if has_manifest {
        match host.remove_file(&manifest_path) {
            Ok(()) => {}
            Err(err) if err.kind == ErrorKind::NotFound => {}
            Err(err) => return Err(write_error(&manifest_path, err)),
        }
    }
    Ok(())
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

fn dir_entry_names(host: &dyn Host, dir: &Path) -> BTreeSet<BString> {
    host.read_dir(dir)
        .map(|entries| entries.into_iter().map(|entry| entry.name).collect())
        .unwrap_or_default()
}

fn is_spelling_only_alias(
    host: &dyn Host,
    build_dir: &Path,
    existing_entries: &BTreeSet<BString>,
    old_name: &str,
    new_name: &str,
) -> bool {
    let old_path = build_dir.join(old_name);
    let new_path = build_dir.join(new_name);
    let Ok(old_meta) = host.metadata_fresh(&old_path, Follow::No) else {
        return false;
    };
    let Ok(new_meta) = host.metadata_fresh(&new_path, Follow::No) else {
        return false;
    };
    if old_meta.ino != 0 && new_meta.ino != 0 {
        return old_meta.dev == new_meta.dev && old_meta.ino == new_meta.ino;
    }
    existing_entries.contains(old_name.as_bytes())
        && !existing_entries.contains(new_name.as_bytes())
}

fn replace_spelling_alias(
    host: &dyn Host,
    build_dir: &Path,
    old_path: &Path,
    new_path: &Path,
    content: &[u8],
    ordinal: usize,
) -> Result<(), GenError> {
    let new_tmp = allocate_temp_file(host, build_dir, content, &format!("rename-{ordinal}"))?;
    let backup = unique_temp_path(host, build_dir, &format!("backup-{ordinal}"))?;
    if let Err(err) = host.rename(old_path, &backup) {
        let _ = host.remove_file(&new_tmp);
        return Err(write_error(old_path, err));
    }
    if let Err(err) = host.rename(&new_tmp, new_path) {
        let _ = host.rename(&backup, old_path);
        let _ = host.remove_file(&new_tmp);
        return Err(write_error(new_path, err));
    }
    let _ = host.remove_file(&backup);
    Ok(())
}

fn retire_if_unmodified(
    host: &dyn Host,
    build_dir: &Path,
    filename: &str,
    allowed_hashes: &BTreeSet<String>,
    active_inodes: &BTreeSet<(u64, u64)>,
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
    if meta.ino != 0 && active_inodes.contains(&(meta.dev, meta.ino)) {
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

fn allocate_temp_file(
    host: &dyn Host,
    staging_dir: &Path,
    content: &[u8],
    tag: &str,
) -> Result<PathBuf, GenError> {
    let pid = host.system().pid;
    let stamp = host.monotonic().as_nanos();
    for attempt in 0..64u32 {
        let candidate =
            staging_dir.join(format!(".rcmake-compdb-{tag}-{pid}-{stamp}-{attempt}.tmp"));
        match host.write(&candidate, content, WriteMode::CreateNew) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(write_error(&candidate, err)),
        }
    }
    Err(GenError::Write {
        path: from_native(staging_dir),
        message: "Could not allocate a unique temporary file for compile_commands.json.".into(),
    })
}

fn unique_temp_path(host: &dyn Host, staging_dir: &Path, tag: &str) -> Result<PathBuf, GenError> {
    let pid = host.system().pid;
    let stamp = host.monotonic().as_nanos();
    for attempt in 0..64u32 {
        let candidate =
            staging_dir.join(format!(".rcmake-compdb-{tag}-{pid}-{stamp}-{attempt}.tmp"));
        match host.metadata_fresh(&candidate, Follow::No) {
            Err(err) if err.kind == ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(err) => return Err(write_error(&candidate, err)),
        }
    }
    Err(GenError::Write {
        path: from_native(staging_dir),
        message: "Could not allocate a unique backup path for compile_commands.json.".into(),
    })
}

fn write_atomic_replace(
    host: &dyn Host,
    staging_dir: &Path,
    dest: &Path,
    content: &[u8],
    tag: &str,
) -> Result<(), GenError> {
    let temp = allocate_temp_file(host, staging_dir, content, tag)?;
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

    #[test]
    fn preflight_detects_non_ascii_case_and_normalization_collisions() {
        for (a, b) in [
            ("Élite", "élite"),
            ("Straße", "STRASSE"),
            ("Café", "Cafe\u{0301}"),
            ("Übung", "U\u{0308}bung"),
            ("한글", "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}"),
        ] {
            let colliding = vec![
                ConfigDatabase {
                    config: a.into(),
                    rows: Vec::new(),
                },
                ConfigDatabase {
                    config: b.into(),
                    rows: Vec::new(),
                },
            ];
            let err = preflight_config_filenames(&colliding)
                .expect_err(&format!("expected collision for ({a:?}, {b:?})"));
            assert!(err.to_string().contains("collide"), "{err}");
        }
    }

    #[test]
    fn spelling_only_rename_preserves_empty_db_on_case_insensitive_and_sensitive_fs()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1. PureHost (case-sensitive): Debug -> debug writes debug and retires Debug.
        let host = PureHost::default();
        let build = Path::new("/build");
        host.create_dir_all(build)?;
        let db1 = MultiConfigCompileDatabase {
            configs: vec![ConfigDatabase {
                config: "Debug".into(),
                rows: Vec::new(),
            }],
            default_index: 0,
            diagnostics: Vec::new(),
        };
        publish(&host, build, &db1)?;
        let db2 = MultiConfigCompileDatabase {
            configs: vec![ConfigDatabase {
                config: "debug".into(),
                rows: Vec::new(),
            }],
            default_index: 0,
            diagnostics: Vec::new(),
        };
        publish(&host, build, &db2)?;
        assert_eq!(
            host.read(
                &build.join("compile_commands_debug.json"),
                ReadRange::All { limit: 1024 }
            )?,
            b"[]\n"
        );
        assert!(
            host.metadata(&build.join("compile_commands_Debug.json"), Follow::No)
                .is_err()
        );

        // 2. NativeHost (tests case-insensitive APFS/NTFS too): Debug -> debug with identical `[]\n`
        // must NEVER delete the active file and must leave the new spelling on disk.
        #[cfg(any(unix, windows))]
        {
            let native = rcmake_host::NativeHost::new()?;
            let temp = std::env::temp_dir().join(format!(
                "rcmake-compdb-case-{}-{}",
                std::process::id(),
                native.monotonic().as_nanos()
            ));
            native.create_dir_all(&temp)?;
            let res = (|| -> Result<(), Box<dyn std::error::Error>> {
                publish(&native, &temp, &db1)?;
                publish(&native, &temp, &db2)?;
                let entries = dir_entry_names(&native, &temp);
                assert!(
                    entries.contains(b"compile_commands_debug.json".as_slice()),
                    "missing new spelling in {entries:?}"
                );
                assert!(
                    !entries.contains(b"compile_commands_Debug.json".as_slice()),
                    "stale spelling remained in {entries:?}"
                );
                assert_eq!(
                    native.read(
                        &temp.join("compile_commands_debug.json"),
                        ReadRange::All { limit: 1024 }
                    )?,
                    b"[]\n"
                );
                Ok(())
            })();
            let _ = native.remove_dir_all(&temp);
            res?;
        }
        Ok(())
    }

    #[test]
    fn prior_snapshot_preserves_retirement_across_cache_cleanup_with_digest_check()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = PureHost::default();
        let build = Path::new("/build");
        host.create_dir_all(build)?;

        let db = MultiConfigCompileDatabase {
            configs: vec![
                ConfigDatabase {
                    config: "Debug".into(),
                    rows: vec![row("Debug", "/src/a.c", "/build/Debug/a.o", &["cc", "-O0"])],
                },
                ConfigDatabase {
                    config: "Release".into(),
                    rows: vec![row(
                        "Release",
                        "/src/a.c",
                        "/build/Release/a.o",
                        &["cc", "-O2"],
                    )],
                },
            ],
            default_index: 0,
            diagnostics: Vec::new(),
        };
        publish(&host, build, &db)?;

        // User edits Release externally; Debug and root remain owned and unmodified.
        host.write(
            &build.join("compile_commands_Release.json"),
            b"/* user modified */\n[]\n",
            WriteMode::Truncate,
        )?;

        // Capture ownership before `--fresh` deletes `CMakeFiles`.
        let snapshot = snapshot_owned(&host, build);
        assert!(!snapshot.is_empty());
        host.remove_dir_all(&build.join("CMakeFiles"))?;

        // Disable transition with snapshot retires unmodified Debug and root, keeps edited Release.
        retire_with_prior(&host, build, Some(&snapshot))?;
        assert!(
            host.metadata(&build.join("compile_commands_Debug.json"), Follow::No)
                .is_err()
        );
        assert!(
            host.metadata(&build.join("compile_commands.json"), Follow::No)
                .is_err()
        );
        assert_eq!(
            host.read(
                &build.join("compile_commands_Release.json"),
                ReadRange::All { limit: 1024 }
            )?,
            b"/* user modified */\n[]\n"
        );
        Ok(())
    }

    #[test]
    fn strip_depfile_argv_preserves_msvc_runtime_flags_and_strips_gnu_depfiles() {
        let msvc: Vec<BString> = [
            "C:/MSVC/bin/cl.exe",
            "/nologo",
            "-MD",
            "-MT",
            "-DAPP=1",
            "-c",
            "main.c",
            "/Fomain.obj",
        ]
        .into_iter()
        .map(BString::from)
        .collect();
        assert_eq!(strip_depfile_argv(&msvc), msvc);

        let msvc_d: Vec<BString> = ["clang-cl.exe", "-MDd", "-MTd", "-DFOO=1", "-c", "main.c"]
            .into_iter()
            .map(BString::from)
            .collect();
        assert_eq!(strip_depfile_argv(&msvc_d), msvc_d);

        let gnu: Vec<BString> = [
            "/usr/bin/cc",
            "-DFOO=1",
            "-MD",
            "-MT",
            "main.c.o",
            "-MF",
            "main.c.o.d",
            "-o",
            "main.c.o",
            "-c",
            "main.c",
        ]
        .into_iter()
        .map(BString::from)
        .collect();
        assert_eq!(
            strip_depfile_argv(&gnu),
            vec![
                BString::from("/usr/bin/cc"),
                BString::from("-DFOO=1"),
                BString::from("-o"),
                BString::from("main.c.o"),
                BString::from("-c"),
                BString::from("main.c"),
            ]
        );
        // Explicitly disabling GNU depfile stripping keeps flags intact even for generic names.
        assert_eq!(strip_depfile_argv_if(&gnu, false), gnu);
    }
}
