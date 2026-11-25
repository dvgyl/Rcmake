use std::{path::Path, time::Duration};

use rcmake_base::BString;
use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, RunRequest, Stream, path};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BuildAndTest {
    source: BString,
    build: BString,
    generator: BString,
    toolset: Option<BString>,
    platform: Option<BString>,
    make_program: Option<BString>,
    config: Option<BString>,
    targets: Vec<BString>,
    configure_options: Vec<BString>,
    test_command: Vec<BString>,
    executable_directory: Option<BString>,
    run_directory: Option<BString>,
    test_timeout: Duration,
    clean: bool,
    configure: bool,
    configure_twice: bool,
}

fn passthrough_error() -> String {
    "The -- option cannot be used with --build-and-test. Use --build-options or --test-command to forward arguments.".into()
}

impl BuildAndTest {
    pub(crate) fn parse(args: &[BString], index: usize) -> Result<Self, String> {
        let Some((before, [_, source, build, after @ ..])) = args.split_at_checked(index) else {
            return Err("--build-and-test requires a source and a build directory.".into());
        };
        let mut input = Self {
            source: source.clone(),
            build: build.clone(),
            generator: BString::default(),
            toolset: None,
            platform: None,
            make_program: None,
            config: None,
            targets: Vec::new(),
            configure_options: Vec::new(),
            test_command: Vec::new(),
            executable_directory: None,
            run_directory: None,
            test_timeout: Duration::from_secs(30 * 60),
            clean: true,
            configure: true,
            configure_twice: false,
        };
        let mut args = before.iter().chain(after).peekable();
        while let Some(flag) = args.next() {
            let mut next = || {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} requires a value."))
            };
            match flag.as_slice() {
                b"--build-generator" => input.generator = next()?,
                b"--build-generator-toolset" => input.toolset = Some(next()?),
                b"--build-generator-platform" => input.platform = Some(next()?),
                b"--build-makeprogram" => input.make_program = Some(next()?),
                b"-C" | b"--build-config" => input.config = Some(next()?),
                b"--build-target" => input.targets.push(next()?),
                b"--build-run-dir" | b"--test-run-dir" => input.run_directory = Some(next()?),
                b"--build-noclean" => input.clean = false,
                b"--build-nocmake" => input.configure = false,
                b"--build-two-config" => input.configure_twice = true,
                b"--build-exe-dir" => input.executable_directory = Some(next()?),
                b"--build-project"
                | b"-S"
                | b"-SP"
                | b"--script"
                | b"--script-new-process"
                | b"--output-log"
                | b"-O" => {
                    next()?;
                }
                b"--no-compress-output" => {}
                b"--test-timeout" => {
                    let raw = next()?;
                    let value = raw
                        .to_string()
                        .parse::<u64>()
                        .map_err(|_| format!("Invalid test timeout '{raw}'."))?;
                    input.test_timeout = Duration::from_secs(value.min(30 * 60));
                }
                b"--" => return Err(passthrough_error()),
                b"--build-options" => {
                    while let Some(option) = args.next_if(|option| {
                        !matches!(
                            option.as_slice(),
                            b"--test-command" | b"--test-timeout" | b"--test-run-dir"
                        )
                    }) {
                        input.configure_options.push(option.clone());
                    }
                }
                b"--test-command" => {
                    input.test_command.extend(args.cloned());
                    break;
                }
                b"-V" | b"--verbose" | b"-VV" | b"--extra-verbose" | b"--output-on-failure" => {}
                _ => return Err(format!("Unknown argument: {flag}")),
            }
        }
        if input.generator.is_empty() {
            return Err("--build-and-test requires --build-generator.".into());
        }
        Ok(input)
    }

    pub(crate) fn run(&self, host: &dyn Host, argv0: Option<&BString>) -> Result<i32, String> {
        let cwd = host.initial_cwd();
        let source = cwd.join(path::to_native(&self.source));
        let build = cwd.join(path::to_native(&self.build));
        if let Err(error) = host.create_dir_all(&build) {
            let reason = if host
                .metadata(&build, rcmake_host::Follow::Yes)
                .is_ok_and(|entry| entry.kind != rcmake_host::FileKind::Dir)
            {
                "Not a directory"
            } else {
                &error.os_message
            };
            host.write_out(
                Stream::Stdout,
                format!(
                    "Failed to change working directory to \"{}\": {reason}\n",
                    build.display()
                )
                .as_bytes(),
            );
            return Ok(1);
        }
        let (ctest, _) = rcmake_host::discover_cmake_root(host, argv0);
        let cmake = if ctest
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            ctest.with_file_name("cmake.exe")
        } else {
            ctest.with_file_name("cmake")
        };
        let program = path::from_native(&cmake);
        if self.configure {
            let mut args = vec![
                "-S".into(),
                path::from_native(&source),
                "-B".into(),
                path::from_native(&build),
                "-G".into(),
                self.generator.clone(),
            ];
            for (option, value) in [("-T", &self.toolset), ("-A", &self.platform)] {
                if let Some(value) = value {
                    args.extend([option.into(), value.clone()]);
                }
            }
            if let Some(config) = &self.config {
                args.push(
                    [b"-DCMAKE_BUILD_TYPE=".as_slice(), config.as_slice()]
                        .concat()
                        .into(),
                );
            }
            if let Some(make) = &self.make_program {
                args.push(
                    [b"-DCMAKE_MAKE_PROGRAM=".as_slice(), make.as_slice()]
                        .concat()
                        .into(),
                );
            }
            args.extend_from_slice(&self.configure_options);
            for _ in 0..=usize::from(self.configure_twice) {
                if execute(host, &cwd, &program, &args, Duration::from_secs(30 * 60))? != 0 {
                    return Ok(1);
                }
            }
        }
        let mut args = vec!["--build".into(), path::from_native(&build)];
        if let Some(config) = &self.config {
            args.extend(["--config".into(), config.clone()]);
        }
        if self.clean {
            args.push("--clean-first".into());
        }
        let targets = self
            .targets
            .iter()
            .map(Some)
            .chain(self.targets.is_empty().then_some(None));
        for target in targets {
            let mut args = args.clone();
            if let Some(target) = target {
                args.extend(["--target".into(), target.clone()]);
            }
            if execute(host, &cwd, &program, &args, Duration::from_secs(30 * 60))? != 0 {
                return Ok(1);
            }
        }
        let Some((program, args)) = self.test_command.split_first() else {
            return Ok(0);
        };
        let executable_directory = self.executable_directory.as_ref().map_or_else(
            || build.clone(),
            |directory| build.join(path::to_native(directory)),
        );
        let mut program = program.clone();
        crate::execution::resolve_program_at(
            host,
            &mut program,
            &executable_directory,
            self.config.as_ref(),
        );
        let cwd = self
            .run_directory
            .as_ref()
            .map_or_else(|| build.clone(), |dir| build.join(path::to_native(dir)));
        execute(host, &cwd, &program, args, self.test_timeout).map(|code| i32::from(code != 0))
    }
}

fn execute(
    host: &dyn Host,
    cwd: &Path,
    program: &BString,
    args: &[BString],
    timeout: Duration,
) -> Result<i32, String> {
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.clone(),
                    args: args.to_vec(),
                }],
                cwd: cwd.to_path_buf(),
                env: host.env_all(),
                stdin: Input::Inherit,
                stdout: Output::Inherit,
                stderr: Output::Inherit,
                timeout: Some(timeout),
                tee: false,
            },
            &mut |_, _| {},
        )
        .map_err(|error| error.to_string())?;
    Ok(match output.results.as_slice() {
        [ProcessResult::Exited(code)] => *code,
        _ => 1,
    })
}
