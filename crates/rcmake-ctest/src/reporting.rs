use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::SystemTime;

use rcmake_base::BString;
use rcmake_host::{FileKind, FileTime, Follow, Host, RunRequest, Stream, WriteMode, path};
mod json;

pub(crate) use json::show_json;

use crate::execution::{ResultLine, SkipReason, Status};
use crate::selection::{Plan, PlannedTest};
use crate::{CtestOptions, cli};

pub(crate) struct Report<'a> {
    host: &'a dyn Host,
    options: &'a CtestOptions,
    total: usize,
    name_width: usize,
    number_width: usize,
    label_summary: bool,
    subproject_summary: bool,
    capture: bool,
    color: bool,
    log_file: Option<PathBuf>,
    records: RefCell<BTreeMap<usize, TestRecord>>,
}

impl<'a> Report<'a> {
    pub(crate) fn new(
        host: &'a dyn Host,
        invocation: &'a cli::Invocation,
        plan: &Plan,
        capture: bool,
    ) -> Result<Self, crate::CtestError> {
        let options = &invocation.options;
        let name_width = invocation.output.max_test_name_width.map_or_else(
            || {
                plan.tests
                    .iter()
                    .map(|test| test.name.len())
                    .max()
                    .unwrap_or(0)
                    .max(30)
            },
            |width| width.max(0) as usize,
        );
        let number_width = plan
            .tests
            .last()
            .map_or(1, |test| test.number.to_string().len());
        let log_file = invocation
            .output
            .output_log_file
            .as_ref()
            .filter(|file| !file.is_empty())
            .map(|file| {
                let dir = if invocation.preset.is_some() {
                    invocation.options.test_dir.clone()
                } else {
                    host.initial_cwd()
                };
                dir.join(path::to_native(file))
            });
        if let Some(file) = &log_file {
            host.write(file, b"", WriteMode::Truncate)
                .map_err(|source| crate::CtestError::Access {
                    path: file.clone(),
                    source,
                })?;
        }
        Ok(Self {
            host,
            options,
            total: plan.tests.len(),
            name_width,
            number_width,
            label_summary: invocation.output.label_summary.unwrap_or(true),
            subproject_summary: invocation.output.subproject_summary.unwrap_or(true),
            capture,
            color: host
                .env_get(b"CLICOLOR_FORCE")
                .is_some_and(|value| !value.is_empty() && value != "0"),
            log_file,
            records: RefCell::new(BTreeMap::new()),
        })
    }

    fn write(&self, message: std::fmt::Arguments<'_>) {
        self.emit(Stream::Stdout, message.to_string().as_bytes());
    }

    fn emit(&self, stream: Stream, bytes: &[u8]) {
        if let Some(file) = &self.log_file {
            let _ = self.host.write(file, bytes, WriteMode::Append);
        }
        if !self.options.quiet {
            self.host.write_out(stream, bytes);
        }
    }

    fn write_summary_line(&self, message: std::fmt::Arguments<'_>, failed: bool) {
        let text = message.to_string();
        if !self.color || self.options.quiet {
            self.emit(Stream::Stdout, text.as_bytes());
            return;
        }
        if let Some(file) = &self.log_file {
            let _ = self.host.write(file, text.as_bytes(), WriteMode::Append);
        }
        let code = if failed { 31 } else { 32 };
        self.host.write_out(
            Stream::Stdout,
            format!("\x1b[{code}m{text}\x1b[0m").as_bytes(),
        );
    }

    pub(crate) fn announce_tests(&self) {
        if self.options.verbose {
            self.write(format_args!("Constructing a list of tests\nDone constructing a list of tests\nUpdating test list for fixtures\n"));
        }
    }

    pub(crate) fn wait_for_load(
        &self,
        load: u64,
        limit: u64,
        smallest: Option<(&PlannedTest, usize)>,
    ) {
        let detail = smallest
            .filter(|_| load < limit)
            .map_or_else(String::new, |(test, slots)| {
                format!(", Smallest test {} requires {slots}", test.name)
            });
        self.write(format_args!(
            "***** WAITING, System Load: {load}, Max Allowed Load: {limit}{detail}*****\n"
        ));
    }

    pub(crate) fn project(&self, path: &Path) {
        self.write(format_args!("Test project {}\n", path.display()));
    }

