use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path,
};
use rcmake_model::GeneratorKind;
use rcmake_presets::{BuildSettings, PresetKind, PresetSettings};

use crate::options::{split_flag, take_value, unknown_mode_option};
use crate::{cache, presets, resolve_cli_path};

const USAGE: &str = "Usage: cmake --build <dir>             [options] [-- [native-options]]
       cmake --build --preset <preset> [options] [-- [native-options]]
Options:
  <dir>          = Project binary directory to be built.
  --preset <preset>, --preset=<preset>
                 = Specify a build preset.
  --presets-file <file>, --presets-file=<file>
                 = Specify the path to a presets file.
  --list-presets[=defined]
                 = List available or all defined build presets.
  --parallel [<jobs>], -j [<jobs>]
                 = Build in parallel using the given number of jobs.\x20
                   If <jobs> is omitted the native build tool's\x20
                   default number is used.
                   The CMAKE_BUILD_PARALLEL_LEVEL environment variable
                   specifies a default parallel level when this option
                   is not given.
  -t <tgt>..., --target <tgt>...
                 = Build <tgt> instead of default targets.
  --config <cfg> = For multi-configuration tools, choose <cfg>.
  --clean-first  = Build target 'clean' first, then build.
                   (To clean only, use --target 'clean'.)
  --resolve-package-references={on|only|off}
                 = Restore/resolve package references during build.
  -v, --verbose  = Enable verbose output - if supported - including
                   the build commands to be executed.\x20
  --             = Pass remaining options to the native tool.
";

const FLAGS: &[&[u8]] = &[
    b"--preset",
    b"--presets-file",
    b"--list-presets",
    b"--parallel",
    b"--target",
    b"--config",
    b"--clean-first",
    b"--resolve-package-references",
    b"--verbose",
    b"-j",
    b"-t",
    b"-v",
];

#[derive(Default)]
struct Options {
    dir: Option<PathBuf>,
    preset: Option<BString>,
    presets_file: Option<PathBuf>,
    list_presets: bool,
    jobs: Option<Jobs>,
    targets: Vec<BString>,
    config: Option<BString>,
    clean_first: bool,
    verbose: bool,
    native: Vec<BString>,
}

#[derive(Clone, Copy)]
enum Jobs {
    Default,
    Count(u32),
}

fn usage(host: &dyn Host, message: &str) -> i32 {
    if !message.is_empty() {
        host.write_out(Stream::Stderr, message.as_bytes());
        host.write_out(Stream::Stderr, b"\n");
    }
    host.write_out(Stream::Stderr, USAGE.as_bytes());
    1
}

pub(crate) fn invalid_value(flag: &[u8]) -> String {
    format!(
        "CMake Error: Invalid value used with {}",
        String::from_utf8_lossy(flag)
    )
}

pub(crate) fn job_count(value: &[u8]) -> Result<u32, &'static str> {
    let text = std::str::from_utf8(value).map_err(|_| "invalid")?;
    let count = text.parse::<i64>().map_err(|err| match err.kind() {
        std::num::IntErrorKind::PosOverflow => "The <jobs> value is too large.",
        _ => "invalid",
    })?;
    if count == 0 {
        return Err("The <jobs> value requires a positive integer argument.");
    }
    if count < 0 {
        return Err("invalid");
    }
    if count > i64::from(i32::MAX) {
        return Err("The <jobs> value is too large.");
    }
    u32::try_from(count).map_err(|_| "invalid")
}

fn parse_jobs(flag: &[u8], value: &[u8], env: bool) -> Result<Jobs, String> {
    if value.is_empty() {
        return Ok(Jobs::Default);
    }
    job_count(value).map(Jobs::Count).map_err(|err| {
        let flag = String::from_utf8_lossy(flag);
        let value = String::from_utf8_lossy(value);
        let label = if env {
            format!("'{flag}' environment variable\n")
        } else {
            format!("'{flag}' ")
        };
        if err == "invalid" {
            format!("{label}invalid number '{value}' given.\n")
        } else {
            format!("{err}\n")
        }
    })
}

