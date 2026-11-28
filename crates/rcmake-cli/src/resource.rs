//! Run the resource compiler with preprocessed input.

use std::io::ErrorKind;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest, Stream, path};

use crate::{emit_cli_error, resolve_cli_path};

pub(crate) fn run(args: &[BString], host: &dyn Host) -> i32 {
    let [source, temporary, commands @ ..] = args else {
        return fail(host, b"Invalid cmake_llvm_rc arguments");
    };
    let mut commands = commands.splitn(2, |arg| arg == b"++");
    let Some((preprocessor, preprocessing_args)) = commands.next().and_then(<[_]>::split_first)
    else {
        return fail(host, b"Empty preprocessing command");
    };
    let Some((compiler, compiler_args)) = commands.next().and_then(<[_]>::split_first) else {
        return fail(host, b"Empty resource compilation command");
    };
    let source = resolve_cli_path(host, source);
    let cwd = host.initial_cwd();
    let source_dir = source
        .parent()
        .map_or_else(|| path::from_native(&cwd), path::from_native);
    let file = resolve_cli_path(host, temporary);
    let preprocessed = execute(
        host,
        preprocessor,
        preprocessing_args.to_vec(),
        Output::File {
            path: file.clone(),
            append: false,
        },
    );
    let compiled = preprocessed
        && execute(
            host,
            compiler,
            resource_args(compiler_args, &source_dir, temporary),
            Output::Null,
        );
    if let Err(err) = host.remove_file(&file)
        && err.kind != ErrorKind::NotFound
    {
        emit_cli_error(host, format!("Could not remove {}: {err}", file.display()));
        return 1;
    }
    i32::from(!compiled)
}

fn resource_args(args: &[BString], source_dir: &[u8], temporary: &BString) -> Vec<BString> {
    let mut output = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(token) = arg.strip_prefix(b"/").or_else(|| arg.strip_prefix(b"-")) else {
            continue;
        };
        let lower = token.to_ascii_lowercase();
        if ![b"fo".as_slice(), b"i", b"d", b"u"]
            .iter()
            .any(|flag| lower.starts_with(flag))
        {
            continue;
        }
        output.push(expand_source(arg, source_dir));
        if matches!(lower.as_slice(), b"fo" | b"i" | b"d" | b"u")
            && let Some(value) = args.next()
        {
            output.push(expand_source(value, source_dir));
        }
    }
    output.push(temporary.clone());
    output
}

fn expand_source(arg: &BString, source: &[u8]) -> BString {
    const MARKER: &[u8] = b"SOURCE_DIR";
    let mut remainder = arg.as_slice();
    let mut expanded = BString::default();
    while let Some(index) = remainder
        .windows(MARKER.len())
        .position(|part| part == MARKER)
    {
        let (prefix, tail) = remainder.split_at(index);
        expanded.extend_from_slice(prefix);
        expanded.extend_from_slice(source);
        remainder = tail.strip_prefix(MARKER).unwrap_or_default();
    }
    expanded.extend_from_slice(remainder);
    expanded
}

fn execute(host: &dyn Host, program: &BString, args: Vec<BString>, stdout: Output) -> bool {
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args,
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Inherit,
        stdout,
        stderr: Output::Inherit,
        timeout: Some(Duration::from_secs(120)),
        tee: false,
        default_error_mode: false,
    };
    match host.run(&request, &mut |_, _| {}) {
        Ok(output) => match output.results.first() {
            Some(ProcessResult::Exited(0)) => true,
            Some(ProcessResult::SpawnFailed(err)) => {
                emit_cli_error(host, format!("Could not execute {program}: {err}"));
                false
            }
            _ => false,
        },
        Err(err) => {
            emit_cli_error(host, format!("Could not execute {program}: {err}"));
            false
        }
    }
}

fn fail(host: &dyn Host, message: &[u8]) -> i32 {
    host.write_out(Stream::Stderr, message);
    1
}
