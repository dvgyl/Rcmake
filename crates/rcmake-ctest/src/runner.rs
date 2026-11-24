use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{FileTime, Host, Stream, path};
use rcmake_presets::ShowOnly;

use crate::cli::Invocation;
use crate::{
    CtestError, CtestOutcome, PresetEnvironment, dart, discovery, execution, instrumentation,
    junit, reporting, schedule, script, selection,
};

pub(crate) fn run_tests(
    host: Arc<dyn Host>,
    invocation: &Invocation,
    full_selection: bool,
) -> Result<CtestOutcome, CtestError> {
    run_tests_with_dashboard(host, invocation, full_selection, None).map(|run| run.outcome)
}

#[derive(Default)]
pub(crate) struct TestRunOutcome {
    pub(crate) outcome: CtestOutcome,
    pub(crate) start_error: bool,
    pub(crate) no_tests: bool,
}

pub(crate) fn run_tests_with_dashboard(
    host: Arc<dyn Host>,
    invocation: &Invocation,
    full_selection: bool,
    dashboard: Option<dart::Dashboard>,
) -> Result<TestRunOutcome, CtestError> {
    let options = &invocation.options;
    let deadline = schedule::stop_deadline(host.as_ref(), invocation.stop_time);
    let dir = host.initial_cwd().join(&options.test_dir);
    let normalized =
        rcmake_base::path_lex::Lexical::new(&path::from_native_cmake(&dir)).normalize();
    let dir = path::to_native(&normalized);
    let show_project = dashboard
        .as_ref()
        .is_none_or(|dashboard| dashboard.show_project);
    let json_only = matches!(
        invocation.execution.show_only,
        Some(ShowOnly::JsonV1 | ShowOnly::JsonV1Raw)
    );
    let run = TestRun {
        host: &host,
        invocation,
        dir,
        full_selection,
    };
    let plan = run.plan(show_project, json_only, dashboard.is_some())?;
    let dashboard = DashboardTarget::select(invocation, dashboard.as_ref());
    let junit = invocation
        .output
        .output_junit_file
        .as_ref()
        .filter(|path| !path.is_empty());
    let report = reporting::Report::new(
        host.as_ref(),
        invocation,
        &plan,
        dashboard.enabled() || junit.is_some(),
    )?;
    let instrument =
        instrumentation::Recorder::open(host.as_ref(), &run.dir, options.config.as_ref())?;
    let run_sample = instrument
        .as_ref()
        .map(instrumentation::Recorder::start_test);
    let run_start = host.monotonic();
    let record_run = |exit_code, show_only, interrupt_signal| {
        if let Some((instrument, sample)) = instrument.as_ref().zip(run_sample)
            && !invocation.command.is_empty()
        {
            instrument.ctest(
                &invocation.command,
                sample,
                host.monotonic().saturating_sub(run_start),
                exit_code,
                show_only,
                interrupt_signal,
            );
        }
    };
    if let Some(format @ (ShowOnly::JsonV1 | ShowOnly::JsonV1Raw)) = invocation.execution.show_only
    {
        if !options.quiet {
            reporting::show_json(host.as_ref(), &plan, options, format == ShowOnly::JsonV1Raw);
        }
        record_run(0, true, None);
        return Ok(TestRunOutcome::default());
    }
    if show_project {
        report.project(&run.dir);
    }
    if invocation.print_labels {
        if !options.quiet {
            reporting::print_labels(host.as_ref(), &plan);
        }
        return Ok(TestRunOutcome::default());
    }
    if options.show_only {
        report.list(&plan);
        record_run(0, true, None);
        return Ok(TestRunOutcome::default());
    }
    if let Some(name) = plan.dependency_cycle() {
        let failure = CtestError::DependencyCycle(name.clone());
        if full_selection {
            return Err(failure);
        }
        if !options.quiet {
            host.write_out(Stream::Stderr, format!("{failure}\n").as_bytes());
        }
        return Ok(TestRunOutcome {
            outcome: CtestOutcome {
                exit_code: 8,
                ..CtestOutcome::default()
            },
            ..TestRunOutcome::default()
        });
    }

    if let Some(signal) = instrument
        .as_ref()
        .and_then(|_| rcmake_instrument::injected_interrupt(host.as_ref()))
    {
        record_run(8, false, Some(signal));
        return Ok(TestRunOutcome {
            outcome: CtestOutcome {
                exit_code: 8,
                ..CtestOutcome::default()
            },
            ..TestRunOutcome::default()
        });
    }
    run.execute(&plan, &report, deadline, dashboard, junit, record_run)
}

enum DashboardTarget<'a> {
    Disabled,
    Existing(&'a dart::Dashboard),
    Create(Option<dart::Model>),
}

