// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-session prelude snapshotting and parallel prewarming for `try_compile`
//! children without a probe journal.
//!
//! Each `Sources`-signature `try_compile` child starts by running
//! `project(CMAKE_TRY_COMPILE LANGUAGES ...)`, which verifies Ninja in the
//! probe's scratch directory and evaluates the bundled system, platform,
//! compiler, and linker modules. When the probe cache journal is inactive
//! (for example under `RCMAKE_NO_PROBE_CACHE=1` or `--profile`), this module:
//!
//! 1. Refreshes `PlatformQueries` in parallel and verifies native Ninja live in
//!    the child's exact scratch directory in parallel with child interpreter
//!    construction.
//! 2. Caches a verified in-memory snapshot of the post-`project()` interpreter
//!    state keyed by the exact seed defines, enabled/probe languages, outer
//!    policies, environment, generator, and live content digests of
//!    `CMakeSystem.cmake` and `CMake<LANG>Compiler.cmake`.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rcmake_base::BString;
use rcmake_diag::PolicyStatus;
use rcmake_eval::{CacheDefine, Cx, ProbeCacheMode};
use rcmake_host::{Follow, Host, path};
use rcmake_model::{CacheEntry, DirId, GeneratorKind, GeneratorSpec, PropertyMap, UsageProps};
use rcmake_store::{Digest, Domain, KeyHasher, TraceMemo};

use crate::configure::build_tool_facts::{BuildToolFacts, is_native_ninja};
use crate::configure::concurrent;
use crate::configure::platform_queries::PlatformQueries;

use super::active::admit_environment;
use super::capture::{CapturingHost, MetaObservation, listing_digest};
use super::plan::process_environment_fingerprint;
use super::project::outer_policies;

const MAX_PRELUDE_ENTRIES: usize = 16;

/// Largest directory listing that a snapshot verifies.
const MAX_VERIFIED_LISTING: usize = 16_384;

/// Variables that depend on the individual probe scratch directory or active
/// listfile and must always be populated fresh by `run_configure` / `project()`.
const SCRATCH_SPECIFIC_VARS: &[&[u8]] = &[
    b"CMAKE_BINARY_DIR",
    b"CMAKE_CACHEFILE_DIR",
    b"CMAKE_CURRENT_BINARY_DIR",
    b"CMAKE_CURRENT_LIST_DIR",
    b"CMAKE_CURRENT_LIST_FILE",
    b"CMAKE_CURRENT_LIST_LINE",
    b"CMAKE_CURRENT_SOURCE_DIR",
    b"CMAKE_FIND_PACKAGE_REDIRECTS_DIR",
    b"CMAKE_HOME_DIRECTORY",
    b"CMAKE_LIST_FILE_NAME",
    b"CMAKE_PARENT_LIST_FILE",
    b"CMAKE_PROJECT_NAME",
    b"CMAKE_SOURCE_DIR",
    b"CMAKE_TRY_COMPILE_BINARY_DIR",
    b"CMAKE_TRY_COMPILE_SOURCE_DIR",
    b"PROJECT_BINARY_DIR",
    b"PROJECT_NAME",
    b"PROJECT_SOURCE_DIR",
];

/// Variables that can inject arbitrary user code or stateful overrides during
/// `project(CMAKE_TRY_COMPILE ...)`; when any is non-empty, prelude caching is
/// bypassed so the user script always executes live.
const HOOK_VARIABLES: &[&[u8]] = &[
    b"CMAKE_TOOLCHAIN_FILE",
    b"CMAKE_MODULE_PATH",
    b"CMAKE_PROJECT_INCLUDE",
    b"CMAKE_PROJECT_INCLUDE_BEFORE",
    b"CMAKE_PROJECT_CMAKE_TRY_COMPILE_INCLUDE",
    b"CMAKE_PROJECT_CMAKE_TRY_COMPILE_INCLUDE_BEFORE",
    b"CMAKE_PROJECT_TOP_LEVEL_INCLUDES",
    b"CMAKE_USER_MAKE_RULES_OVERRIDE",
];

