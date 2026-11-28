//! Check that deep script recursion stops at the native stack guard instead of crashing.
//!
//! This is a separate test binary, so that a crash cannot hide the results of other tests.

use std::path::PathBuf;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Host, Input, NativeHost, Output, ProcessResult, RunRequest, WriteMode,
};

struct Temp<'a>(&'a NativeHost, PathBuf);

impl Drop for Temp<'_> {
    fn drop(&mut self) {
        let _ = self.0.remove_dir_all(&self.1);
    }
}

#[test]
fn deep_eval_recursion_stops_with_an_error_and_exit_code_1()
-> Result<(), Box<dyn std::error::Error>> {
    let host = NativeHost::new()?;
    let dir = Temp(
        &host,
        std::env::temp_dir().join(format!("rcmake-stack-guard-{}", std::process::id())),
    );
    host.create_dir_all(&dir.1)?;
    let script = dir.1.join("recurse.cmake");
    // Each level uses more than 3.3 KB of native stack (64 MiB / 20,000) in all profiles.
    host.write(
        &script,
        b"set(CMAKE_MAXIMUM_RECURSION_DEPTH 20000)\nfunction(f n)\n  cmake_language(EVAL CODE \"f(${n})\")\nendfunction()\nf(0)\n",
        WriteMode::CreateNew,
    )?;
    let output = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: env!("CARGO_BIN_EXE_rcmake").into(),
                args: vec![
                    BString::from("-P"),
                    BString::from(script.to_string_lossy().as_bytes()),
                ],
            }],
            cwd: dir.1.clone(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(300)),
            tee: false,
            default_error_mode: false,
        },
        &mut |_, _| {},
    )?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.results, [ProcessResult::Exited(1)], "{stderr}");
    assert!(
        stderr.contains("Maximum recursion depth exceeded: the native stack is full at depth"),
        "{stderr}"
    );
    Ok(())
}
