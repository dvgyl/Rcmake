//! Run the shared host checks and exercise process input and output.

use std::path::Path;
use std::time::Duration;

use rcmake_base::BString;
#[cfg(unix)]
use rcmake_host::ReadRange;
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, PureHost, RunOutput,
    RunRequest, Stream, WriteMode, conformance,
};
#[cfg(unix)]
use rustix::io::Errno;

#[test]
fn native_conformance() {
    let host = NativeHost::new().expect("native host");
    let temp = tempfile::tempdir().expect("temporary directory");
    conformance::run_all(&host, temp.path());
}

#[cfg(windows)]
#[test]
fn native_windows_system_values_and_path_validation() {
    let host = NativeHost::new().expect("native host");
    let info = host.system();
    assert_eq!(info.sysname, "Windows");
    assert!(!info.hostname.is_empty());
    assert!(!info.release.is_empty());
    assert!(!info.version.is_empty());
    assert!(host.initial_cwd().is_absolute());
    assert!(
        host.metadata(Path::new("/work"), Follow::No)
            .is_err_and(|error| error.kind == std::io::ErrorKind::InvalidInput)
    );
    host.env_set(b"example", Some(b"value"));
    assert_eq!(host.env_get(b"EXAMPLE"), Some(BString::from("value")));
}

#[test]
fn pure_conformance() {
    conformance::run_all(&PureHost::default(), Path::new("/work/conformance"));
}

#[cfg(unix)]
fn shell(command: &str) -> CommandSpec {
    CommandSpec {
        program: BString::from("/bin/sh"),
        args: vec![BString::from("-c"), BString::from(command)],
    }
}

fn request(cwd: &Path, pipeline: Vec<CommandSpec>) -> RunRequest {
    RunRequest {
        pipeline,
        cwd: cwd.to_path_buf(),
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(Duration::from_secs(5)),
        tee: false,
    }
}

#[cfg(unix)]
#[test]
fn native_pipeline_with_bytes_and_explicit_environment() {
    let host = NativeHost::new().expect("native host");
    let temp = tempfile::tempdir().expect("temporary directory");
    let cwd = host.canonicalize(temp.path()).expect("cwd");
    host.env_set(b"RCMAKE_HIDDEN", Some(b"hidden"));
    let mut req = request(
        &cwd,
        vec![
            shell(
                "printf '%s:%s:%s\\n' \"$RCMAKE_VALUE\" \"${RCMAKE_HIDDEN-unset}\" \"$PWD\"; /bin/cat; printf first >&2",
            ),
            shell("/usr/bin/tr '[:lower:]' '[:upper:]'"),
            shell("/bin/cat"),
        ],
    );
    req.env
        .push((BString::from("RCMAKE_VALUE"), BString::from("visible")));
    let input = b"abc\0def\n".repeat(32_768);
    req.stdin = Input::Bytes(input.clone());
    let result = host
        .run(&req, &mut |_, _| panic!("tee disabled"))
        .expect("run");
    assert_eq!(result.results, vec![ProcessResult::Exited(0); 3]);
    let mut expected = format!("visible:unset:{}\n", cwd.display()).into_bytes();
    expected.extend(input);
    expected.make_ascii_uppercase();
    assert_eq!(result.stdout, expected);
    assert_eq!(result.stderr, b"first");
}

#[cfg(unix)]
#[test]
fn native_shares_stdout_and_stderr_with_tee() {
    let host = NativeHost::new().expect("native host");
    let mut req = request(
        &host.initial_cwd(),
        vec![shell("printf a; printf b >&2; printf c; printf d >&2")],
    );
    req.stderr = Output::SharedWithStdout;
    req.tee = true;
    let mut chunks = Vec::new();
    let result = host
        .run(&req, &mut |stream, bytes| {
            assert_eq!(stream, Stream::Stdout);
            chunks.extend_from_slice(bytes);
        })
        .expect("run");
    assert_eq!(result.results, [ProcessResult::Exited(0)]);
    assert_eq!(result.stdout, b"abcd");
    assert!(result.stderr.is_empty());
    assert_eq!(chunks, result.stdout);
}

#[cfg(unix)]
#[test]
fn native_process_file_redirection_and_spawn_failure() {
    let host = NativeHost::new().expect("native host");
    let temp = tempfile::tempdir().expect("temporary directory");
    let input = temp.path().join("input");
    let output = temp.path().join("output");
    host.write(&input, b"bytes", WriteMode::CreateNew)
        .expect("input");
    host.write(&output, b"old", WriteMode::CreateNew)
        .expect("output");
    let mut req = request(temp.path(), vec![shell("/bin/cat; printf error >&2")]);
    req.stdin = Input::File(input);
    req.stdout = Output::File {
        path: output.clone(),
        append: false,
    };
    req.stderr = Output::SharedWithStdout;
    let result = host.run(&req, &mut |_, _| {}).expect("run");
    assert_eq!(result.results, [ProcessResult::Exited(0)]);
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
    req.stdout = Output::File {
        path: output.clone(),
        append: true,
    };
    req.stderr = Output::Null;
    host.run(&req, &mut |_, _| {}).expect("run");
    assert_eq!(
        host.read(&output, ReadRange::All { limit: 64 })
            .expect("output"),
        b"byteserrorbytes"
    );

    let missing = CommandSpec {
        program: BString::from("/nonexistent/rcmake-host-test"),
        args: Vec::new(),
    };
    let req = request(temp.path(), vec![missing, shell("/bin/cat")]);
    let result = host.run(&req, &mut |_, _| {}).expect("run");
    assert!(
        matches!(result.results.as_slice(), [ProcessResult::SpawnFailed(err), ProcessResult::Exited(0)] if err.errno == Some(Errno::NOENT.raw_os_error()))
    );
}