/// Per-configure-session cache of verified post-`project()` probe preludes.
#[derive(Default)]
pub(super) struct PreludeCache {
    entries: Mutex<BTreeMap<Digest, Arc<PreludeSnapshot>>>,
    memo: OnceLock<Arc<TraceMemo>>,
}

impl PreludeCache {
    pub(super) fn trace_memo(&self, parent: Option<Arc<TraceMemo>>) -> Arc<TraceMemo> {
        if let Some(memo) = parent {
            return memo;
        }
        Arc::clone(self.memo.get_or_init(|| Arc::new(TraceMemo::default())))
    }

    fn get(&self, key: &Digest) -> Option<Arc<PreludeSnapshot>> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn insert(&self, key: Digest, snapshot: Arc<PreludeSnapshot>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() < MAX_PRELUDE_ENTRIES || entries.contains_key(&key) {
            entries.insert(key, snapshot);
        }
    }

    fn remove(&self, key: &Digest) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

/// Verified interpreter state captured immediately after `project(CMAKE_TRY_COMPILE)`.
pub(super) struct PreludeSnapshot {
    normal_vars: Vec<(BString, BString)>,
    cache_entries: Vec<(BString, CacheEntry)>,
    global_props: PropertyMap,
    root_dir_props: PropertyMap,
    verified_reads: Vec<(PathBuf, Digest)>,
    verified_missing: Vec<(PathBuf, Follow)>,
    verified_exists: Vec<(PathBuf, Follow, MetaObservation)>,
    verified_dirs: Vec<(PathBuf, Digest)>,
    verified_links: Vec<(PathBuf, PathBuf)>,
    verified_canonical: Vec<(PathBuf, PathBuf)>,
}

impl PreludeSnapshot {
    /// Verify that every file, absence, existence, directory listing, symlink, and
    /// canonicalization observed while capturing this prelude still matches live state.
    fn is_valid(&self, host: &dyn Host, trace_memo: &TraceMemo) -> bool {
        self.verified_reads.iter().all(|(path, digest)| {
            trace_memo
                .file_digest(host, path)
                .is_ok_and(|current| current == *digest)
        }) && self.verified_missing.iter().all(|(path, follow)| {
            host.metadata_fresh(path, *follow)
                .is_err_and(|err| err.kind == ErrorKind::NotFound)
        }) && self.verified_exists.iter().all(|(path, follow, seen)| {
            host.metadata_fresh(path, *follow)
                .is_ok_and(|meta| seen.matches(&meta))
        }) && self.verified_dirs.iter().all(|(dir, digest)| {
            fresh_directory_digest(host, dir).is_some_and(|current| current == *digest)
        }) && self
            .verified_links
            .iter()
            .all(|(link, target)| host.read_link(link).is_ok_and(|current| current == *target))
            && self.verified_canonical.iter().all(|(path, canon)| {
                host.canonicalize_fresh(path)
                    .is_ok_and(|current| current == *canon)
            })
    }

    /// Restore the snapshotted post-`project()` state into a fresh child
    /// interpreter before `run_configure` executes.
    pub(super) fn apply(&self, cx: &mut Cx<'_>) {
        for (name, entry) in &self.cache_entries {
            cx.insert_cache_entry_unrecorded(name.clone(), entry.clone());
        }
        for (name, value) in &self.normal_vars {
            cx.set_var(name, value.clone());
        }
        let model = cx.project();
        for key in self.global_props.names() {
            if let Some(value) = self.global_props.get(&key) {
                model.global.props.set(key, value.clone());
            }
        }
        for key in self.root_dir_props.names() {
            if let Some(value) = self.root_dir_props.get(&key) {
                model.dir_mut(DirId::ROOT).props.set(key, value.clone());
            }
        }
    }
}

/// How one `try_compile` child interacts with [`PreludeCache`].
pub(super) enum PreludePlan {
    Bypass,
    Hit(Arc<PreludeSnapshot>),
    Capture(Arc<PreludeCapture>),
}

/// Active capture handle attached as session data to a child interpreter on a
/// prelude cache miss.
pub(super) struct PreludeCapture {
    cache: Arc<PreludeCache>,
    key: Digest,
    binary_dir: PathBuf,
    /// Every spelling of the scratch source and binary directories: CMake form,
    /// canonical form, and native form. No snapshot value may contain one.
    scratch_paths: Vec<BString>,
    /// Languages that `project()` enables. Each one other than `NONE` must load its
    /// `CMake<LANG>Information.cmake`.
    probe_languages: Vec<BString>,
    capture_host: OnceLock<Arc<CapturingHost>>,
    captured: OnceLock<()>,
}

impl PreludeCapture {
    pub(super) fn attach_host(&self, host: Arc<CapturingHost>) {
        host.begin_prelude_tracking(&self.binary_dir);
        let _ = self.capture_host.set(host);
    }