    pub(crate) fn list(&self, plan: &Plan) {
        self.announce_tests();
        for test in &plan.tests {
            let command = resolved_command(self.host, test, self.options);
            if command.is_none()
                && !test.command.program.is_empty()
                && test.command.program != "NOT_AVAILABLE"
            {
                self.missing_command(test);
            }
            if self.options.verbose {
                let command = command.as_ref().map_or_else(String::new, display_command);
                self.write(format_args!(
                    "\n{}: Test command: {command}\n{}: Working Directory: {}\n",
                    test.number,
                    test.number,
                    test.working_directory().display()
                ));
                let labels = labels(test);
                if !labels.is_empty() {
                    self.write(format_args!(
                        "Labels: {}\n",
                        labels
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
            }
            let disabled = if rcmake_base::truth::is_on(test.property(b"DISABLED")) {
                " (Disabled)"
            } else {
                ""
            };
            self.write(format_args!(
                "  Test {:>width$}: {}{disabled}\n",
                format!("#{}", test.number),
                test.name,
                width = self.number_width + 1
            ));
        }
        self.write(format_args!("\nTotal Tests: {}\n", self.total));
    }

    fn missing_command(&self, test: &PlannedTest) {
        let program = path::to_native(&test.command.program);
        self.write(format_args!(
            "Could not find executable {}\nLooked in the following places:\n",
            test.command.program
        ));
        let without_root = program
            .strip_prefix("/")
            .ok()
            .filter(|path| !path.as_os_str().is_empty());
        for base in std::iter::once(program.as_path()).chain(without_root) {
            self.write(format_args!("{}\n{}\n", base.display(), base.display()));
            for candidate in configuration_candidates(base, self.options.config.as_ref()) {
                self.write(format_args!(
                    "{}\n{}\n",
                    candidate.display(),
                    candidate.display()
                ));
            }
        }
    }

    pub(crate) fn start(
        &self,
        test: &PlannedTest,
        request: Option<&RunRequest>,
        run: Option<(i32, i32)>,
    ) {
        if self.capture {
            let mut records = self.records.borrow_mut();
            let record = records.entry(test.number).or_default();
            record.command = request
                .and_then(|request| request.pipeline.first())
                .map_or_else(String::new, display_command);
            record.started = request.map(|_| self.host.now());
        }
        if self.options.verbose {
            self.write(format_args!("test {}\n", test.number));
        }
        let repeat = run.map_or_else(String::new, |(run, total)| format!(" (run {run}/{total})"));
        self.write(format_args!(
            "    Start {:>width$}: {}{repeat}\n",
            test.number,
            test.name,
            width = self.number_width
        ));
        if self.options.verbose
            && let Some(request) = request
            && let Some(command) = request.pipeline.first()
        {
            self.write(format_args!(
                "\n{}: Test command: {}",
                test.number, command.program
            ));
            for arg in &command.args {
                self.write(format_args!(" \"{arg}\""));
            }
            self.write(format_args!(
                "\n{}: Working Directory: {}\n",
                test.number,
                request.cwd.display()
            ));
            if let Some((_, profile)) = request
                .env
                .iter()
                .find(|(name, _)| name == "LLVM_PROFILE_FILE")
            {
                self.write(format_args!(
                    "{}: Using environment variable LLVM_PROFILE_FILE={profile}\n",
                    test.number
                ));
            }
            if let Some(timeout) = request.timeout {
                self.write(format_args!(
                    "{}: Test timeout computed to be: {}\n",
                    test.number,
                    timeout.as_secs_f64()
                ));
            }
        }
    }

    pub(crate) fn finish(&self, test: &PlannedTest, result: &ResultLine, completed: Option<usize>) {
        if self.capture || self.label_summary || self.subproject_summary {
            let mut records = self.records.borrow_mut();
            let record = records.entry(test.number).or_default();
            record.elapsed = if matches!(result.status, Status::NotRun | Status::Disabled) {
                Duration::ZERO
            } else {
                result.elapsed
            };
            if self.capture {
                record.output.clone_from(&result.output);
                record.detail.clone_from(&result.detail);
                record.exit_code = result.exit_code;
                record.skip_reason = result.skip_reason;
                record.snippet.clone_from(&result.snippet);
            }
        }
        if self.options.quiet {
            return;
        }
        if let Some(detail) = &result.detail {
            if result.status == Status::NotRun {
                self.not_run(test, detail);
            } else {
                self.emit(Stream::Stdout, format!("{detail}\n").as_bytes());
            }
        }
        if self.options.verbose {
            for line in result.output.split_inclusive(|&byte| byte == b'\n') {
                self.write(format_args!("{}: ", test.number));
                self.emit(Stream::Stdout, line);
            }
            self.end_output_line(&result.output);
        }
        let dots = ".".repeat(self.name_width.saturating_sub(test.name.len()).min(4096) + 3);
        let progress_width = self.total.to_string().len();
        let progress = completed.map_or_else(
            || " ".repeat(progress_width * 2 + 1),
            |completed| format!("{completed:>progress_width$}/{}", self.total),
        );
        let number = format!("#{}", test.number);
        self.write(format_args!(
            "{progress} Test {number:>width$}: {} {dots}{} {:>6.2} sec\n",
            test.name,
            status_text(result.status),
            result.elapsed.as_secs_f64(),
            width = self.number_width + 1,
        ));
        if !self.options.verbose && result.status.failed() && self.options.output_on_failure {
            self.emit(Stream::Stdout, &result.output);
            self.end_output_line(&result.output);
        }
    }

    fn not_run(&self, test: &PlannedTest, detail: &str) {
        if test.command.program == "NOT_AVAILABLE"
            || detail.starts_with("Failed test dependencies:")
            || !test.timeout.errors.is_empty()
        {
            self.emit(Stream::Stdout, format!("{detail}\n").as_bytes());
            return;
        }

        let working_directory = test.working_directory();
        let setting = test.property(b"WORKING_DIRECTORY");
        let directory_error = match self.host.metadata(&working_directory, Follow::Yes) {
            Ok(entry) if entry.kind != FileKind::Dir => Some("Not a directory".into()),
            Err(error) => Some(error.os_message),
            _ => None,
        };
        if let Some(error) = environment_error(detail) {
            self.emit(
                Stream::Stderr,
                format!("CMake Error: {error}\n\n").as_bytes(),
            );
        } else if !setting.is_empty()
            && let Some(reason) = directory_error
        {
            self.emit(
                Stream::Stderr,
                format!(
                    "Failed to change working directory to \"{}\": {reason}\n",
                    BString::from(setting)
                )
                .as_bytes(),
            );
        } else if resolved_command(self.host, test, self.options).is_none() {
            self.missing_command(test);
            self.emit(
                Stream::Stderr,
                format!("Unable to find executable: {}\n", test.command.program).as_bytes(),
            );
        } else if detail.starts_with("Unable to find build dependency:") {
            self.emit(Stream::Stderr, format!("{detail}\n").as_bytes());
        } else {
            self.emit(Stream::Stdout, format!("{detail}\n").as_bytes());
        }
    }

    fn end_output_line(&self, output: &[u8]) {
        if output.last().is_some_and(|&byte| byte != b'\n') {
            self.emit(Stream::Stdout, b"\n");
        }
    }

    pub(crate) fn summary(
        &self,
        plan: &Plan,
        statuses: &[Option<Status>],
        elapsed: Duration,
        dir: &Path,
        dashboard: bool,
        subprojects: &BString,
    ) {
        let mut counted = 0;
        let mut failed = 0;
        let mut skipped = false;
        for status in statuses.iter().flatten() {
            counted += usize::from(*status != Status::Disabled);
            failed += usize::from(status.failed());
            skipped |= matches!(status, Status::Disabled | Status::Skipped);
        }
        if counted == 0 {
            if !self.options.ignore_no_tests {
                if dashboard {
                    self.host
                        .write_out(Stream::Stderr, b"No tests were found!!!\n");
                } else {
                    self.emit(Stream::Stderr, b"No tests were found!!!\n");
                }
                if self.options.error_on_no_tests && !dashboard {
                    self.failure_summary(dir);
                }
            }
            return;
        }
        if failed == 0 {
            self.write_summary_line(
                format_args!("\n100% tests passed out of {counted}\n"),
                false,
            );
        } else {
            let percent = (100.0 * (counted - failed) as f64 / counted as f64).round() as u32;
            self.write_summary_line(
                format_args!(
                    "\n{}% tests passed, {failed} tests failed out of {counted}\n",
                    percent.min(99)
                ),
                true,
            );
        }
        if self.label_summary || self.subproject_summary {
            self.label_summary(plan, statuses, subprojects);
        }
        self.write(format_args!(
            "\nTotal Test time (real) = {:>6.2} sec\n",
            elapsed.as_secs_f64()
        ));
        if skipped {
            self.write(format_args!("\nThe following tests did not run:\n"));
            self.status_list(plan, statuses, |status| {
                matches!(status, Status::Disabled | Status::Skipped)
            });
        }
        if failed != 0 {
            self.write(format_args!("\nThe following tests FAILED:\n"));
            self.status_list(plan, statuses, Status::failed);
            if !dashboard {
                self.failure_summary(dir);
            }
        }
    }

    fn failure_summary(&self, dir: &Path) {
        self.emit(Stream::Stderr, failure_message(dir).as_bytes());
    }

    fn label_summary(&self, plan: &Plan, statuses: &[Option<Status>], names: &[u8]) {
        let names: BTreeSet<_> = rcmake_base::list::split(names, rcmake_base::list::Empty::Drop)
            .into_iter()
            .collect();
        let mut subprojects = BTreeMap::<BString, (Duration, usize)>::new();
        let mut labels_total = BTreeMap::<BString, (Duration, usize)>::new();
        let records = self.records.borrow();
        for (test, _) in plan
            .tests
            .iter()
            .zip(statuses)
            .filter(|(_, status)| status.is_some())
        {
            let elapsed = records
                .get(&test.number)
                .map_or(Duration::ZERO, |record| record.elapsed);
            let weighted = elapsed.saturating_mul(processors(test));
            for label in labels(test) {
                let totals = if names.contains(&label) {
                    &mut subprojects
                } else {
                    &mut labels_total
                };
                let (time, count) = totals.entry(label).or_default();
                *time = time.saturating_add(weighted);
                *count += 1;
            }
        }
        for (title, enabled, totals) in [
            ("Subproject", self.subproject_summary, subprojects),
            ("Label", self.label_summary, labels_total),
        ] {
            if enabled && let Some(width) = totals.keys().map(|label| label.len()).max() {
                self.write(format_args!("\n{title} Time Summary:\n"));
                for (label, (time, count)) in totals {
                    let spaces = " ".repeat(width + 4 - label.len());
                    let suffix = if count == 1 { "" } else { "s" };
                    self.write(format_args!(
                        "{label}{spaces}= {:>6.2} sec*proc ({count} test{suffix})\n",
                        time.as_secs_f64()
                    ));
                }
            }
        }
    }

    pub(crate) fn records(&self) -> std::cell::Ref<'_, BTreeMap<usize, TestRecord>> {
        self.records.borrow()
    }

    fn status_list(
        &self,
        plan: &Plan,
        statuses: &[Option<Status>],
        predicate: impl Fn(Status) -> bool,
    ) {
        for (test, status) in plan.tests.iter().zip(statuses) {
            if let Some(status) = status.filter(|&status| predicate(status)) {
                let label = match status {
                    Status::Disabled => "Disabled",
                    _ => status_text(status).trim_start_matches('*').trim(),
                };
                let name = format!("{} ({label})", test.name);
                let labels = labels(test);
                if labels.is_empty() || !status.failed() {
                    let line = format_args!("\t{:>3} - {name}\n", test.number).to_string();
                    if status.failed() {
                        self.write_summary_line(format_args!("{line}"), true);
                    } else {
                        self.write(format_args!("{line}"));
                    }
                } else {
                    let padding = " ".repeat(50usize.saturating_sub(name.len()).max(1));
                    let labels = labels
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.write_summary_line(
                        format_args!("\t{:>3} - {name}{padding}{labels}\n", test.number),
                        true,
                    );
                }
            }
        }
    }
}

fn environment_error(detail: &str) -> Option<String> {
    if let Some(operation) = detail.strip_prefix("Unknown environment modification operation: ") {
        return Some(format!(
            "Error: Unrecognized environment manipulation argument: {operation}"
        ));
    }
    let entry = detail.strip_prefix("Invalid environment modification: ")?;
    if entry.contains('=') {
        Some(format!(
            "Error: Missing `:` after the operation in: {entry}"
        ))
    } else {
        Some(format!(
            "Error: Missing `=` after the variable name in: {entry}"
        ))
    }
}

fn failure_message(dir: &Path) -> String {
    format!(
        "Errors while running CTest\nOutput from these tests are in: {}\nUse \"--rerun-failed --output-on-failure\" to re-run the failed cases verbosely.\n",
        dir.join("Testing/Temporary/LastTest.log").display()
    )
}

pub(crate) fn failure_summary(host: &dyn Host, dir: &Path) {
    host.write_out(Stream::Stderr, failure_message(dir).as_bytes());
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Passed => "   Passed ",
        Status::Failed => "***Failed ",
        Status::Timeout => "***Timeout",
        Status::Disabled => "***Not Run (Disabled)",
        Status::Skipped => "***Skipped",
        Status::NotRun => "***Not Run",
    }
}

pub(crate) fn print_labels(host: &dyn Host, plan: &Plan) {
    let labels: BTreeSet<_> = plan
        .tests
        .iter()
        .flat_map(|test| test.list(b"LABELS"))
        .collect();
    if labels.is_empty() {
        host.write_out(Stream::Stdout, b"No Labels Exist\n");
    } else {
        host.write_out(Stream::Stdout, b"All Labels:\n");
        for label in labels {
            host.write_out(Stream::Stdout, format!("  {label}\n").as_bytes());
        }
    }
}

#[derive(Default)]
pub(crate) struct TestRecord {
    pub(crate) elapsed: Duration,
    pub(crate) started: Option<FileTime>,
    pub(crate) command: String,
    pub(crate) output: Vec<u8>,
    pub(crate) detail: Option<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) skip_reason: Option<SkipReason>,
    pub(crate) snippet: Option<Box<PathBuf>>,
}

