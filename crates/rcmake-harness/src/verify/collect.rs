//! Bounded artifact collection and Ninja dry-run execution for `rcmake-verify`.
//!
//! All build-tree reads go through [`rcmake_buildinfo::ArtifactReader`] so file kind,
//! symlink policy, reply-tree containment, and pre-allocation parse budgets
//! are enforced identically to `rcmake diff-config`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::{BString, cmdline::Arguments, path_lex::Lexical};
use rcmake_buildinfo::{ArtifactReader, ReadError, ReadLimits, parse_cache};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, path,
};
use rcmake_json::{ParseBudget, ReadOptions, Value};

use super::Deadline;
use super::protocol::is_host_absolute;

const REPLY_DIR: &str = ".cmake/api/v1/reply";
const MAX_REPLY_FILES: usize = 200_000;
const MAX_TARGETS: usize = 100_000;
const MAX_COMPDB_ENTRIES: usize = 200_000;
const MAX_COMPDB_TOKENS: usize = 65_536;
const MAX_COMPDB_JSON_DEPTH: usize = 64;
const MAX_RESPONSE_DEPTH: usize = 8;
const MAX_RESPONSE_FILE_BYTES: u64 = 1 << 20;
const MAX_RESPONSE_TOTAL_BYTES: u64 = 16 << 20;
const MAX_NINJA_MANIFEST_BYTES: u64 = 16 << 20;
const MAX_NINJA_DIR_ENTRIES: usize = 4096;
const MAX_NINJA_MANIFESTS: usize = 64;
const MAX_NINJA_INCLUDE_DEPTH: usize = 8;
const MAX_NINJA_SUBFILES: usize = 128;
const MAX_NINJA_GRAPH_BYTES: usize = 32 << 20;
const MAX_NINJA_GRAPH_WORK: usize = 2_000_000;
const MAX_NINJA_OUTPUT_BYTES: usize = 32 << 20;
const MAX_TAIL_CHARS: usize = 4096;
const NINJA_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const NINJA_DRY_RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of a single child process execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProcessOutcome {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    SpawnFailed(String),
}

/// Captured configure execution summary.
#[derive(Clone, Debug)]
pub(crate) struct ConfigureOutcome {
    pub(crate) status: ProcessOutcome,
    pub(crate) elapsed_ns: u64,
    pub(crate) stderr_tail: String,
}

/// Presence state of an expected build-tree artifact.
#[derive(Clone, Debug)]
pub(crate) enum ArtifactState<T> {
    Present(T),
    Missing,
    Unavailable(String),
}

/// One `CMakeCache.txt` entry (comments intentionally ignored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CacheEntry {
    pub(crate) ty: BString,
    pub(crate) value: BString,
}

/// Ordered include or framework directory entry in File API `codemodel-v2`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IncludeEntry {
    pub(crate) path: BString,
    pub(crate) is_system: bool,
}

/// Compile-group settings attached to a source file in File API `codemodel-v2`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CompileGroupModel {
    pub(crate) language: BString,
    pub(crate) standard: Option<BString>,
    pub(crate) defines: Vec<BString>,
    pub(crate) includes: Vec<IncludeEntry>,
    pub(crate) frameworks: Vec<IncludeEntry>,
    pub(crate) fragments: Vec<BString>,
    pub(crate) sysroot: Option<BString>,
}

/// Ordered link/archive command fragment from File API `codemodel-v2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkFragmentModel {
    pub(crate) role: BString,
    pub(crate) fragment: BString,
}

/// Normalized project summary inside one configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectModel {
    pub(crate) name: BString,
    pub(crate) parent: Option<BString>,
    pub(crate) directories: BTreeSet<BString>,
    pub(crate) targets: BTreeSet<BString>,
}

/// Normalized directory summary inside one configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectoryModel {
    pub(crate) source: BString,
    pub(crate) build: BString,
    pub(crate) parent_build: Option<BString>,
    pub(crate) project: Option<BString>,
    pub(crate) minimum_cmake_version: Option<BString>,
}

/// Normalized target summary inside one configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TargetModel {
    pub(crate) name: BString,
    pub(crate) target_type: BString,
    pub(crate) name_on_disk: Option<BString>,
    pub(crate) directory_source: BString,
    pub(crate) directory_build: BString,
    pub(crate) project: BString,
    pub(crate) artifacts: Vec<BString>,
    pub(crate) dependencies: BTreeSet<BString>,
    pub(crate) compiled_sources: BTreeMap<BString, Vec<CompileGroupModel>>,
    pub(crate) uncompiled_sources: BTreeSet<BString>,
    pub(crate) link_language: Option<BString>,
    pub(crate) link_fragments: Vec<LinkFragmentModel>,
}

/// One configuration inside File API `codemodel-v2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigModel {
    pub(crate) name: BString,
    pub(crate) projects: BTreeMap<BString, ProjectModel>,
    pub(crate) directories: BTreeMap<BString, DirectoryModel>,
    pub(crate) targets: BTreeMap<BString, TargetModel>,
}

/// Parsed File API `codemodel-v2` reply tree.
#[derive(Clone, Debug)]
pub(crate) struct CodemodelEvidence {
    pub(crate) paths_source: BString,
    pub(crate) paths_build: BString,
    pub(crate) configurations: BTreeMap<BString, ConfigModel>,
    pub(crate) has_compiled_sources: bool,
}

/// Parsed command-line representation for one `compile_commands.json` entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CompDbInvocation {
    /// Unambiguously tokenized arguments (from `arguments` or confirmed toolchain dialect).
    Tokens(Vec<BString>),
    /// Unparsed raw command when dialect or response file expansion is ambiguous/unavailable.
    Opaque { raw: BString, reason: String },
}

/// One entry in `compile_commands.json`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CompDbEntry {
    pub(crate) config: Option<BString>,
    pub(crate) directory: BString,
    pub(crate) file: BString,
    pub(crate) output: Option<BString>,
    pub(crate) invocation: CompDbInvocation,
}

/// Parsed `compile_commands.json` evidence.
#[derive(Clone, Debug)]
pub(crate) struct CompileDbEvidence {
    pub(crate) entries: Vec<CompDbEntry>,
    pub(crate) opaque_count: usize,
}

/// Compact, non-quadratic build-edge dependency graph for one Ninja manifest.
///
/// Each build edge's input list is stored once in [`Self::edge_inputs`] (`Arc<[BString]>`)
/// and referenced by edge index from [`Self::output_edges`], avoiding quadratic memory
/// blowup on multi-output build edges.
#[derive(Clone, Debug, Default)]
pub(crate) struct NinjaDepGraph {
    pub(crate) edge_inputs: Vec<Arc<[BString]>>,
    pub(crate) output_edges: BTreeMap<BString, Vec<usize>>,
}

/// Shared retained-memory and work budget across Ninja manifest scans for a run.
pub(crate) struct NinjaGraphBudget {
    retained_bytes: usize,
    work_units: usize,
    max_bytes: usize,
    max_work: usize,
}

impl NinjaGraphBudget {
    pub(crate) fn new(max_bytes: usize, max_work: usize) -> Self {
        Self {
            retained_bytes: 0,
            work_units: 0,
            max_bytes,
            max_work,
        }
    }

    fn charge_bytes(&mut self, bytes: usize) -> Result<(), String> {
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        if self.retained_bytes > self.max_bytes {
            return Err(format!(
                "Ninja manifest dependency graph exceeded retained memory budget ({} > {} bytes).",
                self.retained_bytes, self.max_bytes
            ));
        }
        Ok(())
    }

    fn charge_work(&mut self, units: usize) -> Result<(), String> {
        self.work_units = self.work_units.saturating_add(units);
        if self.work_units > self.max_work {
            return Err(format!(
                "Ninja manifest dependency graph exceeded work budget ({} > {} units).",
                self.work_units, self.max_work
            ));
        }
        Ok(())
    }
}

/// One manifest's Ninja dry-run commands plus lightweight build-graph dependencies.
#[derive(Clone, Debug)]
pub(crate) struct NinjaManifestEvidence {
    pub(crate) commands: Vec<BString>,
    /// Shared-edge dependency graph mapping produced output paths to edge input slices.
    pub(crate) dep_edges: NinjaDepGraph,
}

/// Result of the Ninja dry-run stage.
#[derive(Clone, Debug)]
pub(crate) enum NinjaDryRunEvidence {
    Inapplicable(String),
    Unavailable(String),
    Present(BTreeMap<String, NinjaManifestEvidence>),
}

/// Full evidence collected from one staged configure run.
#[derive(Clone, Debug)]
pub(crate) struct RunEvidence {
    pub(crate) configure: ConfigureOutcome,
    pub(crate) cmake_root: Option<BString>,
    pub(crate) generator: Option<BString>,
    pub(crate) codemodel: ArtifactState<CodemodelEvidence>,
    pub(crate) cache: ArtifactState<BTreeMap<BString, CacheEntry>>,
    pub(crate) compile_commands: ArtifactState<CompileDbEvidence>,
    pub(crate) ninja_dry_run: NinjaDryRunEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    Gnu,
    Msvc,
}