    pub(super) fn finish(&self, cx: &Cx<'_>) {
        if self.captured.set(()).is_err() {
            return;
        }
        let Some(capture_host) = self.capture_host.get() else {
            return;
        };
        let Some(trace) = capture_host.finish_prelude_tracking() else {
            return;
        };
        if cx.fatal_occurred() || !capture_host.is_silent() || trace.mutated_external {
            return;
        }
        let model = cx.project_ref();
        if model.targets().next().is_some() {
            return;
        }
        // Refuse model state that the snapshot does not restore.
        let root = model.dir(DirId::ROOT);
        if root.usage != UsageProps::default()
            || !root.define_flags.is_empty()
            || !root.source_props.is_empty()
            || !model.global.defined_props.is_empty()
            || !model.global.instrumentation.is_empty()
            || model.commands().next().is_some()
        {
            return;
        }
        let contains_scratch = |value: &[u8]| {
            self.scratch_paths
                .iter()
                .any(|needle| contains_bytes(value, needle))
        };
        if cx.var_normal_only(b"CMAKE_SYSTEM_LOADED").is_none()
            || cx
                .var_normal_only(b"CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED")
                .is_none()
            || cx
                .var_normal_only(b"CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED")
                .is_none()
        {
            return;
        }
        let loaded = |lang: &BString| {
            let name = [b"CMAKE_".as_slice(), lang, b"_INFORMATION_LOADED"].concat();
            cx.var_normal_only(&name).is_some()
        };
        if !self
            .probe_languages
            .iter()
            .filter(|lang| lang.as_slice() != b"NONE")
            .all(loaded)
        {
            return;
        }

        let mut cache_entries = Vec::new();
        for (name, entry) in model.cache.iter() {
            if is_scratch_var(name) {
                continue;
            }
            if contains_scratch(&entry.value) {
                return;
            }
            cache_entries.push((name.clone(), entry.clone()));
        }

        let mut normal_vars = Vec::new();
        for name in cx.variable_names() {
            if is_scratch_var(&name) {
                continue;
            }
            if let Some(value) = cx.var_normal_only(&name) {
                if contains_scratch(&value) {
                    return;
                }
                normal_vars.push((name, value));
            }
        }

        let global_props = model.global.props.clone();
        if global_props
            .names()
            .iter()
            .any(|key| global_props.get(key).is_some_and(|v| contains_scratch(v)))
        {
            return;
        }
        let root_dir_props = model.dir(DirId::ROOT).props.clone();
        if root_dir_props
            .names()
            .iter()
            .any(|key| root_dir_props.get(key).is_some_and(|v| contains_scratch(v)))
        {
            return;
        }

        // Reject if any path observed both existence/read and absence during capture.
        if trace.missing_files.iter().any(|(path, _)| {
            trace.read_files.contains_key(path)
                || trace.listed_dirs.contains_key(path)
                || trace.existing_files.contains_key(&(path.clone(), true))
                || trace.existing_files.contains_key(&(path.clone(), false))
        }) {
            return;
        }

        // The tracker hashed the exact bytes and listings at read time, so a change
        // between the read and this point makes the snapshot invalid, not wrong.
        let verified_reads: Vec<(PathBuf, Digest)> = trace.read_files.into_iter().collect();
        let verified_missing: Vec<(PathBuf, Follow)> = trace
            .missing_files
            .into_iter()
            .map(|(path, follow_yes)| (path, if follow_yes { Follow::Yes } else { Follow::No }))
            .collect();
        // Keep every metadata observation, also for read files: a content digest does
        // not cover the mode, and a `Follow::No` probe sees the link, not the target.
        let verified_exists: Vec<(PathBuf, Follow, MetaObservation)> = trace
            .existing_files
            .into_iter()
            .map(|((path, follow_yes), seen)| {
                (
                    path,
                    if follow_yes { Follow::Yes } else { Follow::No },
                    seen,
                )
            })
            .collect();
        let verified_dirs: Vec<(PathBuf, Digest)> = trace.listed_dirs.into_iter().collect();
        let verified_links: Vec<(PathBuf, PathBuf)> = trace.read_links.into_iter().collect();
        let verified_canonical: Vec<(PathBuf, PathBuf)> =
            trace.canonical_paths.into_iter().collect();

        self.cache.insert(
            self.key,
            Arc::new(PreludeSnapshot {
                normal_vars,
                cache_entries,
                global_props,
                root_dir_props,
                verified_reads,
                verified_missing,
                verified_exists,
                verified_dirs,
                verified_links,
                verified_canonical,
            }),
        );
    }
}

/// Live prewarming inputs for a non-journaled `try_compile` child.
pub(super) struct PrewarmContext {
    host: Arc<dyn Host>,
    platform_queries: Option<Arc<PlatformQueries>>,
    build_tool_facts: Option<Arc<BuildToolFacts>>,
    trace_memo: Arc<TraceMemo>,
    ninja_program: Option<PathBuf>,
    requires_ninja: bool,
    raw_env: Vec<(BString, BString)>,
}

impl PrewarmContext {
    /// Run `PlatformQueries::refresh` (parallel Darwin queries when locale/env
    /// changed) and live `BuildToolFacts::sample_screened_ninja` in `binary_dir`
    /// concurrently. Returns `true` when Ninja verification either is not
    /// required or succeeded live in `binary_dir`.
    pub(super) fn run(&self, binary_dir: &Path) -> bool {
        let (_, ninja_ok) = concurrent::join(
            || {
                if let Some(queries) = &self.platform_queries {
                    queries.refresh(self.host.as_ref(), &self.raw_env);
                }
            },
            || {
                if !self.requires_ninja {
                    return true;
                }
                let Some(facts) = &self.build_tool_facts else {
                    return false;
                };
                let Some(program) = &self.ninja_program else {
                    return false;
                };
                if !self.host.supports_process_execution()
                    || !program.is_absolute()
                    || !admit_environment(&self.raw_env)
                {
                    return false;
                }
                let Ok(canonical) = self.host.canonicalize(program) else {
                    return false;
                };
                if !is_native_ninja(self.host.as_ref(), &canonical) {
                    return false;
                }
                let Ok(digest) = self.trace_memo.file_digest(self.host.as_ref(), program) else {
                    return false;
                };
                facts
                    .sample_screened_ninja(
                        self.host.as_ref(),
                        Some(self.trace_memo.as_ref()),
                        program,
                        digest,
                        self.raw_env.clone(),
                        binary_dir.to_path_buf(),
                    )
                    .is_some()
            },
        );
        ninja_ok
    }
}

/// Prepare both the live [`PrewarmContext`] and [`PreludePlan`] for a
/// non-journaled `try_compile` child.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare(
    cx: &mut Cx<'_>,
    defines: &[CacheDefine],
    enabled_languages: &[u8],
    probe_languages: &[BString],
    source_dir: &Path,
    binary_dir: &Path,
    source_signature: bool,
    save_cache: bool,
    journaled: bool,
) -> (Option<PrewarmContext>, PreludePlan) {
    if journaled {
        return (None, PreludePlan::Bypass);
    }
    let host = cx.host_arc();
    let raw_env = host.env_all();
    let trace_active = cx.trace_config().is_some();
    let verify_mode = cx.services().speed.probe_cache == ProbeCacheMode::Verify;

    let runtime_memo = cx
        .session_data::<Arc<super::active::ProbeRuntime>>()
        .map(|runtime| Arc::clone(runtime.trace_memo()));
    let cache = cx
        .session_data_mut_or_init::<Arc<PreludeCache>>()
        .map(|cache| Arc::clone(cache));
    let trace_memo = cache
        .as_ref()
        .map_or_else(Default::default, |cache| cache.trace_memo(runtime_memo));

    let platform_queries = (!trace_active
        && !host
            .env_get(b"RCMAKE_NO_PLATFORM_QUERIES")
            .is_some_and(|value| value == b"1"))
    .then(|| cx.session_data::<Arc<PlatformQueries>>().cloned())
    .flatten();
    let build_tool_facts = cx.session_data::<Arc<BuildToolFacts>>().cloned();
    let generator = cx.project_ref().generator;
    let generator_spec = cx.project_ref().generator_spec.clone();
    let requires_ninja = matches!(
        generator,
        Some(GeneratorKind::Ninja | GeneratorKind::NinjaMultiConfig)
    );
    let ninja_program = requires_ninja
        .then(|| {
            cx.peek_var(b"CMAKE_MAKE_PROGRAM")
                .map(|value| path::to_native(value))
                .filter(|program| program.is_absolute())
        })
        .flatten();

    let prewarm = (source_signature
        && !trace_active
        && (platform_queries.is_some() || (requires_ninja && build_tool_facts.is_some())))
    .then(|| PrewarmContext {
        host: Arc::clone(&host),
        platform_queries,
        build_tool_facts,
        trace_memo: Arc::clone(&trace_memo),
        ninja_program,
        requires_ninja,
        raw_env: raw_env.clone(),
    });

    let Some(cache) = cache else {
        return (prewarm, PreludePlan::Bypass);
    };
    if !source_signature
        || save_cache
        || trace_active
        || verify_mode
        || probe_languages.is_empty()
        || !host.supports_process_execution()
        || host
            .env_get(b"RCMAKE_NO_PRELUDE_CACHE")
            .is_some_and(|value| value == b"1")
        || has_user_hooks(cx, defines, probe_languages)
    {
        return (prewarm, PreludePlan::Bypass);
    }

    let cmake_root = path::to_native(&cx.var_or_empty(b"CMAKE_ROOT"));
    let platform_dir = path::to_native(&cx.var_or_empty(b"CMAKE_PLATFORM_INFO_DIR"));
    let policies = outer_policies(cx);
    let Some(key) = prelude_key(
        host.as_ref(),
        trace_memo.as_ref(),
        &cmake_root,
        &platform_dir,
        generator,
        &generator_spec,
        defines,
        enabled_languages,
        probe_languages,
        &policies,
        &raw_env,
    ) else {
        return (prewarm, PreludePlan::Bypass);
    };

    if let Some(snapshot) = cache.get(&key) {
        if snapshot.is_valid(host.as_ref(), trace_memo.as_ref()) {
            return (prewarm, PreludePlan::Hit(snapshot));
        }
        cache.remove(&key);
    }

    let scratch_paths = scratch_spellings(host.as_ref(), &[source_dir, binary_dir]);
    let capture = Arc::new(PreludeCapture {
        cache,
        key,
        binary_dir: binary_dir.to_path_buf(),
        scratch_paths,
        probe_languages: probe_languages.to_vec(),
        capture_host: OnceLock::new(),
        captured: OnceLock::new(),
    });
    (prewarm, PreludePlan::Capture(capture))
}

