//! Dashboard actions selected on the command line.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{
    FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest, Stream, path,
};
use rcmake_presets::{EnvSnapshot, PresetKind};

use crate::cli::Invocation;
use crate::dart::{self, Configuration, Model, Tag};

mod build;
pub(crate) mod coverage;
pub(crate) mod instrumentation_xml;
mod notes;
pub(crate) mod submit;
pub(crate) mod update;

struct Dashboard {
    initial: PathBuf,
    source: PathBuf,
    build: PathBuf,
    config: Configuration,
    tag: Tag,
}

const STEPS: &[(&[u8], &str, i32)] = &[
    (b"start", "Start", 0),
    (b"update", "Update", 1),
    (b"configure", "Configure", 2),
    (b"build", "Build", 4),
    (b"test", "Test", 8),
    (b"coverage", "Coverage", 32),
    (b"memcheck", "MemCheck", 16),
    (b"notes", "Notes", 0),
    (b"submit", "Submit", 64),
];

pub(crate) fn run(
    host: &Arc<dyn Host>,
    invocation: &mut Invocation,
    mut run_tests: impl FnMut(&Invocation, dart::Dashboard) -> Result<i32, String>,
) -> Option<i32> {
    let mut model = invocation.dashboard_model;
    let mut actions = invocation.actions.clone();
    if actions.iter().any(|action| action == "all") {
        actions.extend(STEPS.iter().map(|&(action, _, _)| BString::from(action)));
    }
    for definition in &invocation.definitions {
        for candidate in [Model::Experimental, Model::Continuous, Model::Nightly] {
            let Some(suffix) = definition.strip_prefix(candidate.name().as_bytes()) else {
                continue;
            };
            if matches!(suffix, b"" | b"MemoryCheck") {
                model = Some(candidate);
                let test_step = if suffix.is_empty() {
                    b"test".as_slice()
                } else {
                    b"memcheck"
                };
                actions.extend(
                    STEPS
                        .iter()
                        .map(|&(action, _, _)| action)
                        .filter(|&action| match action {
                            b"update" => candidate != Model::Experimental,
                            b"test" | b"memcheck" => action == test_step,
                            _ => true,
                        })
                        .map(BString::from),
                );
            } else if let Some(&(action, _, _)) = STEPS
                .iter()
                .find(|&&(_, name, _)| suffix == name.as_bytes())
            {
                model = Some(candidate);
                actions.push(action.into());
            }
        }
    }
    if actions.is_empty()
        || (actions == invocation.actions
            && matches!(actions.as_slice(), [action] if action == "test" || action == "memcheck"))
    {
        return None;
    }
    let requested = |name: &[u8]| actions.iter().any(|action| action.as_slice() == name);
    invocation.dashboard_model = model;

    let host_ref = host.as_ref();
    let mut dashboard = match initialize(host_ref, invocation, requested(b"start")) {
        Ok(dashboard) => dashboard,
        Err(message) => {
            host_ref.write_out(
                Stream::Stderr,
                format!("CMake Error: {message}\n").as_bytes(),
            );
            return Some(12);
        }
    };
    invocation.options.test_dir.clone_from(&dashboard.build);

    let mut code = 0;
    for &(step, name, error_bit) in STEPS {
        if !requested(step) && step != b"notes" {
            continue;
        }
        let result = match step {
            b"update" => update::run(host_ref, invocation, &dashboard),
            b"configure" => configure(host_ref, invocation, &mut dashboard),
            b"build" => build::run(host, invocation, &dashboard),
            b"test" | b"memcheck" => {
                let original = std::mem::replace(&mut invocation.actions, vec![step.into()]);
                let result = dashboard
                    .test_run(host_ref, invocation)
                    .and_then(|dashboard| run_tests(invocation, dashboard));
                invocation.actions = original;
                result
            }
            b"coverage" => coverage::run(host, invocation, &dashboard),
            b"notes" => notes::run(host_ref, &dashboard),
            b"submit" => submit::run(host_ref, invocation, &dashboard),
            _ => continue,
        };
        match result {
            Ok(0) => {}
            Ok(_) => code |= error_bit,
            Err(message) => {
                host_ref.write_out(
                    Stream::Stderr,
                    format!("CMake Error: {name}: {message}\n").as_bytes(),
                );
                code |= error_bit;
            }
        }
    }
    Some(code)
}

