//! Run analyzers before the compiler.

mod pvs;

use std::time::Duration;

use rcmake_base::{BString, list};
use rcmake_host::{
    CommandSpec, Follow, Input, Output, ProcessResult, RunOutput, RunRequest, Stream,
};

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
    let mut analyzers = Vec::new();
    let mut launcher = Vec::new();
    let mut source = b"".as_slice();
    let mut object = b"".as_slice();
    let mut args = args.iter();
    for arg in args.by_ref() {
        if arg == b"--" {
            break;
        }
        match arg
            .strip_prefix(b"--")
            .and_then(|option| split_once(option, b'='))
        {
            Some((b"source", value)) => source = value,
            Some((b"object", value)) => object = value,
            Some((b"launcher", value)) => list::split_into(value, list::Empty::Keep, &mut launcher),
            Some((name, value)) if TOOLS.contains(&name) => analyzers.push((name, value)),
            _ => {
                return error(
                    cx,
                    &[b"__run_co_compile given unknown argument: ", arg, b"\n"],
                );
            }
        }
    }
    if analyzers.is_empty() {
        return error(cx, &[MISSING_TOOL]);
    }

    let compile = args.as_slice();
    let run_compiler = analyzers.iter().all(|(name, _)| *name != b"lwyu");
    if run_compiler && compile.is_empty() {
        return error(
            cx,
            &[b"__run_co_compile missing compile command after --\n"],
        );
    }
    for (name, command) in analyzers {
        let command = list::split(command, list::Empty::Keep);
        let code = if name == b"pvs-studio" {
            pvs::analyze(cx, command, compile, source, object)
        } else {
            analyze(cx, name, command, compile, source)
        };
        if code != 0 {
            return code;
        }
    }
    if !run_compiler {
        return 0;
    }
    launcher.extend_from_slice(compile);
    execute(cx, &launcher, Output::Inherit, Output::Inherit)
        .map_or_else(|code| code, |(code, _)| code)
}

fn analyze(
    cx: &ToolCtx<'_>,
    name: &[u8],
    mut command: Vec<BString>,
    compile: &[BString],
    source: &[u8],
) -> i32 {
    if command.first().is_none_or(|program| program.is_empty()) {
        return error(cx, &[MISSING_TOOL]);
    }
    let mut stdout = Output::Capture;
    let mut stderr = Output::Capture;
    match name {
        b"iwyu" => {
            command.extend(compile.iter().skip(1).cloned());
            stdout = Output::Null;
        }
        b"tidy" => {
            for file in command
                .iter()
                .filter_map(|arg| arg.strip_prefix(b"--export-fixes="))
            {
                let file = path(cx, file);
                let _ = cx.host.remove_file(&file);
                if let Some(parent) = file.parent() {
                    let _ = cx.host.create_dir_all(parent);
                }
            }
            let use_database = command.iter().any(|arg| arg == b"-p");
            command.push(source.into());
            if !use_database {
                command.push("--".into());
                command.extend_from_slice(compile);
            }
        }
        b"cpplint" => {
            command.push(source.into());
            stderr = Output::SharedWithStdout;
        }
        b"cppcheck" => {
            command.extend(compile.iter().filter_map(cppcheck_option));
            command.push(source.into());
        }
        b"icstat" => return icstat(cx, command, compile, source),
        b"lwyu" => command.push(source.into()),
        _ => return error(cx, &[b"__run_co_compile does not support --", name, b"\n"]),
    }
    let (code, out) = match execute(cx, &command, stdout, stderr) {
        Ok(result) => result,
        Err(code) => return code,
    };
    match name {
        b"iwyu" => {
            if [
                b"should add these lines:".as_slice(),
                b"should remove these lines:",
            ]
            .iter()
            .any(|marker| contains(&out.stderr, marker))
            {
                write(
                    cx,
                    Stream::Stderr,
                    &[
                        b"Warning: include-what-you-use reported diagnostics:\n",
                        &out.stderr,
                        b"\n",
                    ],
                );
            }
            let errors_enabled = command
                .iter()
                .position(|arg| arg.starts_with(b"--error"))
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| command.get(index))
                .is_some_and(|arg| arg == b"-Xiwyu");
            if errors_enabled { code } else { 0 }
        }
        b"tidy" => {
            write(cx, Stream::Stderr, &[&out.stdout]);
            if code != 0 {
                write(cx, Stream::Stderr, &[&out.stderr]);
            }
            code
        }
        b"cpplint" => {
            if !out.stdout.is_empty() {
                write(
                    cx,
                    Stream::Stderr,
                    &[b"Warning: cpplint diagnostics:\n", &out.stdout],
                );
            }
            0
        }
        b"cppcheck" => {
            write(cx, Stream::Stderr, &[&out.stdout]);
            if [
                b"(error)".as_slice(),
                b"(warning)",
                b"(style)",
                b"(performance)",
                b"(portability)",
                b"(information)",
            ]
            .iter()
            .any(|marker| contains(&out.stderr, marker))
            {
                write(
                    cx,
                    Stream::Stderr,
                    &[if code == 0 {
                        b"Warning: cppcheck reported diagnostics:\n"
                    } else {
                        b"Error: cppcheck reported failure:\n"
                    }],
                );
            }
            write(cx, Stream::Stderr, &[&out.stderr]);
            code
        }
        b"lwyu" => {
            if contains(&out.stdout, b"Unused direct dependencies:") {
                write(cx, Stream::Stderr, &[b"Warning: ", &out.stdout]);
            }
            0
        }
        _ => code,
    }
}