#[allow(clippy::too_many_arguments)]
fn prelude_key(
    host: &dyn Host,
    memo: &TraceMemo,
    cmake_root: &Path,
    platform_dir: &Path,
    generator: Option<GeneratorKind>,
    generator_spec: &GeneratorSpec,
    defines: &[CacheDefine],
    enabled_languages: &[u8],
    probe_languages: &[BString],
    policies: &[(u16, PolicyStatus); 4],
    raw_env: &[(BString, BString)],
) -> Option<Digest> {
    if !platform_dir.is_absolute() {
        return None;
    }
    // Check compatibility hook absence so a custom CMAKE_TRY_COMPILECompatibility.cmake
    // always runs live.
    if host
        .metadata_fresh(
            &cmake_root.join("Modules/CMAKE_TRY_COMPILECompatibility.cmake"),
            Follow::Yes,
        )
        .is_ok()
    {
        return None;
    }
    let system_file = platform_dir.join("CMakeSystem.cmake");
    let system_digest = memo.file_digest(host, &system_file).ok()?;

    let mut key = KeyHasher::new(Domain::ProbeV1);
    key.field(0, b"try-compile-prelude-v1");
    key.field(1, &path::from_native(cmake_root));
    key.field(2, format!("{generator:?}:{generator_spec:?}").as_bytes());
    key.field(3, enabled_languages);
    for lang in probe_languages {
        let compiler_file = platform_dir.join(format!(
            "CMake{}Compiler.cmake",
            String::from_utf8_lossy(lang)
        ));
        let compiler_digest = memo.file_digest(host, &compiler_file).ok()?;
        key.field(4, lang);
        key.field(5, compiler_digest.as_bytes());
    }
    key.field(6, system_digest.as_bytes());
    for &(id, status) in policies {
        key.field(7, &id.to_le_bytes());
        key.field(8, format!("{status:?}").as_bytes());
    }
    for define in defines {
        key.field(9, &define.name);
        key.field(10, define.ty.map_or(b"", |ty| ty.as_str().as_bytes()));
        key.field(11, &define.value);
    }
    let system = host.system();
    key.field(
        12,
        format!(
            "{:?}:{:?}:{:?}:{:?}",
            system.sysname, system.release, system.version, system.machine
        )
        .as_bytes(),
    );
    key.field(13, process_environment_fingerprint(raw_env).as_bytes());
    Some(key.finish())
}