pub(crate) fn labels(test: &PlannedTest) -> BTreeSet<BString> {
    test.list(b"LABELS").into_iter().collect()
}

pub(crate) fn processors(test: &PlannedTest) -> u32 {
    json::json_integer(test.property(b"PROCESSORS"))
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or(1)
        .max(1)
}

fn resolved_command(
    host: &dyn Host,
    test: &PlannedTest,
    options: &CtestOptions,
) -> Option<rcmake_host::CommandSpec> {
    let mut command = test.command.clone();
    if command.program == "NOT_AVAILABLE" || command.program.is_empty() {
        return None;
    }
    crate::execution::resolve_program(host, test, &mut command, options);
    let program = path::to_native(&command.program);
    let exists = |path: &Path| {
        host.metadata(path, Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File)
    };
    let local = path::to_native(&test.command.program);
    for candidate in std::iter::once(local.clone())
        .chain(configuration_candidates(&local, options.config.as_ref()))
    {
        let file = test.directory.join(candidate);
        if exists(&file) {
            command.program = path::from_native(&file);
            return Some(command);
        }
    }
    if program.is_absolute() {
        return None;
    }
    for dir in std::env::split_paths(&path::to_native(&host.env_get(b"PATH").unwrap_or_default())) {
        let file = test.directory.join(dir).join(&program);
        if exists(&file) {
            command.program = path::from_native(&file);
            return Some(command);
        }
    }
    None
}

