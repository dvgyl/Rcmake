use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::{BString, Limits};
use rcmake_host::{
    CommandSpec, CopyOptions, Follow, Host, HostResult, Input, Output, ProcessResult, ReadRange,
    RunRequest, Stream, WriteMode, path,
};

use crate::{emit_cli_error, options, resolve_cli_path};

pub(crate) fn run(argv: &[BString], args: &[BString], host: &dyn Host) -> i32 {
    let (exe, root) = rcmake_host::discover_cmake_root(host, argv.first());
    let Some(root) = root else {
        return crate::missing_cmake_root(host, &exe, false);
    };
    let mut generator = None;
    let mut output = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if let Some(name) = arg.strip_prefix(b"-G") {
            generator = if name.is_empty() {
                args.next().cloned()
            } else {
                Some(name.into())
            };
            if generator.is_none() {
                emit_cli_error(host, "No generator specified for -G");
                options::print_generators(host, Stream::Stderr);
                return 1;
            }
        } else {
            output = Some(resolve_cli_path(host, arg));
        }
    }
    let dir = match work_directory(host) {
        Ok(dir) => dir,
        Err(_) => {
            host.write_out(
                Stream::Stderr,
                b"Error: --system-information must be run from a writable directory!\n",
            );
            return 1;
        }
    };
    let result = write_information(host, &exe, &root, &dir, generator, output);
    let _ = host.remove_dir_all(&dir);
    match result {
        Ok(code) => code,
        Err(err) => {
            emit_cli_error(host, err.to_string());
            1
        }
    }
}

fn work_directory(host: &dyn Host) -> HostResult<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = host.now();
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = host.initial_cwd().join(format!(
        "__cmake_systeminformation-{}-{}-{sequence}",
        now.secs, now.nanos
    ));
    if host.metadata(&dir, Follow::No).is_ok() {
        return Err(rcmake_host::HostError::from_io(
            std::io::ErrorKind::AlreadyExists.into(),
            Some(dir),
        ));
    }
    host.create_dir_all(&dir)?;
    host.write(&dir.join(".rcmake"), b"", WriteMode::CreateNew)?;
    Ok(dir)
}

fn write_information(
    host: &dyn Host,
    exe: &Path,
    root: &Path,
    dir: &Path,
    generator: Option<BString>,
    output: Option<PathBuf>,
) -> HostResult<i32> {
    host.copy_file(
        &root.join("Modules/SystemInformation.cmake"),
        &dir.join("CMakeLists.txt"),
        CopyOptions::default(),
    )?;
    let result_path = output.clone().unwrap_or_else(|| dir.join("results.txt"));
    let mut arg = BString::from("-DRESULT_FILE=");
    arg.extend_from_slice(&path::from_native(&result_path));
    let mut args = vec!["-S".into(), ".".into(), "-B".into(), ".".into(), arg];
    if let Some(generator) = generator {
        args.extend([BString::from("-G"), generator]);
    }
    let result = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: path::from_native(exe),
                args,
            }],
            cwd: dir.to_path_buf(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(120)),
            tee: false,
            default_error_mode: false,
        },
        &mut |_, _| {},
    )?;
    host.write_out(Stream::Stderr, &result.stderr);
    let code = match result.results.first() {
        Some(ProcessResult::Exited(code)) => *code,
        _ => 1,
    };
    if code != 0 {
        host.write_out(
            Stream::Stderr,
            b"Error: --system-information failed on internal CMake!\n",
        );
    } else if output.is_none() {
        let text = host.read(
            &result_path,
            ReadRange::All {
                limit: Limits::default().max_file_read,
            },
        )?;
        host.write_out(Stream::Stdout, &text);
    }
    Ok(code)
}
