use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, ProcessResult, Stream, path};
use rcmake_presets::{EnvSnapshot, PresetKind};

use super::{Dashboard, definition, presets_file};
use crate::cli::Invocation;
use crate::script::build::{
    BuildReport, custom_command, diagnostics, render_command, write_build_report,
};
use crate::script::split_dashboard_command;

pub(super) fn run(
    host: &Arc<dyn Host>,
    invocation: &Invocation,
    dashboard: &Dashboard,
) -> Result<i32, String> {
    let Dashboard {
        source,
        build,
        config,
        tag,
        ..
    } = dashboard;
    let setting = |name| definition(invocation, name).filter(|value| !value.is_empty());
    let specific = setting(b"CTEST_BUILD_PRESET");
    let file = presets_file(invocation, source);
    let mut preset = specific.or_else(|| setting(b"CTEST_PRESET"));
    if let Some(name) = preset {
        let set = match &file {
            Some(file) => rcmake_presets::load_file(source, file, host.as_ref()),
            None => rcmake_presets::load(source, host.as_ref()),
        };
        if let Err(error) = set.and_then(|set| {
            set.resolve(
                PresetKind::Build,
                name,
                &EnvSnapshot::from_host(host.as_ref()),
            )
        }) {
            if specific.is_some() {
                return Err(error.message);
            }
            let name = BString::from(name);
            host.write_out(
                Stream::Stderr,
                format!("No build preset named \"{name}\" found, ignoring CTEST_PRESET.\n")
                    .as_bytes(),
            );
            preset = None;
        }
    }
    let custom = if preset.is_some() {
        BString::default()
    } else if let Some(command) = setting(b"CTEST_BUILD_COMMAND") {
        command.into()
    } else {
        dashboard.value(host.as_ref(), invocation, b"MakeCommand")?
    };
    let cwd = build;
    let command = if let Some(custom) = custom_command(preset.is_some(), &custom) {
        split_dashboard_command(host, custom)?
    } else {
        let mut command = vec![
            path::from_native(&crate::script::cmake_executable(host.as_ref())),
            "--build".into(),
        ];
        if let Some(preset) = preset {
            command.extend([".".into(), "--preset".into(), preset.into()]);
            if let Some(file) = file {
                command.extend(["--presets-file".into(), path::from_native(&file)]);
            }
        } else {
            command.push(path::from_native(build));
        }
        if let Some(config) = setting(b"CTEST_BUILD_CONFIGURATION")
            .or_else(|| setting(b"CTEST_CONFIGURATION_TYPE"))
            .or(invocation
                .options
                .config
                .as_ref()
                .map(|value| value.as_slice()))
        {
            command.extend(["--config".into(), config.into()]);
        }
        if let Some(target) = setting(b"CTEST_BUILD_TARGET") {
            command.extend(["--target".into(), target.into()]);
        }
        if let Some(flags) = setting(b"CTEST_BUILD_FLAGS") {
            command.push("--".into());
            command.extend(split_dashboard_command(host, flags)?);
        }
        command
    };
    let (program, args) = command
        .split_first()
        .filter(|(program, _)| !program.is_empty())
        .ok_or_else(|| "Build command is empty.".to_owned())?;
    let command_line = render_command(&command);
    if !invocation.options.quiet {
        host.write_out(Stream::Stdout, b"Build project\n");
    }
    let launchers = setting(b"CTEST_USE_LAUNCHERS").is_some_and(rcmake_base::truth::is_on);
    let mut env = host.env_all();
    if launchers {
        env.retain(|(key, _)| !key.eq_ignore_ascii_case(b"CTEST_LAUNCH_LOGS"));
        env.push((
            "CTEST_LAUNCH_LOGS".into(),
            path::from_native(&build.join("Testing/Temporary/Launch")),
        ));
    }
    let start = host.now();
    let output = crate::script::run_process(
        host.as_ref(),
        cwd.clone(),
        program.clone(),
        args.to_vec(),
        env,
        false,
    )
    .map_err(|error| error.to_string())?;
    let code = match output.results.last() {
        Some(ProcessResult::Exited(code)) => *code,
        _ => -1,
    };
    let launch = launchers.then(|| crate::launch::diagnostics(host.as_ref(), build, start));
    let (errors, warnings, abnormal) = diagnostics(&output, code, launch.as_deref());
    write_build_report(
        host.as_ref(),
        build,
        tag,
        BuildReport {
            source,
            config,
            command: &command_line,
            cwd,
            arguments: &command,
            output: &output,
            start,
            code,
            errors,
            warnings,
            abnormal,
            launch: launch.as_deref(),
        },
    )?;
    if !invocation.options.quiet {
        host.write_out(
            Stream::Stdout,
            format!("   {errors} Compiler errors\n   {warnings} Compiler warnings\n").as_bytes(),
        );
    }
    if code != 0 || errors != 0 {
        host.write_out(Stream::Stderr, b"Error(s) when building project\n");
    }
    // CTest sets the dashboard error bit when the build handler returns a negative result.
    Ok(match output.results.last() {
        Some(ProcessResult::Exited(code)) => (*code).min(0),
        _ => 0,
    })
}