fn parse(host: &dyn Host, args: &[BString]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut args = args.iter().map(|arg| arg.as_slice()).peekable();
    opts.dir = take_value(&mut args)
        .filter(|arg| !arg.is_empty())
        .map(|dir| resolve_cli_path(host, dir));
    while let Some(arg) = args.next() {
        if arg == b"--" {
            opts.native.extend(args.map(BString::from));
            break;
        }
        let (flag, inline) = split_flag(arg, b'=');
        if let Some((job_flag, suffix)) = [b"--parallel".as_slice(), b"-j"]
            .into_iter()
            .find_map(|name| flag.strip_prefix(name).map(|tail| (name, tail)))
        {
            if inline == Some(b"") {
                return Err(invalid_value(job_flag));
            }
            let value = if suffix.is_empty() {
                inline.or_else(|| {
                    args.next_if(|arg| {
                        !arg.starts_with(b"-") || arg.get(1).is_some_and(u8::is_ascii_digit)
                    })
                })
            } else {
                Some(suffix)
            };
            opts.jobs = Some(parse_jobs(job_flag, value.unwrap_or_default(), false)?);
            continue;
        }
        let mut required = || {
            inline
                .or_else(|| take_value(&mut args))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| invalid_value(flag))
        };
        match flag {
            b"--preset" => opts.preset = Some(required()?.into()),
            b"--presets-file" => opts.presets_file = Some(resolve_cli_path(host, required()?)),
            b"--list-presets" if matches!(inline, None | Some(b"defined")) => {
                opts.list_presets = true
            }
            b"--config" => opts.config = Some(required()?.into()),
            b"--target" | b"-t" => {
                opts.targets.push(required()?.into());
                while let Some(target) = take_value(&mut args) {
                    opts.targets.push(target.into());
                }
            }
            b"--resolve-package-references" => {
                if !inline
                    .or_else(|| take_value(&mut args))
                    .is_some_and(|mode| {
                        [b"on".as_slice(), b"only", b"off"]
                            .iter()
                            .any(|value| value.eq_ignore_ascii_case(mode))
                    })
                {
                    return Err(String::new());
                }
            }
            b"--clean-first" if inline.is_none() => opts.clean_first = true,
            b"-v" | b"--verbose" if inline.is_none() => opts.verbose = true,
            _ => return Err(unknown_mode_option(arg, FLAGS)),
        }
    }
    Ok(opts)
}

impl Options {
    fn apply_preset(&mut self, settings: BuildSettings) {
        if self.jobs.is_none() {
            self.jobs = settings.jobs.map(|n| {
                if n == 0 {
                    Jobs::Default
                } else {
                    Jobs::Count(n)
                }
            });
        }
        if self.targets.is_empty() {
            self.targets = settings.targets.unwrap_or_default();
        }
        if self.config.is_none() {
            self.config = settings.configuration;
        }
        self.clean_first |= settings.clean_first.unwrap_or(false);
        self.verbose |= settings.verbose.unwrap_or(false);
        if let Some(mut native) = settings.native_tool_options {
            native.append(&mut self.native);
            self.native = native;
        }
    }
}

pub(crate) fn run(argv: &[BString], args: &[BString], host: &dyn Host) -> i32 {
    let mut opts = match parse(host, args) {
        Ok(opts) => opts,
        Err(message) => return usage(host, &message),
    };
    if opts.list_presets {
        return presets::list(
            host,
            &host.initial_cwd(),
            opts.presets_file.as_deref(),
            b"build",
        );
    }

    let mut env: BTreeMap<_, _> = host.env_all().into_iter().collect();
    if let Some(name) = &opts.preset {
        if opts.dir.is_some() {
            return usage(host, "");
        }
        let preset = match presets::resolve_for_mode(
            host,
            PresetKind::Build,
            name,
            opts.presets_file.as_deref(),
        ) {
            Ok(preset) => preset,
            Err(code) => return code,
        };
        opts.dir = preset.binary_dir.map(|dir| resolve_cli_path(host, &dir));
        if let PresetSettings::Build(settings) = preset.settings {
            opts.apply_preset(settings);
        }
        for (key, value) in preset.environment {
            if let Some(value) = value {
                env.insert(key, value);
            } else {
                env.remove(&key);
            }
        }
    }
    let Some(dir) = opts.dir.as_deref() else {
        return usage(host, "");
    };
    if opts.targets.len() > 1 && opts.targets.iter().any(|target| target == b"clean") {
        return usage(
            host,
            "Error: Building 'clean' and other targets together is not supported.",
        );
    }
    if opts.jobs.is_none()
        && let Some(value) = env.get(b"CMAKE_BUILD_PARALLEL_LEVEL".as_slice())
    {
        match parse_jobs(b"CMAKE_BUILD_PARALLEL_LEVEL", value, true) {
            Ok(jobs) => opts.jobs = Some(jobs),
            Err(message) => return usage(host, &message),
        }
    }
    opts.verbose |= env.contains_key(b"VERBOSE".as_slice());
    crate::instrument_lifecycle(
        host,
        dir,
        argv,
        rcmake_instrument::SnippetRole::CMakeBuild,
        rcmake_instrument::Hook::PostCMakeBuild,
        || dispatch(host, dir, &opts, env.into_iter().collect()),
    )
}

