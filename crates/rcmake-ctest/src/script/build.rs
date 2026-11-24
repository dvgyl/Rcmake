use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, ProcessResult, RunOutput, Stream, path};

use crate::dart::Tag;
use crate::junit::{escape, write_file};

use super::{
    Arguments, HandlerResult, Start, absolute, configuration, directory, error, handler_process,
    prior_start, process_code, report_size, split_command, verbose, with_capture,
};

pub(super) fn ctest_build(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    with_capture(
        cx,
        args,
        &[
            b"TARGET",
            b"CONFIGURATION",
            b"FLAGS",
            b"NUMBER_ERRORS",
            b"NUMBER_WARNINGS",
            b"PARALLEL_LEVEL",
            b"PROJECT_NAME",
            b"PRESET",
            b"PRESETS_FILE",
        ],
        &[b"APPEND"],
        |cx, args| {
            let previous = prior_start();
            let build = directory(
                cx,
                args.value(b"BUILD"),
                b"CTEST_BINARY_DIRECTORY",
                previous.as_ref().map(|start| start.build.as_path()),
            )?;
            let preset = ctest_build_preset(cx, args, previous.as_ref())?;
            let custom = cx.var_or_empty(b"CTEST_BUILD_COMMAND");
            let (cwd, command) = if custom.is_empty() || preset.is_some() {
                let config = args
                    .value(b"CONFIGURATION")
                    .cloned()
                    .or_else(|| setting(cx, b"CTEST_BUILD_CONFIGURATION"))
                    .or_else(|| setting(cx, b"CTEST_CONFIGURATION_TYPE"))
                    .unwrap_or_default();
                let mut command = vec![cx.var_or_empty(b"CMAKE_COMMAND"), "--build".into()];
                let cwd = if let Some((source, name, file)) = preset {
                    command.extend(["--preset".into(), name]);
                    if let Some(file) = file {
                        command.extend(["--presets-file".into(), path::from_native(&file)]);
                    }
                    if !config.is_empty() {
                        command.extend(["--config".into(), config]);
                    }
                    source
                } else {
                    let config = if config.is_empty() {
                        "Release".into()
                    } else {
                        config
                    };
                    command.extend([".".into(), "--config".into(), config]);
                    build.clone()
                };
                if args.flag(b"PARALLEL_LEVEL") {
                    command.push("--parallel".into());
                    command.extend(args.value(b"PARALLEL_LEVEL").cloned());
                }
                let target = args
                    .value(b"TARGET")
                    .cloned()
                    .unwrap_or_else(|| cx.var_or_empty(b"CTEST_BUILD_TARGET"));
                if !target.is_empty() {
                    command.extend(["--target".into(), target]);
                }
                let flags = args
                    .value(b"FLAGS")
                    .cloned()
                    .unwrap_or_else(|| cx.var_or_empty(b"CTEST_BUILD_FLAGS"));
                if !flags.is_empty() {
                    command.push("--".into());
                    command.extend(split_command(cx, &flags)?);
                }
                (cwd, command)
            } else {
                (build.clone(), split_command(cx, &custom)?)
            };
            let log = verbose() && !args.flag(b"QUIET");
            if log {
                cx.host().write_out(Stream::Stdout, b"Build project\n");
            }
            let command_line = render_command(&command);
            let start = cx.host().now().secs;
            let output = handler_process(cx, args, cwd, command)?;
            let mut warnings = 0;
            let mut errors = 0;
            for line in output
                .stdout
                .split(|&byte| byte == b'\n')
                .chain(output.stderr.split(|&byte| byte == b'\n'))
            {
                let text = String::from_utf8_lossy(line).to_ascii_lowercase();
                errors += usize::from(text.contains("error:") || text.contains(" error "));
                warnings += usize::from(text.contains("warning:") || text.contains(" warning "));
            }
            let code = process_code(&output).unwrap_or(-1);
            let abnormal = matches!(output.results.last(), Some(ProcessResult::Signaled(_)))
                || cfg!(unix) && (129..=192).contains(&code);
            if abnormal && errors == 0 {
                errors = 1;
            }
            args.set(cx, b"NUMBER_ERRORS", errors.to_string());
            let warnings = warnings + usize::from(code != 0);
            args.set(cx, b"NUMBER_WARNINGS", warnings.to_string());
            if code != 0 {
                cx.host()
                    .write_out(Stream::Stderr, b"Error(s) when building project\n");
            }
            if log {
                report_size(cx.host(), &output);
            }
            if verbose() {
                cx.host().write_out(
                    Stream::Stdout,
                    format!("   {errors} Compiler errors\n   {warnings} Compiler warnings\n")
                        .as_bytes(),
                );
            }
            let source = previous.as_ref().map(|start| start.source.as_path());
            let config = configuration(cx, &build, source)?;
            if let Ok(tag) = Tag::read(cx.host(), &build) {
                let source = source.map_or_else(
                    || {
                        absolute(
                            &cx.host().initial_cwd(),
                            &cx.var_or_empty(b"CTEST_SOURCE_DIRECTORY"),
                        )
                    },
                    Path::to_path_buf,
                );
                write_build_report(
                    cx.host(),
                    &build,
                    &tag,
                    BuildReport {
                        source: &source,
                        config: &config,
                        command: &command_line,
                        output: &output,
                        start,
                        code,
                        errors,
                        warnings,
                        abnormal,
                    },
                )
                .map_err(error)?;
            }
            Ok(HandlerResult {
                code,
                capture_error: code != 0 || errors > 0,
            })
        },
    )
}

