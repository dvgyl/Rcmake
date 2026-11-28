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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use rcmake_base::BString;
use rcmake_diag::{DiagState, PolicyStatus};
use rcmake_eval::{CacheDefine, Cx, LogLevel, ProbeCacheMode};
use rcmake_host::{Follow, Host, path};
use rcmake_model::{CacheEntry, DirId, GeneratorKind, GeneratorSpec, PropertyMap, UsageProps};
use rcmake_store::{Digest, Domain, KeyHasher, TraceMemo};

use crate::configure::build_tool_facts::{BuildToolFacts, is_native_ninja};
use crate::configure::concurrent;
use crate::configure::platform_queries::PlatformQueries;

use super::active::admit_environment;
use super::capture::{CapturingHost, MetaObservation, listing_digest};
use super::journal::{JournalHost, PreludeJournalSlice};
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

    fn remove_if_same(&self, key: &Digest, expected: &Arc<PreludeSnapshot>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            entries.remove(key);
        }
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
    verified_scratch_missing: Vec<(PathBuf, Follow)>,
    verified_exists: Vec<(PathBuf, Follow, MetaObservation)>,
    verified_dirs: Vec<(PathBuf, Digest)>,
    verified_links: Vec<(PathBuf, PathBuf)>,
    verified_canonical: Vec<(PathBuf, PathBuf)>,
    journal_slice: Option<PreludeJournalSlice>,
    /// Maximum recursion depth delta reached during prelude execution relative to `project()` entry.
    peak_depth_delta: usize,
}

impl PreludeSnapshot {
    /// Return the journal observations captured with this prelude, if any.
    pub(super) fn journal_slice(&self) -> Option<&PreludeJournalSlice> {
        self.journal_slice.as_ref()
    }

    /// Return the peak recursion depth delta observed during prelude execution.
    pub(super) fn peak_depth_delta(&self) -> usize {
        self.peak_depth_delta
    }

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

    /// Verify that every scratch-relative absence observed during capture still holds in the
    /// target probe's scratch directory (e.g. an optional `include()` fallback to
    /// `CMAKE_CURRENT_SOURCE_DIR` must not miss a file created by `SOURCE_FROM_CONTENT`).
    fn is_scratch_valid(&self, host: &dyn Host, source_dir: &Path, binary_dir: &Path) -> bool {
        let missing_in = |dir: &Path| {
            self.verified_scratch_missing.iter().all(|(rel, follow)| {
                host.metadata_fresh(&dir.join(rel), *follow)
                    .is_err_and(|err| err.kind == ErrorKind::NotFound)
            }) && self
                .journal_slice
                .as_ref()
                .is_none_or(|slice| slice.validate_scratch(host, dir))
        };
        missing_in(binary_dir) && (source_dir == binary_dir || missing_in(source_dir))
    }

    /// Verify both filesystem observations and the captured journal slice against the live host.
    fn is_valid_for_journal(&self, live_host: &dyn Host, trace_memo: &TraceMemo) -> bool {
        self.journal_slice
            .as_ref()
            .is_some_and(|slice| slice.validate(live_host, trace_memo))
            && self.is_valid(live_host, trace_memo)
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

/// Built-in commands used by generated post-`project()` `try_compile` listfiles.
/// If any of these was overridden by a prelude module, refuse to cache the prelude.
/// Flow-control keywords (`if`, `endif`) cannot be overridden by user commands.
const POST_PROJECT_BUILTINS: &[&[u8]] = &[
    b"add_executable",
    b"add_library",
    b"include_directories",
    b"link_directories",
    b"separate_arguments",
    b"set",
    b"set_property",
    b"string",
    b"target_compile_definitions",
    b"target_compile_options",
    b"target_link_libraries",
    b"target_link_options",
];

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
    /// Cache entry names seeded by the caller's `defines`.
    seeded_cache_names: Vec<BString>,
    /// Cache entry names present in the child immediately before `run_configure`.
    initial_cache_names: OnceLock<Vec<BString>>,
    /// Recursion limit in effect before `project()` executed.
    initial_recursion_limit: OnceLock<u32>,
    journaled: bool,
    defer_publish: AtomicBool,
    staged: Mutex<Option<(PreludeSnapshot, u32)>>,
    capture_host: OnceLock<Weak<CapturingHost>>,
    journal_host: OnceLock<Weak<JournalHost<'static>>>,
    body_started: OnceLock<()>,
    captured: OnceLock<()>,
}

impl PreludeCapture {
    /// Defer publishing the captured snapshot until [`Self::finalize_and_publish`] supplies
    /// the child interpreter's observed peak recursion depth.
    pub(super) fn defer_publish(&self) {
        self.defer_publish.store(true, Ordering::Release);
    }

    /// Record the child's initial cache keys and recursion limit before `run_configure`.
    pub(super) fn record_initial_state(&self, cx: &Cx<'_>) {
        let names = cx
            .project_ref()
            .cache
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        let _ = self.initial_cache_names.set(names);
        let _ = self.initial_recursion_limit.set(cx.recursion_limit());
    }

    pub(super) fn attach_host(&self, host: &Arc<CapturingHost>) {
        host.begin_prelude_tracking(&self.binary_dir);
        let _ = self.capture_host.set(Arc::downgrade(host));
    }

