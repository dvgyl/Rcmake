use std::path::PathBuf;

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Stream, path};

use super::{
    Arguments, HandlerResult, Start, absolute, directory, error, handler_process, prior_start,
    process_code, split_command, verbose, with_capture,
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
                    .unwrap_or_else(|| cx.var_or_empty(b"CTEST_BUILD_CONFIGURATION"));
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
                    build
                };
                if let Some(level) = args.value(b"PARALLEL_LEVEL") {
                    command.extend(["--parallel".into(), level.clone()]);
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
                (build, split_command(cx, &custom)?)
            };
            let log = verbose() && !args.flag(b"QUIET");
            if log {
                cx.host().write_out(Stream::Stdout, b"Build project\n");
            }
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
            args.set(cx, b"NUMBER_ERRORS", errors.to_string());
            let warnings = warnings + usize::from(code != 0);
            args.set(cx, b"NUMBER_WARNINGS", warnings.to_string());
            if code != 0 {
                cx.host()
                    .write_out(Stream::Stderr, b"Error(s) when building project\n");
            }
            if log {
                cx.host().write_out(
                    Stream::Stdout,
                    format!("   {errors} Compiler errors\n   {warnings} Compiler warnings\n")
                        .as_bytes(),
                );
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
