//! Isolated black-box differential harness comparing staged `rcmake` binaries against
//! the pinned official **CMake 4.5.1** oracle.
//!
//! By architectural rule, `rcmake-harness` links **zero** core engine crates and
//! interacts with both `rcmake` and CMake 4.5.1 strictly as black-box subprocesses.

#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::exit
)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn normalize_banner(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.starts_with("CMake suite maintained and supported by Kitware")
                || line.starts_with("Rcmake native Rust drop-in replacement for CMake")
            {
                "<VERSION_ATTRIBUTION>"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn run_bin(bin: &Path, args: &[&str], cwd: &Path) -> Result<(i32, String, String), String> {
    let output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("TZ", "UTC")
        .output()
        .map_err(|e| format!("failed to execute {}: {e}", bin.display()))?;
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((code, stdout, stderr))
}

fn run_smoke() -> Result<(), String> {
    let oracle_bin_dir = std::env::var_os("RCMAKE_ORACLE_BIN_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "RCMAKE_ORACLE_BIN_DIR is not set (source env/env.sh first)".to_owned())?;
    let repo_root = std::env::var_os("RCMAKE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let stage_bin_dir = repo_root.join("target/stage/bin");
    if !stage_bin_dir.join("cmake").exists() {
        return Err(format!(
            "staged binary not found at {}; run scripts/stage.sh first",
            stage_bin_dir.display()
        ));
    }

    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir failed: {e}"))?;
    let mut passed = 0usize;
    let cases = [
        ("cmake", &["--version"][..]),
        ("ctest", &["--version"][..]),
        ("cpack", &["--version"][..]),
    ];

    for (tool, args) in cases {
        let oracle_exe = oracle_bin_dir.join(tool);
        let rcmake_exe = stage_bin_dir.join(tool);
        let (o_code, o_out, o_err) = run_bin(&oracle_exe, args, tmp.path())?;
        let (r_code, r_out, r_err) = run_bin(&rcmake_exe, args, tmp.path())?;

        let o_norm = normalize_banner(&o_out);
        let r_norm = normalize_banner(&r_out);
        if o_code != r_code || o_norm != r_norm || o_err != r_err {
            return Err(format!(
                "MISMATCH on `{tool} {}`:\n  exit: oracle={o_code}, rcmake={r_code}\n  oracle stdout: {o_norm:?}\n  rcmake stdout: {r_norm:?}\n  oracle stderr: {o_err:?}\n  rcmake stderr: {r_err:?}",
                args.join(" ")
            ));
        }
        passed += 1;
    }

    println!(
        "rcmake-harness smoke: {passed}/{} cases matched CMake 4.5.1",
        cases.len()
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let suite = if args.first().map(String::as_str) == Some("--suite") {
        args.get(1).map(String::as_str).unwrap_or("smoke")
    } else {
        "smoke"
    };

    match suite {
        "smoke" => match run_smoke() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("rcmake-harness ERROR: {err}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("rcmake-harness ERROR: unknown suite `{other}` (expected `smoke`)");
            ExitCode::FAILURE
        }
    }
}
