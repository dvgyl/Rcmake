use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::SystemTime;

use rcmake_base::{BString, list};
use rcmake_host::{FileKind, FileTime, Follow, Host, RunRequest, Stream, WriteMode, path};
use rcmake_json::Value;

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
    capture: bool,
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
            capture,
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
                    test.directory
                        .join(path::to_native(test.property(b"WORKING_DIRECTORY")))
                        .display()
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
                "  Test #{:>width$}: {}{disabled}\n",
                test.number,
                test.name,
                width = self.number_width
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
        if self.capture || self.label_summary {
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
            }
        }
        if self.options.quiet {
            return;
        }
        if result.status == Status::NotRun
            && test.command.program != "NOT_AVAILABLE"
            && resolved_command(self.host, test, self.options).is_none()
            && result
                .detail
                .as_deref()
                .is_some_and(|detail| !detail.starts_with("Failed test dependencies:"))
        {
            self.missing_command(test);
        } else if let Some(detail) = &result.detail {
            let stream = if detail.starts_with("Unable to find build dependency:") {
                Stream::Stderr
            } else {
                Stream::Stdout
            };
            self.emit(stream, format!("{detail}\n").as_bytes());
        }
        if self.options.verbose {
            for line in result.output.split_inclusive(|&byte| byte == b'\n') {
                self.write(format_args!("{}: ", test.number));
                self.emit(Stream::Stdout, line);
            }
            self.end_output_line(&result.output);
        }
        if result.status == Status::NotRun
            && test.command.program != "NOT_AVAILABLE"
            && resolved_command(self.host, test, self.options).is_none()
            && result
                .detail
                .as_ref()
                .is_some_and(|detail| !detail.starts_with("Failed test dependencies:"))
        {
            self.emit(
                Stream::Stderr,
                format!("Unable to find executable: {}\n", test.command.program).as_bytes(),
            );
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
            self.write(format_args!("\n100% tests passed out of {counted}\n"));
        } else {
            let percent = (100.0 * (counted - failed) as f64 / counted as f64).round() as u32;
            self.write(format_args!(
                "\n{}% tests passed, {failed} tests failed out of {counted}\n",
                percent.min(99)
            ));
        }
        if self.label_summary {
            self.label_summary(plan, statuses);
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

    fn label_summary(&self, plan: &Plan, statuses: &[Option<Status>]) {
        let mut totals = BTreeMap::<BString, (Duration, usize)>::new();
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
            let weight = processors(test);
            for label in test.list(b"LABELS") {
                let (time, _) = totals.entry(label).or_default();
                *time = time.saturating_add(elapsed.saturating_mul(weight));
            }
            for label in labels(test) {
                totals.entry(label).or_default().1 += 1;
            }
        }
        if let Some(width) = totals.keys().map(|label| label.len()).max() {
            self.write(format_args!("\nLabel Time Summary:\n"));
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
                    self.write(format_args!("\t{:>3} - {name}\n", test.number));
                } else {
                    let padding = " ".repeat(50usize.saturating_sub(name.len()).max(1));
                    let labels = labels
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.write(format_args!(
                        "\t{:>3} - {name}{padding}{labels}\n",
                        test.number
                    ));
                }
            }
        }
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

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BacktraceNode {
    file: u64,
    line: Option<u64>,
    command: Option<u64>,
    parent: Option<u64>,
}

#[derive(Default)]
struct BacktraceGraph {
    files: Vec<Value>,
    commands: Vec<Value>,
    nodes: Vec<Value>,
    file_ids: BTreeMap<BString, u64>,
    command_ids: BTreeMap<BString, u64>,
    node_ids: BTreeMap<BacktraceNode, u64>,
}