impl<'a> DashboardTarget<'a> {
    fn select(invocation: &Invocation, dashboard: Option<&'a dart::Dashboard>) -> Self {
        if let Some(dashboard) = dashboard {
            return Self::Existing(dashboard);
        }
        let model = invocation.definitions.iter().find_map(|definition| {
            let model = [b"Experimental".as_slice(), b"Continuous", b"Nightly"]
                .into_iter()
                .find(|&model| matches!(definition.strip_prefix(model), Some(b"" | b"Test")))?;
            dart::Model::parse(model)
        });
        if model.is_some()
            || invocation
                .actions
                .iter()
                .any(|action| matches!(action.as_slice(), b"test" | b"memcheck"))
        {
            Self::Create(invocation.dashboard_model.or(model))
        } else {
            Self::Disabled
        }
    }

    fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

struct TestRun<'a> {
    host: &'a Arc<dyn Host>,
    invocation: &'a Invocation,
    dir: PathBuf,
    full_selection: bool,
}

impl TestRun<'_> {
    fn plan(
        &self,
        show_project: bool,
        json_only: bool,
        dashboard: bool,
    ) -> Result<selection::Plan, CtestError> {
        let host = self.host.as_ref();
        let dir = &self.dir;
        let invocation = self.invocation;
        let options = &invocation.options;
        let mut tests = match discovery::load(Arc::clone(self.host), dir, options.config.as_ref()) {
            Err(error @ CtestError::Load(_)) if show_project && !json_only && !options.quiet => {
                host.write_out(
                    Stream::Stdout,
                    format!("Test project {}\n", dir.display()).as_bytes(),
                );
                return Err(error);
            }
            result => result?,
        };
        if let Some(args) = &invocation.execution.test_passthrough_arguments {
            for test in &mut tests {
                test.command.args.extend_from_slice(args);
            }
        }
        let plan = if self.full_selection {
            match selection::plan_with(host, dir, tests, options, &invocation.selection) {
                Err(error @ CtestError::TestList(_)) if dashboard => {
                    if !options.quiet {
                        host.write_out(
                            Stream::Stdout,
                            format!("Test project {}\n", dir.display()).as_bytes(),
                        );
                    }
                    return Err(error);
                }
                result => result?,
            }
        } else {
            selection::plan(tests, options)?
        };
        Ok(plan)
    }

    fn execute(
        &self,
        plan: &selection::Plan,
        report: &reporting::Report<'_>,
        deadline: Option<Duration>,
        dashboard: DashboardTarget<'_>,
        junit: Option<&BString>,
        record_run: impl FnOnce(i32, bool, Option<i32>),
    ) -> Result<TestRunOutcome, CtestError> {
        let host = self.host.as_ref();
        let dir = &self.dir;
        let invocation = self.invocation;
        let options = &invocation.options;
        report.announce_tests();
        let wall_clock_start = host.now();
        let write_dashboard = dashboard.enabled();
        let created;
        let dashboard = match dashboard {
            DashboardTarget::Disabled => None,
            DashboardTarget::Existing(dashboard) => Some(dashboard),
            DashboardTarget::Create(model) => {
                created = self.create_dashboard(model, wall_clock_start)?;
                Some(&created)
            }
        };
        let started = host.monotonic();
        let loaded;
        let configuration = if let Some(dashboard) = dashboard {
            &dashboard.configuration
        } else {
            loaded = self.configuration()?;
            &loaded
        };
        let coverage_tool = invocation
            .coverage_tool
            .as_ref()
            .unwrap_or(&configuration.coverage_tool);
        let custom = discovery::custom_commands(Arc::clone(self.host), dir)?;
        run_custom_commands(host, dir, custom.before, options.quiet)?;
        let _debug_environment = self.debug_environment(dashboard.is_some());
        let results = schedule::run(host, dir, invocation, plan, report, deadline, coverage_tool)?;
        let elapsed = host.monotonic().saturating_sub(started);
        report.summary(
            plan,
            &results,
            elapsed,
            dir,
            write_dashboard,
            &configuration.labels_for_subprojects,
        );
        run_custom_commands(host, dir, custom.after, options.quiet)?;
        let records = report.records();
        let reports = dart::TestRun {
            plan,
            statuses: &results,
            records: &records,
            started: wall_clock_start,
            elapsed,
            output: &invocation.output,
        };
        let outcome = self.finish(&reports, dashboard, junit)?;
        record_run(outcome.outcome.exit_code, false, None);
        for hook in [
            rcmake_instrument::Hook::PrepareForCDash,
            rcmake_instrument::Hook::PostCTest,
        ] {
            let Err(error) = rcmake_instrument::hooks::run_hook(host, dir, hook) else {
                continue;
            };
            host.write_out(
                Stream::Stderr,
                format!("CTest instrumentation error: {error}\n").as_bytes(),
            );
        }
        Ok(outcome)
    }

    fn configuration(&self) -> Result<dart::Configuration, CtestError> {
        dart::Configuration::load(
            self.host.as_ref(),
            &self.dir,
            &self.invocation.overwrite_configuration_file,
        )
        .map_err(|source| CtestError::Access {
            path: self.dir.clone(),
            source,
        })
    }

    fn create_dashboard(
        &self,
        model: Option<dart::Model>,
        started: FileTime,
    ) -> Result<dart::Dashboard, CtestError> {
        let host = self.host.as_ref();
        let dir = &self.dir;
        let configuration = self.configuration()?;
        let tag = match model {
            None => dart::Tag::read(host, dir).or_else(|_| {
                dart::Tag::create(host, dir, started, dart::Model::Experimental, None, b"")
            }),
            Some(model) => dart::Tag::create(
                host,
                dir,
                started,
                model,
                None,
                &configuration.nightly_start,
            ),
        }
        .map_err(|source| CtestError::Access {
            path: dir.join("Testing/TAG"),
            source,
        })?;
        Ok(dart::Dashboard {
            directory: dir.to_path_buf(),
            tag,
            configuration,
            show_project: true,
        })
    }

    fn debug_environment(&self, dashboard: bool) -> Option<PresetEnvironment<'_>> {
        if !self.full_selection {
            return None;
        }
        let invocation = self.invocation;
        let interactive = invocation
            .execution
            .interactive_debugging
            .unwrap_or_else(|| {
                !dashboard
                    && invocation.actions.is_empty()
                    && invocation.dashboard_model.is_none()
                    && !invocation
                        .definitions
                        .iter()
                        .any(|definition| !definition.contains(&b'='))
            });
        let environment = if interactive {
            BTreeMap::from([("CTEST_INTERACTIVE_DEBUG_MODE".into(), Some("1".into()))])
        } else {
            BTreeMap::from([("DASHBOARD_TEST_FROM_CTEST".into(), Some(BString::default()))])
        };
        Some(PresetEnvironment::apply(self.host.as_ref(), &environment))
    }

    fn finish(
        &self,
        report: &dart::TestRun<'_>,
        dashboard: Option<&dart::Dashboard>,
        junit: Option<&BString>,
    ) -> Result<TestRunOutcome, CtestError> {
        let junit_failed = self.write_junit(report, dashboard, junit)?;
        if let Some(dashboard) = dashboard
            && !crate::memcheck::requested(self.invocation)
        {
            dart::write_test_xml(self.host.as_ref(), dashboard, report).map_err(|source| {
                CtestError::Access {
                    path: dashboard.directory.join("Testing"),
                    source,
                }
            })?;
        }
        let no_tests = report
            .statuses
            .iter()
            .flatten()
            .all(|status| *status == execution::Status::Disabled);
        let mut outcome = CtestOutcome::default();
        let mut start_error = false;
        for (test, &result) in report.plan.tests.iter().zip(report.statuses) {
            match result {
                Some(execution::Status::Passed) => outcome.passed.push(test.name.clone()),
                Some(status) if status.failed() => {
                    outcome.failed.push(test.name.clone());
                    start_error |= status == execution::Status::NotRun
                        && test.timeout.errors.is_empty()
                        && report.records.get(&test.number).is_some_and(|record| {
                            junit::completion(status, record) != "Fixture dependency failed"
                        });
                }
                _ => {}
            }
        }
        if junit_failed
            || !outcome.failed.is_empty()
            || (self.invocation.options.error_on_no_tests && no_tests)
        {
            outcome.exit_code = 8;
        }
        Ok(TestRunOutcome {
            outcome,
            start_error,
            no_tests,
        })
    }

    fn write_junit(
        &self,
        report: &dart::TestRun<'_>,
        dashboard: Option<&dart::Dashboard>,
        junit: Option<&BString>,
    ) -> Result<bool, CtestError> {
        let host = self.host.as_ref();
        let dir = &self.dir;
        let Some(file) = junit else {
            return Ok(false);
        };
        let file = dir.join(path::to_native(file));
        let configuration;
        let config = if let Some(dashboard) = dashboard {
            &dashboard.configuration
        } else {
            configuration = self.configuration()?;
            &configuration
        };
        if let Err(error) = junit::write(
            host,
            &file,
            config,
            report.plan,
            report.statuses,
            report.records,
            report.elapsed,
        ) {
            let message = if matches!(
                error.source.kind,
                std::io::ErrorKind::NotADirectory | std::io::ErrorKind::AlreadyExists
            ) {
                "Not a directory"
            } else {
                error.source.os_message.as_str()
            };
            host.write_out(
                    Stream::Stderr,
                    format!(
                        "CMake Error: Cannot open file for write: {}\nCMake Error: : System Error: {message}\nProblem opening file: {}\n",
                        error.temporary.display(),
                        file.display()
                    ).as_bytes(),
                );
            reporting::failure_summary(host, dir);
            return Ok(true);
        }
        Ok(false)
    }
}

fn run_custom_commands(
    host: &dyn Host,
    dir: &Path,
    commands: Vec<rcmake_host::CommandSpec>,
    quiet: bool,
) -> Result<(), CtestError> {
    for command in commands {
        let program = command.program;
        let result = script::run_process(
            host,
            dir.to_path_buf(),
            program.clone(),
            command.args,
            host.env_all(),
            !quiet,
        )
        .map_err(|source| CtestError::Access {
            path: dir.to_path_buf(),
            source,
        })?;
        if !matches!(
            result.results.as_slice(),
            [rcmake_host::ProcessResult::Exited(0)]
        ) {
            return Err(CtestError::Schedule(format!(
                "CTest custom command failed: {program}"
            )));
        }
    }
    Ok(())
}
