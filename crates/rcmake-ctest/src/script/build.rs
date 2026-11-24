use std::path::Path;

use rcmake_base::BString;
use rcmake_eval::{CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Host, ProcessResult, RunOutput, Stream, path};

use crate::dart::Tag;
use crate::junit::{escape, write_file};

use super::{
    HandlerResult, absolute, configuration, directory, error, handler_process, prior_start,
    process_code, report_size, split_command, verbose, with_capture,
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
            let source = previous.as_ref().map_or_else(
                || {
                    absolute(
                        &cx.host().initial_cwd(),
                        &cx.var_or_empty(b"CTEST_SOURCE_DIRECTORY"),
                    )
                },
                |start| start.source.clone(),
            );
            let preset =
                super::preset::select(cx, args, &source, rcmake_presets::PresetKind::Build)?;
            let custom = cx.var_or_empty(b"CTEST_BUILD_COMMAND");
            let (cwd, command) = if let Some(custom) = custom_command(preset.is_some(), &custom) {
                (build.clone(), split_command(cx, custom)?)
            } else {
                let config = args
                    .value(b"CONFIGURATION")
                    .cloned()
                    .or_else(|| setting(cx, b"CTEST_BUILD_CONFIGURATION"))
                    .or_else(|| setting(cx, b"CTEST_CONFIGURATION_TYPE"))
                    .unwrap_or_default();
                let mut command = vec![cx.var_or_empty(b"CMAKE_COMMAND"), "--build".into()];
                let cwd = if let Some(preset) = preset {
                    preset.append_to(&mut command);
                    if !config.is_empty() {
                        command.extend(["--config".into(), config]);
                    }
                    source.clone()
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
            };
            let log = verbose() && !args.flag(b"QUIET");
            if log {
                cx.host().write_out(Stream::Stdout, b"Build project\n");
            }
            let command_line = render_command(&command);
            let start = cx.host().now();
            let output = handler_process(cx, args, cwd, &build, command)?;
            let code = process_code(&output).unwrap_or(-1);
            let (errors, warnings, abnormal) = diagnostics(&output, code);
            args.set(cx, b"NUMBER_ERRORS", errors.to_string());
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
            let config = configuration(cx, &build, Some(&source))?;
            let tag = previous
                .as_ref()
                .map(|start| start.tag.clone())
                .or_else(|| Tag::read(cx.host(), &build).ok());
            if let Some(tag) = tag {
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

pub(crate) fn custom_command(preset: bool, command: &[u8]) -> Option<&[u8]> {
    (!preset && !command.is_empty()).then_some(command)
}

pub(crate) fn diagnostics(output: &RunOutput, code: i32) -> (usize, usize, bool) {
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
    let abnormal = matches!(output.results.last(), Some(ProcessResult::Signaled(_)))
        || cfg!(unix) && (129..=192).contains(&code);
    if abnormal && errors == 0 {
        errors = 1;
    }
    (errors, warnings + usize::from(code != 0), abnormal)
}

fn setting(cx: &Cx<'_>, name: &[u8]) -> Option<BString> {
    let value = cx.var_or_empty(name);
    (!value.is_empty()).then_some(value)
}

pub(crate) fn render_command(command: &[BString]) -> String {
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

pub(crate) struct BuildReport<'a> {
    pub(crate) source: &'a Path,
    pub(crate) config: &'a crate::dart::Configuration,
    pub(crate) command: &'a str,
    pub(crate) output: &'a RunOutput,
    pub(crate) start: rcmake_host::FileTime,
    pub(crate) code: i32,
    pub(crate) errors: usize,
    pub(crate) warnings: usize,
    pub(crate) abnormal: bool,
}

pub(crate) fn write_build_report(
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
    xml.push_str(">\n");
    crate::dart::write_subprojects(&mut xml, &report.config.labels_for_subprojects);
    let command = escape(report.command.as_bytes(), false);
    let stdout = escape(&report.output.stdout, false);
    let stderr = escape(&report.output.stderr, false);
    let source = escape(&path::from_native(report.source), false);
    let binary = escape(&path::from_native(dir), false);
    let _ = write!(
        xml,
        "<Build>\n<StartBuildTime>{}</StartBuildTime>\n<BuildCommand>{command}</BuildCommand>\n<SourceDirectory>{source}</SourceDirectory>\n<BinaryDirectory>{binary}</BinaryDirectory>\n<Log>{stdout}{stderr}</Log>\n",
        report.start.secs
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
    if let Some(instrumentation) =
        crate::dashboard::instrumentation_xml::Instrumentation::load(host, dir)
    {
        xml.push_str(&instrumentation.build(host, dir, report.start.secs));
    }
    xml.push_str(&crate::launch::diagnostics(host, dir, report.start));
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