fn has_user_hooks(cx: &Cx<'_>, defines: &[CacheDefine], languages: &[BString]) -> bool {
    let nonempty_in_cx = |name: &[u8]| cx.peek_var(name).is_some_and(|value| !value.is_empty());
    let nonempty_in_defines = |name: &[u8]| {
        defines
            .iter()
            .any(|define| define.name.as_slice() == name && !define.value.is_empty())
    };
    if HOOK_VARIABLES
        .iter()
        .any(|&var| nonempty_in_cx(var) || nonempty_in_defines(var))
    {
        return true;
    }
    languages.iter().any(|lang| {
        let override_var = [
            b"CMAKE_USER_MAKE_RULES_OVERRIDE_".as_slice(),
            lang.as_slice(),
        ]
        .concat();
        nonempty_in_cx(&override_var) || nonempty_in_defines(&override_var)
    })
}

/// Return the CMake, canonical, and native spellings of `dirs`, without duplicates.
fn scratch_spellings(host: &dyn Host, dirs: &[&Path]) -> Vec<BString> {
    let mut spellings: Vec<BString> = Vec::new();
    for dir in dirs {
        let canonical = host.canonicalize(dir).ok();
        for form in std::iter::once(*dir).chain(canonical.as_deref()) {
            let native = BString::from(form.to_string_lossy().as_bytes());
            for spelling in [path::from_native_cmake(form), native] {
                if !spelling.is_empty() && !spellings.contains(&spelling) {
                    spellings.push(spelling);
                }
            }
        }
    }
    spellings
}

/// Hash a fresh, bounded listing of `dir` the same way the capture tracker does.
///
/// Return `None` when the listing fails or has more than [`MAX_VERIFIED_LISTING`] entries.
fn fresh_directory_digest(host: &dyn Host, dir: &Path) -> Option<Digest> {
    match host.read_dir_bounded(dir, MAX_VERIFIED_LISTING) {
        Ok((entries, false)) => Some(listing_digest(&entries)),
        _ => None,
    }
}

fn is_scratch_var(name: &[u8]) -> bool {
    SCRATCH_SPECIFIC_VARS.contains(&name)
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
