//! `rcmake-fuzz` binary: run one fuzz surface (ADR-016).
//!
//! Exit code 0: no failure. Exit code 1: at least one failure, or a run error.
//! Exit code 2: usage error.

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stderr = std::io::stderr().lock();
    let options = match rcmake_fuzz::parse_args(&args) {
        Ok(options) => options,
        Err(message) => {
            let _ = write!(
                stderr,
                "rcmake-fuzz: {message}\n{}",
                rcmake_fuzz::options::usage()
            );
            return ExitCode::from(2);
        }
    };
    match rcmake_fuzz::run(&options) {
        Ok(report) => {
            let _ = writeln!(
                stderr,
                "rcmake-fuzz: {}: {} iterations, {} rejected, {} failures",
                options.surface.name(),
                report.iterations,
                report.rejected,
                report.failures.len()
            );
            for failure in &report.failures {
                let _ = writeln!(stderr, "  seed {}: {:?}", failure.seed, failure.outcome);
            }
            if report.failures.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            let _ = writeln!(stderr, "rcmake-fuzz: {err}");
            ExitCode::FAILURE
        }
    }
}
