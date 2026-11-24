use rcmake_base::BString;
use rcmake_host::{Host, ProcessResult, Stream, path};

use super::definition;
use crate::cli::Invocation;
use crate::dart::{Configuration, Model, Tag};
use crate::script::build::{BuildReport, diagnostics, render_command, write_build_report};

pub(super) fn run(host: &dyn Host, invocation: &Invocation) -> Result<i32, String> {
    let cwd = host.initial_cwd();
    let initial = cwd.join(&invocation.options.test_dir);
    let mut config = Configuration::load(host, &initial, &invocation.overwrite_configuration_file)
        .map_err(|error| error.to_string())?;
    let setting = |name| definition(invocation, name);
    let source = invocation.source_dir.as_ref().map_or_else(
        || initial.join(path::to_native(&config.source_directory)),
        |source| cwd.join(source),
    );
    let build = if invocation.build_dir.is_none() && !config.build_directory.is_empty() {
        cwd.join(path::to_native(&config.build_directory))
    } else {
        initial
    };
    for (key, value) in [
        (b"CTEST_SITE".as_slice(), &mut config.site),
        (b"CTEST_BUILD_NAME", &mut config.build_name),
        (b"CTEST_CHANGE_ID", &mut config.change_id),
    ] {
        if let Some(setting) = setting(key) {
            *value = setting.into();
        }
    }
    let model = invocation
        .dashboard_model
        .or_else(|| {
            invocation
                .definitions
                .iter()
                .find_map(|entry| entry.strip_suffix(b"Build").and_then(Model::parse))
        })
        .unwrap_or(Model::Experimental);
    let tag = Tag::read(host, &build)
        .or_else(|_| Tag::create(host, &build, host.now(), model, None, &config.nightly_start))
        .map_err(|error| error.to_string())?;
    let program = path::from_native(&crate::script::cmake_executable(host));
    let preset = setting(b"CTEST_BUILD_PRESET")
        .filter(|value| !value.is_empty())
        .or_else(|| setting(b"CTEST_PRESET").filter(|value| !value.is_empty()));
    let mut args = vec!["--build".into()];
    if let Some(preset) = preset {
        args.extend([BString::from("--preset"), preset.into()]);
        let file = invocation
            .presets_file
            .as_ref()
            .map(|file| cwd.join(file))
            .or_else(|| {
                setting(b"CTEST_PRESETS_FILE").map(|file| source.join(path::to_native(file)))
            });
        if let Some(file) = file {
            args.extend(["--presets-file".into(), path::from_native(&file)]);
        }
    } else {
        args.push(path::from_native(&build));
    }
    if let Some(config) = setting(b"CTEST_BUILD_CONFIGURATION")
        .filter(|value| !value.is_empty())
        .or_else(|| setting(b"CTEST_CONFIGURATION_TYPE").filter(|value| !value.is_empty()))
        .or_else(|| {
            invocation
                .options
                .config
                .as_ref()
                .map(|value| value.as_slice())
        })
    {
        args.extend(["--config".into(), config.into()]);
    }
    if let Some(target) = setting(b"CTEST_BUILD_TARGET") {
        args.extend(["--target".into(), target.into()]);
    }
    let command = render_command(
        &std::iter::once(program.clone())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>(),
    );
    if !invocation.options.quiet {
        host.write_out(Stream::Stdout, b"Build project\n");
    }
    let start = host.now();
    let output =
        crate::script::run_process(host, build.clone(), program, args, host.env_all(), false)
            .map_err(|error| error.to_string())?;
    let code = match output.results.last() {
        Some(ProcessResult::Exited(code)) => *code,
        _ => -1,
    };
    let (errors, warnings, abnormal) = diagnostics(&output, code);
    write_build_report(
        host,
        &build,
        &tag,
        BuildReport {
            source: &source,
            config: &config,
            command: &command,
            output: &output,
            start,
            code,
            errors,
            warnings,
            abnormal,
        },
    )?;
    if !invocation.options.quiet {
        host.write_out(
            Stream::Stdout,
            format!("   {errors} Compiler errors\n   {warnings} Compiler warnings\n").as_bytes(),
        );
    }
    if code != 0 {
        host.write_out(Stream::Stderr, b"Error(s) when building project\n");
    }
    Ok(0)
}