#[derive(Clone, Debug)]
struct ToolchainCompiler {
    path: BString,
    dialect: Option<Dialect>,
}

/// Collect bounded evidence from `staged_bin` immediately after `configure` finished.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_run_evidence(
    host: &dyn Host,
    source_dir: &Path,
    staged_src: &Path,
    staged_bin: &Path,
    scratch_root: &Path,
    child_env: &[(BString, BString)],
    configure: ConfigureOutcome,
    request_compile_commands: bool,
    compile_commands_user_disabled: bool,
    deadline: &Deadline,
) -> RunEvidence {
    let succeeded = matches!(configure.status, ProcessOutcome::Exited(0));
    if !succeeded {
        return RunEvidence {
            configure,
            cmake_root: None,
            generator: None,
            codemodel: ArtifactState::Missing,
            cache: ArtifactState::Missing,
            compile_commands: ArtifactState::Missing,
            ninja_dry_run: NinjaDryRunEvidence::Inapplicable(
                "Configure did not succeed.".to_owned(),
            ),
        };
    }

    let Some(mut reader) = ArtifactReader::new(host, staged_bin, ReadLimits::default()) else {
        return RunEvidence {
            configure,
            cmake_root: None,
            generator: None,
            codemodel: ArtifactState::Missing,
            cache: ArtifactState::Missing,
            compile_commands: ArtifactState::Missing,
            ninja_dry_run: NinjaDryRunEvidence::Unavailable(
                "Staged build directory is unreadable.".to_owned(),
            ),
        };
    };

    let (fileapi_meta, codemodel, toolchains) = read_fileapi(&mut reader);
    let cache = read_cache(&mut reader);

    let cache_map = match &cache {
        ArtifactState::Present(map) => Some(map),
        _ => None,
    };

    let generator = fileapi_meta
        .generator
        .or_else(|| cache_value(cache_map, b"CMAKE_GENERATOR"));
    let cmake_root = fileapi_meta
        .cmake_root
        .or_else(|| cache_value(cache_map, b"CMAKE_ROOT"));

    let mut compilers = toolchains;
    augment_compilers_from_cache(&mut compilers, cache_map);

    let configs: Vec<BString> = match &codemodel {
        ArtifactState::Present(cm) => cm.configurations.keys().cloned().collect(),
        _ => Vec::new(),
    };

    let compile_commands = if compile_commands_user_disabled
        || (!request_compile_commands
            && cache_value(cache_map, b"CMAKE_EXPORT_COMPILE_COMMANDS")
                .as_deref()
                .is_some_and(|v| !rcmake_base::truth::is_on(v)))
    {
        ArtifactState::Missing
    } else {
        read_compile_commands(&mut reader, &compilers, &configs)
    };

    let ninja_dry_run = collect_ninja_dry_run(
        host,
        source_dir,
        staged_src,
        staged_bin,
        scratch_root,
        child_env,
        generator.as_ref().map(|g| g.as_slice()),
        cache_map,
        &mut reader,
        deadline,
    );

    RunEvidence {
        configure,
        cmake_root,
        generator,
        codemodel,
        cache,
        compile_commands,
        ninja_dry_run,
    }
}

/// Format bounded tail excerpt of captured stdout/stderr bytes.
pub(crate) fn bounded_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_TAIL_CHARS {
        return text.into_owned();
    }
    let start = text.len().saturating_sub(MAX_TAIL_CHARS);
    let aligned = text
        .char_indices()
        .find(|(idx, _)| *idx >= start)
        .map_or(text.len(), |(idx, _)| idx);
    format!("...(truncated)...\n{}", text.get(aligned..).unwrap_or(""))
}

#[derive(Default)]
struct FileApiMeta {
    generator: Option<BString>,
    cmake_root: Option<BString>,
}

fn read_fileapi(
    reader: &mut ArtifactReader<'_>,
) -> (
    FileApiMeta,
    ArtifactState<CodemodelEvidence>,
    Vec<ToolchainCompiler>,
) {
    let dir = PathBuf::from(REPLY_DIR);
    let entries = match reader.list_checked(&dir, MAX_REPLY_FILES) {
        Ok(entries) => entries,
        Err(ReadError::TooLarge) => {
            return (
                FileApiMeta::default(),
                ArtifactState::Unavailable(format!(
                    "File API reply directory exceeds entry limit ({MAX_REPLY_FILES})."
                )),
                Vec::new(),
            );
        }
        Err(ReadError::Missing) => {
            return (FileApiMeta::default(), ArtifactState::Missing, Vec::new());
        }
        Err(err) => {
            return (
                FileApiMeta::default(),
                ArtifactState::Unavailable(format!(
                    "Could not list File API reply directory: {err}"
                )),
                Vec::new(),
            );
        }
    };
    let names = entries
        .into_iter()
        .filter(|(_, kind)| *kind == FileKind::File)
        .map(|(name, _)| name);
    let Some((index_name, is_error)) = current_index(names) else {
        return (FileApiMeta::default(), ArtifactState::Missing, Vec::new());
    };
    let index_path = dir.join(String::from_utf8_lossy(&index_name).as_ref());
    let index = match reader.json(&index_path) {
        Ok(val) => val,
        Err(ReadError::Missing) => {
            return (FileApiMeta::default(), ArtifactState::Missing, Vec::new());
        }
        Err(err) => {
            return (
                FileApiMeta::default(),
                ArtifactState::Unavailable(format!("Could not read File API index: {err}")),
                Vec::new(),
            );
        }
    };

    let cmake = index.get("cmake");
    let meta = FileApiMeta {
        generator: json_bytes(
            cmake
                .and_then(|c| c.get("generator"))
                .and_then(|g| g.get("name")),
        ),
        cmake_root: json_bytes(
            cmake
                .and_then(|c| c.get("paths"))
                .and_then(|p| p.get("root")),
        ),
    };

    if is_error {
        return (
            meta,
            ArtifactState::Unavailable(
                "The newest File API reply is an error index from a failed generation.".to_owned(),
            ),
            Vec::new(),
        );
    }

    let refs = index_references(&index);
    let mut toolchains = Vec::new();
    if let Some((_, file)) = refs.get(b"toolchains".as_slice())
        && plain_name(file)
        && let Ok(tc_val) = reader.json(&dir.join(String::from_utf8_lossy(file).as_ref()))
    {
        toolchains = parse_toolchains(&tc_val);
    }

    let Some((_, cm_file)) = refs.get(b"codemodel".as_slice()) else {
        return (meta, ArtifactState::Missing, toolchains);
    };
    if !plain_name(cm_file) {
        return (
            meta,
            ArtifactState::Unavailable(format!(
                "Rejected File API codemodel reference '{}': not a plain file name.",
                String::from_utf8_lossy(cm_file)
            )),
            toolchains,
        );
    }

    let cm_val = match reader.json(&dir.join(String::from_utf8_lossy(cm_file).as_ref())) {
        Ok(val) => val,
        Err(err) => {
            return (
                meta,
                ArtifactState::Unavailable(format!(
                    "Could not read File API codemodel '{}': {err}",
                    String::from_utf8_lossy(cm_file)
                )),
                toolchains,
            );
        }
    };

    match parse_codemodel(reader, &dir, &cm_val) {
        Ok(cm) => (meta, ArtifactState::Present(cm), toolchains),
        Err(msg) => (meta, ArtifactState::Unavailable(msg), toolchains),
    }
}

