//! Isolated companion binary for explicit `rcmake verify --oracle` comparisons.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(2)
}