    /// Attach a weak reference to the active [`JournalHost`] and arm harness-phase scratch detection.
    pub(super) fn attach_journal(&self, journal: &Arc<JournalHost<'static>>) {
        journal.begin_prelude_harness();
        let _ = self.journal_host.set(Arc::downgrade(journal));
    }

    /// Enter the `project()` prelude body, switching both host trackers to strict scratch
    /// detection with zero exemptions for `CMakeLists.txt` or scratch directories.
    pub(super) fn begin_body(&self, cx: &Cx<'_>) {
        if self.body_started.set(()).is_err() {
            return;
        }
        let _ = self.initial_recursion_limit.set(cx.recursion_limit());
        if let Some(capture_host) = self.capture_host.get().and_then(Weak::upgrade) {
            capture_host.enter_prelude_body();
        }
        if let Some(journal) = self.journal_host.get().and_then(Weak::upgrade) {
            journal.enter_prelude_body();
        }
    }

    /// Finalize a deferred snapshot with the child's observed `max_start_depth` and publish it.
    pub(super) fn finalize_and_publish(
        &self,
        configure_ok: bool,
        recursion_exceeded: bool,
        max_start_depth: u32,
    ) {
        let Some((mut snapshot, initial_limit)) = self
            .staged
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        if !configure_ok || recursion_exceeded || max_start_depth > initial_limit {
            return;
        }
        snapshot.peak_depth_delta = initial_limit.saturating_sub(max_start_depth) as usize;
        self.cache.insert(self.key, Arc::new(snapshot));
    }