fn current_index(names: impl IntoIterator<Item = BString>) -> Option<(BString, bool)> {
    names
        .into_iter()
        .filter_map(|name| {
            let stem = name.strip_suffix(b".json")?;
            let (suffix, is_error) = if let Some(rest) = stem.strip_prefix(b"index-") {
                (rest, false)
            } else {
                (stem.strip_prefix(b"error-")?, true)
            };
            Some((BString::from(suffix), is_error, name.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, is_error, name)| (name, is_error))
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !name.contains(&b'/')
        && !name.contains(&b'\\')
        && !name.contains(&b':')
        && name != b"."
        && name != b".."
}

fn index_references(index: &Value) -> BTreeMap<BString, (u64, BString)> {
    let mut found: BTreeMap<BString, (u64, BString)> = BTreeMap::new();
    let mut add = |reference: &Value| {
        let (Some(kind), Some(file)) = (
            json_bytes(reference.get("kind")),
            json_bytes(reference.get("jsonFile")),
        ) else {
            return;
        };
        let expected_major = match kind.as_slice() {
            b"codemodel" => Some(2),
            b"toolchains" => Some(1),
            _ => None,
        };
        let version = reference.get("version");
        let major = version.and_then(|v| v.get("major")).and_then(Value::as_u64);
        let minor = version
            .and_then(|v| v.get("minor"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if expected_major.is_none() || major != expected_major {
            return;
        }
        let newer = found.get(&kind).is_none_or(|(old, _)| minor > *old);
        if newer {
            found.insert(kind, (minor, file));
        }
    };
    for reference in json_array(index.get("objects")) {
        add(reference);
    }
    if let Some(reply) = index.get("reply").and_then(Value::as_object) {
        for (key, value) in reply {
            if key.starts_with(b"client-") {
                for nested in value.as_object().into_iter().flat_map(|map| map.values()) {
                    add(nested);
                    for response in json_array(nested.get("responses")) {
                        add(response);
                    }
                }
            } else {
                add(value);
            }
        }
    }
    found
}

fn parse_toolchains(val: &Value) -> Vec<ToolchainCompiler> {
    json_array(val.get("toolchains"))
        .iter()
        .filter_map(|entry| {
            let compiler = entry.get("compiler")?;
            let path = json_bytes(compiler.get("path"))?;
            let id = json_bytes(compiler.get("id"));
            let dialect = id.as_deref().map(|id| {
                if id == b"MSVC" {
                    Dialect::Msvc
                } else {
                    Dialect::Gnu
                }
            });
            Some(ToolchainCompiler { path, dialect })
        })
        .collect()
}

fn parse_codemodel(
    reader: &mut ArtifactReader<'_>,
    reply_dir: &Path,
    cm: &Value,
) -> Result<CodemodelEvidence, String> {
    let paths_source =
        json_bytes(cm.get("paths").and_then(|p| p.get("source"))).unwrap_or_default();
    let paths_build = json_bytes(cm.get("paths").and_then(|p| p.get("build"))).unwrap_or_default();

    let mut configurations = BTreeMap::new();
    let mut has_compiled_sources = false;
    let mut target_count: usize = 0;

    for cfg in json_array(cm.get("configurations")) {
        let cfg_name = json_bytes(cfg.get("name")).unwrap_or_default();

        let dir_entries = json_array(cfg.get("directories"));
        let proj_entries = json_array(cfg.get("projects"));
        let target_entries = json_array(cfg.get("targets"));

        let dir_builds: Vec<BString> = dir_entries
            .iter()
            .map(|d| json_bytes(d.get("build")).unwrap_or_default())
            .collect();
        let dir_sources: Vec<BString> = dir_entries
            .iter()
            .map(|d| json_bytes(d.get("source")).unwrap_or_default())
            .collect();
        let proj_names: Vec<BString> = proj_entries
            .iter()
            .map(|p| json_bytes(p.get("name")).unwrap_or_default())
            .collect();
        let target_names: Vec<BString> = target_entries
            .iter()
            .map(|t| json_bytes(t.get("name")).unwrap_or_default())
            .collect();

        // Map transient target IDs (`app::@hash`) to stable target names.
        let mut id_to_name: BTreeMap<BString, BString> = BTreeMap::new();
        for t in target_entries {
            if let (Some(id), Some(name)) = (json_bytes(t.get("id")), json_bytes(t.get("name"))) {
                id_to_name.insert(id, name);
            }
        }

        let mut directories = BTreeMap::new();
        for d in dir_entries {
            let source = json_bytes(d.get("source")).unwrap_or_default();
            let build = json_bytes(d.get("build")).unwrap_or_default();
            let parent_build = d
                .get("parentIndex")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| dir_builds.get(i).cloned());
            let project = d
                .get("projectIndex")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| proj_names.get(i).cloned());
            let minimum_cmake_version =
                json_bytes(d.get("minimumCMakeVersion").and_then(|v| v.get("string")));
            directories.insert(
                build.clone(),
                DirectoryModel {
                    source,
                    build,
                    parent_build,
                    project,
                    minimum_cmake_version,
                },
            );
        }

        let mut projects = BTreeMap::new();
        for p in proj_entries {
            let name = json_bytes(p.get("name")).unwrap_or_default();
            let parent = p
                .get("parentIndex")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| proj_names.get(i).cloned());
            let dirs_set: BTreeSet<BString> = json_array(p.get("directoryIndexes"))
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|i| usize::try_from(i).ok())
                .filter_map(|i| dir_builds.get(i).cloned())
                .collect();
            let targets_set: BTreeSet<BString> = json_array(p.get("targetIndexes"))
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|i| usize::try_from(i).ok())
                .filter_map(|i| target_names.get(i).cloned())
                .collect();
            projects.insert(
                name.clone(),
                ProjectModel {
                    name,
                    parent,
                    directories: dirs_set,
                    targets: targets_set,
                },
            );
        }

        let mut targets = BTreeMap::new();
        for entry in target_entries {
            target_count += 1;
            if target_count > MAX_TARGETS {
                return Err(format!(
                    "File API codemodel exceeds target limit ({MAX_TARGETS})."
                ));
            }
            let Some(name) = json_bytes(entry.get("name")) else {
                continue;
            };
            let Some(json_file) = json_bytes(entry.get("jsonFile")) else {
                return Err(format!(
                    "File API target '{}' is missing a jsonFile reference.",
                    String::from_utf8_lossy(&name)
                ));
            };
            if !plain_name(&json_file) {
                return Err(format!(
                    "Rejected File API target reference '{}': not a plain file name.",
                    String::from_utf8_lossy(&json_file)
                ));
            }
            let target_val = reader
                .json(&reply_dir.join(String::from_utf8_lossy(&json_file).as_ref()))
                .map_err(|err| {
                    format!(
                        "Could not read File API target reply '{}': {err}",
                        String::from_utf8_lossy(&json_file)
                    )
                })?;

            let dir_idx = entry
                .get("directoryIndex")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok());
            let proj_idx = entry
                .get("projectIndex")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok());
            let directory_source = dir_idx
                .and_then(|i| dir_sources.get(i).cloned())
                .or_else(|| json_bytes(target_val.get("paths").and_then(|p| p.get("source"))))
                .unwrap_or_default();
            let directory_build = dir_idx
                .and_then(|i| dir_builds.get(i).cloned())
                .or_else(|| json_bytes(target_val.get("paths").and_then(|p| p.get("build"))))
                .unwrap_or_default();
            let project = proj_idx
                .and_then(|i| proj_names.get(i).cloned())
                .unwrap_or_default();

            let model = parse_target_reply(
                &target_val,
                &name,
                directory_source,
                directory_build,
                project,
                &id_to_name,
            );
            if !model.compiled_sources.is_empty() {
                has_compiled_sources = true;
            }
            targets.insert(name, model);
        }

        configurations.insert(
            cfg_name.clone(),
            ConfigModel {
                name: cfg_name,
                projects,
                directories,
                targets,
            },
        );
    }

    Ok(CodemodelEvidence {
        paths_source,
        paths_build,
        configurations,
        has_compiled_sources,
    })
}

