use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, list};
use rcmake_eval::{Cx, Interp, ProbeCacheMode, Services};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, ReadRange, WriteMode, path};

use crate::configure::lookahead::{
    ExpandedProbeCandidate, ExpandedProbeSpec, ProbeKind, VarLookup, lookahead_expanded,
    lookahead_foreach,
};
use crate::configure::probe_pool::{
    ChildBudget, ProbePool, ProbeQueue, WorkerContext, job_limit, physical_memory, reap_scratch,
};

use super::super::args::{Signature, Source};
use super::super::cache::ObservedOutcome;
use super::super::child::{ChildBuild, ChildConfig};
use super::super::journal::{
    CompileIntervals, Journal, JournalHost, JournalMode, ProcessVerifier, speculative_now,
};
use super::super::plan::{ProbeRecipe, Templated, compute_probe_key};
use super::super::project::write_bracket_arg;
use super::super::{ProbeDirs, ProbeResult};
use super::{PreparedProbe, ProbeGeneratorFacts, ProbeRuntime, compiler};

static NEXT_SESSION_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// Isolated per-session scratch namespace kept outside the project source and build trees,
/// removed automatically once all speculative workers have exited.
struct SessionScratch<'a> {
    host: &'a dyn Host,
    path: PathBuf,
}

impl<'a> SessionScratch<'a> {
    fn create(host: &'a dyn Host, source: &Path, build: &Path) -> Option<Self> {
        let user_root = isolated_scratch_parent(host, source, build)?;
        reap_scratch(host, &user_root);
        let pid = host.system().pid;
        for _ in 0..16 {
            let seq = NEXT_SESSION_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let nonce = RandomState::new().hash_one((pid, seq));
            let session = user_root.join(format!("s-{pid:x}-{nonce:016x}-{seq:x}"));
            match host.create_private_dir(&session) {
                Ok(()) => {
                    return Some(Self {
                        host,
                        path: session,
                    });
                }
                Err(error) if error.kind == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for SessionScratch<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

fn isolated_scratch_parent(host: &dyn Host, source: &Path, build: &Path) -> Option<PathBuf> {
    let uid_tag = host
        .effective_uid()
        .map_or_else(|| "user".to_owned(), |uid| uid.to_string());
    let leaf = format!("rcmake-spec-{uid_tag}");
    let canon_source = host.canonicalize(source).ok();
    let canon_build = host.canonicalize(build).ok();

    let mut candidates: Vec<PathBuf> = Vec::new();
    for key in [b"TMPDIR".as_slice(), b"TEMP", b"TMP", b"XDG_RUNTIME_DIR"] {
        if let Some(value) = host.env_get(key).filter(|v| !v.is_empty()) {
            candidates.push(path::to_native(&value));
        }
    }
    if host.system().sysname == b"Windows" {
        if let Some(local) = host.env_get(b"LOCALAPPDATA").filter(|v| !v.is_empty()) {
            candidates.push(path::to_native(&local).join("Temp"));
        }
        candidates.push(PathBuf::from("C:/Temp"));
    } else {
        candidates.push(PathBuf::from("/tmp"));
    }

    for base in candidates {
        if !base.is_absolute() || base.components().any(|part| part == Component::ParentDir) {
            continue;
        }
        let parent = base.join(&leaf);
        if parent.starts_with(build) || parent.starts_with(source) {
            continue;
        }
        if !host
            .metadata(&base, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
            && host.create_dir_all(&base).is_err()
        {
            continue;
        }
        let ready = match host.create_private_dir(&parent) {
            Ok(()) => true,
            Err(error) if error.kind == std::io::ErrorKind::AlreadyExists => {
                host.metadata(&parent, Follow::No).is_ok_and(|meta| {
                    meta.kind == FileKind::Dir
                        && (host.effective_uid().is_none() || meta.mode & 0o077 == 0)
                })
            }
            Err(_) => false,
        };
        if !ready {
            continue;
        }
        let canon_parent = host
            .canonicalize(&parent)
            .ok()
            .unwrap_or_else(|| parent.clone());
        let overlaps_visible = canon_build
            .as_deref()
            .is_some_and(|dir| canon_parent.starts_with(dir))
            || canon_source
                .as_deref()
                .is_some_and(|dir| canon_parent.starts_with(dir))
            || canon_parent.starts_with(build)
            || canon_parent.starts_with(source);
        if !overlaps_visible {
            return Some(parent);
        }
    }
    None
}

impl ProbeRuntime {
    pub(in crate::configure) fn evaluate<R>(
        &self,
        interp: &mut Interp,
        services: &Services,
        evaluate: impl FnOnce(&mut Interp) -> R,
    ) -> R {
        if matches!(self.mode, ProbeCacheMode::Verify | ProbeCacheMode::ReadOnly)
            || interp.has_command_observers()
        {
            return evaluate(interp);
        }
        let host = self.host.as_ref();
        if host
            .env_get(b"RCMAKE_ORACLE_MODE")
            .is_some_and(|value| value == b"1")
            || job_limit(host, 0, services.speed.probe_jobs).requested <= 1
        {
            return evaluate(interp);
        }
        let Some(scratch) = SessionScratch::create(host, &self.source, &self.build) else {
            return evaluate(interp);
        };
        let context = WorkerContext {
            host,
            store: &self.store,
            cache: &self.cache,
            scratch_root: &scratch.path,
            process_verifier: None,
        };
        let result = ProbePool::scoped(
            &context,
            physical_memory(host),
            services.speed.probe_jobs,
            services.cancel.clone().unwrap_or_default(),
            |pool| {
                interp.add_session_data(pool.queue());
                evaluate(interp)
            },
        );
        drop(scratch);
        if let Some(intervals) = &self.intervals {
            let _ = intervals.write(host, &self.build);
        }
        result
    }
}

/// Side-effect-free lookup view anchored at an enclosing caller's variable scope depth so
/// inner module function locals never shadow caller variables during speculative lookahead.
struct CallerLookup<'a> {
    cx: &'a Cx<'a>,
    scope_depth: usize,
}

impl VarLookup for CallerLookup<'_> {
    fn var(&self, name: &[u8]) -> Option<&[u8]> {
        self.cx
            .peek_var_at_depth(self.scope_depth, name)
            .map(|value| value.as_slice())
    }

    fn cache_var(&self, name: &[u8]) -> Option<&[u8]> {
        self.cx.peek_cache_var(name).map(|value| value.as_slice())
    }

    fn is_builtin(&self, name: &[u8]) -> bool {
        self.cx.is_builtin(name)
    }
}

fn collect_caller_candidates(
    cx: &Cx<'_>,
    skip_calls: usize,
    kind: ProbeKind,
) -> Option<(usize, Vec<ExpandedProbeCandidate>)> {
    let (current, remaining, caller_depth, foreach_frame) = cx.caller_lookahead(skip_calls)?;
    let lookup = CallerLookup {
        cx,
        scope_depth: caller_depth,
    };
    let mut upcoming = Vec::new();
    let straight = lookahead_expanded(&lookup, Some(current), remaining, |candidate_kind, _, _| {
        candidate_kind == kind
    });
    upcoming.extend(straight.into_iter().skip(1));
    if let Some(frame) = foreach_frame {
        let loop_upcoming =
            lookahead_foreach(&lookup, caller_depth, &frame, |candidate_kind, _, _| {
                candidate_kind == kind
            });
        upcoming.extend(loop_upcoming);
    }
    Some((caller_depth, upcoming))
}

impl PreparedProbe {
    pub(in crate::configure::try_compile) fn queue(
        &self,
        cx: &Cx<'_>,
        signature: &Signature<'_>,
        target: Option<&[u8]>,
        child: &ChildConfig,
        directory: &Path,
    ) {
        if cx.observes_commands() || cx.trace_config().is_some() {
            return;
        }
        let Some(queue) = cx.session_data::<ProbeQueue>().cloned() else {
            return;
        };
        if !queue.has_capacity() {
            return;
        }
        let Signature::Sources { sources, .. } = signature else {
            return;
        };
        let Some(target) = target.and_then(|target| std::str::from_utf8(target).ok()) else {
            return;
        };

        // 1. Stock `check_include_file` (straight-line or active `foreach()`):
        //    `self.recipe` already carries the exact `CMakeLists.txt`, seed, and
        //    compiler evidence; only `CheckIncludeFile.c` changes per header.
        if let [Source::Variable(source_name, var_name)] = sources.as_slice()
            && *source_name == b"CheckIncludeFile.c"
            && *var_name == b"_CIF_SOURCE_CONTENT"
            && let Some((caller_depth, upcoming)) =
                collect_caller_candidates(cx, 1, ProbeKind::IncludeFile)
            && !upcoming.is_empty()
            && is_stock_check_include_file_call(cx, caller_depth)
        {
            self.submit_check_include_file_candidates(
                cx,
                &queue,
                target,
                child,
                directory,
                caller_depth,
                upcoming,
            );
            return;
        }

        // 2. Stock `check_symbol_exists` (straight-line or active `foreach()`):
        //    `self.recipe` already carries the exact `CMakeLists.txt`, seed, and
        //    compiler evidence; only `CheckSymbolExists.c` / `.cxx` changes per symbol.
        if let [Source::Variable(source_name, var_name)] = sources.as_slice()
            && matches!(
                *source_name,
                b"CheckSymbolExists.c" | b"CheckSymbolExists.cxx"
            )
            && *var_name == b"_CSE_SOURCE"
            && let Some((caller_depth, upcoming)) =
                collect_caller_candidates(cx, 2, ProbeKind::SymbolExists)
            && !upcoming.is_empty()
            && is_stock_check_symbol_exists_call(cx, caller_depth)
        {
            self.submit_check_symbol_exists_candidates(
                cx,
                &queue,
                target,
                child,
                directory,
                caller_depth,
                source_name,
                upcoming,
            );
            return;
        }

        // 3. Stock `check_c_compiler_flag` / `check_cxx_compiler_flag`
        //    (straight-line or active `foreach()`): source (`src.c` / `src.cxx`) is
        //    constant; only `target_compile_definitions` / `target_compile_options` in
        //    `CMakeLists.txt` change per flag.
        if let [Source::Variable(source_name, var_name)] = sources.as_slice()
            && matches!(*source_name, b"src.c" | b"src.cxx")
            && *var_name == b"_source"
        {
            let kind = if *source_name == b"src.cxx" {
                ProbeKind::CxxCompilerFlag
            } else {
                ProbeKind::CCompilerFlag
            };
            if let Some((caller_depth, upcoming)) = collect_caller_candidates(cx, 3, kind)
                && !upcoming.is_empty()
                && is_stock_check_compiler_flag_call(cx, caller_depth, kind)
            {
                self.submit_check_compiler_flag_candidates(
                    cx,
                    &queue,
                    target,
                    child,
                    directory,
                    caller_depth,
                    kind,
                    upcoming,
                );
                return;
            }
        }

        // 4. Direct `try_compile` (`SOURCE_FROM_CONTENT` or `SOURCE_FROM_VAR`): straight-line
        //    expanded lookahead plus active `foreach()` loop iterations when directly inside
        //    the loop body.
        if matches!(
            sources.as_slice(),
            [Source::Content(..) | Source::Variable(..)]
        ) {
            let mut candidates: Vec<(BString, BString)> = Vec::new();
            if let Some((current, remaining)) = cx.lookahead() {
                let batch = lookahead_expanded(cx, Some(current), remaining, |kind, cmd, _| {
                    kind == ProbeKind::TryCompile && cx.is_builtin(&cmd.lower)
                });
                for candidate in batch.into_iter().skip(1) {
                    if let ExpandedProbeSpec::TryCompile { filename, content } = candidate.spec
                        && compiler::admit_source(&filename, &content)
                    {
                        candidates.push((filename, content));
                    }
                }
            }
            if cx.cur_cmd_in_foreach()
                && let Some(frame) = cx.foreach_lookahead()
            {
                let upcoming =
                    lookahead_foreach(cx, cx.var_scope_depth(), &frame, |kind, cmd, _| {
                        kind == ProbeKind::TryCompile && cx.is_builtin(&cmd.lower)
                    });
                for candidate in upcoming {
                    if let ExpandedProbeSpec::TryCompile { filename, content } = candidate.spec
                        && compiler::admit_source(&filename, &content)
                    {
                        candidates.push((filename, content));
                    }
                }
            }
            if !candidates.is_empty() {
                self.submit_try_compile_candidates(
                    cx, &queue, target, child, directory, candidates,
                );
            }
        }
    }

    fn submit_try_compile_candidates(
        &self,
        cx: &Cx<'_>,
        queue: &ProbeQueue,
        target: &str,
        child: &ChildConfig,
        directory: &Path,
        candidates: Vec<(BString, BString)>,
    ) {
        let environment = self.runtime.host.env_all();
        let template = super::TemplateCapture::new(&self.recipe.bindings);
        // `self.recipe` does not change in this loop, so hash it at most once.
        let mut current_key = None;
        for (name, content) in candidates {
            if !queue.has_capacity() {
                break;
            }
            let Some((project, language)) = super::super::project::render_literal_probe(
                cx,
                &directory.join(path::to_native(&name)),
                target,
            ) else {
                continue;
            };
            let Some(project) = template.capture(&project) else {
                continue;
            };
            let (defines, enabled_languages) = child.seed_inputs();
            let Some((seed, bound)) = super::seed_digest_peek(
                cx,
                defines,
                enabled_languages,
                &[language],
                &template,
                self.runtime.host.as_ref(),
            ) else {
                continue;
            };
            let Some(source) = Templated::literal(&content) else {
                continue;
            };
            let (tools, verifier) = if compiler::source_language(&name) == Some(self.compiler.cxx())
                && !self.compiler.has_external_source()
            {
                if !self.compiler.confines_speculative_writes() {
                    continue;
                }
                (self.recipe.tools.clone(), Arc::clone(&self.compiler))
            } else {
                let Some(selected) = compiler::ProbeCompiler::select_speculative(
                    Arc::clone(&self.runtime.journal_host),
                    cx,
                    &self.runtime.compiler,
                    &name,
                    &self.runtime.cache.memo,
                    &self.compiler,
                ) else {
                    continue;
                };
                if !selected.verifier.confines_speculative_writes() {
                    continue;
                }
                (selected.identity, selected.verifier)
            };
            let mut recipe = self.recipe.clone();
            recipe.files = [(BString::from("CMakeLists.txt"), project), (name, source)].into();
            recipe.tools = tools;
            recipe.seed = seed;
            let current = *current_key.get_or_insert_with(|| compute_probe_key(&self.recipe));
            if compute_probe_key(&recipe) == current {
                continue;
            }
            self.enqueue_recipe(
                cx,
                queue,
                recipe,
                environment.clone(),
                verifier,
                target,
                child,
                bound,
                false,
                directory.as_os_str().len(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_check_include_file_candidates(
        &self,
        cx: &Cx<'_>,
        queue: &ProbeQueue,
        target: &str,
        child: &ChildConfig,
        directory: &Path,
        caller_depth: usize,
        upcoming: Vec<ExpandedProbeCandidate>,
    ) {
        if self.compiler.cxx()
            || self.compiler.has_external_source()
            || !self.compiler.confines_speculative_writes()
        {
            return;
        }
        let cmake_root = match cx.peek_var(b"CMAKE_ROOT") {
            Some(root) if !root.is_empty() => path::to_native(root),
            _ => cx.cmake_root().to_path_buf(),
        };
        let template_path = cmake_root.join("Modules/CheckIncludeFile.c.in");
        let Ok(template_bytes) = self
            .runtime
            .host
            .read(&template_path, ReadRange::All { limit: 64 * 1024 })
        else {
            return;
        };
        let placeholder = b"${CHECK_INCLUDE_FILE_VAR}";
        let Some(pos) = template_bytes
            .windows(placeholder.len())
            .position(|window| window == placeholder)
        else {
            return;
        };
        let (Some(prefix), Some(suffix)) = (
            template_bytes.get(..pos),
            template_bytes.get(pos.saturating_add(placeholder.len())..),
        ) else {
            return;
        };
        // Reject unexpected extra variable references or escapes in a modified template.
        if prefix.contains(&b'$')
            || prefix.contains(&b'@')
            || prefix.contains(&b'\\')
            || suffix.contains(&b'$')
            || suffix.contains(&b'@')
            || suffix.contains(&b'\\')
        {
            return;
        }
        let environment = self.runtime.host.env_all();
        let current = compute_probe_key(&self.recipe);
        for candidate in upcoming {
            if !queue.has_capacity() {
                break;
            }
            let ExpandedProbeSpec::IncludeFile { header } = candidate.spec else {
                continue;
            };
            // Skip headers whose result variable is already defined (`if(NOT DEFINED ...)`).
            if cx
                .peek_var_at_depth(caller_depth, &candidate.result)
                .is_some()
                || !safe_include_header(&header)
            {
                continue;
            }
            let mut content = Vec::with_capacity(
                prefix
                    .len()
                    .saturating_add(header.len())
                    .saturating_add(suffix.len()),
            );
            content.extend_from_slice(prefix);
            content.extend_from_slice(&header);
            content.extend_from_slice(suffix);
            if !compiler::admit_source(b"CheckIncludeFile.c", &content) {
                continue;
            }
            let Some(source) = Templated::literal(&content) else {
                continue;
            };
            let mut recipe = self.recipe.clone();
            recipe
                .files
                .insert(BString::from("CheckIncludeFile.c"), source);
            let key = compute_probe_key(&recipe);
            if key == current {
                continue;
            }
            self.enqueue_recipe(
                cx,
                queue,
                recipe,
                environment.clone(),
                Arc::clone(&self.compiler),
                target,
                child,
                self.bound.clone(),
                true,
                directory.as_os_str().len(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_check_symbol_exists_candidates(
        &self,
        cx: &Cx<'_>,
        queue: &ProbeQueue,
        target: &str,
        child: &ChildConfig,
        directory: &Path,
        caller_depth: usize,
        source_name: &[u8],
        upcoming: Vec<ExpandedProbeCandidate>,
    ) {
        let expect_cxx = source_name == b"CheckSymbolExists.cxx";
        if self.compiler.cxx() != expect_cxx
            || self.compiler.has_external_source()
            || !self.compiler.confines_speculative_writes()
        {
            return;
        }
        let environment = self.runtime.host.env_all();
        let current = compute_probe_key(&self.recipe);
        for candidate in upcoming {
            if !queue.has_capacity() {
                break;
            }
            let ExpandedProbeSpec::SymbolExists { symbol, headers } = candidate.spec else {
                continue;
            };
            // `CheckSymbolExists.cmake` skips when defined and not equal to its own name.
            if cx
                .peek_var_at_depth(caller_depth, &candidate.result)
                .is_some_and(|value| value.as_slice() != candidate.result.as_slice())
            {
                continue;
            }
            let Some(content) = render_check_symbol_exists_source(&symbol, &headers) else {
                continue;
            };
            if !compiler::admit_source(source_name, &content) {
                continue;
            }
            let Some(source) = Templated::literal(&content) else {
                continue;
            };
            let mut recipe = self.recipe.clone();
            recipe.files.insert(BString::from(source_name), source);
            let key = compute_probe_key(&recipe);
            if key == current {
                continue;
            }
            self.enqueue_recipe(
                cx,
                queue,
                recipe,
                environment.clone(),
                Arc::clone(&self.compiler),
                target,
                child,
                self.bound.clone(),
                true,
                directory.as_os_str().len(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_check_compiler_flag_candidates(
        &self,
        cx: &Cx<'_>,
        queue: &ProbeQueue,
        target: &str,
        child: &ChildConfig,
        directory: &Path,
        caller_depth: usize,
        kind: ProbeKind,
        upcoming: Vec<ExpandedProbeCandidate>,
    ) {
        let expect_cxx = kind == ProbeKind::CxxCompilerFlag;
        if self.compiler.cxx() != expect_cxx
            || self.compiler.has_external_source()
            || !self.compiler.confines_speculative_writes()
        {
            return;
        }
        let Some(current_var) = cx.peek_var(b"_var") else {
            return;
        };
        let current_flag = cx
            .peek_var(b"CMAKE_REQUIRED_DEFINITIONS")
            .map_or(&[][..], |value| value.as_slice());
        let Some(current_lists) = self
            .recipe
            .files
            .get(b"CMakeLists.txt".as_slice())
            .and_then(|template| template.render_bounded(&self.recipe.bindings, 1 << 20))
        else {
            return;
        };
        let Some((prefix, suffix)) =
            split_flag_project_lists(&current_lists, target, current_var, current_flag)
        else {
            return;
        };

        let template = super::TemplateCapture::new(&self.recipe.bindings);
        let environment = self.runtime.host.env_all();
        let current = compute_probe_key(&self.recipe);
        for candidate in upcoming {
            if !queue.has_capacity() {
                break;
            }
            let flag = match (&candidate.spec, kind) {
                (ExpandedProbeSpec::CCompilerFlag { flag }, ProbeKind::CCompilerFlag)
                | (ExpandedProbeSpec::CxxCompilerFlag { flag }, ProbeKind::CxxCompilerFlag) => flag,
                _ => continue,
            };
            if cx
                .peek_var_at_depth(caller_depth, &candidate.result)
                .is_some()
            {
                continue;
            }
            let Some(block) = render_flag_compile_block(target, &candidate.result, flag) else {
                continue;
            };
            let mut next_lists = Vec::with_capacity(
                prefix
                    .len()
                    .saturating_add(block.len())
                    .saturating_add(suffix.len()),
            );
            next_lists.extend_from_slice(prefix);
            next_lists.extend_from_slice(&block);
            next_lists.extend_from_slice(suffix);
            let Some(project) = template.capture(&next_lists) else {
                continue;
            };
            let mut recipe = self.recipe.clone();
            recipe
                .files
                .insert(BString::from("CMakeLists.txt"), project);
            let key = compute_probe_key(&recipe);
            if key == current {
                continue;
            }
            self.enqueue_recipe(
                cx,
                queue,
                recipe,
                environment.clone(),
                Arc::clone(&self.compiler),
                target,
                child,
                self.bound.clone(),
                true,
                directory.as_os_str().len(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_recipe(
        &self,
        cx: &Cx<'_>,
        queue: &ProbeQueue,
        recipe: ProbeRecipe,
        environment: Vec<(BString, BString)>,
        compiler: Arc<compiler::CompilerEvidence>,
        target: &str,
        child: &ChildConfig,
        bound: std::collections::BTreeSet<PathBuf>,
        allow_failure: bool,
        target_dir_len: usize,
    ) {
        if !compiler.confines_speculative_writes() {
            return;
        }
        let host = Arc::clone(&self.runtime.journal_host);
        let memo = Arc::clone(&self.runtime.cache.memo);
        let target = BString::from(target);
        let child_bytes = child.retained_bytes();
        let child = child.clone();
        let intervals = self.runtime.intervals.clone();
        let facts = cx
            .session_data::<Arc<crate::configure::build_tool_facts::BuildToolFacts>>()
            .cloned();
        queue.submit(
            recipe,
            environment,
            Some(Box::new(move |scratch, recipe, environment, budget| {
                let execution = SpeculativeExecution {
                    host,
                    compiler,
                    memo,
                    target,
                    child,
                    bound,
                    intervals,
                    facts,
                    allow_failure,
                    target_dir_len,
                };
                execution.run(scratch, recipe, environment, budget)
            })),
            child_bytes,
        );
    }
}

fn canonical_module(cx: &Cx<'_>, rel: &str) -> Option<PathBuf> {
    let host = cx.host();
    host.canonicalize(&cx.cmake_root().join("Modules").join(rel))
        .ok()
}

fn command_defined_in(cx: &Cx<'_>, name: &[u8], expected_canon: &Path) -> bool {
    let Some(def_file) = cx.user_command_file(name) else {
        return false;
    };
    cx.host()
        .canonicalize(&path::to_native(def_file))
        .is_ok_and(|canon| canon == expected_canon)
}

fn is_stock_check_common(cx: &Cx<'_>, caller_depth: usize) -> bool {
    // `Internal/CheckCommon.cmake` mutates `CMAKE_REQUIRED_FLAGS` across invocations when it
    // contains a semicolon (`string(REPLACE "\\;" "\\\\;" ...)` / `list(POP_FRONT ...)`).
    if cx
        .peek_var_at_depth(caller_depth, b"CMAKE_REQUIRED_FLAGS")
        .is_some_and(|value| value.contains(&b';'))
    {
        return false;
    }
    for builtin in [
        b"set".as_slice(),
        b"unset",
        b"file",
        b"string",
        b"list",
        b"message",
        b"cmake_policy",
        b"try_compile",
        b"get_property",
        b"cmake_parse_arguments",
    ] {
        if !cx.is_builtin(builtin) {
            return false;
        }
    }
    let Some(common_canon) = canonical_module(cx, "Internal/CheckCommon.cmake") else {
        return false;
    };
    for helper in [
        b"cmake_check_common_init_args".as_slice(),
        b"cmake_check_common_init_compile_args",
        b"cmake_check_common_init_compile_flags",
        b"cmake_check_common_init_include_directories",
        b"cmake_check_common_init_link_options",
        b"cmake_check_common_init_link_directories",
        b"cmake_check_common_cleanup",
    ] {
        if !command_defined_in(cx, helper, &common_canon) {
            return false;
        }
    }
    true
}

fn is_stock_check_include_file_call(cx: &Cx<'_>, caller_depth: usize) -> bool {
    if !is_stock_check_common(cx, caller_depth) {
        return false;
    }
    let Some(expected_canon) = canonical_module(cx, "CheckIncludeFile.cmake") else {
        return false;
    };
    let backtrace = cx.backtrace();
    let Some(frame) = backtrace.top() else {
        return false;
    };
    cx.host()
        .canonicalize(&path::to_native(&frame.file))
        .is_ok_and(|canon| canon == expected_canon)
        && command_defined_in(cx, b"check_include_file", &expected_canon)
}

fn is_stock_check_symbol_exists_call(cx: &Cx<'_>, caller_depth: usize) -> bool {
    if !is_stock_check_common(cx, caller_depth) {
        return false;
    }
    let Some(expected_canon) = canonical_module(cx, "CheckSymbolExists.cmake") else {
        return false;
    };
    let backtrace = cx.backtrace();
    let mut frames = backtrace.frames();
    let (Some(impl_frame), Some(outer_frame)) = (frames.next(), frames.next()) else {
        return false;
    };
    let host = cx.host();
    if !host
        .canonicalize(&path::to_native(&impl_frame.file))
        .is_ok_and(|canon| canon == expected_canon)
        || !host
            .canonicalize(&path::to_native(&outer_frame.file))
            .is_ok_and(|canon| canon == expected_canon)
    {
        return false;
    }
    for helper in [
        b"check_symbol_exists".as_slice(),
        b"__check_symbol_exists_impl",
        b"__check_symbol_exists_filter_flags",
        b"__check_symbol_exists_restore_flags",
    ] {
        if !command_defined_in(cx, helper, &expected_canon) {
            return false;
        }
    }
    true
}

fn is_stock_check_compiler_flag_call(cx: &Cx<'_>, caller_depth: usize, kind: ProbeKind) -> bool {
    if !is_stock_check_common(cx, caller_depth) {
        return false;
    }
    let (wrapper_mod, wrapper_cmd) = match kind {
        ProbeKind::CCompilerFlag => (
            "CheckCCompilerFlag.cmake",
            b"check_c_compiler_flag".as_slice(),
        ),
        ProbeKind::CxxCompilerFlag => (
            "CheckCXXCompilerFlag.cmake",
            b"check_cxx_compiler_flag".as_slice(),
        ),
        _ => return false,
    };
    let (
        Some(wrapper_canon),
        Some(flag_canon),
        Some(compiles_canon),
        Some(common_cfg_canon),
        Some(patterns_canon),
    ) = (
        canonical_module(cx, wrapper_mod),
        canonical_module(cx, "Internal/CheckCompilerFlag.cmake"),
        canonical_module(cx, "Internal/CheckSourceCompiles.cmake"),
        canonical_module(cx, "Internal/CheckFlagCommonConfig.cmake"),
        canonical_module(cx, "CMakeCheckCompilerFlagCommonPatterns.cmake"),
    )
    else {
        return false;
    };
    let backtrace = cx.backtrace();
    let mut frames = backtrace.frames();
    let (Some(f0), Some(f1), Some(f2)) = (frames.next(), frames.next(), frames.next()) else {
        return false;
    };
    let host = cx.host();
    if !host
        .canonicalize(&path::to_native(&f0.file))
        .is_ok_and(|canon| canon == compiles_canon)
        || !host
            .canonicalize(&path::to_native(&f1.file))
            .is_ok_and(|canon| canon == flag_canon)
        || !host
            .canonicalize(&path::to_native(&f2.file))
            .is_ok_and(|canon| canon == wrapper_canon)
    {
        return false;
    }
    command_defined_in(cx, wrapper_cmd, &wrapper_canon)
        && command_defined_in(cx, b"cmake_check_compiler_flag", &flag_canon)
        && command_defined_in(cx, b"cmake_check_source_compiles", &compiles_canon)
        && command_defined_in(cx, b"cmake_check_flag_common_init", &common_cfg_canon)
        && command_defined_in(cx, b"cmake_check_flag_common_finish", &common_cfg_canon)
        && command_defined_in(cx, b"check_compiler_flag_common_patterns", &patterns_canon)
}

fn safe_include_header(header: &[u8]) -> bool {
    !header.is_empty()
        && header.len() <= 256
        && !header.starts_with(b"/")
        && !header.windows(2).any(|w| w == b"..")
        && header.iter().all(|&byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b'+')
        })
}

fn safe_symbol_name(symbol: &[u8]) -> bool {
    !symbol.is_empty()
        && symbol.len() <= 256
        && symbol
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':'))
}

fn is_c_macro_identifier(symbol: &[u8]) -> bool {
    let Some((&first, rest)) = symbol.split_first() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && rest
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn render_check_symbol_exists_source(symbol: &[u8], headers: &[u8]) -> Option<Vec<u8>> {
    if !safe_symbol_name(symbol) {
        return None;
    }
    let header_list = list::split(headers, list::Empty::Drop);
    if header_list.len() > 64 {
        return None;
    }
    let mut out = Vec::from(b"/* */\n".as_slice());
    for header in &header_list {
        if !safe_include_header(header) {
            return None;
        }
        out.extend_from_slice(b"#include <");
        out.extend_from_slice(header);
        out.extend_from_slice(b">\n");
    }
    out.extend_from_slice(b"\nint main(int argc, char** argv)\n{\n  (void)argv;");
    if is_c_macro_identifier(symbol) {
        out.extend_from_slice(b"\n#ifndef ");
        out.extend_from_slice(symbol);
        out.extend_from_slice(b"\n  return ((int*)(&");
        out.extend_from_slice(symbol);
        out.extend_from_slice(b"))[argc];\n#else\n  (void)argc;\n  return 0;\n#endif");
    } else {
        out.extend_from_slice(b"\n  return ((int*)(&");
        out.extend_from_slice(symbol);
        out.extend_from_slice(b"))[argc];");
    }
    out.extend_from_slice(b"\n}\n");
    Some(out)
}

fn split_simple_flag_words(flag: &[u8]) -> Option<Vec<BString>> {
    if flag.len() > 4096 {
        return None;
    }
    if flag.iter().any(|&byte| {
        matches!(
            byte,
            b'"' | b'\'' | b'\\' | b'$' | b'\n' | b'\r' | b'\t' | b'#' | 0
        )
    }) {
        return None;
    }
    let mut words = Vec::new();
    for item in list::iter(flag, list::Empty::Drop) {
        for word in item
            .split(|&byte| byte == b' ')
            .filter(|word| !word.is_empty())
        {
            if !compiler::admit_definition(word) {
                return None;
            }
            words.push(BString::from(word));
        }
    }
    Some(words)
}

fn escape_definition_list_item(value: &[u8]) -> BString {
    let mut escaped = BString::default();
    for &byte in value {
        if byte == b';' {
            escaped.push(b'\\');
        }
        escaped.push(byte);
    }
    escaped
}

fn render_flag_compile_block(target: &str, result_var: &[u8], flag: &[u8]) -> Option<BString> {
    if result_var.is_empty()
        || !result_var
            .iter()
            .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let mut def_arg = Vec::with_capacity(2 + result_var.len());
    def_arg.extend_from_slice(b"-D");
    def_arg.extend_from_slice(result_var);
    if !compiler::admit_definition(&def_arg) {
        return None;
    }
    let mut compile_flags = Vec::with_capacity(4);
    compile_flags.push(BString::from(def_arg));
    compile_flags.extend(split_simple_flag_words(flag)?);

    let mut code = BString::default();
    for (command, definitions) in [
        ("target_compile_definitions", true),
        ("target_compile_options", false),
    ] {
        let values: Vec<_> = compile_flags
            .iter()
            .filter_map(|arg| {
                let arg = arg.as_slice();
                let definition = arg.strip_prefix(b"-D").or_else(|| arg.strip_prefix(b"/D"));
                if definitions {
                    definition
                } else {
                    definition.is_none().then_some(arg)
                }
            })
            .map(escape_definition_list_item)
            .collect();
        if !values.is_empty() {
            code.extend_from_slice(format!("{command}({target} PRIVATE").as_bytes());
            for value in &values {
                code.push(b' ');
                write_bracket_arg(&mut code, value);
            }
            code.extend_from_slice(b")\n");
        }
    }
    Some(code)
}

fn split_flag_project_lists<'a>(
    lists: &'a [u8],
    target: &str,
    current_var: &[u8],
    current_flag: &[u8],
) -> Option<(&'a [u8], &'a [u8])> {
    let expected = render_flag_compile_block(target, current_var, current_flag)?;
    let needle = expected.as_slice();
    if needle.is_empty() {
        return None;
    }
    let mut match_pos = None;
    let mut offset = 0usize;
    while let Some(rel) = lists
        .get(offset..)?
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let pos = offset.saturating_add(rel);
        if pos == 0 || lists.get(pos - 1) == Some(&b'\n') {
            if match_pos.is_some() {
                return None;
            }
            match_pos = Some(pos);
        }
        offset = pos.saturating_add(1);
    }
    let start = match_pos?;
    let end = start.checked_add(needle.len())?;
    let suffix = lists.get(end..)?;
    let valid_next = suffix.starts_with(b"separate_arguments(_probe_link_flags ")
        || suffix.starts_with(
            format!("set_property(TARGET {target} PROPERTY STATIC_LIBRARY_OPTIONS").as_bytes(),
        )
        || suffix.starts_with(format!("target_link_options({target} PRIVATE").as_bytes())
        || suffix.starts_with(format!("target_link_libraries({target}").as_bytes());
    valid_next.then(|| (lists.get(..start).unwrap_or_default(), suffix))
}

struct SpeculativeExecution {
    host: Arc<dyn Host>,
    compiler: Arc<compiler::CompilerEvidence>,
    memo: Arc<rcmake_store::TraceMemo>,
    target: BString,
    child: ChildConfig,
    bound: std::collections::BTreeSet<PathBuf>,
    intervals: Option<Arc<CompileIntervals>>,
    facts: Option<Arc<crate::configure::build_tool_facts::BuildToolFacts>>,
    allow_failure: bool,
    target_dir_len: usize,
}

impl SpeculativeExecution {
    fn run(
        mut self,
        scratch: &Path,
        recipe: &ProbeRecipe,
        environment: Vec<(BString, BString)>,
        budget: ChildBudget,
    ) -> HostResult<(ObservedOutcome, Journal)> {
        let fail = || HostError::unsupported("Cannot verify speculative probe execution.");
        self.child.limit_speculation(
            budget.deadline.saturating_duration_since(speculative_now()),
            budget.shutdown.clone(),
        );
        let preverified_ninja = self.compiler.verified_ninja().cloned();
        let compiler = Arc::new(self.compiler.rebind_ninja_for_speculation(scratch))
            as Arc<dyn ProcessVerifier>;
        let journal = Arc::new(
            JournalHost::from_shared(self.host.clone(), scratch, compiler, JournalMode::Speculate)?
                .with_trace_memo(self.memo.clone())
                .with_bound_inputs(self.bound)
                .with_compile_intervals(self.intervals)
                .with_speculation(environment, budget.cancel, budget.shutdown, budget.deadline),
        );
        let mut has_file_macro = false;
        for (name, template) in &recipe.files {
            let file = path::to_native(name);
            if file.components().count() != 1
                || !matches!(
                    file.components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                return Err(fail());
            }
            let bytes = template
                .render_bounded(&recipe.bindings, 1 << 20)
                .ok_or_else(fail)?;
            if contains_file_macro(&bytes) {
                has_file_macro = true;
            }
            journal.write(&scratch.join(file), &bytes, WriteMode::CreateNew)?;
        }
        let allow_failure = self.allow_failure
            && !has_file_macro
            && (scratch.as_os_str().len() <= 200
                || scratch.as_os_str().len() <= self.target_dir_len);
        let scratch = PathBuf::from(scratch);
        let dirs = ProbeDirs {
            source: scratch.clone(),
            binary: scratch.clone(),
            scratch: Some(scratch),
        };
        let (result, recursion_exceeded) = self.child.run(
            Arc::clone(&journal) as Arc<dyn Host>,
            &dirs,
            ChildBuild {
                target: Some(&self.target),
                imports: Vec::new(),
                save_cache: false,
                source_signature: true,
                generator_facts: self.facts.map(|facts| {
                    Arc::new(ProbeGeneratorFacts {
                        real: self.host,
                        facts,
                        journal: Some(journal.clone()),
                        memo: self.memo,
                        speculative: true,
                        preverified_ninja,
                    }) as Arc<dyn rcmake_resolve::GeneratorFacts>
                }),
                prewarm: None,
                prelude: super::super::prelude::PreludePlan::Bypass,
            },
            |_, _| {},
        );
        let outcome = read_speculative(&result, journal.as_ref(), &dirs, allow_failure);
        let journal = Arc::try_unwrap(journal).map_err(|_| fail())?.finish();
        if recursion_exceeded || journal.impure().is_some() || !journal.output().is_empty() {
            return Err(fail());
        }
        Ok((outcome.ok_or_else(fail)?, journal))
    }
}

fn contains_file_macro(bytes: &[u8]) -> bool {
    let compact: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'\\')
        .collect();
    [
        b"__FILE__".as_slice(),
        b"__BASE_FILE__".as_slice(),
        b"__FILE_NAME__".as_slice(),
    ]
    .iter()
    .any(|needle| {
        compact
            .windows(needle.len())
            .any(|window| window == *needle)
    })
}

fn read_speculative(
    result: &ProbeResult,
    host: &dyn Host,
    dirs: &ProbeDirs,
    allow_failure: bool,
) -> Option<ObservedOutcome> {
    if result.failure.is_some()
        || !result.diagnostics.is_empty()
        || result.output.len() > 4 * 1024 * 1024
        || result.outcome.success != (result.outcome.exit_code == 0)
        || (!allow_failure && !result.outcome.success)
    {
        return None;
    }
    let artifact = if result.outcome.success {
        Some(super::read_artifact(
            host,
            dirs,
            result.artifact.as_deref()?,
        )?)
    } else {
        None
    };
    Some(ObservedOutcome {
        max_start_depth: result.max_start_depth,
        compiled: result.outcome.success,
        exit_code: result.outcome.exit_code,
        output: result.output.to_vec(),
        artifact,
        diagnostics: Vec::new(),
    })
}
