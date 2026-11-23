use std::path::Path;
use std::time::Duration;

use rcmake_host::{Host, RunRequest, Stream};

use crate::CtestOptions;
use crate::execution::{ResultLine, Status};
use crate::selection::{Plan, PlannedTest};

pub(crate) struct Report<'a> {
    host: &'a dyn Host,
    options: &'a CtestOptions,
    total: usize,
    name_width: usize,
    number_width: usize,
}

impl<'a> Report<'a> {
    pub(crate) fn new(host: &'a dyn Host, options: &'a CtestOptions, plan: &Plan) -> Self {
        let name_width = plan
            .tests
            .iter()
            .map(|test| test.name.len())
            .max()
            .unwrap_or(0)
            .max(30);
        let number_width = plan
            .tests
            .last()
            .map_or(1, |test| test.number.to_string().len());
        Self {
            host,
            options,
            total: plan.tests.len(),
            name_width,
            number_width,
        }
    }

    fn write(&self, message: std::fmt::Arguments<'_>) {
        if !self.options.quiet {
            self.host
                .write_out(Stream::Stdout, message.to_string().as_bytes());
        }
    }

    pub(crate) fn project(&self, path: &Path) {
        self.write(format_args!("Test project {}\n", path.display()));
    }

    pub(crate) fn list(&self, plan: &Plan) {
        for test in &plan.tests {
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

    pub(crate) fn start(&self, test: &PlannedTest, request: Option<&RunRequest>) {
        self.write(format_args!(
            "    Start {:>width$}: {}\n",
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
            if let Some(timeout) = request.timeout {
                self.write(format_args!(
                    "{}: Test timeout computed to be: {}\n",
                    test.number,
                    timeout.as_secs_f64()
                ));
            }
        }
    }

    pub(crate) fn finish(&self, test: &PlannedTest, result: &ResultLine, completed: usize) {
        if self.options.quiet {
            return;
        }
        if let Some(detail) = &result.detail {
            self.write(format_args!("{detail}\n"));
        }
        if self.options.verbose {
            for line in result.output.split_inclusive(|&byte| byte == b'\n') {
                self.write(format_args!("{}: ", test.number));
                self.host.write_out(Stream::Stdout, line);
            }
            self.end_output_line(&result.output);
        }
        let dots = ".".repeat(self.name_width + 3 - test.name.len());
        self.write(format_args!(
            "{completed:>progress_width$}/{} Test #{:>width$}: {} {dots}{} {:>6.2} sec\n",
            self.total,
            test.number,
            test.name,
            status_text(result.status),
            result.elapsed.as_secs_f64(),
            width = self.number_width,
            progress_width = self.total.to_string().len(),
        ));
        if !self.options.verbose && result.status.failed() && self.options.output_on_failure {
            self.host.write_out(Stream::Stdout, &result.output);
            self.end_output_line(&result.output);
        }
    }

    fn end_output_line(&self, output: &[u8]) {
        if output.last().is_some_and(|&byte| byte != b'\n') {
            self.host.write_out(Stream::Stdout, b"\n");
        }
    }

    pub(crate) fn summary(&self, plan: &Plan, statuses: &[Option<Status>], elapsed: Duration) {
        if self.options.quiet {
            return;
        }
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
                self.host
                    .write_out(Stream::Stderr, b"No tests were found!!!\n");
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
            self.host
                .write_out(Stream::Stderr, b"Errors while running CTest\n");
        }
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
                    _ => status_text(status).trim_start_matches(['*', ' ']),
                };
                self.write(format_args!(
                    "\t{:>3} - {} ({label})\n",
                    test.number, test.name
                ));
            }
        }
    }
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
