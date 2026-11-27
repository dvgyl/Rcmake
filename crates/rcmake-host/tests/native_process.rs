//! Exercise a native test executable in process pipelines and job objects.

use std::io::{self, Write};
use std::process::{Command, ExitCode};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Host, Input, NativeHost, Output, ProcessResult, RunRequest, Stream, path,
};

#[allow(clippy::disallowed_methods, clippy::zombie_processes)]
fn child(mode: &str) -> io::Result<ExitCode> {
    match mode {
        "pass" => {
            io::copy(&mut io::stdin().lock(), &mut io::stdout().lock())?;
        }
        "report" => {
            io::stdout().write_all(b"out")?;
            io::stdout().flush()?;
            io::stderr().write_all(b"err")?;
            return Ok(ExitCode::from(23));
        }
        "descendant" => {
            io::stdout().write_all(b"ready\n")?;
            io::stdout().flush()?;
            let _descendant = Command::new(std::env::current_exe()?)
                .args(["--child", "sleep"])
                .spawn()?;
        }
        "tree" => {
            io::stdout().write_all(b"ready\n")?;
            io::stdout().flush()?;
            Command::new(std::env::current_exe()?)
                .args(["--child", "sleep"])
                .status()?;
        }
        "sleep" => {
            std::thread::sleep(Duration::from_secs(15));
        }
        _ => return Err(io::Error::other("Unknown child mode")),
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--child") {
        return Ok(child(
            &args
                .next()
                .ok_or_else(|| io::Error::other("Missing child mode"))?,
        )?);
    }

    let host = NativeHost::new()?;
    let exe = path::from_native(&host.current_exe()?);
    let stage = |mode: &str| CommandSpec {
        program: exe.clone(),
        args: vec![BString::from("--child"), BString::from(mode)],
    };
    let mut req = RunRequest {
        pipeline: vec![stage("pass"), stage("pass")],
        stdin: Input::Bytes(b"binary\0line\n".repeat(30_000)),
        stdout: Output::Capture,
        stderr: Output::Capture,
        env: host.env_all(),
        cwd: host.initial_cwd(),
        timeout: Some(Duration::from_secs(10)),
        tee: false,
    };
    let output = host.run(&req, &mut |_, _| {})?;
    let Input::Bytes(bytes) = &req.stdin else {
        return Err(io::Error::other("Missing test input").into());
    };
    assert_eq!(output.stdout, *bytes);
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0), ProcessResult::Exited(0)]
    );
    assert!(host.run_bounded(&req, 64).is_err());
    let total = bytes.len();

    req.stdout = Output::Stream;
    let mut streamed = 0;
    let output = host.run_bounded_streaming(&req, 64, &mut |stream, bytes| {
        assert_eq!(stream, Stream::Stdout);
        streamed += bytes.len();
    })?;
    assert_eq!(streamed, total);
    assert!(output.stdout.is_empty());

    req.pipeline = vec![stage("report")];
    req.stdin = Input::Null;
    let mut live = Vec::new();
    let output = host.run(&req, &mut |stream, bytes| {
        live.push((stream, bytes.to_vec()))
    })?;
    assert_eq!(live, [(Stream::Stdout, b"out".to_vec())]);
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"err");

    req.stdout = Output::Capture;
    req.stderr = Output::SharedWithStdout;
    req.tee = true;
    let mut live = Vec::new();
    let output = host.run(&req, &mut |stream, bytes| {
        assert_eq!(stream, Stream::Stdout);
        live.extend_from_slice(bytes);
    })?;
    assert_eq!(live, b"outerr");
    assert_eq!(output.stdout, live);
    assert!(output.stderr.is_empty());
    assert_eq!(output.results, [ProcessResult::Exited(23)]);

    req.timeout = Some(Duration::from_secs(2));
    for mode in ["descendant", "tree"] {
        req.pipeline = vec![stage(mode)];
        let start = host.monotonic();
        let mut live = Vec::new();
        let result = host.run(&req, &mut |_, bytes| {
            assert!(host.monotonic().saturating_sub(start) < Duration::from_secs(2));
            live.extend_from_slice(bytes);
        })?;
        assert!(host.monotonic().saturating_sub(start) < Duration::from_secs(10));
        assert!(live.starts_with(b"ready"));
        assert_eq!(result.results, [ProcessResult::TimedOut]);
    }
    Ok(ExitCode::SUCCESS)
}