fn parse_target_reply(
    target: &Value,
    name: &BString,
    directory_source: BString,
    directory_build: BString,
    project: BString,
    id_to_name: &BTreeMap<BString, BString>,
) -> TargetModel {
    let target_type = json_bytes(target.get("type")).unwrap_or_default();
    let name_on_disk = json_bytes(target.get("nameOnDisk"));
    let artifacts: Vec<BString> = json_array(target.get("artifacts"))
        .iter()
        .filter_map(|a| json_bytes(a.get("path")))
        .collect();
    let dependencies: BTreeSet<BString> = json_array(target.get("dependencies"))
        .iter()
        .filter_map(|d| json_bytes(d.get("id")))
        .map(|id| {
            id_to_name.get(&id).cloned().unwrap_or_else(|| {
                // Strip transient `::@<hash>` suffix when id was external/unresolved.
                if let Some(pos) = find_bytes(&id, b"::@") {
                    BString::from(id.get(..pos).unwrap_or(id.as_slice()))
                } else {
                    id
                }
            })
        })
        .collect();

    let groups: Vec<CompileGroupModel> = json_array(target.get("compileGroups"))
        .iter()
        .map(|g| {
            let language = json_bytes(g.get("language")).unwrap_or_default();
            let standard = json_bytes(g.get("languageStandard").and_then(|s| s.get("standard")));
            let defines: Vec<BString> = json_array(g.get("defines"))
                .iter()
                .filter_map(|d| json_bytes(d.get("define")))
                .collect();
            let includes: Vec<IncludeEntry> = json_array(g.get("includes"))
                .iter()
                .filter_map(|inc| {
                    Some(IncludeEntry {
                        path: json_bytes(inc.get("path"))?,
                        is_system: inc
                            .get("isSystem")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect();
            let frameworks: Vec<IncludeEntry> = json_array(g.get("frameworks"))
                .iter()
                .filter_map(|fw| {
                    Some(IncludeEntry {
                        path: json_bytes(fw.get("path"))?,
                        is_system: fw.get("isSystem").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect();
            let fragments: Vec<BString> = json_array(g.get("compileCommandFragments"))
                .iter()
                .filter_map(|f| json_bytes(f.get("fragment")))
                .map(|frag| BString::from(frag.trim_ascii()))
                .filter(|frag| !frag.is_empty())
                .collect();
            let sysroot = json_bytes(g.get("sysroot").and_then(|s| s.get("path")));
            CompileGroupModel {
                language,
                standard,
                defines,
                includes,
                frameworks,
                fragments,
                sysroot,
            }
        })
        .collect();

    let mut compiled_sources: BTreeMap<BString, Vec<CompileGroupModel>> = BTreeMap::new();
    let mut uncompiled_sources: BTreeSet<BString> = BTreeSet::new();
    for src in json_array(target.get("sources")) {
        let Some(src_path) = json_bytes(src.get("path")) else {
            continue;
        };
        if let Some(group_idx) = src
            .get("compileGroupIndex")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            && let Some(group) = groups.get(group_idx)
        {
            compiled_sources
                .entry(src_path)
                .or_default()
                .push(group.clone());
        } else {
            uncompiled_sources.insert(src_path);
        }
    }

    let (link_language, link_fragments) = if let Some(link) = target.get("link") {
        (json_bytes(link.get("language")), parse_link_fragments(link))
    } else if let Some(archive) = target.get("archive") {
        (None, parse_link_fragments(archive))
    } else {
        (None, Vec::new())
    };

    TargetModel {
        name: name.clone(),
        target_type,
        name_on_disk,
        directory_source,
        directory_build,
        project,
        artifacts,
        dependencies,
        compiled_sources,
        uncompiled_sources,
        link_language,
        link_fragments,
    }
}

fn parse_link_fragments(step: &Value) -> Vec<LinkFragmentModel> {
    json_array(step.get("commandFragments"))
        .iter()
        .filter_map(|f| {
            let raw = json_bytes(f.get("fragment"))?;
            let trimmed = raw.trim_ascii();
            if trimmed.is_empty() {
                return None;
            }
            Some(LinkFragmentModel {
                role: json_bytes(f.get("role")).unwrap_or_else(|| BString::from("flags")),
                fragment: BString::from(trimmed),
            })
        })
        .collect()
}

fn read_cache(reader: &mut ArtifactReader<'_>) -> ArtifactState<BTreeMap<BString, CacheEntry>> {
    match reader.read(Path::new("CMakeCache.txt")) {
        Ok(bytes) => {
            let parsed = parse_cache(&bytes);
            let map = parsed
                .into_iter()
                .map(|(k, rec)| {
                    (
                        k,
                        CacheEntry {
                            ty: rec.ty,
                            value: rec.value,
                        },
                    )
                })
                .collect();
            ArtifactState::Present(map)
        }
        Err(ReadError::Missing) => ArtifactState::Missing,
        Err(err) => ArtifactState::Unavailable(format!("Could not read CMakeCache.txt: {err}")),
    }
}

fn cache_value(cache: Option<&BTreeMap<BString, CacheEntry>>, key: &[u8]) -> Option<BString> {
    cache
        .and_then(|map| map.get(key))
        .map(|entry| entry.value.clone())
        .filter(|val| !val.is_empty())
}

fn augment_compilers_from_cache(
    compilers: &mut Vec<ToolchainCompiler>,
    cache: Option<&BTreeMap<BString, CacheEntry>>,
) {
    let Some(cache) = cache else { return };
    for lang in [
        b"C".as_slice(),
        b"CXX",
        b"OBJC",
        b"OBJCXX",
        b"CUDA",
        b"Fortran",
        b"ASM",
        b"Swift",
    ] {
        let mut comp_key = BString::from("CMAKE_");
        comp_key.extend_from_slice(lang);
        comp_key.extend_from_slice(b"_COMPILER");
        let Some(comp_path) = cache.get(&comp_key).map(|e| e.value.clone()) else {
            continue;
        };
        if comp_path.is_empty() || compilers.iter().any(|c| c.path == comp_path) {
            continue;
        }
        let mut id_key = BString::from("CMAKE_");
        id_key.extend_from_slice(lang);
        id_key.extend_from_slice(b"_COMPILER_ID");
        let dialect = cache.get(&id_key).map(|e| {
            if e.value.as_slice() == b"MSVC" {
                Dialect::Msvc
            } else {
                Dialect::Gnu
            }
        });
        compilers.push(ToolchainCompiler {
            path: comp_path,
            dialect,
        });
    }
}

fn read_compile_commands(
    reader: &mut ArtifactReader<'_>,
    compilers: &[ToolchainCompiler],
    configs: &[BString],
) -> ArtifactState<CompileDbEvidence> {
    let bytes = match reader.read(Path::new("compile_commands.json")) {
        Ok(bytes) => bytes,
        Err(ReadError::Missing) => return ArtifactState::Missing,
        Err(err) => {
            return ArtifactState::Unavailable(format!(
                "Could not read compile_commands.json: {err}"
            ));
        }
    };

    // Enforce the row limit and split top-level array elements before parsing individual rows so
    // an oversized or malformed compile database never allocates an unbounded Value tree.
    let ranges = match split_json_array_elements(&bytes, MAX_COMPDB_ENTRIES) {
        Ok(ranges) => ranges,
        Err(msg) => return ArtifactState::Unavailable(msg),
    };

    let mut entries = Vec::with_capacity(ranges.len());
    let mut opaque_count = 0usize;
    let mut response_bytes_used = 0u64;

    for (start, end) in ranges {
        let slice = bytes.get(start..end).unwrap_or(&[]);
        let row_budget = ParseBudget::artifact(slice.len().max(1024));
        let item = match rcmake_json::parse_bounded(slice, &ReadOptions::default(), &row_budget) {
            Ok(val) => val,
            Err(err) => {
                return ArtifactState::Unavailable(format!(
                    "Invalid JSON entry in compile_commands.json: {}",
                    err.message
                ));
            }
        };
        if !matches!(item, Value::Object(_)) {
            return ArtifactState::Unavailable(
                "compile_commands.json contains a non-object row.".to_owned(),
            );
        }

        let Some(file) = json_bytes(item.get("file")).filter(|f| !f.is_empty()) else {
            return ArtifactState::Unavailable(
                "compile_commands.json contains a row missing a valid non-empty 'file' string."
                    .to_owned(),
            );
        };
        let Some(directory) = json_bytes(item.get("directory")) else {
            return ArtifactState::Unavailable(
                "compile_commands.json contains a row missing a valid 'directory' string."
                    .to_owned(),
            );
        };
        let output = match item.get("output") {
            None | Some(Value::Null) => None,
            Some(val) => match val.as_bytes() {
                Some(b) => Some(BString::from(b)),
                None => {
                    return ArtifactState::Unavailable(
                        "compile_commands.json contains a row with a non-string 'output' field."
                            .to_owned(),
                    );
                }
            },
        };

        let invocation = match item.get("arguments") {
            Some(Value::Array(arr)) => {
                if arr.len() > MAX_COMPDB_TOKENS {
                    opaque_count += 1;
                    CompDbInvocation::Opaque {
                        raw: BString::from("<oversized arguments array>"),
                        reason: format!("arguments array exceeds {MAX_COMPDB_TOKENS} tokens"),
                    }
                } else {
                    let Some(raw_tokens) = arr
                        .iter()
                        .map(|v| v.as_bytes().map(BString::from))
                        .collect::<Option<Vec<_>>>()
                    else {
                        return ArtifactState::Unavailable(
                            "compile_commands.json contains a row with a non-string token in 'arguments'."
                                .to_owned(),
                        );
                    };
                    let dialect =
                        detect_tokens_dialect(&raw_tokens, compilers).unwrap_or(Dialect::Gnu);
                    match expand_response_files(
                        reader,
                        raw_tokens.clone(),
                        &directory,
                        dialect,
                        &mut response_bytes_used,
                    ) {
                        Ok(expanded) => CompDbInvocation::Tokens(expanded),
                        Err(reason) => {
                            opaque_count += 1;
                            let joined = raw_tokens
                                .iter()
                                .map(|t| t.as_slice())
                                .collect::<Vec<_>>()
                                .join(&b' ');
                            CompDbInvocation::Opaque {
                                raw: BString::from(joined),
                                reason,
                            }
                        }
                    }
                }
            }
            Some(_) => {
                return ArtifactState::Unavailable(
                    "compile_commands.json contains a row with non-array 'arguments'.".to_owned(),
                );
            }
            None => match item.get("command") {
                Some(val) => {
                    let Some(cmd_bytes) = val.as_bytes() else {
                        return ArtifactState::Unavailable(
                            "compile_commands.json contains a row with a non-string 'command' field."
                                .to_owned(),
                        );
                    };
                    let cmd = BString::from(cmd_bytes);
                    match unambiguous_command_dialect(&cmd, compilers) {
                        Some(dialect) => {
                            let tokens: Vec<BString> =
                                Arguments::new(&cmd, dialect == Dialect::Msvc)
                                    .take(MAX_COMPDB_TOKENS + 1)
                                    .collect();
                            if tokens.len() > MAX_COMPDB_TOKENS {
                                opaque_count += 1;
                                CompDbInvocation::Opaque {
                                    raw: cmd,
                                    reason: format!("command exceeds {MAX_COMPDB_TOKENS} tokens"),
                                }
                            } else {
                                match expand_response_files(
                                    reader,
                                    tokens,
                                    &directory,
                                    dialect,
                                    &mut response_bytes_used,
                                ) {
                                    Ok(expanded) => CompDbInvocation::Tokens(expanded),
                                    Err(reason) => {
                                        opaque_count += 1;
                                        CompDbInvocation::Opaque { raw: cmd, reason }
                                    }
                                }
                            }
                        }
                        None => {
                            opaque_count += 1;
                            CompDbInvocation::Opaque {
                                raw: cmd,
                                reason: "unconfirmed compiler command-line dialect".to_owned(),
                            }
                        }
                    }
                }
                None => {
                    return ArtifactState::Unavailable(
                        "compile_commands.json contains a row missing both 'arguments' and 'command'."
                            .to_owned(),
                    );
                }
            },
        };

        let inferred_output = output.clone().or_else(|| {
            if let CompDbInvocation::Tokens(tokens) = &invocation {
                extract_output_from_tokens(tokens)
            } else {
                None
            }
        });
        let config = inferred_output
            .as_deref()
            .and_then(|out| infer_config_from_output(out, configs));

        entries.push(CompDbEntry {
            config,
            directory,
            file,
            output: inferred_output,
            invocation,
        });
    }

    ArtifactState::Present(CompileDbEvidence {
        entries,
        opaque_count,
    })
}

/// Split a top-level JSON array into byte ranges of its elements while enforcing `max_entries`
/// and nesting depth before building any `Value` tree.
fn split_json_array_elements(
    bytes: &[u8],
    max_entries: usize,
) -> Result<Vec<(usize, usize)>, String> {
    let mut i = if bytes.starts_with(b"\xEF\xBB\xBF") {
        3
    } else {
        0
    };
    skip_json_ws(bytes, &mut i);
    if bytes.get(i) != Some(&b'[') {
        return Err("compile_commands.json top-level value is not an array.".to_owned());
    }
    i += 1;
    skip_json_ws(bytes, &mut i);
    if bytes.get(i) == Some(&b']') {
        i += 1;
        skip_json_ws(bytes, &mut i);
        if i != bytes.len() {
            return Err("Unexpected trailing characters in compile_commands.json.".to_owned());
        }
        return Ok(Vec::new());
    }

    let mut ranges = Vec::new();
    loop {
        if ranges.len() >= max_entries {
            return Err(format!(
                "compile_commands.json exceeds entry limit ({max_entries})."
            ));
        }
        let elem_start = i;
        scan_json_value(bytes, &mut i, MAX_COMPDB_JSON_DEPTH)?;
        let elem_end = i;
        ranges.push((elem_start, elem_end));

        skip_json_ws(bytes, &mut i);
        match bytes.get(i) {
            Some(&b',') => {
                i += 1;
                skip_json_ws(bytes, &mut i);
            }
            Some(&b']') => {
                i += 1;
                skip_json_ws(bytes, &mut i);
                if i != bytes.len() {
                    return Err(
                        "Unexpected trailing characters in compile_commands.json.".to_owned()
                    );
                }
                break;
            }
            _ => {
                return Err("Expected ',' or ']' in compile_commands.json.".to_owned());
            }
        }
    }
    Ok(ranges)
}

fn skip_json_ws(bytes: &[u8], i: &mut usize) {
    while matches!(bytes.get(*i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        *i += 1;
    }
}

fn scan_json_string(bytes: &[u8], i: &mut usize) -> Result<(), String> {
    if bytes.get(*i) != Some(&b'"') {
        return Err("Invalid string literal in compile_commands.json.".to_owned());
    }
    *i += 1;
    while let Some(&b) = bytes.get(*i) {
        match b {
            b'"' => {
                *i += 1;
                return Ok(());
            }
            b'\\' => {
                *i += 1;
                if *i >= bytes.len() {
                    return Err("Unterminated escape sequence in compile_commands.json.".to_owned());
                }
                *i += 1;
            }
            0x00..=0x1F => {
                return Err("Unescaped control character in compile_commands.json.".to_owned());
            }
            _ => {
                *i += 1;
            }
        }
    }
    Err("Unterminated string literal in compile_commands.json.".to_owned())
}

fn scan_json_value(bytes: &[u8], i: &mut usize, max_depth: usize) -> Result<(), String> {
    let Some(&first) = bytes.get(*i) else {
        return Err("Unexpected end of compile_commands.json.".to_owned());
    };
    match first {
        b'"' => scan_json_string(bytes, i),
        b'{' | b'[' => {
            let mut stack = vec![if first == b'{' { b'}' } else { b']' }];
            *i += 1;
            while !stack.is_empty() {
                let Some(&b) = bytes.get(*i) else {
                    return Err("Unterminated container in compile_commands.json.".to_owned());
                };
                match b {
                    b'"' => scan_json_string(bytes, i)?,
                    b'{' => {
                        if stack.len() >= max_depth {
                            return Err(format!(
                                "compile_commands.json exceeds JSON nesting limit ({max_depth})."
                            ));
                        }
                        stack.push(b'}');
                        *i += 1;
                    }
                    b'[' => {
                        if stack.len() >= max_depth {
                            return Err(format!(
                                "compile_commands.json exceeds JSON nesting limit ({max_depth})."
                            ));
                        }
                        stack.push(b']');
                        *i += 1;
                    }
                    b'}' | b']' => match stack.pop() {
                        Some(expected) if expected == b => {
                            *i += 1;
                        }
                        _ => {
                            return Err("Mismatched container delimiter in compile_commands.json."
                                .to_owned());
                        }
                    },
                    _ => {
                        *i += 1;
                    }
                }
            }
            Ok(())
        }
        b']' | b'}' | b',' | b':' => {
            Err("Expected JSON value in compile_commands.json.".to_owned())
        }
        _ => {
            while let Some(&b) = bytes.get(*i) {
                if matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}') {
                    break;
                }
                *i += 1;
            }
            Ok(())
        }
    }
}

fn detect_tokens_dialect(tokens: &[BString], compilers: &[ToolchainCompiler]) -> Option<Dialect> {
    for token in tokens.iter().take(8) {
        if let Some(comp) = compilers
            .iter()
            .find(|c| c.path == *token || basename_stem(&c.path) == basename_stem(token))
            && let Some(d) = comp.dialect
        {
            return Some(d);
        }
    }
    uniform_dialect(compilers)
}

fn unambiguous_command_dialect(cmd: &[u8], compilers: &[ToolchainCompiler]) -> Option<Dialect> {
    let gnu_head: Vec<BString> = Arguments::new(cmd, false).take(8).collect();
    let msvc_head: Vec<BString> = Arguments::new(cmd, true).take(8).collect();
    for head in [&gnu_head, &msvc_head] {
        for token in head {
            if let Some(comp) = compilers
                .iter()
                .find(|c| c.path == *token || basename_stem(&c.path) == basename_stem(token))
                && let Some(d) = comp.dialect
            {
                return Some(d);
            }
        }
    }
    uniform_dialect(compilers)
}

fn uniform_dialect(compilers: &[ToolchainCompiler]) -> Option<Dialect> {
    let mut found: Option<Dialect> = None;
    for comp in compilers {
        let Some(d) = comp.dialect else { continue };
        match found {
            None => found = Some(d),
            Some(prev) if prev == d => {}
            Some(_) => return None,
        }
    }
    found
}

fn expand_response_files(
    reader: &mut ArtifactReader<'_>,
    tokens: Vec<BString>,
    directory: &[u8],
    dialect: Dialect,
    used: &mut u64,
) -> Result<Vec<BString>, String> {
    let mut out = Vec::with_capacity(tokens.len().min(MAX_COMPDB_TOKENS));
    let mut active = BTreeSet::new();
    expand_response_tokens(
        reader,
        tokens,
        directory,
        dialect,
        0,
        used,
        &mut active,
        &mut out,
    )?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn expand_response_tokens(
    reader: &mut ArtifactReader<'_>,
    tokens: Vec<BString>,
    directory: &[u8],
    dialect: Dialect,
    depth: usize,
    used: &mut u64,
    active: &mut BTreeSet<PathBuf>,
    out: &mut Vec<BString>,
) -> Result<(), String> {
    for token in tokens {
        let Some(resp) = token.strip_prefix(b"@").filter(|r| !r.is_empty()) else {
            if out.len() >= MAX_COMPDB_TOKENS {
                return Err(format!(
                    "expanded compile command exceeds {MAX_COMPDB_TOKENS} tokens"
                ));
            }
            out.push(token);
            continue;
        };
        if depth >= MAX_RESPONSE_DEPTH {
            return Err(format!(
                "nested response file '{}' exceeds nesting limit ({MAX_RESPONSE_DEPTH})",
                String::from_utf8_lossy(resp)
            ));
        }
        let resolved = join_lexical(directory, resp);
        let native = path::to_native(&resolved);
        let Some(rel) = reader.artifact_relative(&native) else {
            return Err(format!(
                "response file '{}' lies outside the staged build tree",
                String::from_utf8_lossy(resp)
            ));
        };
        if !active.insert(rel.clone()) {
            return Err(format!(
                "cyclic response file reference '{}'",
                rel.display()
            ));
        }
        if *used >= MAX_RESPONSE_TOTAL_BYTES {
            active.remove(&rel);
            return Err("cumulative response-file byte budget exceeded".to_owned());
        }
        let limit = MAX_RESPONSE_FILE_BYTES.min(MAX_RESPONSE_TOTAL_BYTES - *used);
        let bytes = match reader.read_limited(&rel, limit) {
            Ok(bytes) => bytes,
            Err(err) => {
                active.remove(&rel);
                return Err(format!(
                    "response file '{}' unreadable: {err}",
                    rel.display()
                ));
            }
        };
        *used = used.saturating_add(bytes.len() as u64);
        // Probe one token beyond remaining room so truncation at MAX_COMPDB_TOKENS is caught
        // even when `@file` is the entire command.
        let room = MAX_COMPDB_TOKENS.saturating_sub(out.len());
        let inner: Vec<BString> = Arguments::new(&bytes, dialect == Dialect::Msvc)
            .take(room.saturating_add(1))
            .collect();
        if inner.len() > room {
            active.remove(&rel);
            return Err(format!(
                "expanded response file '{}' exceeds {MAX_COMPDB_TOKENS} tokens",
                rel.display()
            ));
        }
        let nested = expand_response_tokens(
            reader,
            inner,
            directory,
            dialect,
            depth + 1,
            used,
            active,
            out,
        );
        active.remove(&rel);
        nested?;
    }
    Ok(())
}

fn extract_output_from_tokens(tokens: &[BString]) -> Option<BString> {
    let mut iter = tokens.iter();
    while let Some(tok) = iter.next() {
        let s = tok.as_slice();
        if s == b"-o" {
            return iter.next().cloned();
        }
        if let Some(rest) = s.strip_prefix(b"/Fo").filter(|r| !r.is_empty()) {
            return Some(BString::from(rest));
        }
    }
    None
}

fn infer_config_from_output(output: &[u8], configs: &[BString]) -> Option<BString> {
    if configs.len() <= 1 {
        return None;
    }
    let parts: Vec<&[u8]> = output.split(|&b| b == b'/' || b == b'\\').collect();
    let at = parts.iter().position(|p| *p == b"CMakeFiles")?;
    let seg = *parts.get(at + 2)?;
    configs.iter().find(|c| c.as_slice() == seg).cloned()
}

#[allow(clippy::too_many_arguments)]
fn collect_ninja_dry_run(
    host: &dyn Host,
    source_dir: &Path,
    staged_src: &Path,
    staged_bin: &Path,
    scratch_root: &Path,
    child_env: &[(BString, BString)],
    generator: Option<&[u8]>,
    cache: Option<&BTreeMap<BString, CacheEntry>>,
    reader: &mut ArtifactReader<'_>,
    deadline: &Deadline,
) -> NinjaDryRunEvidence {
    let Some(generator_name) = generator else {
        return NinjaDryRunEvidence::Inapplicable("Generator name is unknown.".to_owned());
    };
    let multi = if generator_name == b"Ninja" {
        false
    } else if generator_name == b"Ninja Multi-Config" {
        true
    } else {
        return NinjaDryRunEvidence::Inapplicable(format!(
            "Generator '{}' is not a Ninja generator.",
            String::from_utf8_lossy(generator_name)
        ));
    };

    // Inspect cached CMAKE_MAKE_PROGRAM purely as metadata; never execute a build-generated path.
    let Some(make_prog) = cache_value(cache, b"CMAKE_MAKE_PROGRAM") else {
        return NinjaDryRunEvidence::Unavailable(
            "CMAKE_MAKE_PROGRAM is missing from CMakeCache.txt.".to_owned(),
        );
    };
    if let Err(reason) = validate_cached_make_program_name(
        host,
        &make_prog,
        source_dir,
        staged_src,
        staged_bin,
        scratch_root,
    ) {
        return NinjaDryRunEvidence::Unavailable(reason);
    }

    let trusted_ninja =
        match resolve_trusted_ninja(host, source_dir, staged_src, staged_bin, scratch_root) {
            Ok(exe) => exe,
            Err(reason) => return NinjaDryRunEvidence::Unavailable(reason),
        };

    if let Err(reason) =
        validate_ninja_executable(host, staged_bin, child_env, &trusted_ninja, deadline)
    {
        return NinjaDryRunEvidence::Unavailable(reason);
    }

    let manifests: Vec<(String, String)> = if multi {
        let dir_entries = match reader.list_checked(Path::new("."), MAX_NINJA_DIR_ENTRIES) {
            Ok(entries) => entries,
            Err(ReadError::TooLarge) => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Staged build directory exceeds Ninja manifest discovery limit ({MAX_NINJA_DIR_ENTRIES} entries)."
                ));
            }
            Err(err) => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Could not list staged build directory for Ninja manifests: {err}"
                ));
            }
        };
        let mut found: Vec<(String, String)> = dir_entries
            .into_iter()
            .filter(|(_, kind)| *kind == FileKind::File)
            .filter_map(|(name, _)| {
                let s = std::str::from_utf8(&name).ok()?.to_owned();
                let cfg = s.strip_prefix("build-")?.strip_suffix(".ninja")?.to_owned();
                (!cfg.is_empty()).then_some((cfg, s))
            })
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        if found.is_empty() {
            return NinjaDryRunEvidence::Unavailable(
                "No build-<Config>.ninja manifests were found in the staged build directory."
                    .to_owned(),
            );
        }
        if found.len() > MAX_NINJA_MANIFESTS {
            return NinjaDryRunEvidence::Unavailable(format!(
                "Ninja Multi-Config manifest count ({}) exceeds limit ({MAX_NINJA_MANIFESTS}).",
                found.len()
            ));
        }
        found
    } else {
        vec![("default".to_owned(), "build.ninja".to_owned())]
    };

    let mut out = BTreeMap::new();
    let mut graph_budget = NinjaGraphBudget::new(MAX_NINJA_GRAPH_BYTES, MAX_NINJA_GRAPH_WORK);
    for (label, filename) in manifests {
        let Some(timeout) = deadline.step_timeout(host, NINJA_DRY_RUN_TIMEOUT) else {
            return NinjaDryRunEvidence::Unavailable(
                "Overall verify deadline exceeded during Ninja dry-run.".to_owned(),
            );
        };

        let mut dep_edges = NinjaDepGraph::default();
        let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
        if let Err(reason) = scan_ninja_manifest(
            reader,
            Path::new(&filename),
            0,
            &mut visited,
            &mut dep_edges,
            &mut graph_budget,
        ) {
            return NinjaDryRunEvidence::Unavailable(reason);
        }

        let mut args = Vec::new();
        if filename != "build.ninja" {
            args.push(BString::from("-f"));
            args.push(BString::from(filename.as_str()));
        }
        args.push(BString::from("-n"));
        args.push(BString::from("-v"));
        args.push(BString::from("-j1"));

        let req = RunRequest {
            pipeline: vec![CommandSpec {
                program: trusted_ninja.clone(),
                args,
            }],
            cwd: staged_bin.to_path_buf(),
            env: child_env.to_vec(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(timeout),
            tee: false,
            default_error_mode: false,
        };
        let run = match host.run_bounded(&req, MAX_NINJA_OUTPUT_BYTES) {
            Ok(res) => res,
            Err(err) => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Ninja dry-run failed for {filename}: {err}"
                ));
            }
        };
        match run.results.first() {
            Some(ProcessResult::Exited(0)) => {}
            Some(ProcessResult::TimedOut) => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Ninja dry-run timed out for {filename}."
                ));
            }
            Some(ProcessResult::Exited(code)) => {
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&run.stdout),
                    String::from_utf8_lossy(&run.stderr)
                );
                if combined.to_ascii_lowercase().contains("dyndep") {
                    return NinjaDryRunEvidence::Unavailable(format!(
                        "Ninja dry-run for {filename} requires an unresolved dyndep file."
                    ));
                }
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Ninja dry-run exited with status {code} for {filename}."
                ));
            }
            Some(ProcessResult::Signaled(sig)) => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Ninja dry-run for {filename} terminated by signal {sig}."
                ));
            }
            Some(ProcessResult::SpawnFailed(err)) => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Could not start Ninja executable for {filename}: {err}"
                ));
            }
            None => {
                return NinjaDryRunEvidence::Unavailable(format!(
                    "Ninja dry-run produced no exit status for {filename}."
                ));
            }
        }

        let commands = match parse_ninja_dry_run_output(&run.stdout, &run.stderr) {
            Ok(cmds) => cmds,
            Err(reason) => return NinjaDryRunEvidence::Unavailable(reason),
        };

        out.insert(
            label,
            NinjaManifestEvidence {
                commands,
                dep_edges,
            },
        );
    }

    NinjaDryRunEvidence::Present(out)
}

