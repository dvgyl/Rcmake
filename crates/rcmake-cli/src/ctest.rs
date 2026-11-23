use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_ctest::{CtestError, CtestOptions, CtestOutcome};
use rcmake_host::{Host, Stream};
use rcmake_presets::{PresetKind, PresetSettings, ShowOnly, TestJobs, TestSettings, TestVerbosity};

use crate::options::{split_flag, take_value};
use crate::{presets, resolve_cli_path};

#[derive(Default)]
struct Options {
    test_dir: Option<PathBuf>,
    config: Option<BString>,
    include: Option<BString>,
    exclude: Option<BString>,
    jobs: Option<usize>,
    output_on_failure: bool,
    verbosity: Option<TestVerbosity>,
    quiet: bool,
    show_only: Option<ShowOnly>,
    timeout: Option<f64>,
    preset: Option<BString>,
    presets_file: Option<PathBuf>,
}

fn parse(args: &[BString], host: &dyn Host) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut args = args.iter().map(|arg| arg.as_slice()).peekable();
    while let Some(arg) = args.next() {
        let (flag, inline) = split_flag(arg, b'=');
        let mut value = || {
            inline
                .or_else(|| take_value(&mut args))
                .ok_or_else(|| format!("{} requires a value", String::from_utf8_lossy(flag)))
        };
        match flag {
            b"--test-dir" => opts.test_dir = Some(resolve_cli_path(host, value()?)),
            b"-C" | b"--build-config" => opts.config = Some(value()?.into()),
            b"-R" | b"--tests-regex" => opts.include = Some(value()?.into()),
            b"-E" | b"--exclude-regex" => opts.exclude = Some(value()?.into()),
            b"--preset" => opts.preset = Some(value()?.into()),
            b"--presets-file" => opts.presets_file = Some(resolve_cli_path(host, value()?)),
            b"--timeout" => {
                let raw = value()?;
                opts.timeout = Some(
                    std::str::from_utf8(raw)
                        .ok()
                        .and_then(|value| value.parse::<f64>().ok())
                        .filter(|value| value.is_finite() && *value >= 0.0)
                        .ok_or_else(|| {
                            format!("Invalid --timeout value: {}", String::from_utf8_lossy(raw))
                        })?,
                );
            }
            b"-V" | b"--verbose" if inline.is_none() => {
                opts.verbosity = Some(TestVerbosity::Verbose)
            }
            b"-VV" | b"--extra-verbose" if inline.is_none() => {
                opts.verbosity = Some(TestVerbosity::Extra)
            }
            b"-Q" | b"--quiet" if inline.is_none() => opts.quiet = true,
            b"--output-on-failure" if inline.is_none() => opts.output_on_failure = true,
            b"-N" | b"--show-only" => {
                let mode = inline.unwrap_or(b"human");
                opts.show_only = Some(match mode {
                    b"human" => ShowOnly::Human,
                    b"json-v1" => ShowOnly::JsonV1,
                    b"json-v1-raw" => ShowOnly::JsonV1Raw,
                    _ => {
                        return Err(format!(
                            "'--show-only=' given unknown value '{}'",
                            String::from_utf8_lossy(mode)
                        ));
                    }
                });
            }
            _ => {
                if let Some(suffix) = flag
                    .strip_prefix(b"-j")
                    .or_else(|| flag.strip_prefix(b"--parallel"))
                {
                    let value = if suffix.is_empty() {
                        inline.or_else(|| take_value(&mut args))
                    } else {
                        Some(suffix)
                    };
                    opts.jobs = Some(match value {
                        Some(value) => parse_jobs(value, host)?,
                        None => automatic_jobs(host),
                    });
                } else {
                    return Err(format!("Unknown argument {}", String::from_utf8_lossy(arg)));
                }
            }
        }
    }
    Ok(opts)
}

fn automatic_jobs(host: &dyn Host) -> usize {
    host.system().cpu_count.max(2) as usize
}

fn parse_jobs(value: &[u8], host: &dyn Host) -> Result<usize, String> {
    if value.is_empty() {
        return Ok(automatic_jobs(host));
    }
    let count = std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            format!(
                "Invalid parallel job count: {}",
                String::from_utf8_lossy(value)
            )
        })?;
    // The runner accepts a positive concurrency limit.
    Ok(if count == 0 { usize::MAX } else { count })
}

impl Options {
    fn apply_preset(&mut self, settings: TestSettings, host: &dyn Host) {
        self.config = self.config.take().or(settings.configuration);
        if let Some(filter) = settings.filter {
            self.include = self
                .include
                .take()
                .or_else(|| filter.include.and_then(|part| part.name));
            self.exclude = self
                .exclude
                .take()
                .or_else(|| filter.exclude.and_then(|part| part.name));
        }
        if let Some(output) = settings.output {
            self.output_on_failure |= output.output_on_failure.unwrap_or(false);
            self.quiet |= output.quiet.unwrap_or(false);
            self.verbosity = self.verbosity.or(output.verbosity);
        }
        if let Some(execution) = settings.execution {
            self.jobs = self.jobs.or_else(|| {
                execution.jobs.map(|jobs| match jobs {
                    TestJobs::Automatic => automatic_jobs(host),
                    TestJobs::Count(0) => usize::MAX,
                    TestJobs::Count(count) => count as usize,
                })
            });
            self.timeout = self.timeout.or(execution.timeout.map(f64::from));
            self.show_only = self.show_only.or(execution.show_only);
        }
    }
}

