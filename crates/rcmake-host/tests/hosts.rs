//! Run the shared host checks and exercise process input and output.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, PureHost, ReadRange,
    RunOutput, RunRequest, Stream, WriteMode, conformance,
};
use rustix::fs::Mode;
use rustix::io::Errno;

struct TempDir<'a> {
    host: &'a NativeHost,
    path: PathBuf,
}

impl<'a> TempDir<'a> {
    fn new(host: &'a NativeHost) -> Result<Self, Errno> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(format!("/tmp/rcmake-host-test-{}-{id}", host.system().pid));
            match rustix::fs::mkdir(&path, Mode::RWXU) {
                Ok(()) => return Ok(Self { host, path }),
                Err(Errno::EXIST) => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for TempDir<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

#[test]
fn native_conformance() {
    let host = NativeHost::new().expect("native host");
    let temp = TempDir::new(&host).expect("temporary directory");
    conformance::run_all(&host, &temp.path);
}

#[test]
fn pure_conformance() {
    conformance::run_all(&PureHost::default(), Path::new("/work/conformance"));
}

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

#[test]
fn native_pipeline_with_bytes_and_explicit_environment() {
    let host = NativeHost::new().expect("native host");
    let temp = TempDir::new(&host).expect("temporary directory");
    let cwd = host.canonicalize(&temp.path).expect("cwd");
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

#[test]
fn native_process_file_redirection_and_spawn_failure() {
    let host = NativeHost::new().expect("native host");
    let temp = TempDir::new(&host).expect("temporary directory");
    let input = temp.path.join("input");
    let output = temp.path.join("output");
    host.write(&input, b"bytes", WriteMode::CreateNew)
        .expect("input");
    host.write(&output, b"old", WriteMode::CreateNew)
        .expect("output");
    let mut req = request(&temp.path, vec![shell("/bin/cat; printf error >&2")]);
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
    let req = request(&temp.path, vec![missing, shell("/bin/cat")]);
    let result = host.run(&req, &mut |_, _| {}).expect("run");
    assert!(
        matches!(result.results.as_slice(), [ProcessResult::SpawnFailed(err), ProcessResult::Exited(0)] if err.errno == Some(Errno::NOENT.raw_os_error()))
    );
}

#[test]
fn native_timeout_kills_pipeline_and_descendants() {
    let host = NativeHost::new().expect("native host");
    let temp = TempDir::new(&host).expect("temporary directory");
    let mut req = request(
        &temp.path,
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
    host.set_command_handler(|host, req, emit| {
        assert_eq!(req.pipeline, [shell("scripted")]);
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
    let mut req = request(&host.initial_cwd(), vec![shell("scripted")]);
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
            .errno,
        Some(Errno::LOOP.raw_os_error())
    );
}