fn is_ninja_stem(stem: &[u8]) -> bool {
    stem.eq_ignore_ascii_case(b"ninja") || stem.eq_ignore_ascii_case(b"ninja-build")
}

fn path_inside_untrusted_trees(
    host: &dyn Host,
    candidate: &Path,
    source_dir: &Path,
    staged_src: &Path,
    staged_bin: &Path,
    scratch_root: &Path,
) -> bool {
    let cand_canon = host
        .canonicalize(candidate)
        .unwrap_or_else(|_| candidate.to_path_buf());
    for root in [source_dir, staged_src, staged_bin, scratch_root] {
        let root_canon = host
            .canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf());
        if candidate.starts_with(root) || cand_canon.starts_with(&root_canon) {
            return true;
        }
    }
    false
}

fn validate_cached_make_program_name(
    host: &dyn Host,
    make_prog: &[u8],
    source_dir: &Path,
    staged_src: &Path,
    staged_bin: &Path,
    scratch_root: &Path,
) -> Result<(), String> {
    let stem = basename_stem(make_prog);
    if !is_ninja_stem(&stem) {
        return Err(format!(
            "Unsupported build tool '{}' in CMAKE_MAKE_PROGRAM; Ninja dry-run requires ninja.",
            String::from_utf8_lossy(make_prog)
        ));
    }
    if is_host_absolute(host, make_prog) {
        let native = path::to_native(make_prog);
        if path_inside_untrusted_trees(
            host,
            &native,
            source_dir,
            staged_src,
            staged_bin,
            scratch_root,
        ) {
            return Err(format!(
                "CMAKE_MAKE_PROGRAM '{}' resides inside the source or scratch tree and is not trusted.",
                String::from_utf8_lossy(make_prog)
            ));
        }
    }
    Ok(())
}