impl BacktraceGraph {
    fn push(&mut self, triples: &[u8]) -> Option<Value> {
        let parts = rcmake_base::list::split(triples, rcmake_base::list::Empty::Keep);
        if !parts.len().is_multiple_of(3) {
            return None;
        }
        for frame in parts.chunks(3) {
            if let Some(file) = frame.first().filter(|file| !file.is_empty()) {
                self.file_ids.entry(file.clone()).or_insert_with(|| {
                    let index = self.files.len() as u64;
                    self.files.push(Value::string(file.clone()));
                    index
                });
            }
        }
        let mut parent = None;
        for frame in parts.chunks(3).rev() {
            let Some(file) = frame.first().filter(|file| !file.is_empty()) else {
                continue;
            };
            let file_id = *self.file_ids.get(file)?;
            let line = frame
                .get(1)
                .and_then(|line| json_integer(line))
                .filter(|line| *line > 0)
                .map(|line| line as u64);
            let command = frame
                .get(2)
                .filter(|command| !command.is_empty())
                .map(|command| {
                    *self.command_ids.entry(command.clone()).or_insert_with(|| {
                        let index = self.commands.len() as u64;
                        self.commands.push(Value::string(command.clone()));
                        index
                    })
                });
            let id = *self
                .node_ids
                .entry(BacktraceNode {
                    file: file_id,
                    line,
                    command,
                    parent,
                })
                .or_insert_with(|| {
                    let mut node = BTreeMap::from([("file".into(), Value::UInt(file_id))]);
                    for (field, value) in [("line", line), ("command", command), ("parent", parent)]
                    {
                        if let Some(value) = value {
                            node.insert(field.into(), Value::UInt(value));
                        }
                    }
                    let index = self.nodes.len() as u64;
                    self.nodes.push(Value::Object(node));
                    index
                });
            parent = Some(id);
        }
        parent.map(Value::UInt)
    }
}

fn json_integer(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.trim().parse().ok()
}

fn json_property(key: &[u8], value: &BString) -> Value {
    use rcmake_base::{list, truth};
    match key {
        b"DISABLED" | b"PROCESSOR_AFFINITY" | b"RUN_SERIAL" | b"WILL_FAIL" => {
            Value::Bool(truth::is_on(value))
        }
        b"PROCESSORS" | b"SKIP_RETURN_CODE" => Value::Int(json_integer(value).unwrap_or(-1)),
        b"TIMEOUT" | b"COST" | b"TIMEOUT_SIGNAL_GRACE_PERIOD" => Value::Real(
            cli::number_prefix(value)
                .filter(|value| value.is_finite())
                .unwrap_or_default(),
        ),
        b"FIXTURES_SETUP" | b"FIXTURES_CLEANUP" | b"FIXTURES_REQUIRED" | b"LABELS"
        | b"RESOURCE_LOCK" => {
            let values: BTreeSet<_> = list::split(value, list::Empty::Drop).into_iter().collect();
            Value::Array(values.into_iter().map(Value::string).collect())
        }
        b"ATTACHED_FILES"
        | b"ATTACHED_FILES_ON_FAIL"
        | b"DEPENDS"
        | b"ENVIRONMENT"
        | b"ENVIRONMENT_MODIFICATION"
        | b"FAIL_REGULAR_EXPRESSION"
        | b"PASS_REGULAR_EXPRESSION"
        | b"REQUIRED_FILES"
        | b"SKIP_REGULAR_EXPRESSION" => Value::Array(
            list::split(value, list::Empty::Drop)
                .into_iter()
                .map(Value::string)
                .collect(),
        ),
        b"MEASUREMENT" => {
            let text = value.to_string();
            let measurements = if text.is_empty() {
                Vec::new()
            } else {
                let (name, value) = text.split_once('=').unwrap_or((&text, "1"));
                vec![object([
                    ("measurement", Value::string(name)),
                    ("value", Value::string(value)),
                ])]
            };
            Value::Array(measurements)
        }
        b"RESOURCE_GROUPS" => resource_groups(value),
        _ => Value::string(value.clone()),
    }
}