fn initialize(host: &dyn Host, invocation: &Invocation, start: bool) -> Result<Dashboard, String> {
    let cwd = host.initial_cwd();
    let initial = cwd.join(&invocation.options.test_dir);
    let overrides = &invocation.overwrite_configuration_file;
    let mut config =
        Configuration::load(host, &initial, overrides).map_err(|error| error.to_string())?;
    let source = invocation.source_dir.as_ref().map_or_else(
        || {
            definition(invocation, b"CTEST_SOURCE_DIRECTORY").map_or_else(
                || initial.join(path::to_native(&config.source_directory)),
                |value| cwd.join(path::to_native(value)),
            )
        },
        |source| cwd.join(source),
    );
    let mut build = initial.clone();
    if invocation.build_dir.is_none() && invocation.preset.is_none() {
        if let Some(value) =
            definition(invocation, b"CTEST_BINARY_DIRECTORY").filter(|value| !value.is_empty())
        {
            build = cwd.join(path::to_native(value));
        } else if !config.build_directory.is_empty() {
            build = cwd.join(path::to_native(&config.build_directory));
        } else if let Some(name) = configure_preset(invocation) {
            let file = presets_file(invocation, &source);
            let presets = match &file {
                Some(file) => rcmake_presets::load_file(&source, file, host),
                None => rcmake_presets::load(&source, host),
            };
            let preset = presets.and_then(|presets| {
                presets.resolve(PresetKind::Configure, name, &EnvSnapshot::from_host(host))
            });
            if let Ok(preset) = preset
                && let Some(dir) = preset.binary_dir
            {
                build = source.join(path::to_native(&dir));
            }
        }
    }
    if build != initial && configuration_exists(host, &build) {
        config = Configuration::load(host, &build, overrides).map_err(|error| error.to_string())?;
    }
    apply_config_definitions(invocation, &mut config);
    let create_tag = || {
        Tag::create(
            host,
            &build,
            host.now(),
            invocation.dashboard_model.unwrap_or(Model::Experimental),
            None,
            &config.nightly_start,
        )
    };
    let tag = if start {
        create_tag()
    } else {
        Tag::read_with_model(host, &build, invocation.dashboard_model).or_else(|_| create_tag())
    }
    .map_err(|error| error.to_string())?;
    if start && !invocation.options.quiet {
        host.write_out(
            Stream::Stdout,
            format!(
                "   Site: {}\n   Build name: {}\n",
                config.site, config.build_name
            )
            .as_bytes(),
        );
    }
    Ok(Dashboard {
        initial,
        source,
        build,
        config,
        tag,
    })
}

fn configure_preset(invocation: &Invocation) -> Option<&[u8]> {
    definition(invocation, b"CTEST_CONFIGURE_PRESET")
        .filter(|value| !value.is_empty())
        .or_else(|| definition(invocation, b"CTEST_PRESET").filter(|value| !value.is_empty()))
}

fn apply_config_definitions(invocation: &Invocation, config: &mut Configuration) {
    for (key, field) in [
        (b"CTEST_SITE".as_slice(), &mut config.site),
        (b"CTEST_BUILD_NAME", &mut config.build_name),
        (b"CTEST_CHANGE_ID", &mut config.change_id),
        (b"CTEST_NIGHTLY_START_TIME", &mut config.nightly_start),
    ] {
        if let Some(value) = definition(invocation, key) {
            *field = value.into();
        }
    }
}

fn presets_file(invocation: &Invocation, source: &Path) -> Option<PathBuf> {
    invocation.presets_file.clone().or_else(|| {
        definition(invocation, b"CTEST_PRESETS_FILE")
            .filter(|value| !value.is_empty())
            .map(|file| source.join(path::to_native(file)))
    })
}

