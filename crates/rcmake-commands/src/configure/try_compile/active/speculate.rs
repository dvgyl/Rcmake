use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::{Cx, Interp, Services};
use rcmake_host::{Host, HostError, HostResult, WriteMode, path};

use crate::configure::lookahead::{ProbeKind, lookahead};
use crate::configure::probe_pool::{
    ChildBudget, ProbePool, ProbeQueue, WorkerContext, job_limit, physical_memory,
};

use super::super::args::{Signature, Source};
use super::super::cache::ObservedOutcome;
use super::super::child::{ChildBuild, ChildConfig};
use super::super::journal::{Journal, JournalHost, JournalMode, ProcessVerifier, speculative_now};
use super::super::plan::{ProbeRecipe, Templated, compute_probe_key};
use super::super::{ProbeDirs, ProbeResult};
use super::{PreparedProbe, ProbeRuntime, compiler};

impl ProbeRuntime {
    pub(in crate::configure) fn evaluate<R>(
        &self,
        interp: &mut Interp,
        services: &Services,
        evaluate: impl FnOnce(&mut Interp) -> R,
    ) -> R {
        if !self.cache.writable() || interp.has_command_observers() {
            return evaluate(interp);
        }
        let host = self.host.as_ref();
        if host.system().sysname != "Darwin"
            || job_limit(host, 0, services.speed.probe_jobs).requested == 1
        {
            return evaluate(interp);
        }
        let scratch_root = self.build.join("CMakeFiles/CMakeScratch");
        let context = WorkerContext {
            host,
            store: &self.store,
            cache: &self.cache,
            scratch_root: &scratch_root,
            process_verifier: None,
        };
        ProbePool::scoped(
            &context,
            physical_memory(host),
            services.speed.probe_jobs,
            services.cancel.clone().unwrap_or_default(),
            |pool| {
                interp.add_session_data(pool.queue());
                evaluate(interp)
            },
        )
    }
}

impl PreparedProbe {
    pub(in crate::configure::try_compile) fn queue(
        &self,
        cx: &Cx<'_>,
        signature: &Signature<'_>,
        target: Option<&[u8]>,
        child: &ChildConfig,
    ) {
        let Some(queue) = cx
            .session_data::<ProbeQueue>()
            .filter(|queue| queue.available())
        else {
            return;
        };
        let Some((current, remaining)) = cx.lookahead() else {
            return;
        };
        let Signature::Sources { sources, .. } = signature else {
            return;
        };
        let [Source::Content(name, _)] = sources.as_slice() else {
            return;
        };
        let Some(target) = target else { return };
        let candidates = lookahead(Some(current), remaining, |kind, candidate, results| {
            kind == ProbeKind::TryCompile
                && cx.is_builtin(&candidate.lower)
                && matches!(
                    candidate.args.as_slice(),
                    [result, _, source, content, ..]
                        if !result.text.starts_with(b"CMAKE_")
                            && source.text == *name
                            && compiler::admit_source(&source.text, &content.text)
                            && !results.contains(content.text.as_slice())
                )
        });
        if candidates.is_empty() {
            return;
        }
        let environment = self.runtime.host.env_all();
        for candidate in candidates.into_iter().skip(1) {
            let [_, _, _, content, ..] = candidate.command.args.as_slice() else {
                continue;
            };
            let Some(source) = Templated::literal(&content.text) else {
                continue;
            };
            let mut recipe = self.recipe.clone();
            recipe.files.insert((*name).into(), source);
            if compute_probe_key(&recipe) == compute_probe_key(&self.recipe) {
                continue;
            }
            let host = Arc::clone(&self.runtime.host);
            let compiler = Arc::clone(&self.compiler);
            let target = BString::from(target);
            let child_bytes = child.retained_bytes();
            let child = child.clone();
            queue.submit(
                recipe,
                environment.clone(),
                Some(Box::new(move |scratch, recipe, environment, budget| {
                    let execution = SpeculativeExecution {
                        host,
                        compiler,
                        target,
                        child,
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
    compiler: Arc<compiler::CompilerEvidence>,
    target: BString,
    child: ChildConfig,
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
                self.host,
                scratch,
                self.compiler as Arc<dyn ProcessVerifier>,
                JournalMode::Speculate,
            )?
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
        compiled: true,
        exit_code: result.outcome.exit_code,
        output: result.output.to_vec(),
        artifact: Some(artifact),
        diagnostics: Vec::new(),
    })
}