pub(crate) fn show_json(host: &dyn Host, plan: &Plan, options: &CtestOptions, raw: bool) {
    let mut graph = BacktraceGraph::default();
    let tests = plan
        .tests
        .iter()
        .filter(|test| test.command.program != "NOT_AVAILABLE")
        .map(|test| {
            let mut fields = BTreeMap::from([("name".into(), Value::string(test.name.clone()))]);
            if let Some(command) = resolved_command(host, test, options) {
                fields.insert(
                    "command".into(),
                    Value::Array(
                        std::iter::once(command.program)
                            .chain(command.args)
                            .map(Value::string)
                            .collect(),
                    ),
                );
            }
            if let Some(config) = &options.config {
                fields.insert("config".into(), Value::string(config.clone()));
            }
            if let Some(frame) = graph.push(test.property(b"_BACKTRACE_TRIPLES")) {
                fields.insert("backtrace".into(), frame);
            }
            fields.insert(
                "properties".into(),
                Value::Array(json_properties(test, raw)),
            );
            Value::Object(fields)
        })
        .collect();
    let data = object([
        ("kind", Value::string("ctestInfo")),
        (
            "version",
            object([("major", Value::UInt(1)), ("minor", Value::UInt(0))]),
        ),
        (
            "backtraceGraph",
            object([
                ("commands", Value::Array(graph.commands)),
                ("files", Value::Array(graph.files)),
                ("nodes", Value::Array(graph.nodes)),
            ]),
        ),
        ("tests", Value::Array(tests)),
    ]);
    host.write_out(Stream::Stdout, &rcmake_json::write_styled(&data, "  "));
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
}

pub(crate) fn labels(test: &PlannedTest) -> BTreeSet<BString> {
    test.list(b"LABELS").into_iter().collect()
}