pub(crate) fn run(args: &[BString], host: Arc<dyn Host>) -> i32 {
    run_with(args, host, rcmake_ctest::run)
}

fn run_with(
    args: &[BString],
    host: Arc<dyn Host>,
    runner: impl FnOnce(Arc<dyn Host>, &CtestOptions) -> Result<CtestOutcome, CtestError>,
) -> i32 {
    let mut opts = match parse(args, host.as_ref()) {
        Ok(opts) => opts,
        Err(message) => return error(host.as_ref(), &message),
    };
    let mut environment = Default::default();
    if let Some(name) = &opts.preset {
        let preset = match presets::resolve_for_mode(
            host.as_ref(),
            PresetKind::Test,
            name,
            opts.presets_file.as_deref(),
        ) {
            Ok(preset) => preset,
            Err(code) => return code,
        };
        opts.test_dir = opts.test_dir.or_else(|| {
            preset
                .binary_dir
                .map(|dir| resolve_cli_path(host.as_ref(), &dir))
        });
        if let PresetSettings::Test(settings) = preset.settings {
            opts.apply_preset(*settings, host.as_ref());
        }
        environment = preset.environment;
    }
    if let Some(flag) = [
        (opts.quiet, "--quiet"),
        (
            matches!(
                opts.verbosity,
                Some(TestVerbosity::Verbose | TestVerbosity::Extra)
            ),
            "--verbose",
        ),
        (opts.show_only.is_some(), "--show-only"),
        (opts.timeout.is_some(), "--timeout"),
    ]
    .into_iter()
    .find_map(|(enabled, flag)| enabled.then_some(flag))
    {
        return error(
            host.as_ref(),
            &format!("The CTest runner does not support {flag} yet."),
        );
    }
    let env_get = |key: &[u8]| match environment.get(key) {
        Some(value) => value.clone(),
        None => host.env_get(key),
    };
    let jobs = match opts.jobs {
        Some(jobs) => jobs,
        None => {
            match env_get(b"CTEST_PARALLEL_LEVEL").map(|value| parse_jobs(&value, host.as_ref())) {
                Some(Ok(jobs)) => jobs,
                Some(Err(message)) => return error(host.as_ref(), &message),
                None => 1,
            }
        }
    };
    let opts = CtestOptions {
        test_dir: opts.test_dir.unwrap_or_else(|| host.initial_cwd()),
        config: opts.config,
        include: opts.include,
        exclude: opts.exclude,
        jobs,
        output_on_failure: opts.output_on_failure
            || env_get(b"CTEST_OUTPUT_ON_FAILURE")
                .is_some_and(|value| rcmake_base::truth::is_on(&value)),
    };
    let mut previous = Vec::new();
    for (key, value) in environment {
        previous.push((key.clone(), host.env_get(&key)));
        host.env_set(&key, value.as_ref().map(|value| value.as_slice()));
    }
    let result = runner(Arc::clone(&host), &opts);
    for (key, value) in previous {
        host.env_set(&key, value.as_ref().map(|value| value.as_slice()));
    }
    match result {
        Ok(outcome) => outcome.exit_code,
        Err(err) => error(host.as_ref(), &err.to_string()),
    }
}

fn error(host: &dyn Host, message: &str) -> i32 {
    host.write_out(
        Stream::Stderr,
        format!("CMake Error: {message}\n").as_bytes(),
    );
    1
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn parses_output_modes_and_fractional_timeout() {
        let args = ["-VV", "-Q", "--show-only=json-v1", "--timeout", "1.5"].map(BString::from);
        let options = parse(&args, &PureHost::default()).unwrap();
        assert_eq!(options.verbosity, Some(TestVerbosity::Extra));
        assert!(options.quiet);
        assert_eq!(options.show_only, Some(ShowOnly::JsonV1));
        assert_eq!(options.timeout, Some(1.5));
    }

    #[test]
    fn forwards_preset_and_cli_options_to_runner() {
        let host = Arc::new(PureHost::default());
        host.env_set(b"PRESET_ENV", Some(b"parent"));
        host.seed_file(
            Path::new("/work/CMakePresets.json"),
            br#"{
              "version": 6,
              "configurePresets": [{
                "name": "config", "generator": "Ninja", "binaryDir": "${sourceDir}/out"
              }],
              "testPresets": [{
                "name": "test", "configurePreset": "config", "configuration": "Debug",
                "filter": {"include": {"name": "include"}, "exclude": {"name": "exclude"}},
                "execution": {"jobs": 2},
                "environment": {"PRESET_ENV": "child", "CTEST_OUTPUT_ON_FAILURE": "ON"}
              }]
            }"#,
        )
        .unwrap();
        let args = ["--preset=test", "-C", "Release", "-j4", "-E", "override"].map(BString::from);
        let exit = run_with(&args, host.clone(), |host, options| {
            assert_eq!(
                options,
                &CtestOptions {
                    test_dir: "/work/out".into(),
                    config: Some("Release".into()),
                    include: Some("include".into()),
                    exclude: Some("override".into()),
                    jobs: 4,
                    output_on_failure: true,
                }
            );
            assert_eq!(host.env_get(b"PRESET_ENV"), Some("child".into()));
            Ok(CtestOutcome {
                exit_code: 8,
                ..CtestOutcome::default()
            })
        });
        assert_eq!(exit, 8);
        assert_eq!(host.env_get(b"PRESET_ENV"), Some("parent".into()));
    }
}