fn configuration_candidates(program: &Path, config: Option<&BString>) -> Vec<std::path::PathBuf> {
    let Some(name) = program.file_name() else {
        return Vec::new();
    };
    let parent = program.parent().unwrap_or_else(|| Path::new(""));
    if let Some(config) = config.filter(|config| !config.is_empty()) {
        let config = path::to_native(config);
        return vec![
            parent.join(&config).join(name),
            config.join(program.strip_prefix("/").unwrap_or(program)),
        ];
    }
    [
        "Release",
        "Debug",
        "MinSizeRel",
        "RelWithDebInfo",
        "Deployment",
        "Development",
    ]
    .into_iter()
    .map(|config| parent.join(config).join(name))
    .collect()
}

fn display_command(command: &rcmake_host::CommandSpec) -> String {
    use std::fmt::Write as _;
    let mut display = command.program.to_string();
    for arg in &command.args {
        let _ = write!(display, " \"{arg}\"");
    }
    display
}

pub(crate) fn format_date(host: &dyn Host, time: FileTime, utc: bool, format: &str) -> String {
    let seconds = Duration::from_secs(time.secs.unsigned_abs());
    let base = if time.secs < 0 {
        SystemTime::UNIX_EPOCH.checked_sub(seconds)
    } else {
        SystemTime::UNIX_EPOCH.checked_add(seconds)
    };
    let Some(instant) =
        base.and_then(|base| base.checked_add(Duration::from_nanos(time.nanos.into())))
    else {
        return String::new();
    };
    let zone = host.time_zone();
    let Ok(stamp) = instant.try_into() else {
        return String::new();
    };
    let local = zone.to_datetime(stamp);
    if utc {
        stamp.strftime(format).to_string()
    } else {
        local.strftime(format).to_string()
    }
}

#[cfg(test)]
mod tests;
