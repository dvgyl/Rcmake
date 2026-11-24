//! Dashboard actions selected on the command line.

use std::io::ErrorKind;
use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path};
use rcmake_presets::{EnvSnapshot, PresetKind};

use crate::cli::Invocation;
use crate::dart::{self, Configuration, Model, Tag};

mod build;
pub(crate) mod instrumentation_xml;
pub(crate) mod submit;
pub(crate) mod update;

pub(crate) fn run(host: &dyn Host, invocation: &Invocation) -> Result<Option<i32>, String> {
    let configure = invocation
        .actions
        .iter()
        .any(|action| action == "configure")
        || invocation.definitions.iter().any(|definition| {
            definition
                .strip_suffix(b"Configure")
                .and_then(Model::parse)
                .is_some()
        });
    let build = invocation.actions.iter().any(|action| action == "build")
        || invocation.definitions.iter().any(|definition| {
            definition
                .strip_suffix(b"Build")
                .and_then(Model::parse)
                .is_some()
        });
    let start =
        invocation.actions.len() == 1 && invocation.actions.iter().any(|action| action == "start");
    let action = |name: &[u8]| {
        invocation
            .actions
            .iter()
            .any(|action| action.as_slice() == name)
    };
    if !configure {
        if build {
            let code = build::run(host, invocation)?;
            if code != 0 || !(action(b"test") || action(b"memcheck")) {
                return Ok(Some(code));
            }
            return Ok(None);
        }
        if action(b"update") {
            return update::run(host, invocation).map(Some);
        }
        if action(b"submit") {
            return submit::run(host, invocation).map(Some);
        }
        if action(b"coverage") && !action(b"test") {
            let dir = &invocation.options.test_dir;
            missing_configuration(host, dir);
            host.write_out(
                Stream::Stderr,
                b" Cannot find any coverage files. Ignoring Coverage request.\n",
            );
            return Ok(Some(0));
        }
        if !start {
            return Ok(None);
        }
    }

    let definition = |name: &[u8]| definition(invocation, name);
    let cwd = host.initial_cwd();
    let initial_build = &invocation.options.test_dir;
    let mut config = Configuration::load(
        host,
        initial_build,
        &invocation.overwrite_configuration_file,
    )
    .map_err(|error| error.to_string())?;
    if let Some(value) = definition(b"CTEST_SITE") {
        config.site = value.into();
    }
    if let Some(value) = definition(b"CTEST_BUILD_NAME") {
        config.build_name = value.into();
    }
    let source = invocation.source_dir.as_ref().map_or_else(
        || initial_build.join(path::to_native(&config.source_directory)),
        |source| cwd.join(source),
    );
    if start && !configure {
        Tag::create(
            host,
            initial_build,
            host.now(),
            invocation.dashboard_model.unwrap_or(Model::Experimental),
            None,
            &config.nightly_start,
        )
        .map_err(|error| error.to_string())?;
        if !invocation.options.quiet {
            host.write_out(
                Stream::Stdout,
                format!(
                    "   Site: {}\n   Build name: {}\n",
                    config.site, config.build_name
                )
                .as_bytes(),
            );
        }
        return Ok(Some(0));
    }
    if !host
        .metadata(&source.join("CMakeLists.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        host.write_out(
            Stream::Stderr,
            b"ctest_configure called with invalid source directory\n",
        );
        return Ok(Some(2));
    }

    missing_configuration(host, initial_build);
    let preset = definition(b"CTEST_CONFIGURE_PRESET")
        .filter(|value| !value.is_empty())
        .or_else(|| definition(b"CTEST_PRESET").filter(|value| !value.is_empty()));
    let presets_file = invocation
        .presets_file
        .as_ref()
        .map(|file| cwd.join(file))
        .or_else(|| {
            definition(b"CTEST_PRESETS_FILE").map(|file| source.join(path::to_native(file)))
        });
    let mut build = if invocation.build_dir.is_none() && !config.build_directory.is_empty() {
        cwd.join(path::to_native(&config.build_directory))
    } else {
        initial_build.clone()
    };
    let mut args = Vec::<BString>::new();
    if let Some(name) = preset {
        let presets = match &presets_file {
            Some(file) => rcmake_presets::load_file(&source, file, host),
            None => rcmake_presets::load(&source, host),
        }
        .map_err(|error| {
            let message = error.message;
            if let Some((_, detail)) = message.split_once('\n') {
                format!(
                    "Could not read presets from \"{}\"\n{detail}",
                    source.display()
                )
            } else {
                message
            }
        })?;
        let preset =
            match presets.resolve(PresetKind::Configure, name, &EnvSnapshot::from_host(host)) {
                Ok(preset) => preset,
                Err(error) => {
                    host.write_out(Stream::Stderr, format!("{}\n", error.message).as_bytes());
                    return Ok(Some(1));
                }
            };
        if invocation.build_dir.is_none()
            && config.build_directory.is_empty()
            && let Some(dir) = preset.binary_dir
        {
            build = source.join(path::to_native(&dir));
        }
        args.extend(["--preset".into(), name.into()]);
        if let Some(file) = presets_file {
            args.extend(["--presets-file".into(), path::from_native(&file)]);
        }
    }
    args.extend([
        "-S".into(),
        path::from_native(&source),
        "-B".into(),
        path::from_native(&build),
    ]);
    for (flag, key) in [
        ("-G", b"CTEST_CMAKE_GENERATOR".as_slice()),
        ("-A", b"CTEST_CMAKE_GENERATOR_PLATFORM"),
        ("-T", b"CTEST_CMAKE_GENERATOR_TOOLSET"),
    ] {
        if let Some(value) = definition(key).filter(|value| !value.is_empty()) {
            args.extend([flag.into(), value.into()]);
        }
    }
    host.create_dir_all(&build)
        .map_err(|error| error.to_string())?;
    if !invocation.options.quiet {
        host.write_out(Stream::Stdout, b"Configure project\n");
    }
    let program = path::from_native(&crate::script::cmake_executable(host));
    let command = rcmake_host::CommandSpec { program, args };
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![command.clone()],
                cwd: source.clone(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::SharedWithStdout,
                timeout: Some(std::time::Duration::from_secs(600)),
                tee: invocation.options.verbose && !invocation.options.quiet,
            },
            &mut |stream, data| host.write_out(stream, data),
        )
        .map_err(|error| error.to_string())?;
    let code = match output.results.last() {
        Some(ProcessResult::Exited(code)) => *code,
        Some(ProcessResult::SpawnFailed(error)) if error.kind == ErrorKind::NotFound => {
            return Err(error.to_string());
        }
        _ => 1,
    };
    if code == 0 {
        config
            .update_file(host, &source, &build)
            .map_err(|error| error.to_string())?;
    }
    let tag = Tag::create(
        host,
        &build,
        host.now(),
        invocation.dashboard_model.unwrap_or(Model::Experimental),
        None,
        &config.nightly_start,
    )
    .map_err(|error| error.to_string())?;
    let file = build
        .join("Testing")
        .join(tag.stamp())
        .join("Configure.xml");
    write_configure_report(
        host,
        &file,
        &command,
        &output,
        code,
        &config.labels_for_subprojects,
    )?;
    Ok(Some(if code == 0 { 0 } else { 2 }))
}