fn ctest_build_preset(
    cx: &mut Cx<'_>,
    args: &Arguments<'_>,
    start: Option<&Start>,
) -> Result<Option<(PathBuf, BString, Option<PathBuf>)>, CmdError> {
    let specific = args.value(b"PRESET").cloned().or_else(|| {
        cx.var(b"CTEST_BUILD_PRESET")
            .filter(|name| !name.is_empty())
    });
    let Some(name) = specific
        .clone()
        .or_else(|| cx.var(b"CTEST_PRESET").filter(|name| !name.is_empty()))
    else {
        return Ok(None);
    };
    let cwd = cx.host().initial_cwd();
    let source = start.map_or_else(
        || absolute(&cwd, &cx.var_or_empty(b"CTEST_SOURCE_DIRECTORY")),
        |start| start.source.clone(),
    );
    let file = args
        .value(b"PRESETS_FILE")
        .cloned()
        .or_else(|| cx.var(b"CTEST_PRESETS_FILE"))
        .filter(|file| !file.is_empty())
        .map(|file| absolute(&source, &file));
    let host = cx.host();
    let presets = match &file {
        Some(file) => rcmake_presets::load_file(&source, file, host),
        None => rcmake_presets::load(&source, host),
    };
    let resolved = presets.and_then(|presets| {
        presets.resolve(
            rcmake_presets::PresetKind::Build,
            &name,
            &rcmake_presets::EnvSnapshot::from_host(host),
        )
    });
    match resolved {
        Ok(_) => Ok(Some((source, name, file))),
        Err(failure) if specific.is_some() => Err(error(failure.message)),
        Err(_) => {
            host.write_out(
                Stream::Stderr,
                format!("No build preset named \"{name}\" found, ignoring CTEST_PRESET.\n")
                    .as_bytes(),
            );
            Ok(None)
        }
    }
}

fn setting(cx: &Cx<'_>, name: &[u8]) -> Option<BString> {
    let value = cx.var_or_empty(name);
    (!value.is_empty()).then_some(value)
}

fn render_command(command: &[BString]) -> String {
    use std::fmt::Write as _;
    let mut line = String::new();
    for arg in command {
        if !line.is_empty() {
            line.push(' ');
        }
        if arg.starts_with(b"--") {
            let _ = write!(line, "{arg}");
        } else {
            let _ = write!(line, "\"{arg}\"");
        }
    }
    line
}

struct BuildReport<'a> {
    source: &'a Path,
    config: &'a crate::dart::Configuration,
    command: &'a str,
    output: &'a RunOutput,
    start: i64,
    code: i32,
    errors: usize,
    warnings: usize,
    abnormal: bool,
}

fn write_build_report(
    host: &dyn Host,
    dir: &Path,
    tag: &Tag,
    report: BuildReport<'_>,
) -> Result<(), String> {
    use std::fmt::Write as _;
    let file = dir.join("Testing").join(tag.stamp()).join("Build.xml");
    let name = escape(&report.config.build_name, true);
    let site = escape(&report.config.site, true);
    let stamp = escape(format!("{}-{}", tag.stamp(), tag.track).as_bytes(), true);
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site BuildName=\"{name}\" BuildStamp=\"{stamp}\" Name=\"{site}\""
    );
    if !report.config.change_id.is_empty() {
        let id = escape(&report.config.change_id, true);
        let _ = write!(xml, " ChangeId=\"{id}\"");
    }
    let command = escape(report.command.as_bytes(), false);
    let stdout = escape(&report.output.stdout, false);
    let stderr = escape(&report.output.stderr, false);
    let source = escape(&path::from_native(report.source), false);
    let binary = escape(&path::from_native(dir), false);
    let _ = write!(
        xml,
        ">\n<Build>\n<StartBuildTime>{}</StartBuildTime>\n<BuildCommand>{command}</BuildCommand>\n<SourceDirectory>{source}</SourceDirectory>\n<BinaryDirectory>{binary}</BinaryDirectory>\n<Log>{stdout}{stderr}</Log>\n",
        report.start
    );
    for line in [
        report.output.stdout.as_slice(),
        report.output.stderr.as_slice(),
    ]
    .into_iter()
    .flat_map(|output| output.split(|&byte| byte == b'\n'))
    {
        let lower = line.to_ascii_lowercase();
        let diagnostic = if lower.windows(6).any(|part| part == b"error:") {
            "Error"
        } else if lower.windows(8).any(|part| part == b"warning:") {
            "Warning"
        } else {
            continue;
        };
        let text = escape(line, false);
        let _ = writeln!(xml, "<{diagnostic}><Text>{text}</Text></{diagnostic}>");
    }
    if report.abnormal {
        xml.push_str("<Failure type=\"Error\"><Result>Terminated abnormally</Result></Failure>\n");
    }
    let _ = writeln!(
        xml,
        "<BuildStatus>{}</BuildStatus>\n<ErrorCount>{}</ErrorCount>\n<WarningCount>{}</WarningCount>\n<EndBuildTime>{}</EndBuildTime>\n</Build></Site>",
        report.code,
        report.errors,
        report.warnings,
        host.now().secs
    );
    write_file(host, &file, xml.as_bytes()).map_err(|failure| failure.to_string())
}
