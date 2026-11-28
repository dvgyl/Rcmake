//! Record CTest and test process results for instrumentation queries.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    FileTime, Host, HostError, HostResult, ProcessResult, RunOutput, RunRequest, Stream, path,
};
use rcmake_instrument::launcher::command_line;
use rcmake_instrument::snippet::{self, ProcessMetrics, SnippetDetails, SystemSample};
use rcmake_instrument::{InstrumentOption, Snippet, SnippetRole, query};

use crate::{CtestError, selection::PlannedTest};

pub(crate) struct Recorder<'a> {
    host: &'a dyn Host,
    data: PathBuf,
    build: PathBuf,
    config: Option<&'a BString>,
    capture: bool,
    dynamic: bool,
    process_metrics: bool,
}

pub(crate) struct TestSample {
    time: FileTime,
    system: Option<SystemSample>,
}

impl<'a> Recorder<'a> {
    pub(crate) fn open(
        host: &'a dyn Host,
        build: &Path,
        config: Option<&'a BString>,
    ) -> Result<Option<Self>, CtestError> {
        let queries = query::read_queries(host, build).map_err(|error| CtestError::Access {
            path: build.to_path_buf(),
            source: HostError::unsupported(error.to_string()),
        })?;
        let enabled = |option| queries.iter().any(|query| query.options.contains(&option));
        Ok((!queries.is_empty()).then(|| Self {
            host,
            data: build.join(".cmake/instrumentation/v1/data"),
            build: build.to_path_buf(),
            config,
            capture: enabled(InstrumentOption::CaptureOutput),
            dynamic: enabled(InstrumentOption::DynamicSystemInformation),
            process_metrics: enabled(InstrumentOption::ProcessMetrics),
        }))
    }

    pub(crate) fn ctest(
        &self,
        command: &[BString],
        sample: TestSample,
        elapsed: Duration,
        code: i32,
        show_only: bool,
        interrupt_signal: Option<i32>,
    ) {
        let snippet = Snippet {
            role: SnippetRole::CTest,
            command: command_line(command),
            working_dir: path::from_native_cmake(&self.build),
            result: interrupt_signal.is_none().then_some(i64::from(code)),
            time_start: millis(sample.time),
            duration: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            target: None,
            outputs: Vec::new(),
            source: None,
            language: None,
            test_name: None,
            config: None,
            stdout: None,
            stderr: None,
        };
        if let Err(error) = snippet::write_snippet_with_details(
            self.host,
            &self.data,
            &snippet,
            &SnippetDetails {
                dynamic: sample
                    .system
                    .map(|before| (before, SystemSample::collect(self.host))),
                show_only,
                interrupt_signal,
                ..SnippetDetails::default()
            },
        ) {
            self.report(error);
        }
    }

    pub(crate) fn start_test(&self) -> TestSample {
        TestSample {
            time: self.host.now(),
            system: self.dynamic.then(|| SystemSample::collect(self.host)),
        }
    }

    pub(crate) fn run_test(
        &self,
        request: &RunRequest,
    ) -> HostResult<(RunOutput, Option<ProcessMetrics>)> {
        rcmake_instrument::metrics::run(
            self.host,
            request,
            &self.data,
            self.process_metrics,
            &mut |_, _| {},
        )
    }

    pub(crate) fn test(
        &self,
        test: &PlannedTest,
        request: &RunRequest,
        run: (&RunOutput, Option<ProcessMetrics>),
        sample: TestSample,
        elapsed: Duration,
    ) -> Option<PathBuf> {
        let command = request.pipeline.first()?;
        let (run, metrics) = run;
        let (result, interrupt_signal) = match run.results.first() {
            Some(ProcessResult::Exited(code)) => (Some(i64::from(*code)), None),
            Some(ProcessResult::Signaled(signal)) => (None, Some(*signal)),
            Some(ProcessResult::TimedOut) => (None, None),
            _ => return None,
        };
        let command = std::iter::once(&command.program)
            .chain(&command.args)
            .cloned()
            .collect::<Vec<_>>();
        let snippet = Snippet {
            role: SnippetRole::Test,
            command: command_line(&command),
            working_dir: path::from_native_cmake(&request.cwd),
            result,
            time_start: millis(sample.time),
            duration: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            target: None,
            outputs: Vec::new(),
            source: None,
            language: None,
            test_name: Some(test.name.clone()),
            config: self.config.cloned(),
            stdout: self.capture.then(|| run.stdout.clone().into()),
            stderr: self.capture.then(|| run.stderr.clone().into()),
        };
        let details = SnippetDetails {
            test_index: Some(test.number),
            interrupt_signal,
            dynamic: sample
                .system
                .map(|before| (before, SystemSample::collect(self.host))),
            process_metrics: self.process_metrics.then_some(metrics),
            ..SnippetDetails::default()
        };
        match snippet::write_snippet_with_details(self.host, &self.data, &snippet, &details) {
            Ok(path) => Some(path),
            Err(error) => {
                self.report(error);
                None
            }
        }
    }

    fn report(&self, error: rcmake_instrument::InstrumentError) {
        self.host.write_out(
            Stream::Stderr,
            format!("CTest instrumentation error: {error}\\n").as_bytes(),
        );
    }
}

fn millis(time: FileTime) -> u64 {
    u64::try_from(time.secs)
        .unwrap_or_default()
        .saturating_mul(1000)
        .saturating_add(u64::from(time.nanos / 1_000_000))
}