fn error(host: &dyn Host, message: &str) -> i32 {
    host.write_out(Stream::Stderr, format!("Error: {message}\n").as_bytes());
    1
}

fn dispatch(host: &dyn Host, dir: &Path, opts: &Options, env: Vec<(BString, BString)>) -> i32 {
    if !host
        .metadata(dir, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return error(host, &format!("{} is not a directory", dir.display()));
    }
    if !host
        .metadata(&dir.join("CMakeCache.txt"), Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return error(host, "not a CMake build directory (missing CMakeCache.txt)");
    }
    let Ok(cache) = cache::load(host, dir) else {
        return error(host, "could not load cache");
    };
    let Some(generator) = cache.get(b"CMAKE_GENERATOR") else {
        return error(host, "could not find CMAKE_GENERATOR in Cache");
    };
    let Some(kind) = GeneratorKind::from_name(&generator.value) else {
        return error(
            host,
            &format!("could not create CMAKE_GENERATOR \"{}\"", generator.value),
        );
    };
    let make = kind == GeneratorKind::UnixMakefiles;
    let program = cache
        .get(b"CMAKE_MAKE_PROGRAM")
        .map(|entry| entry.value.clone())
        .unwrap_or_else(|| if make { "make".into() } else { "ninja".into() });
    let mut args = vec!["-C".into(), path::from_native(dir)];
    if kind.multi_config()
        && let Some(config) = &opts.config
    {
        let mut file = BString::from("build-");
        file.extend_from_slice(config);
        file.extend_from_slice(b".ninja");
        args.extend(["-f".into(), file]);
    }
    if let Some(Jobs::Count(count)) = opts.jobs {
        args.extend(["-j".into(), count.to_string().into()]);
    } else if make && matches!(opts.jobs, Some(Jobs::Default)) {
        args.push("-j".into());
    }
    if opts.verbose {
        args.push(if make {
            "VERBOSE=1".into()
        } else {
            "-v".into()
        });
    }
    let request = |targets: &[BString]| {
        let mut args = args.clone();
        args.extend(targets.iter().cloned());
        args.extend(opts.native.iter().cloned());
        RunRequest {
            pipeline: vec![CommandSpec {
                program: program.clone(),
                args,
            }],
            cwd: dir.to_owned(),
            env: env.clone(),
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: true,
        }
    };
    let clean = [BString::from("clean")];
    if opts.clean_first && opts.targets.as_slice() != clean {
        let code = run_native(host, &request(&clean));
        if code != 0 {
            return code;
        }
    }
    run_native(host, &request(&opts.targets))
}

fn run_native(host: &dyn Host, request: &RunRequest) -> i32 {
    let result = host.run(request, &mut |stream, bytes| host.write_out(stream, bytes));
    match result {
        Ok(output) => match output.results.first() {
            Some(ProcessResult::Exited(code)) => *code,
            Some(ProcessResult::Signaled(signal)) => 128 + signal,
            Some(ProcessResult::SpawnFailed(err)) => error(host, &err.to_string()),
            _ => error(host, "build command did not complete"),
        },
        Err(err) => error(host, &err.to_string()),
    }
}