/// Resolve an independent, explicitly trusted Ninja executable from `RCMAKE_VERIFY_NINJA` or
/// absolute directories on the parent `PATH` outside the source and scratch trees.
fn resolve_trusted_ninja(
    host: &dyn Host,
    source_dir: &Path,
    staged_src: &Path,
    staged_bin: &Path,
    scratch_root: &Path,
) -> Result<BString, String> {
    if let Some(override_exe) = host
        .env_get(b"RCMAKE_VERIFY_NINJA")
        .filter(|v| !v.is_empty())
    {
        if !is_host_absolute(host, &override_exe) {
            return Err(format!(
                "Trusted Ninja override RCMAKE_VERIFY_NINJA '{}' must be an absolute path.",
                String::from_utf8_lossy(&override_exe)
            ));
        }
        let candidate = path::to_native(&override_exe);
        if !host
            .metadata(&candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Err(format!(
                "Trusted Ninja override RCMAKE_VERIFY_NINJA '{}' does not exist.",
                candidate.display()
            ));
        }
        if path_inside_untrusted_trees(
            host,
            &candidate,
            source_dir,
            staged_src,
            staged_bin,
            scratch_root,
        ) {
            return Err(format!(
                "Trusted Ninja override RCMAKE_VERIFY_NINJA '{}' must lie outside the source and scratch trees.",
                candidate.display()
            ));
        }
        return Ok(path::from_native(&candidate));
    }

    let windows = host.system().sysname.eq_ignore_ascii_case(b"Windows");
    let sep = if windows { b';' } else { b':' };
    let names: &[&str] = if windows {
        &["ninja.exe", "ninja-build.exe", "ninja", "ninja-build"]
    } else {
        &["ninja", "ninja-build"]
    };

    if let Some(path_var) = host.env_get(b"PATH") {
        for dir_bytes in path_var.split(|&b| b == sep).filter(|d| !d.is_empty()) {
            if !is_host_absolute(host, dir_bytes) {
                continue;
            }
            let dir = path::to_native(dir_bytes);
            for leaf in names {
                let candidate = dir.join(leaf);
                if !host
                    .metadata(&candidate, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::File)
                {
                    continue;
                }
                if path_inside_untrusted_trees(
                    host,
                    &candidate,
                    source_dir,
                    staged_src,
                    staged_bin,
                    scratch_root,
                ) {
                    continue;
                }
                return Ok(path::from_native(&candidate));
            }
        }
    }

    Err(
        "Could not locate an independent trusted Ninja executable on PATH (or via RCMAKE_VERIFY_NINJA)."
            .to_owned(),
    )
}

