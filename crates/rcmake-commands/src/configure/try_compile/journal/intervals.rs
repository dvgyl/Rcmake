// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write as _;

use super::*;

#[derive(Default)]
/// Records compile intervals from speculative workers.
pub(crate) struct CompileIntervals(Mutex<Vec<CompileInterval>>);

struct CompileInterval {
    scratch: PathBuf,
    start: Duration,
    end: Duration,
    exit: i32,
}

impl CompileIntervals {
    pub(crate) fn write(&self, host: &dyn Host, build: &Path) -> HostResult<()> {
        let Ok(intervals) = self.0.lock() else {
            return Ok(());
        };
        let mut text = String::new();
        for interval in intervals.iter() {
            if let Some(name) = interval.scratch.file_name().and_then(|name| name.to_str()) {
                let _ = writeln!(
                    text,
                    "{name} {} {} {}",
                    interval.start.as_nanos(),
                    interval.end.as_nanos(),
                    interval.exit
                );
            }
        }
        host.write(
            &build.join("CMakeFiles/rcmake-probe-intervals.txt"),
            text.as_bytes(),
            rcmake_host::WriteMode::Truncate,
        )
    }
}

impl JournalHost<'_> {
    pub(crate) fn with_compile_intervals(
        mut self,
        intervals: Option<Arc<CompileIntervals>>,
    ) -> Self {
        self.compile_intervals = intervals;
        self
    }

    pub(super) fn compile_start(&self, request: &RunRequest, verified: bool) -> Option<Duration> {
        if verified
            && self.compile_intervals.is_some()
            && self
                .verify_process
                .as_deref()
                .is_some_and(|verifier| verifier.is_probe_compile(request))
        {
            return Some(self.inner.monotonic());
        }
        None
    }

    pub(super) fn compile_end(&self, start: Option<Duration>, result: &HostResult<RunOutput>) {
        let (Some(start), Some(intervals), Ok(output)) = (start, &self.compile_intervals, result)
        else {
            return;
        };
        let [rcmake_host::ProcessResult::Exited(exit)] = output.results.as_slice() else {
            return;
        };
        let end = self.inner.monotonic();
        if let Ok(mut records) = intervals.0.lock()
            && records.len() < MAX_OBSERVATIONS
        {
            records.push(CompileInterval {
                scratch: self.scratch.clone(),
                start,
                end,
                exit: *exit,
            });
        }
    }
}
