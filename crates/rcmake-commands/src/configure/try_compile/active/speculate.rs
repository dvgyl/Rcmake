use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::{Cx, Interp, ProbeCacheMode, Services};
use rcmake_host::{Host, HostError, HostResult, WriteMode, path};

use crate::configure::lookahead::{ProbeKind, lookahead};
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
        let scratch_root = self.build.join("CMakeFiles/CMakeScratch");
        reap_scratch(host, &scratch_root);
        if job_limit(host, 0, services.speed.probe_jobs).requested == 1 {
            return evaluate(interp);
        }
        let context = WorkerContext {
            host,
            store: &self.store,
            cache: &self.cache,
            scratch_root: &scratch_root,
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
        if let Some(intervals) = &self.intervals {
            let _ = intervals.write(host, &self.build);
        }
        result
    }
}

impl PreparedProbe {
    pub(in crate::configure::try_compile) fn queue(
        &self,
        cx: &mut Cx<'_>,
        signature: &Signature<'_>,
        target: Option<&[u8]>,
        child: &ChildConfig,
        directory: &Path,
    ) {
        if !cx
            .session_data::<ProbeQueue>()
            .is_some_and(ProbeQueue::available)
        {
            return;
        }
        let Some((current, remaining)) = cx.lookahead() else {
            return;
        };
        let Signature::Sources { sources, .. } = signature else {
            return;
        };
        let [Source::Content(..)] = sources.as_slice() else {
            return;
        };
        let Some(target) = target.and_then(|target| std::str::from_utf8(target).ok()) else {
            return;
        };
        let candidates = lookahead(Some(current), remaining, |kind, candidate, results| {
            kind == ProbeKind::TryCompile
                && cx.is_builtin(&candidate.lower)
                && matches!(
                    candidate.args.as_slice(),
                    [result, keyword, source, content, ..]
                        if !result.text.starts_with(b"CMAKE_")
                            && keyword.text == "SOURCE_FROM_CONTENT"
                            && content.text.len() <= 1 << 20
                            && compiler::admit_source(&source.text, &content.text)
                            && !results.contains(content.text.as_slice())
                )
        });
        if candidates.is_empty() {
            return;
        }
        let candidates: Vec<_> = candidates
            .into_iter()
            .skip(1)
            .filter_map(|candidate| {
                let [_, _, source, content, ..] = candidate.command.args.as_slice() else {
                    return None;
                };
                Some((source.text.clone(), content.text.clone()))
            })
            .collect();
        let Some(queue) = cx.session_data::<ProbeQueue>().cloned() else {
            return;
        };
        let environment = self.runtime.host.env_all();
        let template = super::TemplateCapture::new(&self.recipe.bindings);
        for (name, content) in candidates {
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
            let Some((seed, bound)) = super::seed_digest(
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
            let Some(compiler) = compiler::ProbeCompiler::select(
                Arc::clone(&self.runtime.host),
                cx,
                &self.runtime.compiler,
                &name,
                &self.runtime.cache.memo,
                None,
            ) else {
                continue;
            };
            let mut recipe = self.recipe.clone();
            recipe.files = [(BString::from("CMakeLists.txt"), project), (name, source)].into();
            recipe.tools = compiler.identity;
            recipe.seed = seed;
            if compute_probe_key(&recipe) == compute_probe_key(&self.recipe) {
                continue;
            }
            let host = Arc::clone(&self.runtime.journal_host);
            let compiler = compiler.verifier;
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
                environment.clone(),
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
}

struct SpeculativeExecution {
    host: Arc<dyn Host>,
    compiler: Arc<dyn ProcessVerifier>,
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
        let journal = Arc::new(
            JournalHost::from_shared(
                self.host.clone(),
                scratch,
                self.compiler,
                JournalMode::Speculate,
            )?
            .with_trace_memo(self.memo.clone())
            .with_bound_inputs(self.bound)
            .with_compile_intervals(self.intervals)
            .with_speculation(
                environment,
                budget.cancel,
                budget.shutdown,
                budget.deadline,
            ),
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
                    }) as Arc<dyn rcmake_resolve::GeneratorFacts>
                }),
            },
            |_| {},
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
