// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use depfile::missing_input_diagnostic;

impl JournalHost<'_> {
    pub(super) fn verified_platform_query(&self, request: &RunRequest) -> Option<RunOutput> {
        if !self.private_path(&request.cwd) {
            return None;
        }
        if let Some(spec) = &self.speculation
            && (spec.cancel.is_cancelled()
                || spec.shutdown.is_cancelled()
                || spec.deadline <= speculative_now())
        {
            return None;
        }
        self.verify_process
            .as_deref()?
            .recorded_platform_query(request)
    }

    fn limit_speculative_request(&self, request: &mut RunRequest) -> HostResult<()> {
        let Some(spec) = &self.speculation else {
            return Ok(());
        };
        let remaining = spec.deadline.saturating_duration_since(speculative_now());
        if spec.cancel.is_cancelled() || spec.shutdown.is_cancelled() || remaining.is_zero() {
            self.mark_impure(ImpureReason::ProcessFailure);
            return Err(HostError::unsupported(
                "Speculative probe was cancelled or expired.",
            ));
        }
        request.timeout = Some(
            request
                .timeout
                .map_or(remaining, |timeout| timeout.min(remaining)),
        );
        Ok(())
    }

    pub(super) fn process_request<'request>(
        &self,
        request: &'request RunRequest,
    ) -> HostResult<(std::borrow::Cow<'request, RunRequest>, Option<Trace>)> {
        use std::borrow::Cow;

        let original = request;
        let mut request = Cow::Borrowed(original);
        if let Some(spec) = &self.speculation {
            let request = request.to_mut();
            request.env.clone_from(&spec.environment);
            self.limit_speculative_request(request)?;
        }
        if let Some(verifier) = self.verify_process.as_deref() {
            let request = request.to_mut();
            super::super::plan::process_environment(&mut request.env);
            verifier.instrument(request);
        }
        let preflight = match self.before_process(&request) {
            Ok(trace) => trace,
            Err(error) => {
                self.record_request(&request);
                return Err(error);
            }
        };
        if preflight.is_none() && self.mode == JournalMode::Observe {
            request = Cow::Borrowed(original);
        }
        if let Cow::Owned(request) = &mut request {
            self.limit_speculative_request(request)?;
        }
        self.record_request(&request);
        if self.mode == JournalMode::Speculate && self.state().impure.is_some() {
            return Err(HostError::unsupported(
                "Cannot verify speculative probe inputs.",
            ));
        }
        Ok((request, preflight))
    }

    pub(super) fn write_scope(&self, request: &RunRequest, verified: bool) -> Vec<PathBuf> {
        if !cfg!(unix)
            || !verified
            || self.state().impure.is_some()
            || !self
                .verify_process
                .as_deref()
                .is_some_and(|tool| tool.confines_writes(request))
            || [&request.stdout, &request.stderr].into_iter().any(
                |output| matches!(output, Output::File { path, .. } if !self.private_path(path)),
            )
        {
            return Vec::new();
        }
        let mut paths = vec![self.scratch.clone(), self.canonical_scratch.clone()];
        paths.extend(["/tmp", "/var/tmp"].map(PathBuf::from));
        if cfg!(target_os = "macos") {
            paths.extend(
                ["/private/tmp", "/private/var/tmp", "/private/var/folders"].map(PathBuf::from),
            );
        }
        paths.extend(
            request
                .env
                .iter()
                .filter(|(key, value)| {
                    !value.is_empty()
                        && matches!(key.as_slice(), b"TMPDIR" | b"TMP" | b"TEMP" | b"TEMPDIR")
                })
                .map(|(_, value)| request.cwd.join(path::to_native(value))),
        );
        paths
    }

    fn before_process(&self, request: &RunRequest) -> HostResult<Option<Trace>> {
        if let Input::File(path) = &request.stdin {
            self.record_dependency(path)
                .or_else(|_| self.guard_effect(ImpureReason::UnverifiableInput))?;
        }
        for output in [&request.stdout, &request.stderr] {
            if let Output::File { path, .. } = output {
                self.guard_write(path)?;
            }
        }

        let bounded = request.timeout.is_some_and(|time| {
            !time.is_zero() && (self.mode == JournalMode::Observe || time <= SPECULATIVE_TIMEOUT)
        });
        let private = self.private_path(&request.cwd)
            && request.stdin != Input::Inherit
            && request.stdout != Output::Inherit
            && request.stderr != Output::Inherit
            && (self.mode == JournalMode::Observe || !request.tee);
        if !bounded || !private {
            return self
                .guard_effect(ImpureReason::UnverifiedProcess)
                .map(|()| None);
        }
        let Some(trace) = self
            .verify_process
            .as_deref()
            .and_then(|verify| verify.preflight(request))
        else {
            return self
                .guard_effect(ImpureReason::UnverifiedProcess)
                .map(|()| None);
        };
        // A pipeline needs separate evidence for every stage.
        if request.pipeline.len() != 1 {
            return self
                .guard_effect(ImpureReason::UnverifiedProcess)
                .map(|()| None);
        }
        for command in &request.pipeline {
            let program = path::to_native(&command.program);
            if !is_absolute(&program) || self.record_dependency(&program).is_err() {
                return self
                    .guard_effect(ImpureReason::UnverifiedProcess)
                    .map(|()| None);
            }
        }
        if trace
            .iter()
            .any(|stamp| matches!(stamp, InputStamp::FileStat { .. }))
            || verify_inputs(&*self.inner, &trace, self.memo.as_deref()) != VerifyOutcome::Hit
        {
            return self.guard_effect(ImpureReason::ChangedInput).map(|()| None);
        }
        for stamp in &trace {
            if let InputStamp::FileHash { path, .. } = stamp
                && self.record_dependency(path).is_err()
            {
                return self
                    .guard_effect(ImpureReason::UnverifiedProcess)
                    .map(|()| None);
            }
        }
        let mut state = self.state();
        for stamp in &trace {
            state.stamp(stamp.clone());
        }
        Ok(Some(trace))
    }

    pub(super) fn after_process(
        &self,
        request: &RunRequest,
        spawn: FileTime,
        result: &HostResult<RunOutput>,
        preflight: Option<&[InputStamp]>,
    ) {
        let Some(preflight) = preflight else {
            return;
        };
        let Ok(output) = result else {
            self.mark_impure(ImpureReason::ProcessFailure);
            return;
        };
        if output.results.len() != request.pipeline.len()
            || !output
                .results
                .iter()
                .all(|result| matches!(result, ProcessResult::Exited(_)))
        {
            self.mark_impure(ImpureReason::ProcessFailure);
            return;
        }
        let Some(verifier) = self.verify_process.as_deref() else {
            self.mark_impure(ImpureReason::UnverifiedProcess);
            return;
        };
        let Some(closure) = verifier.dependencies(request, output, preflight) else {
            self.mark_impure(ImpureReason::UnverifiedProcess);
            return;
        };
        let only_tools = closure.files.iter().all(|file| {
            request
                .pipeline
                .iter()
                .any(|command| path::to_native(&command.program) == *file)
        });
        if (only_tools && closure.searches.is_empty())
            || closure.files.len().saturating_add(closure.searches.len()) > MAX_OBSERVATIONS
        {
            self.mark_impure(ImpureReason::UnverifiedProcess);
            return;
        }
        let failed = output
            .results
            .iter()
            .any(|result| matches!(result, ProcessResult::Exited(code) if *code != 0));
        if failed
            && missing_input_diagnostic(output)
            && !closure.searches.iter().any(|search| !search.found)
        {
            self.mark_impure(ImpureReason::UnverifiedProcess);
            return;
        }
        let spawn = time_nanos(spawn);
        let time = Some(ObservationTime {
            spawn,
            cutoff: spawn - verifier.timestamp_granularity().as_nanos() as i128,
        });
        for file in &closure.files {
            if self.record_dependency_since(file, time).is_err() {
                self.mark_impure(ImpureReason::UnverifiableInput);
            }
        }
        for search in &closure.searches {
            if self.record_search_since(&search.root, &search.relative, time) != Ok(search.found) {
                self.mark_impure(ImpureReason::ChangedInput);
            }
        }
    }
}
