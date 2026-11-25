use std::{fmt::Write, path::Path, time::Duration};

use rcmake_base::{BString, Limits, hash::HashAlgo};
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
    let cwd = host.initial_cwd();
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.clone(),
                    args: arguments.to_vec(),
                }],
                cwd: cwd.clone(),
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
    let (code, exit) = process_exit(&output.results);
    if let Some(logs) = host
        .env_get(b"CTEST_LAUNCH_LOGS")
        .filter(|dir| !dir.is_empty())
    {
        let stdout = diagnostic_output(&output.stdout);
        let stderr = diagnostic_output(&output.stderr);
        let warning = stdout.1 || stderr.1;
        if code != 0 || warning {
            let kind = if code == 0 { "Warning" } else { "Error" };
            let mut xml = format!("<Failure type=\"{kind}\">\n<Action>\n");
            for (key, tag) in [
                (b"--target-name".as_slice(), "TargetName"),
                (b"--language", "Language"),
                (b"--source", "SourceFile"),
                (b"--output", "OutputFile"),
            ] {
                if let Some(value) = option(args, key) {
                    element(&mut xml, tag, value);
                    if tag == "OutputFile" && option(args, b"--language").is_some() {
                        element(&mut xml, "OutputType", b"object file");
                    }
                }
            }
            xml.push_str("</Action>\n");
            let canonical = host.canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
            let cwd_value = path::from_native_cmake(&canonical);
            let command = || std::iter::once(program).chain(arguments);
            write_command(&mut xml, &cwd_value, command());
            write_result(&mut xml, &stdout.0, &stderr.0, &exit);
            let mut identity = cwd_value;
            for arg in command() {
                identity.extend_from_slice(arg);
            }
            let build = option(args, b"--build-dir").map(|dir| cwd.join(path::to_native(dir)));
            if let Some((build, target)) = build.as_deref().zip(option(args, b"--target-name")) {
                let labels = instrumentation_xml::target_labels(host, build, target);
                if !labels.is_empty() {
                    xml.push_str("<Labels>\n");
                    for label in labels {
                        element(&mut xml, "Label", &label);
                    }
                    xml.push_str("</Labels>\n");
                }
            }
            xml.push_str("</Failure>\n");
            let logs = cwd.join(path::to_native(&logs));
            host.create_dir_all(&logs)
                .map_err(|error| error.to_string())?;
            let digest = HashAlgo::Md5.hex_digest(&identity);
            let file = logs.join(format!("{}-{digest}.xml", kind.to_ascii_lowercase()));
            host.write(&file, xml.as_bytes(), WriteMode::Truncate)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(code)
}

pub(crate) fn build_failure(
    xml: &mut String,
    cwd: &Path,
    command: &[BString],
    output: &rcmake_host::RunOutput,
) {
    let (_, exit) = process_exit(&output.results);
    let (stdout, _) = diagnostic_output(&output.stdout);
    let (stderr, _) = diagnostic_output(&output.stderr);
    xml.push_str("<Failure type=\"Error\">\n<Action/>\n");
    write_command(xml, &path::from_native_cmake(cwd), command);
    write_result(xml, &stdout, &stderr, &exit);
    xml.push_str("</Failure>\n");
}

fn process_exit(results: &[ProcessResult]) -> (i32, String) {
    match results {
        [ProcessResult::Exited(code)] => (*code, code.to_string()),
        [ProcessResult::Signaled(signal)] => {
            let reason = match signal {
                2 => "User interrupt",
                9 => "Subprocess killed",
                15 => "Subprocess terminated",
                _ => "Subprocess received a signal",
            };
            (1, format!("Terminated abnormally: {reason}"))
        }
        [ProcessResult::SpawnFailed(error)] => (
            1,
            format!(
                "Error administrating child process: {}",
                error.os_message.to_ascii_lowercase()
            ),
        ),
        _ => (1, "Child process timeout".into()),
    }
}

fn write_command<'a>(xml: &mut String, cwd: &[u8], args: impl IntoIterator<Item = &'a BString>) {
    xml.push_str("<Command>\n");
    element(xml, "WorkingDirectory", cwd);
    for arg in args {
        element(xml, "Argument", arg);
    }
    xml.push_str("</Command>\n");
}

fn write_result(xml: &mut String, stdout: &[u8], stderr: &[u8], exit: &str) {
    xml.push_str("<Result>\n");
    element(xml, "StdOut", stdout);
    element(xml, "StdErr", stderr);
    element(xml, "ExitCondition", exit.as_bytes());
    xml.push_str("</Result>\n");
}

fn diagnostic_output(bytes: &[u8]) -> (BString, bool) {
    let mut output = BString::default();
    let mut warning = false;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    for (index, line) in bytes.split(|&byte| byte == b'\n').enumerate() {
        if index != 0 {
            output.push(b'\n');
        }
        if line
            .windows(8)
            .any(|part| part.eq_ignore_ascii_case(b"warning:"))
        {
            warning = true;
            output.extend_from_slice(b"[CTest: warning matched] ");
        }
        output.extend_from_slice(line);
    }
    (output, warning)
}

fn element(xml: &mut String, tag: &str, value: &[u8]) {
    if value.is_empty() {
        let _ = writeln!(xml, "<{tag}/>");
    } else {
        let _ = writeln!(xml, "<{tag}>{}</{tag}>", escape(value, false));
    }
}

fn option<'a>(args: &'a [BString], key: &[u8]) -> Option<&'a BString> {
    args.windows(2)
        .find_map(|pair| match pair {
            [name, value] if name == key => Some(value),
            _ => None,
        })
        .filter(|value| !value.is_empty() && !value.starts_with(b"--"))
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