    pub(super) fn finish(&self, cx: &Cx<'_>) {
        if self.captured.set(()).is_err() {
            return;
        }
        let Some(capture_host) = self.capture_host.get().and_then(Weak::upgrade) else {
            return;
        };
        let Some(trace) = capture_host.finish_prelude_tracking() else {
            return;
        };
        let journal_slice = if self.journaled {
            let Some(journal) = self.journal_host.get().and_then(Weak::upgrade) else {
                return;
            };
            let slice = journal.capture_prelude_slice();
            drop(journal);
            let Some(slice) = slice else {
                return;
            };
            Some(slice)
        } else {
            None
        };
        let current_limit = cx.recursion_limit();
        if let Some(&initial_limit) = self.initial_recursion_limit.get()
            && current_limit != initial_limit
        {
            return;
        }
        if cx.fatal_occurred()
            || !capture_host.is_silent()
            || trace.mutated_external
            || cx.observes_commands()
        {
            return;
        }
        if cx.log_level() != LogLevel::Status
            || *cx.diag_state() != DiagState::default()
            || cx
                .defer_get_call_ids(DirId::ROOT)
                .is_some_and(|ids| !ids.is_empty())
            || POST_PROJECT_BUILTINS
                .iter()
                .any(|builtin| !cx.is_builtin(builtin))
        {
            return;
        }
        let stock_modules = [
            path::from_native_cmake(cx.cmake_root()).as_slice(),
            b"/Modules/",
        ]
        .concat();
        for cmd_name in cx.registered_command_names() {
            if cx.user_command_kind(&cmd_name).is_some() {
                let overridden = [b"_".as_slice(), cmd_name.as_slice()].concat();
                let in_stock_modules = cx
                    .user_command_file(&cmd_name)
                    .is_some_and(|file| file.starts_with(&stock_modules));
                if cx.is_builtin(&overridden) || !in_stock_modules {
                    return;
                }
            }
        }
        let model = cx.project_ref();
        if model.targets().next().is_some()
            || model.dirs().count() > 1
            || !model.installs.is_empty()
            || !model.exports.is_empty()
        {
            return;
        }
        // Reject if `project()` removed any seeded or pre-configure cache entry (`unset(... CACHE)`).
        if self
            .seeded_cache_names
            .iter()
            .chain(self.initial_cache_names.get().into_iter().flatten())
            .filter(|name| !is_scratch_var(name))
            .any(|name| model.cache.get(name).is_none())
        {
            return;
        }
        // Refuse model state that the snapshot does not restore.
        let root = model.dir(DirId::ROOT);
        if root.usage != UsageProps::default()
            || !root.define_flags.is_empty()
            || !root.source_props.is_empty()
            || !root.source_groups.is_empty()
            || !root.generated_sources.is_empty()
            || !root.local_targets.is_empty()
            || !root.tests.is_empty()
            || !root.discoveries.is_empty()
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
            // Refuse if a normal-scope `unset()` shadows this cache entry, since
            // `PreludeSnapshot::apply` does not replay `Binding::Unset`.
            if cx.peek_var(name).is_none() {
                return;
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
        let verified_scratch_missing: Vec<(PathBuf, Follow)> = trace
            .missing_scratch
            .into_iter()
            .map(|(rel, follow_yes)| (rel, if follow_yes { Follow::Yes } else { Follow::No }))
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

        let snapshot = PreludeSnapshot {
            normal_vars,
            cache_entries,
            global_props,
            root_dir_props,
            verified_reads,
            verified_missing,
            verified_scratch_missing,
            verified_exists,
            verified_dirs,
            verified_links,
            verified_canonical,
            journal_slice,
            peak_depth_delta: 0,
        };
        if self.defer_publish.load(Ordering::Acquire) {
            *self.staged.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((snapshot, current_limit));
        } else {
            self.cache.insert(self.key, Arc::new(snapshot));
        }
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

/// Prepare both the live [`PrewarmContext`] and [`PreludePlan`] for a `try_compile` child.
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
    let runtime = cx
        .session_data::<Arc<super::active::ProbeRuntime>>()
        .cloned();
    let live_host = runtime
        .as_ref()
        .map_or_else(|| cx.host_arc(), |r| Arc::clone(r.live_host()));
    let raw_env: Arc<[(BString, BString)]> = runtime
        .as_ref()
        .map_or_else(|| Arc::from(live_host.env_all()), |r| r.env_snapshot().raw);
    let trace_active = cx.trace_config().is_some();
    let verify_mode = cx.services().speed.probe_cache == ProbeCacheMode::Verify;

    let runtime_memo = runtime
        .as_ref()
        .map(|runtime| Arc::clone(runtime.trace_memo()));
    let cache = cx
        .session_data_mut_or_init::<Arc<PreludeCache>>()
        .map(|cache| Arc::clone(cache));
    let trace_memo = cache
        .as_ref()
        .map_or_else(Default::default, |cache| cache.trace_memo(runtime_memo));

    let generator = cx.project_ref().generator;
    let generator_spec = cx.project_ref().generator_spec.clone();
    let prewarm = if !journaled {
        let platform_queries = (!trace_active
            && !live_host
                .env_get(b"RCMAKE_NO_PLATFORM_QUERIES")
                .is_some_and(|value| value == b"1"))
        .then(|| cx.session_data::<Arc<PlatformQueries>>().cloned())
        .flatten();
        let build_tool_facts = cx.session_data::<Arc<BuildToolFacts>>().cloned();
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
        (source_signature
            && !trace_active
            && (platform_queries.is_some() || (requires_ninja && build_tool_facts.is_some())))
        .then(|| PrewarmContext {
            host: Arc::clone(&live_host),
            platform_queries,
            build_tool_facts,
            trace_memo: Arc::clone(&trace_memo),
            ninja_program,
            requires_ninja,
            raw_env: raw_env.to_vec(),
        })
    } else {
        None
    };

    let Some(cache) = cache else {
        return (prewarm, PreludePlan::Bypass);
    };
    if !source_signature
        || save_cache
        || trace_active
        || verify_mode
        || probe_languages.is_empty()
        || !live_host.supports_process_execution()
        || live_host
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
        live_host.as_ref(),
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
        let caller_depth = cx.recursion_depth() as usize;
        let max_depth = cx.recursion_limit() as usize;
        if caller_depth.saturating_add(snapshot.peak_depth_delta) > max_depth {
            return (prewarm, PreludePlan::Bypass);
        }
        if journaled {
            // Never serve an unjournaled snapshot to a journaled child.
            if snapshot.journal_slice.is_none() {
                return (prewarm, PreludePlan::Bypass);
            }
            if snapshot.is_valid_for_journal(live_host.as_ref(), trace_memo.as_ref()) {
                if snapshot.is_scratch_valid(live_host.as_ref(), source_dir, binary_dir) {
                    return (prewarm, PreludePlan::Hit(snapshot));
                }
                return (prewarm, PreludePlan::Bypass);
            }
            cache.remove_if_same(&key, &snapshot);
        } else {
            let slice_ok = snapshot
                .journal_slice
                .as_ref()
                .is_none_or(|slice| slice.validate(live_host.as_ref(), trace_memo.as_ref()));
            if slice_ok && snapshot.is_valid(live_host.as_ref(), trace_memo.as_ref()) {
                if snapshot.is_scratch_valid(live_host.as_ref(), source_dir, binary_dir) {
                    return (prewarm, PreludePlan::Hit(snapshot));
                }
                return (prewarm, PreludePlan::Bypass);
            }
            cache.remove_if_same(&key, &snapshot);
        }
    }

    let scratch_paths = scratch_spellings(live_host.as_ref(), &[source_dir, binary_dir]);
    let seeded_cache_names = defines.iter().map(|define| define.name.clone()).collect();
    let capture = Arc::new(PreludeCapture {
        cache,
        key,
        binary_dir: binary_dir.to_path_buf(),
        scratch_paths,
        probe_languages: probe_languages.to_vec(),
        seeded_cache_names,
        initial_cache_names: OnceLock::new(),
        initial_recursion_limit: OnceLock::new(),
        journaled,
        defer_publish: AtomicBool::new(false),
        staged: Mutex::new(None),
        capture_host: OnceLock::new(),
        journal_host: OnceLock::new(),
        body_started: OnceLock::new(),
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

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{Host, PureHost, WriteMode};
    use rcmake_store::TraceMemo;

    use super::*;
    use crate::configure::try_compile::journal::{
        DependencyClosure, JournalHost, JournalMode, ProcessVerifier,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct NoopVerifier;
    impl ProcessVerifier for NoopVerifier {
        fn preflight(&self, _: &rcmake_host::RunRequest) -> Option<rcmake_store::Trace> {
            None
        }
        fn dependencies(
            &self,
            _: &rcmake_host::RunRequest,
            _: &rcmake_host::RunOutput,
            _: &[rcmake_store::InputStamp],
        ) -> Option<DependencyClosure> {
            None
        }
    }

    fn setup_host() -> Result<Arc<PureHost>, Box<dyn std::error::Error>> {
        let host = Arc::new(PureHost::default());
        let modules = Path::new("/work/cmake/Modules");
        host.create_dir_all(modules)?;
        host.seed_file(
            &modules.join("CMakeSystemSpecificInitialize.cmake"),
            b"set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)\n",
        )?;
        host.seed_file(
            &modules.join("CMakeSystemSpecificInformation.cmake"),
            b"set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)\n",
        )?;
        host.seed_file(
            &modules.join("CMakeCInformation.cmake"),
            b"set(CMAKE_C_INFORMATION_LOADED 1)\n",
        )?;
        host.seed_file(
            Path::new("/work/build/CMakeFiles/4.5.1/CMakeSystem.cmake"),
            b"set(CMAKE_SYSTEM_LOADED 1)\n",
        )?;
        host.seed_file(
            Path::new("/work/build/CMakeFiles/4.5.1/CMakeCCompiler.cmake"),
            b"set(CMAKE_C_COMPILER_LOADED 1)\n",
        )?;
        Ok(host)
    }

    fn make_parent(host: Arc<dyn Host>, cache: Arc<PreludeCache>) -> Interp {
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host,
                mode: RunMode::Configure,
                cmake_root: PathBuf::from("/work/cmake"),
                cmake_command: PathBuf::from("/work/cmake/bin/cmake"),
                cwd: PathBuf::from("/work"),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        interp.add_session_data(cache);
        {
            let mut cx = interp.cx();
            cx.set_var(b"CMAKE_ROOT", "/work/cmake".into());
            cx.set_var(
                b"CMAKE_PLATFORM_INFO_DIR",
                "/work/build/CMakeFiles/4.5.1".into(),
            );
        }
        interp
    }

    fn make_child(host: Arc<dyn Host>, cwd: &Path) -> Interp {
        let mut child = Interp::new(
            InterpConfig {
                services: Default::default(),
                host,
                mode: RunMode::Configure,
                cmake_root: PathBuf::from("/work/cmake"),
                cmake_command: PathBuf::from("/work/cmake/bin/cmake"),
                cwd: cwd.to_path_buf(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        {
            let mut cx = child.cx();
            cx.set_var(b"CMAKE_SYSTEM_LOADED", "1".into());
            cx.set_var(b"CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED", "1".into());
            cx.set_var(b"CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED", "1".into());
            cx.set_var(b"CMAKE_C_INFORMATION_LOADED", "1".into());
        }
        child
    }

    /// 1. Bundled `Modules/` file changes mid-session -> miss and re-run live.
    #[test]
    fn bundled_module_change_invalidates_prelude_snapshot() -> TestResult {
        let host = setup_host()?;
        let cache = Arc::new(PreludeCache::default());
        let mut parent = make_parent(host.clone(), cache.clone());
        let scratch1 = PathBuf::from("/work/scratch1");
        host.create_dir_all(&scratch1)?;
        let langs = vec![BString::from("C")];

        let plan = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch1,
            &scratch1,
            true,
            false,
            true,
        )
        .1;
        let PreludePlan::Capture(capture) = plan else {
            return Err("expected Capture on first run".into());
        };
        let journal = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch1,
            Arc::new(NoopVerifier),
            JournalMode::Observe,
        )?);
        let cap_host = CapturingHost::for_test(journal.clone(), &scratch1, host.system());
        capture.attach_host(&cap_host);
        capture.attach_journal(&journal);
        // Simulate reading a bundled module during the prelude.
        let module = Path::new("/work/cmake/Modules/CMakeCInformation.cmake");
        let _ = cap_host.read(module, rcmake_host::ReadRange::All { limit: 1 << 20 })?;
        let mut child = make_child(cap_host.clone(), &scratch1);
        capture.finish(&child.cx());
        drop(child);
        drop(cap_host);
        // Verify Arc::try_unwrap(journal) succeeds (no strong ref leak in PreludeCapture).
        let _finished = Arc::try_unwrap(journal)
            .map_err(|_| "journal leaked strong reference")?
            .finish();

        // Second probe hits cleanly before any file change.
        let scratch2 = PathBuf::from("/work/scratch2");
        host.create_dir_all(&scratch2)?;
        let hit = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch2,
            &scratch2,
            true,
            false,
            true,
        )
        .1;
        assert!(matches!(hit, PreludePlan::Hit(_)));

        // Mutate the bundled module mid-session -> MUST miss and return Capture.
        host.write(
            module,
            b"set(CMAKE_C_INFORMATION_LOADED 1)\nset(NEW_FLAG 1)\n",
            WriteMode::Truncate,
        )?;
        let after_edit = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch2,
            &scratch2,
            true,
            false,
            true,
        )
        .1;
        assert!(matches!(after_edit, PreludePlan::Capture(_)));
        Ok(())
    }

    /// 2. Env var read by platform module (`SDKROOT` or `PWD`, including absent vs empty)
    ///    changes -> `PreludeJournalSlice::validate` rejects and misses.
    #[test]
    fn env_var_change_invalidates_prelude_journal_slice() -> TestResult {
        let host = setup_host()?;
        host.env_set(b"SDKROOT", None);
        host.env_set(b"PWD", Some(b"/work/dir1"));
        let scratch = PathBuf::from("/work/scratch-env");
        host.create_dir_all(&scratch)?;
        let journal = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch,
            Arc::new(NoopVerifier),
            JournalMode::Observe,
        )?);
        journal.begin_prelude_watch();
        assert_eq!(journal.env_get(b"SDKROOT"), None);
        assert_eq!(journal.env_get(b"PWD"), Some(BString::from("/work/dir1")));
        let slice = journal
            .capture_prelude_slice()
            .ok_or("expected pure prelude slice")?;
        let memo = TraceMemo::default();
        assert!(slice.validate(host.as_ref(), &memo));

        // Absent (`None`) -> empty (`Some("")`) must invalidate!
        host.env_set(b"SDKROOT", Some(b""));
        assert!(!slice.validate(host.as_ref(), &memo));
        host.env_set(b"SDKROOT", None);
        assert!(slice.validate(host.as_ref(), &memo));

        // Changing `$ENV{PWD}` must also invalidate!
        host.env_set(b"PWD", Some(b"/work/dir2"));
        assert!(!slice.validate(host.as_ref(), &memo));
        Ok(())
    }