fn configure(
    host: &dyn Host,
    invocation: &Invocation,
    dashboard: &mut Dashboard,
) -> Result<i32, String> {
    let Dashboard {
        initial,
        source,
        build,
        config,
        tag,
    } = dashboard;
    let definition = |name: &[u8]| definition(invocation, name);
    if !host
        .metadata(&source.join("CMakeLists.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        host.write_out(
            Stream::Stderr,
            b"ctest_configure called with invalid source directory\n",
        );
        return Ok(2);
    }

    missing_configuration(host, initial);
    let mut args = Vec::<BString>::new();
    if let Some(name) = configure_preset(invocation) {
        let file = presets_file(invocation, source);
        let presets = match &file {
            Some(file) => rcmake_presets::load_file(source, file, host),
            None => rcmake_presets::load(source, host),
        }
        .map_err(|error| {
            if let Some((_, detail)) = error.message.split_once('\n') {
                format!(
                    "Could not read presets from \"{}\"\n{detail}",
                    source.display()
                )
            } else {
                error.message
            }
        })?;
        if let Err(error) =
            presets.resolve(PresetKind::Configure, name, &EnvSnapshot::from_host(host))
        {
            host.write_out(Stream::Stderr, format!("{}\n", error.message).as_bytes());
            return Ok(2);
        }
        args.extend(["--preset".into(), name.into()]);
        if let Some(file) = file {
            args.extend(["--presets-file".into(), path::from_native(&file)]);
        }
    }
    args.extend([
        "-S".into(),
        path::from_native(source),
        "-B".into(),
        path::from_native(build),
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
    host.create_dir_all(build)
        .map_err(|error| error.to_string())?;
    if !invocation.options.quiet {
        host.write_out(Stream::Stdout, b"Configure project\n");
    }
    let program = path::from_native(&crate::script::cmake_executable(host));
    let env = crate::script::builtin_cmake_env(host, &program);
    let command = rcmake_host::CommandSpec { program, args };
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![command.clone()],
                cwd: source.clone(),
                env,
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
        _ => -1,
    };
    if code == 0 {
        config
            .update_file(host, source, build)
            .map_err(|error| error.to_string())?;
        *config = Configuration::load(host, build, &invocation.overwrite_configuration_file)
            .map_err(|error| error.to_string())?;
        apply_config_definitions(invocation, config);
    } else {
        host.write_out(Stream::Stderr, b"Error(s) when configuring the project\n");
    }
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
    Ok(code.min(0))
}

fn definition<'a>(invocation: &'a Invocation, name: &[u8]) -> Option<&'a [u8]> {
    invocation.definitions.iter().rev().find_map(|value| {
        let (key, value) = dart::split(value, b'=')?;
        let key = dart::split(key, b':').map_or(key, |(key, _)| key);
        (key == name).then_some(value)
    })
}

const CONFIGURATION_FILES: &[&str] = &["CTestConfiguration.ini", "DartConfiguration.tcl"];

fn configuration_exists(host: &dyn Host, dir: &Path) -> bool {
    CONFIGURATION_FILES
        .iter()
        .any(|name| host.metadata(&dir.join(name), Follow::Yes).is_ok())
}

pub(crate) fn configuration_value(
    host: &dyn Host,
    dir: &Path,
    overrides: &[BString],
    key: &[u8],
) -> Result<BString, String> {
    if let Some(value) = overrides.iter().rev().find_map(|entry| {
        let (name, value) = dart::split(entry, b'=')?;
        (name == key).then_some(value)
    }) {
        return Ok(value.into());
    }
    for name in CONFIGURATION_FILES {
        match host.read(&dir.join(name), ReadRange::All { limit: 1024 * 1024 }) {
            Ok(data) => {
                return Ok(data
                    .rsplit(|&byte| byte == b'\n')
                    .find_map(|line| {
                        let (name, value) = dart::split(line, b':')?;
                        (name.trim_ascii() == key).then(|| BString::from(value.trim_ascii()))
                    })
                    .unwrap_or_default());
            }
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(BString::default())
}

impl Dashboard {
    fn test_run(
        &self,
        host: &dyn Host,
        invocation: &Invocation,
    ) -> Result<dart::Dashboard, String> {
        let directory = if configuration_exists(host, &self.build) {
            &self.build
        } else {
            &self.initial
        };
        let mut configuration =
            Configuration::load(host, directory, &invocation.overwrite_configuration_file)
                .map_err(|error| error.to_string())?;
        apply_config_definitions(invocation, &mut configuration);
        Ok(dart::Dashboard {
            directory: self.build.clone(),
            tag: self.tag.clone(),
            configuration,
            show_project: true,
        })
    }

    fn value(
        &self,
        host: &dyn Host,
        invocation: &Invocation,
        key: &[u8],
    ) -> Result<BString, String> {
        let overrides = &invocation.overwrite_configuration_file;
        let value = configuration_value(host, &self.build, overrides, key)?;
        if value.is_empty() && self.build != self.initial {
            configuration_value(host, &self.initial, overrides, key)
        } else {
            Ok(value)
        }
    }
}

fn missing_configuration(host: &dyn Host, dir: &Path) {
    if !configuration_exists(host, dir) {
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