#[cfg(unix)]
#[test]
fn native_timeout_kills_pipeline_and_descendants() {
    let host = NativeHost::new().expect("native host");
    let temp = tempfile::tempdir().expect("temporary directory");
    let mut req = request(
        temp.path(),
        vec![
            shell("(/bin/sleep 5; : > survived) & wait"),
            shell("/bin/cat"),
        ],
    );
    req.timeout = Some(Duration::from_millis(250));
    req.stdin = Input::Bytes(vec![0; 1_048_576]);
    let start = host.monotonic();
    let result = host.run(&req, &mut |_, _| {}).expect("run");
    assert_eq!(
        result.results,
        [ProcessResult::TimedOut, ProcessResult::TimedOut]
    );
    assert!(host.monotonic() - start < Duration::from_secs(4));
}

#[test]
fn pure_process_hook_and_virtual_io() {
    let host = PureHost::default();
    let path = Path::new("/work/seed/nested");
    host.seed_file(path, b"fixture").expect("seed file");
    let original = host.metadata(path, Follow::Yes).expect("metadata");
    host.write(path, b"!", WriteMode::Append).expect("append");
    let changed = host.metadata(path, Follow::Yes).expect("metadata");
    assert!(changed.atime > original.atime && changed.mtime > original.mtime);

    host.seed_stdin(b"abc").expect("seed stdin");
    assert_eq!(host.read_stdin(1).expect("stdin"), b"a");
    let command = CommandSpec {
        program: BString::from("scripted"),
        args: Vec::new(),
    };
    let mut req = request(&host.initial_cwd(), vec![command.clone()]);
    host.set_command_handler(move |host, req, emit| {
        assert_eq!(req.pipeline.as_slice(), std::slice::from_ref(&command));
        let stdout = host.read_stdin(5)?;
        host.write_out(Stream::Stderr, b"logged");
        emit(Stream::Stdout, &stdout);
        Ok(RunOutput {
            stdout,
            results: vec![ProcessResult::Exited(2)],
            ..RunOutput::default()
        })
    })
    .expect("handler");
    req.tee = true;
    let mut chunks = Vec::new();
    let result = host
        .run(&req, &mut |stream, bytes| {
            chunks.push((stream, bytes.to_vec()))
        })
        .expect("run");
    assert_eq!(result.results, [ProcessResult::Exited(2)]);
    assert_eq!(result.stdout, b"bc");
    assert_eq!(chunks, [(Stream::Stdout, b"bc".to_vec())]);
    assert_eq!(host.stream_bytes(Stream::Stderr), BString::from("logged"));
    assert_eq!(host.events().len(), 1);
    assert!(host.read_stdin(5).expect("stdin").is_empty());
}

#[cfg(unix)]
#[test]
fn native_timeout_after_pipeline_leader_exits() {
    let host = NativeHost::new().expect("native host");
    let mut req = request(
        &host.initial_cwd(),
        vec![shell("/bin/sleep 5 & printf 'started'")],
    );
    req.timeout = Some(Duration::from_millis(500));
    let result = host.run(&req, &mut |_, _| {}).expect("run");
    assert_eq!(result.stdout, b"started");
    assert_eq!(result.results, [ProcessResult::TimedOut]);
}

#[test]
fn pure_symlink_hop_limit() {
    let host = PureHost::default();
    let base = Path::new("/work");
    host.seed_file(&base.join("0"), b"target").expect("seed");
    for hop in 1..=41 {
        host.symlink(
            Path::new(&(hop - 1).to_string()),
            &base.join(hop.to_string()),
        )
        .expect("link");
    }
    assert_eq!(
        host.canonicalize(&base.join("40")).expect("resolve"),
        base.join("0")
    );
    assert_eq!(
        host.canonicalize(&base.join("41"))
            .expect_err("link limit")
            .os_message,
        "Too many levels of symbolic links"
    );
}

/// Hold a lock in a second thread. The waiting thread must get the lock after the
/// release, both without a timeout and with a timeout that is long enough.
fn check_lock_wait(host: &(dyn Host + Sync), path: &Path) -> Result<(), String> {
    for timeout in [None, Some(Duration::from_secs(30))] {
        let guard = host
            .lock_file(path, Some(Duration::ZERO))
            .map_err(|err| format!("first lock: {err}"))?;
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| host.lock_file(path, timeout).map(|_| ()));
            std::thread::sleep(Duration::from_millis(50));
            drop(guard);
            match waiter.join() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(format!("waiting lock: {err}")),
                Err(_) => Err("the waiting thread panicked".to_owned()),
            }
        })?;
    }
    Ok(())
}

#[test]
fn native_lock_waits_for_release() {
    let host = NativeHost::new().expect("native host");
    let temp = tempfile::tempdir().expect("temporary directory");
    assert_eq!(
        check_lock_wait(&host, &temp.path().join("wait.lock")),
        Ok(())
    );
}

#[test]
fn pure_lock_waits_for_release() {
    let host = PureHost::default();
    host.create_dir_all(Path::new("/work"))
        .expect("work directory");
    assert_eq!(check_lock_wait(&host, Path::new("/work/wait.lock")), Ok(()));
}
