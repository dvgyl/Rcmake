use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_eval::{Cx, Interp, ProbeCacheMode, Services};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, ReadRange, WriteMode, path};

use crate::configure::lookahead::{
    ExpandedProbeCandidate, ExpandedProbeSpec, ProbeKind, lookahead_expanded, lookahead_foreach,
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

        // 1. Stock `check_include_file` inside an active `foreach()` loop:
        //    `self.recipe` already carries the exact `CMakeLists.txt`, seed, and
        //    compiler evidence; only `CheckIncludeFile.c` changes per header.
        if let [Source::Variable(source_name, var_name)] = sources.as_slice()
            && *source_name == b"CheckIncludeFile.c"
            && *var_name == b"_CIF_SOURCE_CONTENT"
            && is_stock_check_include_file_call(cx)
        {
            let upcoming = cx
                .foreach_lookahead()
                .map(|frame| {
                    lookahead_foreach(cx, cx.var_scope_depth(), &frame, |kind, _, _| {
                        kind == ProbeKind::IncludeFile
                    })
                })
                .unwrap_or_default();
            if !upcoming.is_empty() {
                self.submit_check_include_file_candidates(cx, &queue, target, child, upcoming);
            }
            return;
        }

        // 2. Direct `try_compile` (`SOURCE_FROM_CONTENT` or `SOURCE_FROM_VAR`): straight-line
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
            );
        }
    }

    fn submit_check_include_file_candidates(
        &self,
        cx: &Cx<'_>,
        queue: &ProbeQueue,
        target: &str,
        child: &ChildConfig,
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
            if cx.peek_var(&candidate.result).is_some()
                || cx.peek_cache_var(&candidate.result).is_some()
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
            if compute_probe_key(&recipe) == current {
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
                };
                execution.run(scratch, recipe, environment, budget)
            })),
            child_bytes,
        );
    }
}

fn is_stock_check_include_file_call(cx: &Cx<'_>) -> bool {
    // `Internal/CheckCommon.cmake` mutates `CMAKE_REQUIRED_FLAGS` across invocations when it
    // contains a semicolon (`string(REPLACE "\\;" "\\\\;" ...)` / `list(POP_FRONT ...)`).
    if cx
        .peek_var(b"CMAKE_REQUIRED_FLAGS")
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
    ] {
        if !cx.is_builtin(builtin) {
            return false;
        }
    }
    let host = cx.host();
    let expected = cx.cmake_root().join("Modules/CheckIncludeFile.cmake");
    let Ok(expected_canon) = host.canonicalize(&expected) else {
        return false;
    };
    let backtrace = cx.backtrace();
    let Some(frame) = backtrace.top() else {
        return false;
    };
    let Ok(frame_canon) = host.canonicalize(&path::to_native(&frame.file)) else {
        return false;
    };
    if frame_canon != expected_canon {
        return false;
    }
    let Some(def_file) = cx.user_command_file(b"check_include_file") else {
        return false;
    };
    if !host
        .canonicalize(&path::to_native(def_file))
        .is_ok_and(|canon| canon == expected_canon)
    {
        return false;
    }
    let common = cx.cmake_root().join("Modules/Internal/CheckCommon.cmake");
    let Ok(common_canon) = host.canonicalize(&common) else {
        return false;
    };
    for helper in [
        b"cmake_check_common_init_compile_args".as_slice(),
        b"cmake_check_common_init_compile_flags",
        b"cmake_check_common_init_include_directories",
        b"cmake_check_common_init_link_options",
        b"cmake_check_common_init_link_directories",
        b"cmake_check_common_cleanup",
    ] {
        let Some(helper_file) = cx.user_command_file(helper) else {
            return false;
        };
        if !host
            .canonicalize(&path::to_native(helper_file))
            .is_ok_and(|canon| canon == common_canon)
        {
            return false;
        }
    }
    true
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

struct SpeculativeExecution {
    host: Arc<dyn Host>,
    compiler: Arc<compiler::CompilerEvidence>,
    memo: Arc<rcmake_store::TraceMemo>,
    target: BString,
    child: ChildConfig,
    bound: std::collections::BTreeSet<PathBuf>,
    intervals: Option<Arc<CompileIntervals>>,
    facts: Option<Arc<crate::configure::build_tool_facts::BuildToolFacts>>,
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
            journal.write(&scratch.join(file), &bytes, WriteMode::CreateNew)?;
        }
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
            },
            |_, _| {},
        );
        let outcome = read_speculative(&result, journal.as_ref(), &dirs);
        let journal = Arc::try_unwrap(journal).map_err(|_| fail())?.finish();
        if recursion_exceeded || journal.impure().is_some() || !journal.output().is_empty() {
            return Err(fail());
        }
        Ok((outcome.ok_or_else(fail)?, journal))
    }
}

fn read_speculative(
    result: &ProbeResult,
    host: &dyn Host,
    dirs: &ProbeDirs,
) -> Option<ObservedOutcome> {
    let artifact = super::read_artifact(host, dirs, result.artifact.as_deref()?)?;
    (result.outcome.success && result.output.len() <= 4 * 1024 * 1024).then(|| ObservedOutcome {
        max_start_depth: result.max_start_depth,
        compiled: true,
        exit_code: result.outcome.exit_code,
        output: result.output.to_vec(),
        artifact: Some(artifact),
        diagnostics: Vec::new(),
    })
}
