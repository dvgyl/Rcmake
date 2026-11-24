use std::{fmt::Write, path::Path, time::Duration};

use rcmake_base::{BString, Limits};
use rcmake_host::{
    CommandSpec, FileTime, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest,
    Stream, WriteMode, path,
};

use crate::{dashboard::instrumentation_xml, junit::escape};

pub(crate) fn run(host: &dyn Host, args: &[BString]) -> i32 {
    let Some((options, [_, command @ ..])) = args
        .iter()
        .position(|arg| arg == "--")
        .and_then(|index| args.split_at_checked(index))
    else {
        host.write_out(
            Stream::Stderr,
            b"No launch/command separator ('--') found!\n",
        );
        return 1;
    };
    let Some((program, arguments)) = command.split_first() else {
        return 0;
    };
    if program == ":" {
        return 0;
    }
    match run_command(host, options, program, arguments) {
        Ok(code) => code,
        Err(error) => {
            host.write_out(Stream::Stderr, format!("{error}\n").as_bytes());
            1
        }
    }
}

fn run_command(
    host: &dyn Host,
    args: &[BString],
    program: &BString,
    arguments: &[BString],
) -> Result<i32, String> {
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.clone(),
                    args: arguments.to_vec(),
                }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Inherit,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30 * 60)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .map_err(|error| error.to_string())?;
    host.write_out(Stream::Stdout, &output.stdout);
    host.write_out(Stream::Stderr, &output.stderr);
    let code = match output.results.as_slice() {
        [ProcessResult::Exited(code)] => *code,
        [ProcessResult::Signaled(signal)] => 128_i32.saturating_add(*signal),
        [ProcessResult::SpawnFailed(error)] => return Err(error.to_string()),
        _ => 1,
    };
    let warning = output
        .stderr
        .split(|&byte| byte == b'\n')
        .chain(output.stdout.split(|&byte| byte == b'\n'))
        .any(|line| {
            line.windows(8)
                .any(|part| part.eq_ignore_ascii_case(b"warning:"))
        });
    if (code != 0 || warning)
        && let Some(build) = option(args, b"--build-dir")
        && let Some(target) = option(args, b"--target-name")
    {
        let build = host.initial_cwd().join(path::to_native(build));
        write_failure(host, &build, target, code, &output)?;
    }
    Ok(code)
}

fn option<'a>(args: &'a [BString], key: &[u8]) -> Option<&'a BString> {
    args.windows(2)
        .find_map(|pair| match pair {
            [name, value] if name == key => Some(value),
            _ => None,
        })
        .filter(|value| !value.is_empty() && !value.starts_with(b"--"))
}

fn write_failure(
    host: &dyn Host,
    build: &Path,
    target: &[u8],
    code: i32,
    output: &rcmake_host::RunOutput,
) -> Result<(), String> {
    let dir = build.join("Testing/Temporary/Launch");
    host.create_dir_all(&dir)
        .map_err(|error| error.to_string())?;
    let now = host.now();
    let file = dir.join(format!(
        "{}-{}-{}.xml",
        now.secs,
        now.nanos,
        host.system().pid
    ));
    let kind = if code == 0 { "Warning" } else { "Error" };
    let mut xml = format!("<Failure type=\"{kind}\">\n");
    let name = escape(target, false);
    let stdout = escape(&output.stdout, false);
    let stderr = escape(&output.stderr, false);
    let _ = writeln!(
        xml,
        "<TargetName>{name}</TargetName>\n<Stdout>{stdout}</Stdout>\n<Stderr>{stderr}</Stderr>"
    );
    let labels = instrumentation_xml::target_labels(host, build, target);
    if !labels.is_empty() {
        xml.push_str("<Labels>\n");
        for label in labels {
            let _ = writeln!(xml, "<Label>{}</Label>", escape(&label, false));
        }
        xml.push_str("</Labels>\n");
    }
    xml.push_str("</Failure>\n");
    host.write(&file, xml.as_bytes(), WriteMode::CreateNew)
        .map_err(|error| error.to_string())
}

pub(crate) fn diagnostics(host: &dyn Host, build: &Path, since: FileTime) -> String {
    let dir = build.join("Testing/Temporary/Launch");
    let mut entries = host.read_dir(&dir).unwrap_or_default();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut xml = String::new();
    for entry in entries
        .into_iter()
        .filter(|entry| entry.name.ends_with(b".xml"))
    {
        let file = dir.join(path::to_native(&entry.name));
        if host
            .metadata(&file, Follow::No)
            .is_ok_and(|info| info.mtime >= since)
            && let Ok(data) = host.read(
                &file,
                ReadRange::All {
                    limit: Limits::default().max_file_read,
                },
            )
        {
            xml.push_str(&String::from_utf8_lossy(&data));
        }
    }
    xml
}