fn definition<'a>(invocation: &'a Invocation, name: &[u8]) -> Option<&'a [u8]> {
    invocation.definitions.iter().rev().find_map(|value| {
        let (key, value) = dart::split(value, b'=')?;
        let key = dart::split(key, b':').map_or(key, |(key, _)| key);
        (key == name).then_some(value)
    })
}

fn missing_configuration(host: &dyn Host, dir: &Path) {
    if ["DartConfiguration.tcl", "CTestConfiguration.ini"]
        .iter()
        .all(|name| host.metadata(&dir.join(name), Follow::Yes).is_err())
    {
        host.write_out(
            Stream::Stderr,
            format!(
                "Cannot find file: {}\n",
                dir.join("DartConfiguration.tcl").display()
            )
            .as_bytes(),
        );
    }
}

pub(crate) fn write_configure_report(
    host: &dyn Host,
    file: &Path,
    command: &rcmake_host::CommandSpec,
    output: &rcmake_host::RunOutput,
    code: i32,
    labels: &[u8],
) -> Result<(), String> {
    use crate::junit::escape;
    use std::fmt::Write as _;

    let mut line = String::new();
    for arg in std::iter::once(&command.program).chain(&command.args) {
        if !line.is_empty() {
            line.push(' ');
        }
        let _ = write!(line, "\"{arg}\"");
    }
    let stdout = escape(&output.stdout, false);
    let stderr = escape(&output.stderr, false);
    let command = escape(line.as_bytes(), false);
    let build = file.ancestors().nth(3);
    let instrumentation = build
        .and_then(|build| {
            instrumentation_xml::Instrumentation::load(host, build)
                .map(|query| query.configure(host, build, 0))
        })
        .unwrap_or_default();
    let mut subprojects = String::new();
    dart::write_subprojects(&mut subprojects, labels);
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site>\n{subprojects}<Configure>\n<ConfigureCommand>{command}</ConfigureCommand>\n<Log>{stdout}{stderr}</Log>\n<ConfigureStatus>{code}</ConfigureStatus>\n{instrumentation}</Configure></Site>\n"
    );
    crate::junit::write_file(host, file, xml.as_bytes()).map_err(|error| error.to_string())
}
