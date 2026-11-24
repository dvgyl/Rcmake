//! Run supported analyzers before the compiler.

use std::time::Duration;

use rcmake_base::{BString, list};
use rcmake_host::{CommandSpec, Follow, Input, Output, ProcessResult, RunRequest, Stream};

use super::{ToolCtx, error, path, split_once, write};

const TOOLS: &[&[u8]] = &[
    b"cppcheck",
    b"cpplint",
    b"icstat",
    b"iwyu",
    b"lwyu",
    b"pvs-studio",
    b"tidy",
];
const MISSING_TOOL: &[u8] = b"__run_co_compile missing command to run. Looking for one or more of the following:\n--cppcheck=\n--cpplint=\n--icstat=\n--iwyu=\n--lwyu=\n--pvs-studio=\n--tidy=\n";

pub(super) fn run(args: &[BString], cx: &ToolCtx<'_>) -> i32 {
    let mut args = args.iter();
    let mut analyzers = Vec::new();
    let mut source = b"".as_slice();
    loop {
        match args.next() {
            Some(arg) if arg == b"--" => break,
            Some(arg) => {
                match arg
                    .strip_prefix(b"--")
                    .and_then(|option| split_once(option, b'='))
                {
                    Some((b"source", value)) => source = value,
                    Some((name, value)) if TOOLS.contains(&name) => analyzers.push((name, value)),
                    _ => {
                        return error(
                            cx,
                            &[b"__run_co_compile given unknown argument: ", arg, b"\n"],
                        );
                    }
                }
            }
            None => {
                return error(
                    cx,
                    &[b"__run_co_compile missing -- before compile command\n"],
                );
            }
        }
    }
    let compile = args.as_slice();
    if compile.is_empty() {
        return error(
            cx,
            &[b"__run_co_compile missing compile command after --\n"],
        );
    }
    if analyzers.is_empty() {
        return error(cx, &[MISSING_TOOL]);
    }
    for (name, command) in analyzers {
        let code = analyze(cx, name, command, compile, source);
        if code != 0 {
            return code;
        }
    }
    execute(cx, compile, false)
}

fn analyze(
    cx: &ToolCtx<'_>,
    name: &[u8],
    command: &[u8],
    compile: &[BString],
    source: &[u8],
) -> i32 {
    let mut command = list::split(command, list::Empty::Drop);
    match name {
        b"iwyu" => {
            command.extend(compile.iter().skip(1).cloned());
        }
        b"tidy" => {
            command.push(source.into());
            command.push("--".into());
            command.extend_from_slice(compile);
        }
        b"cppcheck" | b"cpplint" => command.push(source.into()),
        _ => return error(cx, &[b"__run_co_compile does not support --", name, b"\n"]),
    }
    let fixes = (name == b"tidy")
        .then(|| {
            command
                .iter()
                .find_map(|arg| arg.strip_prefix(b"--export-fixes="))
                .map(|file| path(cx, file))
        })
        .flatten();
    let code = execute(cx, &command, name == b"iwyu");
    if let Some(file) = fixes
        && cx
            .host
            .metadata(&file, Follow::Yes)
            .is_ok_and(|metadata| metadata.len == 0)
    {
        let _ = cx.host.remove_file(&file);
    }
    if name == b"cpplint" { 0 } else { code }
}

fn execute(cx: &ToolCtx<'_>, command: &[BString], iwyu: bool) -> i32 {
    let Some((program, args)) = command.split_first() else {
        return error(cx, &[MISSING_TOOL]);
    };
    let output = if iwyu {
        Output::Capture
    } else {
        Output::Inherit
    };
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
        }],
        cwd: cx.cwd.to_path_buf(),
        env: cx.host.env_all(),
        stdin: Input::Inherit,
        stdout: output.clone(),
        stderr: output,
        timeout: Some(Duration::from_secs(30 * 60)),
        tee: false,
    };
    let result = cx.host.run(&request, &mut |_, _| {});
    let failure = match result.as_ref() {
        Ok(output) => match output.results.first() {
            Some(ProcessResult::Exited(_)) if iwyu => {
                if output
                    .stderr
                    .windows(b"should add these lines:".len())
                    .any(|part| part == b"should add these lines:")
                {
                    write(
                        cx,
                        Stream::Stderr,
                        &[
                            b"Warning: include-what-you-use diagnostics:\n",
                            &output.stderr,
                        ],
                    );
                }
                return 0;
            }
            Some(ProcessResult::Exited(code)) => return *code,
            Some(ProcessResult::SpawnFailed(err)) => err.os_message.as_str(),
            Some(ProcessResult::TimedOut) => "Process terminated due to timeout",
            Some(ProcessResult::Signaled(_)) => "Subprocess terminated",
            None => "No subprocess result",
        },
        Err(err) => err.os_message.as_str(),
    };
    error(
        cx,
        &[
            b"Error running '",
            program,
            b"': ",
            failure.as_bytes(),
            b"\n",
        ],
    )
}