fn icstat(cx: &ToolCtx<'_>, mut command: Vec<BString>, compile: &[BString], source: &[u8]) -> i32 {
    if cx
        .host
        .metadata(&cx.cwd.join("cstat_sel_checks.txt"), Follow::Yes)
        .is_err()
    {
        let compiler = compile.first().map_or(b"".as_slice(), |arg| arg.as_slice());
        let directory = rcmake_base::path_lex::Lexical::new(compiler).parent();
        let program = if directory.is_empty() {
            "ichecks".into()
        } else {
            [directory, b"/ichecks"].concat().into()
        };
        if let Err(code) = execute(
            cx,
            &[program, "--default=stdchecks".into()],
            Output::Capture,
            Output::Capture,
        ) {
            return code;
        }
    }
    command.push("--".into());
    command.extend_from_slice(compile);
    let (code, output) = match execute(cx, &command, Output::Capture, Output::Capture) {
        Ok(result) => result,
        Err(code) => return code,
    };
    if code == 0 {
        write(cx, Stream::Stderr, &[b"C-STAT: "]);
        if !contains(&output.stdout, b"Analyzing") {
            let name = source
                .rsplit(|&byte| matches!(byte, b'/' | b'\\'))
                .next()
                .unwrap_or(source);
            write(cx, Stream::Stderr, &[b"Analyzing ", name, b" (cached)\n"]);
        }
        let mut cleaned = Vec::with_capacity(output.stdout.len());
        for (index, line) in output
            .stdout
            .split_inclusive(|&byte| byte == b'\n')
            .enumerate()
        {
            if index != 0
                && let Some(mut tail) = line.strip_prefix(b"\"")
            {
                cleaned.push(b'"');
                while let Some(rest) = tail
                    .strip_prefix(b"../")
                    .or_else(|| tail.strip_prefix(b"..\\"))
                {
                    tail = rest;
                }
                cleaned.extend_from_slice(tail);
            } else {
                cleaned.extend_from_slice(line);
            }
        }
        write(cx, Stream::Stderr, &[&cleaned, &output.stderr]);
    } else {
        write(
            cx,
            Stream::Stderr,
            &[b"C-STAT ", &output.stdout, &output.stderr],
        );
    }
    code
}

fn cppcheck_option(arg: &BString) -> Option<BString> {
    let (prefix, tail) = arg.split_first()?;
    if !matches!(tail, [b'D' | b'I' | b'U', _, ..]) {
        return None;
    }
    if *prefix == b'-' {
        Some(arg.clone())
    } else if cfg!(windows) && *prefix == b'/' {
        Some([b"-", tail].concat().into())
    } else {
        None
    }
}

fn contains(text: &[u8], marker: &[u8]) -> bool {
    text.windows(marker.len()).any(|part| part == marker)
}

fn execute(
    cx: &ToolCtx<'_>,
    command: &[BString],
    stdout: Output,
    stderr: Output,
) -> Result<(i32, RunOutput), i32> {
    let Some((program, args)) = command.split_first() else {
        return Err(error(cx, &[MISSING_TOOL]));
    };
    let request = RunRequest {
        pipeline: vec![CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
        }],
        cwd: cx.cwd.to_path_buf(),
        env: cx.host.env_all(),
        stdin: Input::Inherit,
        stdout,
        stderr,
        timeout: Some(Duration::from_secs(30 * 60)),
        tee: false,
    };
    let failure = match cx.host.run_bounded(&request, 64 * 1024 * 1024) {
        Ok(output) => match output.results.first() {
            Some(ProcessResult::Exited(code)) => return Ok((*code, output)),
            Some(ProcessResult::SpawnFailed(err)) => err.os_message.clone(),
            Some(ProcessResult::TimedOut) => "Process terminated due to timeout".into(),
            Some(ProcessResult::Signaled(_)) => "Subprocess terminated".into(),
            None => "No subprocess result".into(),
        },
        Err(err) => err.os_message,
    };
    Err(error(
        cx,
        &[
            b"Error running '",
            program,
            b"': ",
            failure.as_bytes(),
            b"\n",
        ],
    ))
}