/// Validate that the resolved trusted executable reports Ninja version and subtools.
fn validate_ninja_executable(
    host: &dyn Host,
    cwd: &Path,
    child_env: &[(BString, BString)],
    ninja_exe: &BString,
    deadline: &Deadline,
) -> Result<(), String> {
    let Some(ver_timeout) = deadline.step_timeout(host, NINJA_PROBE_TIMEOUT) else {
        return Err("Overall verify deadline exceeded before Ninja probe.".to_owned());
    };
    let ver_req = RunRequest {
        pipeline: vec![CommandSpec {
            program: ninja_exe.clone(),
            args: vec![BString::from("--version")],
        }],
        cwd: cwd.to_path_buf(),
        env: child_env.to_vec(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(ver_timeout),
        tee: false,
        default_error_mode: false,
    };
    let ver = host
        .run_bounded(&ver_req, 64 * 1024)
        .map_err(|err| format!("Could not probe trusted Ninja --version: {err}"))?;
    if !matches!(ver.results.first(), Some(ProcessResult::Exited(0))) {
        return Err("Trusted Ninja executable failed --version probe.".to_owned());
    }
    let line = String::from_utf8_lossy(&ver.stdout);
    let first = line.lines().next().unwrap_or("").trim();
    if !looks_like_ninja_version(first) {
        return Err(format!(
            "Trusted Ninja executable reported unexpected --version '{first}'."
        ));
    }

    let Some(tools_timeout) = deadline.step_timeout(host, NINJA_PROBE_TIMEOUT) else {
        return Err("Overall verify deadline exceeded before Ninja subtool probe.".to_owned());
    };
    let tools_req = RunRequest {
        pipeline: vec![CommandSpec {
            program: ninja_exe.clone(),
            args: vec![BString::from("-t"), BString::from("list")],
        }],
        cwd: cwd.to_path_buf(),
        env: child_env.to_vec(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(tools_timeout),
        tee: false,
        default_error_mode: false,
    };
    let tools = host
        .run_bounded(&tools_req, 64 * 1024)
        .map_err(|err| format!("Could not probe trusted Ninja -t list: {err}"))?;
    if !matches!(tools.results.first(), Some(ProcessResult::Exited(0))) {
        return Err("Trusted Ninja executable failed -t list probe.".to_owned());
    }
    let tools_text = String::from_utf8_lossy(&tools.stdout);
    let has_commands_tool = tools_text
        .lines()
        .any(|l| l.split_whitespace().next().is_some_and(|w| w == "commands"));
    if !has_commands_tool {
        return Err("Trusted Ninja executable is missing the 'commands' subtool.".to_owned());
    }
    Ok(())
}

fn looks_like_ninja_version(line: &str) -> bool {
    let mut parts = line.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    !major.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && !minor.is_empty()
        && minor.bytes().all(|b| b.is_ascii_digit())
}

/// Inspect Ninja manifests via `ArtifactReader` to verify complete containment, detect
/// unresolved `dyndep` files, and collect build-edge dependencies.
pub(crate) fn scan_ninja_manifest(
    reader: &mut ArtifactReader<'_>,
    rel_path: &Path,
    depth: usize,
    visited: &mut BTreeSet<PathBuf>,
    dep_edges: &mut NinjaDepGraph,
    budget: &mut NinjaGraphBudget,
) -> Result<(), String> {
    if depth > MAX_NINJA_INCLUDE_DEPTH {
        return Err(format!(
            "Ninja manifest include depth exceeded limit ({MAX_NINJA_INCLUDE_DEPTH}) at {}.",
            rel_path.display()
        ));
    }
    if !visited.insert(rel_path.to_path_buf()) {
        return Ok(());
    }
    if visited.len() > MAX_NINJA_SUBFILES {
        return Err(format!(
            "Ninja manifest include count exceeded limit ({MAX_NINJA_SUBFILES}) at {}.",
            rel_path.display()
        ));
    }
    let bytes = reader
        .read_limited(rel_path, MAX_NINJA_MANIFEST_BYTES)
        .map_err(|err| {
            format!(
                "Could not read Ninja manifest {}: {err}",
                rel_path.display()
            )
        })?;
    let text = String::from_utf8_lossy(&bytes);

    // Join Ninja `$` line continuations.
    let mut logical_lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for raw in text.lines() {
        budget.charge_work(1)?;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(prefix) = line.strip_suffix('$') {
            current.push_str(prefix);
        } else {
            current.push_str(line);
            logical_lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        logical_lines.push(current);
    }

    let mut subfiles: Vec<PathBuf> = Vec::new();
    for line in &logical_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("include ")
            .or_else(|| trimmed.strip_prefix("subninja "))
        {
            let target = unescape_ninja_path_checked(rest.trim()).map_err(|reason| {
                format!(
                    "Ninja manifest {} has an unresolved include/subninja directive: {reason}.",
                    rel_path.display()
                )
            })?;
            if target.is_empty() {
                return Err(format!(
                    "Ninja manifest {} has an empty include/subninja directive.",
                    rel_path.display()
                ));
            }
            subfiles.push(PathBuf::from(target));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("dyndep")
            && let Some(val) = rest.trim_start().strip_prefix('=')
        {
            let dyndep_rel = unescape_ninja_path_checked(val.trim()).map_err(|reason| {
                format!(
                    "Ninja manifest {} references a variable-dependent dyndep path: {reason}.",
                    rel_path.display()
                )
            })?;
            if dyndep_rel.is_empty() {
                return Err(format!(
                    "Ninja manifest {} references an empty dyndep path.",
                    rel_path.display()
                ));
            }
            let candidate = Path::new(&dyndep_rel);
            if reader.check(candidate).is_err() {
                return Err(format!(
                    "Ninja manifest {} references unresolved dyndep file '{}' that requires an actual build.",
                    rel_path.display(),
                    dyndep_rel
                ));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("build ") {
            parse_ninja_build_edge(rest, dep_edges, budget)?;
        }
    }

    for sub in subfiles {
        if let Err(err) = reader.check(&sub) {
            return Err(format!(
                "Ninja manifest {} includes unreadable or external manifest '{}': {err}",
                rel_path.display(),
                sub.display()
            ));
        }
        scan_ninja_manifest(reader, &sub, depth + 1, visited, dep_edges, budget)?;
    }
    Ok(())
}

fn parse_ninja_build_edge(
    rest: &str,
    dep_edges: &mut NinjaDepGraph,
    budget: &mut NinjaGraphBudget,
) -> Result<(), String> {
    let Some((lhs, rhs)) = split_ninja_colon(rest) else {
        return Ok(());
    };
    let outputs = split_ninja_paths(lhs);
    let mut rhs_tokens = split_ninja_paths(rhs).into_iter();
    // First token after `:` is the rule name.
    let _rule = rhs_tokens.next();
    let mut seen_inputs: BTreeSet<BString> = BTreeSet::new();
    let mut inputs: Vec<BString> = Vec::new();
    for tok in rhs_tokens {
        budget.charge_work(1)?;
        if tok == "|" || tok == "||" {
            continue;
        }
        let b = BString::from(tok);
        if seen_inputs.insert(b.clone()) {
            inputs.push(b);
        }
    }
    if inputs.is_empty() {
        return Ok(());
    }

    let valid_outputs: Vec<BString> = outputs
        .into_iter()
        .filter(|out| out != "|")
        .map(BString::from)
        .collect();
    if valid_outputs.is_empty() {
        return Ok(());
    }
    budget.charge_work(valid_outputs.len())?;

    let input_bytes: usize = inputs
        .iter()
        .map(|inp| std::mem::size_of::<BString>().saturating_add(inp.len()))
        .fold(std::mem::size_of::<Arc<[BString]>>(), usize::saturating_add);
    budget.charge_bytes(input_bytes)?;

    let edge_idx = dep_edges.edge_inputs.len();
    dep_edges.edge_inputs.push(Arc::from(inputs));

    for out in valid_outputs {
        let is_new_key = !dep_edges.output_edges.contains_key(&out);
        let key_bytes = if is_new_key {
            std::mem::size_of::<BString>()
                .saturating_add(out.len())
                .saturating_add(64)
        } else {
            0
        };
        budget.charge_bytes(key_bytes.saturating_add(std::mem::size_of::<usize>()))?;
        let refs = dep_edges.output_edges.entry(out).or_default();
        if refs.last().copied() != Some(edge_idx) {
            refs.push(edge_idx);
        }
    }
    Ok(())
}

fn split_ninja_colon(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = *bytes.get(i)?;
        if b == b'$' {
            i += 2;
            continue;
        }
        if b == b':' {
            return Some((s.get(..i)?, s.get(i + 1..)?));
        }
        i += 1;
    }
    None
}

fn split_ninja_paths(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            match chars.next() {
                Some(' ') => cur.push(' '),
                Some(':') => cur.push(':'),
                Some('$') => cur.push('$'),
                Some(other) => {
                    cur.push('$');
                    cur.push(other);
                }
                None => {}
            }
        } else if ch.is_ascii_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Unescape Ninja path escapes (`$ `, `$:`, `$$`) and reject variable references (`$var`, `${var}`).
fn unescape_ninja_path_checked(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            match chars.next() {
                Some(' ') => out.push(' '),
                Some(':') => out.push(':'),
                Some('$') => out.push('$'),
                Some(other) => {
                    return Err(format!("variable-dependent path containing '${other}'"));
                }
                None => {
                    return Err("dangling '$' escape".to_owned());
                }
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

fn parse_ninja_dry_run_output(stdout: &[u8], stderr: &[u8]) -> Result<Vec<BString>, String> {
    let err_text = String::from_utf8_lossy(stderr);
    let err_lower = err_text.to_ascii_lowercase();
    if err_lower.contains("dyndep") {
        return Err("Ninja dry-run reported a dyndep dependency error.".to_owned());
    }
    if err_lower.contains("ninja: error:") {
        return Err("Ninja dry-run reported an error on stderr.".to_owned());
    }

    let text = String::from_utf8_lossy(stdout);
    let mut commands = Vec::new();
    let mut saw_no_work = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "ninja: no work to do." {
            saw_no_work = true;
            continue;
        }
        if is_regeneration_command(line) {
            return Err(
                "Ninja dry-run requires manifest regeneration before the build can be compared."
                    .to_owned(),
            );
        }
        if let Some(cmd) = strip_ninja_progress_prefix(line) {
            let trimmed = cmd.trim();
            if trimmed.is_empty() {
                return Err(format!(
                    "Ninja dry-run emitted an empty command line: '{line}'."
                ));
            }
            if is_regeneration_command(trimmed) {
                return Err(
                    "Ninja dry-run scheduled manifest regeneration before the build can be compared."
                        .to_owned(),
                );
            }
            commands.push(BString::from(trimmed));
        } else {
            let excerpt: String = line.chars().take(120).collect();
            return Err(format!(
                "Ninja dry-run produced unrecognized output: '{excerpt}'."
            ));
        }
    }

    if saw_no_work && !commands.is_empty() {
        return Err(
            "Ninja dry-run produced contradictory 'no work to do' and command lines.".to_owned(),
        );
    }
    if !saw_no_work && commands.is_empty() {
        return Err(
            "Ninja dry-run produced empty output without 'ninja: no work to do.'.".to_owned(),
        );
    }
    Ok(commands)
}

fn strip_ninja_progress_prefix(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let counter = rest.get(..close)?;
    let (lhs, rhs) = counter.split_once('/')?;
    if lhs.is_empty()
        || rhs.is_empty()
        || !lhs.bytes().all(|b| b.is_ascii_digit())
        || !rhs.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    rest.get(close + 1..).map(str::trim_start)
}

fn is_regeneration_command(cmd: &str) -> bool {
    cmd.contains("--regenerate-during-build") || cmd.contains("Re-running CMake")
}

fn json_bytes(val: Option<&Value>) -> Option<BString> {
    val.and_then(Value::as_bytes).map(BString::from)
}

fn json_array(val: Option<&Value>) -> &[Value] {
    val.and_then(Value::as_array).unwrap_or_default()
}

fn basename_stem(p: &[u8]) -> BString {
    let base = p.rsplit(|&b| b == b'/' || b == b'\\').next().unwrap_or(p);
    let stem = base
        .strip_suffix(b".exe")
        .or_else(|| base.strip_suffix(b".EXE"))
        .unwrap_or(base);
    BString::from(stem)
}

fn windows_like_path(p: &[u8]) -> bool {
    matches!(p, [letter, b':', ..] if letter.is_ascii_alphabetic())
        || p.starts_with(b"\\\\")
        || p.starts_with(b"//")
}

fn join_lexical(dir: &[u8], rel: &[u8]) -> BString {
    if windows_like_path(dir) || windows_like_path(rel) {
        let dir_fwd: Vec<u8> = dir
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect();
        let rel_fwd: Vec<u8> = rel
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect();
        if Lexical::windows(&rel_fwd).absolute() || dir_fwd.is_empty() {
            return Lexical::windows(&rel_fwd).normalize();
        }
        let joined = Lexical::windows(&dir_fwd).append(&rel_fwd);
        return Lexical::windows(&joined).normalize();
    }
    if rel.starts_with(b"/") || dir.is_empty() {
        Lexical::new(rel).normalize()
    } else {
        let joined = [dir, b"/", rel].concat();
        Lexical::new(&joined).normalize()
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