    /// 3. Impure prelude or scratch-filename-branching prelude is NEVER cached.
    #[test]
    fn impure_or_scratch_branching_prelude_is_never_cached() -> TestResult {
        let host = setup_host()?;
        let scratch = PathBuf::from("/work/scratch-impure");
        host.create_dir_all(&scratch)?;
        host.seed_file(&scratch.join("probe_secret.c"), b"int x;")?;

        // Case A: Prelude reads a scratch file (including CMakeLists.txt) or stats scratch
        // root / CMakeFiles inside the prelude body -> refused.
        host.seed_file(
            &scratch.join("CMakeLists.txt"),
            b"project(CMAKE_TRY_COMPILE C)\n",
        )?;
        host.create_dir_all(&scratch.join("CMakeFiles"))?;

        for path in [
            scratch.join("probe_secret.c"),
            scratch.join("CMakeLists.txt"),
        ] {
            let journal_read = Arc::new(JournalHost::from_shared(
                host.clone(),
                &scratch,
                Arc::new(NoopVerifier),
                JournalMode::Observe,
            )?);
            journal_read.begin_prelude_watch();
            let _ = journal_read.read(&path, rcmake_host::ReadRange::All { limit: 1024 })?;
            assert!(
                journal_read.capture_prelude_slice().is_none(),
                "prelude body read of {} must be refused",
                path.display()
            );
        }

        for dir in [scratch.clone(), scratch.join("CMakeFiles")] {
            let journal_stat = Arc::new(JournalHost::from_shared(
                host.clone(),
                &scratch,
                Arc::new(NoopVerifier),
                JournalMode::Observe,
            )?);
            journal_stat.begin_prelude_watch();
            let _ = journal_stat.metadata(&dir, Follow::No)?;
            assert!(
                journal_stat.capture_prelude_slice().is_none(),
                "prelude body stat of {} must be refused",
                dir.display()
            );
        }

        // CapturingHost in prelude body mode also rejects scratch reads and metadata stats.
        let cap_host = CapturingHost::for_test(host.clone(), &scratch, host.system());
        cap_host.begin_prelude_tracking(&scratch);
        cap_host.enter_prelude_body();
        let _ = cap_host.read(
            &scratch.join("CMakeLists.txt"),
            rcmake_host::ReadRange::All { limit: 1024 },
        )?;
        let trace = cap_host
            .finish_prelude_tracking()
            .ok_or("missing capture trace")?;
        assert!(trace.mutated_external);

        // Case B: Impure operation (clock/env_set) during prelude -> refused.
        let journal_b = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch,
            Arc::new(NoopVerifier),
            JournalMode::Observe,
        )?);
        journal_b.begin_prelude_watch();
        let _ = journal_b.now();
        assert!(journal_b.capture_prelude_slice().is_none());
        Ok(())
    }

    /// 4. Snapshot captured without a journal (`journal_slice: None`) is NEVER served
    ///    to a journaled child, while a journaled snapshot splices cleanly into a new journal.
    #[test]
    fn unjournaled_snapshot_never_serves_journaled_child_and_splicing_preserves_observations()
    -> TestResult {
        let host = setup_host()?;
        let cache = Arc::new(PreludeCache::default());
        let mut parent = make_parent(host.clone(), cache.clone());
        let scratch1 = PathBuf::from("/work/scratch-noj");
        host.create_dir_all(&scratch1)?;
        let langs = vec![BString::from("C")];

        // Capture a non-journaled snapshot (`journaled = false`).
        let plan = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch1,
            &scratch1,
            true,
            false,
            false,
        )
        .1;
        let PreludePlan::Capture(capture) = plan else {
            return Err("expected Capture".into());
        };
        let cap_host = CapturingHost::for_test(host.clone(), &scratch1, host.system());
        capture.attach_host(&cap_host);
        let mut child = make_child(cap_host, &scratch1);
        capture.finish(&child.cx());
        drop(child);

        // A journaled child MUST get `PreludePlan::Bypass` (never `Hit`!).
        let journaled_plan = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch1,
            &scratch1,
            true,
            false,
            true,
        )
        .1;
        assert!(matches!(journaled_plan, PreludePlan::Bypass));

        // Now verify splicing a real `PreludeJournalSlice` transfers env & file observations.
        let scratch2 = PathBuf::from("/work/scratch-splice-src");
        let scratch3 = PathBuf::from("/work/scratch-splice-dst");
        host.create_dir_all(&scratch2)?;
        host.create_dir_all(&scratch3)?;
        host.env_set(b"SDKROOT", Some(b"/SDK/MacOSX.sdk"));

        let j_src = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch2,
            Arc::new(NoopVerifier),
            JournalMode::Observe,
        )?);
        j_src.begin_prelude_watch();
        let _ = j_src.env_get(b"SDKROOT");
        let module = Path::new("/work/cmake/Modules/CMakeCInformation.cmake");
        let _ = j_src.read(module, rcmake_host::ReadRange::All { limit: 1 << 20 })?;
        let slice = j_src
            .capture_prelude_slice()
            .ok_or("expected slice from pure journal")?;

        let j_dst = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch3,
            Arc::new(NoopVerifier),
            JournalMode::Observe,
        )?);
        assert!(j_dst.splice_prelude_slice(&slice));
        let finished = Arc::try_unwrap(j_dst).map_err(|_| "j_dst leaked")?.finish();
        assert_eq!(finished.impure(), None);
        assert_eq!(
            finished.environment().get(b"SDKROOT".as_slice()),
            Some(&Some(BString::from("/SDK/MacOSX.sdk")))
        );
        assert!(finished.verified_inputs(host.as_ref()).is_some());

        // Mutating the module read during the prelude must invalidate the spliced journal!
        host.write(module, b"changed\n", WriteMode::Truncate)?;
        assert!(finished.verified_inputs(host.as_ref()).is_none());
        Ok(())
    }

    #[test]
    fn prelude_rejects_unset_cache_variable_watch_and_scripted_command() -> TestResult {
        let host = setup_host()?;
        let langs = vec![BString::from("C")];
        let scratch = PathBuf::from("/work/scratch-guards");
        host.create_dir_all(&scratch)?;

        // Case A: `project()` removes a seeded cache entry (`unset(... CACHE)`).
        {
            let cache = Arc::new(PreludeCache::default());
            let mut parent = make_parent(host.clone(), cache.clone());
            let defines = vec![CacheDefine {
                name: BString::from("SEEDED_FLAG"),
                ty: Some(rcmake_model::CacheType::String),
                value: BString::from("1"),
            }];
            let PreludePlan::Capture(capture) = prepare(
                &mut parent.cx(),
                &defines,
                b"C",
                &langs,
                &scratch,
                &scratch,
                true,
                false,
                false,
            )
            .1
            else {
                return Err("expected Capture".into());
            };
            let cap_host = CapturingHost::for_test(host.clone(), &scratch, host.system());
            capture.attach_host(&cap_host);
            let mut child = make_child(cap_host, &scratch);
            {
                let mut cx = child.cx();
                cx.insert_cache_entry_unrecorded(
                    BString::from("SEEDED_FLAG"),
                    CacheEntry {
                        value: BString::from("1"),
                        ty: rcmake_model::CacheType::String,
                        help: BString::default(),
                        advanced: false,
                        props: Default::default(),
                    },
                );
                capture.record_initial_state(&cx);
                cx.remove_cache_entry(b"SEEDED_FLAG");
            }
            capture.finish(&child.cx());
            assert!(matches!(
                prepare(
                    &mut parent.cx(),
                    &defines,
                    b"C",
                    &langs,
                    &scratch,
                    &scratch,
                    true,
                    false,
                    false,
                )
                .1,
                PreludePlan::Capture(_)
            ));
        }

        // Case B: `project()` registers a variable watch.
        {
            let cache = Arc::new(PreludeCache::default());
            let mut parent = make_parent(host.clone(), cache.clone());
            let PreludePlan::Capture(capture) = prepare(
                &mut parent.cx(),
                &[],
                b"C",
                &langs,
                &scratch,
                &scratch,
                true,
                false,
                false,
            )
            .1
            else {
                return Err("expected Capture".into());
            };
            let cap_host = CapturingHost::for_test(host.clone(), &scratch, host.system());
            capture.attach_host(&cap_host);
            let mut child = make_child(cap_host, &scratch);
            child.cx().add_variable_watch(b"WATCHED_VAR", None);
            capture.finish(&child.cx());
            assert!(matches!(
                prepare(
                    &mut parent.cx(),
                    &[],
                    b"C",
                    &langs,
                    &scratch,
                    &scratch,
                    true,
                    false,
                    false,
                )
                .1,
                PreludePlan::Capture(_)
            ));
        }

        // Case C: `project()` overrides a builtin / defines a non-stock scripted command.
        {
            let cache = Arc::new(PreludeCache::default());
            let mut parent = make_parent(host.clone(), cache.clone());
            let PreludePlan::Capture(capture) = prepare(
                &mut parent.cx(),
                &[],
                b"C",
                &langs,
                &scratch,
                &scratch,
                true,
                false,
                false,
            )
            .1
            else {
                return Err("expected Capture".into());
            };
            let cap_host = CapturingHost::for_test(host.clone(), &scratch, host.system());
            capture.attach_host(&cap_host);
            let mut child = make_child(cap_host, &scratch);
            let _ = child
                .cx()
                .eval_code(b"function(custom_hook)\nendfunction()\n");
            capture.finish(&child.cx());
            assert!(matches!(
                prepare(
                    &mut parent.cx(),
                    &[],
                    b"C",
                    &langs,
                    &scratch,
                    &scratch,
                    true,
                    false,
                    false,
                )
                .1,
                PreludePlan::Capture(_)
            ));
        }

        Ok(())
    }

    #[test]
    fn prelude_peak_depth_delta_blocks_deep_callers_and_tightens_allowance() -> TestResult {
        let host = setup_host()?;
        let cache = Arc::new(PreludeCache::default());
        let mut parent = make_parent(host.clone(), cache.clone());
        let scratch = PathBuf::from("/work/scratch-depth");
        host.create_dir_all(&scratch)?;
        let langs = vec![BString::from("C")];

        let PreludePlan::Capture(capture) = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch,
            &scratch,
            true,
            false,
            false,
        )
        .1
        else {
            return Err("expected Capture".into());
        };
        capture.defer_publish();
        let cap_host = CapturingHost::for_test(host.clone(), &scratch, host.system());
        capture.attach_host(&cap_host);
        let mut child = make_child(cap_host, &scratch);
        let initial_limit = child.cx().recursion_limit();
        capture.record_initial_state(&child.cx());
        capture.finish(&child.cx());
        // Simulate a prelude that reached 6 nested frames below entry.
        let simulated_delta = 6_u32;
        capture.finalize_and_publish(true, false, initial_limit - simulated_delta);

        // Shallow caller (depth 0, limit 10): 0 + 6 <= 10 -> Hit with recorded delta 6.
        parent
            .cx()
            .set_var(b"CMAKE_MAXIMUM_RECURSION_DEPTH", BString::from("10"));
        let shallow = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch,
            &scratch,
            true,
            false,
            false,
        )
        .1;
        let PreludePlan::Hit(snapshot) = shallow else {
            return Err("expected Hit for shallow caller".into());
        };
        assert_eq!(snapshot.peak_depth_delta(), simulated_delta as usize);

        // Deep caller (depth 5, limit 10): 5 + 6 = 11 > 10 -> Bypass without evicting cache.
        parent.inherit_recursion_depth(5);
        let deep = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch,
            &scratch,
            true,
            false,
            false,
        )
        .1;
        assert!(matches!(deep, PreludePlan::Bypass));

        // Shallower caller again (depth 4, limit 10): 4 + 6 = 10 <= 10 -> still hits!
        parent.inherit_recursion_depth(4);
        let exact = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch,
            &scratch,
            true,
            false,
            false,
        )
        .1;
        assert!(matches!(exact, PreludePlan::Hit(_)));
        Ok(())
    }

    #[test]
    fn splice_rebases_process_request_checks_and_validates_journal_slice() -> TestResult {
        use std::collections::BTreeSet;
        use std::time::Duration;

        use rcmake_host::{CommandSpec, Input, Output, ProcessResult, RunOutput, RunRequest};

        use super::super::plan::request_digest;

        struct AllowVerifier;
        impl ProcessVerifier for AllowVerifier {
            fn preflight(&self, _: &RunRequest) -> Option<rcmake_store::Trace> {
                Some(Vec::new())
            }
            fn dependencies(
                &self,
                _: &RunRequest,
                _: &RunOutput,
                _: &[rcmake_store::InputStamp],
            ) -> Option<DependencyClosure> {
                Some(DependencyClosure {
                    files: BTreeSet::from([PathBuf::from("/work/input.h")]),
                    searches: Vec::new(),
                })
            }
        }

        let host = setup_host()?;
        let tool = PathBuf::from("/work/fake-tool");
        host.seed_file(&tool, b"tool-binary")?;
        host.seed_file(Path::new("/work/input.h"), b"int x;\n")?;
        host.sleep(Duration::from_secs(5));
        let scratch = PathBuf::from("/work/scratch-rebase");
        host.create_dir_all(&scratch)?;
        host.set_command_handler(|_, _, _| {
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;

        let make_req = |arg: &str| RunRequest {
            pipeline: vec![CommandSpec {
                program: BString::from("/work/fake-tool"),
                args: vec![BString::from(arg)],
            }],
            cwd: scratch.clone(),
            env: Vec::new(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(5)),
            tee: false,
            default_error_mode: false,
        };
        let req0 = make_req("--existing");
        let req1 = make_req("--spliced-0");
        let req2 = make_req("--spliced-1");

        // Record an initial verified process at index 0 in the destination journal.
        let j_dst = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch,
            Arc::new(AllowVerifier),
            JournalMode::Observe,
        )?);
        let _ = j_dst.run_bounded(&req0, 1024)?;

        // Splice a prelude slice carrying two process requests with Check::Request(0..2).
        let slice = PreludeJournalSlice::from_requests_for_test(vec![req1.clone(), req2.clone()]);
        let memo = TraceMemo::default();
        assert!(slice.validate(host.as_ref(), &memo));
        assert!(j_dst.splice_prelude_slice(&slice));

        let finished = Arc::try_unwrap(j_dst).map_err(|_| "j_dst leaked")?.finish();
        assert_eq!(finished.impure(), None);
        assert_eq!(finished.processes().len(), 3);
        assert_eq!(
            finished.request_check_digests(),
            vec![
                (0, request_digest(&req0)),
                (1, request_digest(&req1)),
                (2, request_digest(&req2)),
            ]
        );
        Ok(())
    }

    #[test]
    fn journal_slice_validation_catches_mismatch_when_outer_tracker_bypassed() -> TestResult {
        let host = setup_host()?;
        let direct_file = PathBuf::from("/work/cmake/Modules/DirectOnly.cmake");
        host.seed_file(&direct_file, b"set(DIRECT 1)\n")?;

        let cache = Arc::new(PreludeCache::default());
        let mut parent = make_parent(host.clone(), cache.clone());
        let scratch = PathBuf::from("/work/scratch-bypass-tracker");
        host.create_dir_all(&scratch)?;
        let langs = vec![BString::from("C")];

        let PreludePlan::Capture(capture) = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch,
            &scratch,
            true,
            false,
            true,
        )
        .1
        else {
            return Err("expected Capture".into());
        };
        let journal = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch,
            Arc::new(NoopVerifier),
            JournalMode::Observe,
        )?);
        let cap_host = CapturingHost::for_test(journal.clone(), &scratch, host.system());
        capture.attach_host(&cap_host);
        capture.attach_journal(&journal);
        // Read directly through `journal`, completely bypassing `cap_host`'s `PreludeTracker`.
        let _ = journal.read(&direct_file, rcmake_host::ReadRange::All { limit: 1024 })?;
        let mut child = make_child(cap_host.clone(), &scratch);
        capture.finish(&child.cx());
        drop(child);
        drop(cap_host);
        let _ = Arc::try_unwrap(journal).map_err(|_| "journal leaked")?;

        // Mutate the file that only the journal observed.
        host.write(&direct_file, b"set(DIRECT 2)\n", WriteMode::Truncate)?;

        // Confirm the outer tracker alone would still consider the snapshot valid...
        let cached_snapshot = cache
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .next()
            .cloned()
            .ok_or("expected cached snapshot")?;
        let memo = TraceMemo::default();
        assert!(cached_snapshot.is_valid(host.as_ref(), &memo));
        // ...yet journal slice validation catches the mismatch!
        assert!(!cached_snapshot.is_valid_for_journal(host.as_ref(), &memo));

        // And `prepare` rejects the stale snapshot and returns `Capture`.
        let next = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch,
            &scratch,
            true,
            false,
            true,
        )
        .1;
        assert!(matches!(next, PreludePlan::Capture(_)));
        Ok(())
    }

    #[test]
    fn missing_scratch_metadata_reuses_until_target_scratch_contains_matching_file() -> TestResult {
        let host = setup_host()?;
        let cache = Arc::new(PreludeCache::default());
        let mut parent = make_parent(host.clone(), cache.clone());
        let scratch1 = PathBuf::from("/work/scratch-opt-1");
        let scratch2 = PathBuf::from("/work/scratch-opt-2");
        let scratch3 = PathBuf::from("/work/scratch-opt-3");
        host.create_dir_all(&scratch1)?;
        host.create_dir_all(&scratch2)?;
        host.create_dir_all(&scratch3)?;
        host.seed_file(&scratch3.join("OptionalHook"), b"set(HOOKED 1)\n")?;
        let langs = vec![BString::from("C")];

        let PreludePlan::Capture(capture) = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch1,
            &scratch1,
            true,
            false,
            false,
        )
        .1
        else {
            return Err("expected Capture".into());
        };
        let cap_host = CapturingHost::for_test(host.clone(), &scratch1, host.system());
        capture.attach_host(&cap_host);
        let mut child = make_child(cap_host.clone(), &scratch1);
        capture.begin_body(&child.cx());
        // Simulate `include(OptionalHook OPTIONAL)` probing missing `<scratch>/OptionalHook`.
        let _ = cap_host.metadata(&scratch1.join("OptionalHook"), Follow::Yes);
        capture.finish(&child.cx());
        drop(child);

        // Scratch 2 also lacks `OptionalHook` -> snapshot Hit!
        let hit = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch2,
            &scratch2,
            true,
            false,
            false,
        )
        .1;
        assert!(matches!(hit, PreludePlan::Hit(_)));

        // Scratch 3 contains `OptionalHook` -> must Bypass so the probe sees its own file!
        let bypass = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch3,
            &scratch3,
            true,
            false,
            false,
        )
        .1;
        assert!(matches!(bypass, PreludePlan::Bypass));

        // Scratch 2 still hits because scratch-local bypass did not evict the globally valid snapshot.
        let hit_again = prepare(
            &mut parent.cx(),
            &[],
            b"C",
            &langs,
            &scratch2,
            &scratch2,
            true,
            false,
            false,
        )
        .1;
        assert!(matches!(hit_again, PreludePlan::Hit(_)));
        Ok(())
    }
}