pub(crate) fn processors(test: &PlannedTest) -> u32 {
    json_integer(test.property(b"PROCESSORS"))
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

const JSON_PROPERTIES: &[&[u8]] = &[
    b"ATTACHED_FILES_ON_FAIL",
    b"ATTACHED_FILES",
    b"COST",
    b"DEPENDS",
    b"DISABLED",
    b"ENVIRONMENT",
    b"ENVIRONMENT_MODIFICATION",
    b"FAIL_REGULAR_EXPRESSION",
    b"SKIP_REGULAR_EXPRESSION",
    b"FIXTURES_CLEANUP",
    b"FIXTURES_REQUIRED",
    b"FIXTURES_SETUP",
    b"GENERATED_RESOURCE_SPEC_FILE",
    b"LABELS",
    b"MEASUREMENT",
    b"PASS_REGULAR_EXPRESSION",
    b"RESOURCE_GROUPS",
    b"PROCESSOR_AFFINITY",
    b"PROCESSORS",
    b"REQUIRED_FILES",
    b"RESOURCE_LOCK",
    b"RUN_SERIAL",
    b"SKIP_RETURN_CODE",
    b"TIMEOUT",
    b"TIMEOUT_SIGNAL_NAME",
    b"TIMEOUT_SIGNAL_GRACE_PERIOD",
    b"WILL_FAIL",
    b"WORKING_DIRECTORY",
];

fn json_properties(test: &PlannedTest, raw: bool) -> Vec<Value> {
    let working_directory = path::from_native(&test.directory);
    let properties = JSON_PROPERTIES.iter().filter_map(|&key| {
        let source = test
            .properties
            .get(key)
            .or_else(|| (key == b"WORKING_DIRECTORY").then_some(&working_directory))?;
        let typed = json_property(key, source);
        let present = match (&typed, key) {
            (Value::Bool(on), _) => *on,
            (Value::Real(cost), b"COST") => *cost != 0.0,
            (Value::Int(count), b"PROCESSORS") => *count > 1,
            (Value::Int(code), b"SKIP_RETURN_CODE") => (0..=255).contains(code),
            (Value::Array(items), _) => !items.is_empty(),
            (Value::String(_), _) => !source.is_empty(),
            _ => true,
        };
        present.then(|| {
            object([
                ("name", Value::string(BString::from(key))),
                (
                    "value",
                    if raw {
                        Value::string(source.clone())
                    } else {
                        typed
                    },
                ),
            ])
        })
    });

    properties
        .chain(
            test.properties
                .iter()
                .filter(|(key, _)| {
                    !JSON_PROPERTIES.contains(&key.as_slice())
                        && !matches!(
                            key.as_slice(),
                            b"_BACKTRACE_TRIPLES" | b"TIMEOUT_AFTER_MATCH"
                        )
                })
                .map(|(key, value)| {
                    object([
                        ("name", Value::string(key.clone())),
                        ("value", Value::string(value.clone())),
                    ])
                }),
        )
        .collect()
}

fn resource_groups(value: &[u8]) -> Value {
    let mut groups = Vec::new();
    for group in list::split(value, list::Empty::Drop) {
        let mut requirements = group.split(|&byte| byte == b',').peekable();
        let count = requirements
            .peek()
            .and_then(|field| json_integer(field))
            .map_or(1, |count| {
                requirements.next();
                count.clamp(0, 4096) as usize
            });
        let requirements = requirements
            .filter_map(|field| {
                let separator = field.iter().position(|&byte| byte == b':')?;
                let (name, slots) = field.split_at(separator);
                Some(object([
                    (".type", Value::string(name)),
                    ("slots", Value::Int(json_integer(slots.get(1..)?)?)),
                ]))
            })
            .collect();
        let group = object([("requirements", Value::Array(requirements))]);
        groups.extend(std::iter::repeat_n(group, count.min(4096 - groups.len())));
    }
    Value::Array(groups)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::PathBuf;
    use std::sync::Arc;

    use rcmake_host::{
        CommandSpec, Input, NativeHost, Output, ProcessResult, PureHost, ReadRange, RunOutput,
        WriteMode,
    };

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    fn invoke(host: Arc<dyn Host>, args: &[BString]) -> i32 {
        let argv: Vec<_> = std::iter::once("ctest".into())
            .chain(args.iter().cloned())
            .collect();
        crate::main(&argv, host)
    }

    #[test]
    fn lists_tests_and_filtered_labels_and_summarizes_processor_time() -> TestResult {
        let make_host = || -> rcmake_host::HostResult<Arc<PureHost>> {
            let host = Arc::new(PureHost::default());
            host.seed_file(
                Path::new("/work/CTestTestfile.cmake"),
                br#"
                add_test(a runner)
                add_test(b runner)
                set_tests_properties(a PROPERTIES LABELS "z;group" PROCESSORS 2)
                set_tests_properties(b PROPERTIES LABELS "group;other" DISABLED ON)
            "#,
            )?;
            host.seed_file(Path::new("/work/runner"), b"")?;
            host.set_command_handler(|host, _, _| {
                host.sleep(Duration::from_millis(150));
                Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                })
            })?;
            Ok(host)
        };
        let host = make_host()?;
        assert_eq!(invoke(host.clone(), &["-N".into()]), 0);
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            "Test project /work\n  Test #1: a\n  Test #2: b (Disabled)\n\nTotal Tests: 2\n"
        );
        let host = make_host()?;
        assert_eq!(
            invoke(
                host.clone(),
                &["--print-labels".into(), "-R".into(), "^b$".into()]
            ),
            0
        );
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            "Test project /work\nAll Labels:\n  group\n  other\n"
        );
        let host = make_host()?;
        assert_eq!(invoke(host.clone(), &[]), 0);
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        assert!(stdout.contains("Label Time Summary:\ngroup    =   0.30 sec*proc (2 tests)"));
        let host = make_host()?;
        assert_eq!(invoke(host.clone(), &["--no-label-summary".into()]), 0);
        assert!(
            !host
                .stream_bytes(Stream::Stdout)
                .to_string()
                .contains("Label Time Summary")
        );
        Ok(())
    }

    struct Fixture<'a> {
        host: &'a dyn Host,
        dir: PathBuf,
    }

    impl Drop for Fixture<'_> {
        fn drop(&mut self) {
            let _ = self.host.remove_dir_all(&self.dir);
        }
    }

    fn oracle(
        host: &dyn Host,
        dir: &Path,
        executable: &BString,
        args: &[BString],
    ) -> rcmake_host::HostResult<RunOutput> {
        host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: executable.clone(),
                    args: args.to_vec(),
                }],
                cwd: dir.to_path_buf(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(10)),
                tee: false,
            },
            &mut |_, _| {},
        )
    }

    fn stable_junit(bytes: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter(|line| !line.starts_with("\ttime=") && !line.starts_with("\ttimestamp="))
            .map(|line| {
                if let Some((start, rest)) = line.split_once(" time=\"")
                    && let Some((_, end)) = rest.split_once('"')
                {
                    return format!("{start}{end}");
                }
                line.to_owned()
            })
            .collect()
    }

    fn stable_cdash(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.lines().skip_while(|line| !line.contains("<Testing>"));
        let mut stable = Vec::new();
        while let Some(line) = lines.next() {
            if line.contains("DateTime>") || line.contains("TestTime>") {
                continue;
            }
            stable.push(line.to_owned());
            if line.contains("name=\"Execution Time\"") {
                lines.next();
            }
        }
        stable
    }

    #[test]
    #[cfg(unix)]
    fn reports_match_ctest_451() -> TestResult {
        let host = Arc::new(NativeHost::new()?);
        let Some(executable) = host.env_get(b"RCMAKE_CTEST_COMPARISON") else {
            return Ok(());
        };
        if host
            .metadata(&path::to_native(&executable), Follow::Yes)
            .is_err()
        {
            return Ok(());
        }
        let version = oracle(
            host.as_ref(),
            &host.initial_cwd(),
            &executable,
            &["--version".into()],
        )?;
        assert!(version.stdout.starts_with(b"ctest version 4.5.1\n"));
        let system = host.system();
        let fixture = Fixture {
            host: host.as_ref(),
            dir: std::env::temp_dir().join(format!(
                "rcmake-ctest-report-{}-{}",
                system.pid,
                host.now().nanos
            )),
        };
        let dir = &fixture.dir;
        host.create_dir_all(dir)?;
        let read = |path: &Path| host.read(path, ReadRange::All { limit: 256 * 1024 });
        host.write(
            &dir.join("CTestConfiguration.ini"),
            b"Site: local\nBuildName: report fixture\n",
            WriteMode::CreateNew,
        )?;
        host.write(&dir.join("CTestTestfile.cmake"), br#"
            add_test([=[pass<&"]=] /bin/echo [=[<output> & "quoted"]=])
            add_test(fail /usr/bin/false)
            add_test(disabled /usr/bin/true)
            add_test(missing /nonexistent/rcmake-ctest-fixture)
            add_test(skipped /usr/bin/false)
            set_tests_properties([=[pass<&"]=] PROPERTIES LABELS "z;group;group" PROCESSORS 2 RUN_SERIAL OFF TIMEOUT 5 CUSTOM "a;b" _BACKTRACE_TRIPLES "/fixture/CMakeLists.txt;7;add_test;/fixture/CMakeLists.txt;0;")
            set_tests_properties(disabled PROPERTIES DISABLED YES)
            set_tests_properties(skipped PROPERTIES SKIP_RETURN_CODE 1)
        "#, WriteMode::CreateNew)?;
        let args = ["--test-dir".into(), path::from_native(dir)];
        let options = rcmake_json::ReadOptions::default();
        for mode in ["--show-only=json-v1", "--show-only=json-v1-raw"] {
            let argv = [args.as_slice(), &[mode.into(), "-C".into(), "Debug".into()]].concat();
            let expected = oracle(host.as_ref(), dir, &executable, &argv)?;
            assert_eq!(expected.results, [ProcessResult::Exited(0)]);
            let actual = Arc::new(rcmake_host::PureHost::new(
                dir.to_path_buf(),
                dir.join("ctest"),
            ));
            actual.seed_file(
                &dir.join("CTestTestfile.cmake"),
                &read(&dir.join("CTestTestfile.cmake"))?,
            )?;
            for program in ["/bin/echo", "/usr/bin/false", "/usr/bin/true"] {
                actual.seed_file(Path::new(program), b"")?;
            }
            assert_eq!(invoke(actual.clone(), &argv), 0);
            let expected = rcmake_json::parse(&expected.stdout, &options)?;
            let actual = rcmake_json::parse(&actual.stream_bytes(Stream::Stdout), &options)?;
            assert_eq!(actual, expected, "{mode}");
        }

        let argv = [
            args.as_slice(),
            &[
                "-Q".into(),
                "-T".into(),
                "Test".into(),
                "--output-junit".into(),
                "junit.xml".into(),
            ],
        ]
        .concat();
        let result = oracle(host.as_ref(), dir, &executable, &argv)?;
        assert_eq!(result.results, [ProcessResult::Exited(8)]);
        let junit = dir.join("junit.xml");
        let tag = crate::dart::Tag::read(host.as_ref(), dir)?;
        let cdash = dir.join("Testing").join(tag.stamp()).join("Test.xml");
        let expected_junit = stable_junit(&read(&junit)?);
        let expected_cdash = stable_cdash(&read(&cdash)?);
        assert_eq!(invoke(host.clone(), &argv), 8);
        assert_eq!(stable_junit(&read(&junit)?), expected_junit);
        assert_eq!(stable_cdash(&read(&cdash)?), expected_cdash);
        Ok(())
    }
}
