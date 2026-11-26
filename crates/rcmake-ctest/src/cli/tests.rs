use std::path::Path;

use rcmake_host::PureHost;
use rcmake_presets::{TestIndex, TestIndexRange};

use super::*;

fn parse_args(host: &dyn Host, args: &[&str]) -> Result<Invocation, String> {
    parse(
        &args.iter().copied().map(BString::from).collect::<Vec<_>>(),
        host,
    )
}

#[test]
fn reports_unknown_options_with_one_edit_suggestions() {
    let host = PureHost::default();
    for (argument, message) in [
        ("--verbos", "--verbos. Did you mean: --verbose?"),
        ("--quiat", "--quiat. Did you mean: --quiet?"),
        ("--quite", "--quite"),
        ("--verbose=", "--verbose=. Did you mean: --verbose?"),
        ("--nosuch", "--nosuch"),
    ] {
        assert_eq!(
            parse_args(&host, &[argument]).unwrap_err(),
            format!(
                "Unknown argument: {message}\nCMake Error: Run 'ctest --help' for all supported options."
            )
        );
    }
}

#[test]
fn reads_selection_and_execution_flags() -> Result<(), String> {
    let host = PureHost::default();
    let invocation = parse_args(
        &host,
        &[
            "-R",
            "^app",
            "-E=slow",
            "-L",
            "unit",
            "--label-regex",
            "fast",
            "-LE",
            "linux",
            "-LE",
            "net",
            "-FA",
            "both",
            "-FS",
            "setup",
            "-FC",
            "cleanup",
            "-I",
            "1,5,2,4",
            "-U",
            "--tests-from-file",
            "include.txt",
            "--exclude-from-file",
            "exclude.txt",
            "--rerun-failed",
            "--out-of-date",
            "-C",
            "Release",
            "-j4",
            "--timeout",
            "1.5",
            "--stop-on-failure",
            "--repeat=after-timeout:2",
            "--schedule-random",
            "--schedule-random-seed",
            "42",
            "--test-load",
            "5",
            "--stop-time",
            "13:02:03",
            "--test-dir",
            "out",
            "--no-tests=error",
        ],
    )?;
    assert_eq!(
        invocation.selection,
        SelectionOptions {
            indices: Some(IndexSource::Argument("1,5,2,4".into())),
            union: true,
            exclude_fixture: Some("both".into()),
            exclude_fixture_setup: Some("setup".into()),
            exclude_fixture_cleanup: Some("cleanup".into()),
            tests_from_file: Some("include.txt".into()),
            exclude_from_file: Some("exclude.txt".into()),
            rerun_failed: true,
            out_of_date: true,
            schedule_random: true,
            random_seed: Some(42),
        }
    );
    assert_eq!(
        invocation.options,
        CtestOptions {
            test_dir: "/work/out".into(),
            config: Some("Release".into()),
            include: Some("^app".into()),
            exclude: Some("slow".into()),
            include_labels: vec!["unit".into(), "fast".into()],
            exclude_labels: vec!["linux".into(), "net".into()],
            jobs: 4,
            timeout: Some(Duration::from_millis(1500)),
            stop_on_failure: true,
            error_on_no_tests: true,
            ..CtestOptions::default()
        }
    );
    assert_eq!(
        invocation.execution.repeat,
        Some(TestRepeat {
            mode: RepeatMode::AfterTimeout,
            count: 2
        })
    );
    assert_eq!(invocation.test_load, Some(5));
    assert_eq!(invocation.stop_time, Some(46_923));
    Ok(())
}

#[test]
fn reads_output_scripts_and_arguments_for_test_processes() -> Result<(), String> {
    let host = PureHost::default();
    let invocation = parse_args(
        &host,
        &[
            "-N",
            "--show-only=json-v1-raw",
            "--output-on-failure",
            "--output-junit",
            "junit.xml",
            "-O",
            "ctest.log",
            "-V",
            "-VV",
            "-Q",
            "--progress",
            "--print-labels",
            "--no-label-summary",
            "-T",
            "Test",
            "-D",
            "COUNT:STRING=1",
            "-DVAR=2",
            "-S",
            "run.cmake,arg",
            "-SP",
            "other.cmake",
            "--",
            "--help",
            "data",
        ],
    )?;
    assert_eq!(invocation.execution.show_only, Some(ShowOnly::JsonV1Raw));
    assert_eq!(
        invocation.execution.test_passthrough_arguments,
        Some(vec!["--help".into(), "data".into()])
    );
    assert_eq!(
        invocation.output,
        TestOutput {
            output_on_failure: Some(true),
            output_junit_file: Some("junit.xml".into()),
            output_log_file: Some("ctest.log".into()),
            verbosity: Some(TestVerbosity::Extra),
            quiet: Some(true),
            short_progress: Some(true),
            label_summary: Some(false),
            ..TestOutput::default()
        }
    );
    assert!(invocation.print_labels);
    assert_eq!(invocation.actions, ["test"]);
    assert_eq!(invocation.definitions, ["COUNT:STRING=1", "VAR=2"]);
    assert_eq!(
        invocation.scripts,
        [
            Script {
                argument: "run.cmake,arg".into(),
                new_process: false
            },
            Script {
                argument: "other.cmake".into(),
                new_process: true
            },
        ]
    );
    assert_eq!(
        parse_args(&host, &["--help"])?.mode,
        Mode::Help(vec!["--help".into()])
    );
    assert_eq!(
        parse_args(&host, &["--version"])?.mode,
        Mode::Version {
            json: false,
            file: None
        }
    );
    assert_eq!(
        parse_args(&host, &["--list-presets=defined"])?.mode,
        Mode::ListPresets { defined: true }
    );
    assert_eq!(
        parse_args(&host, &["--list-presets"])?.mode,
        Mode::ListPresets { defined: false }
    );
    assert_eq!(
        parse_args(
            &host,
            &["--instrument", "--role", "test", "--", "program", "--arg"]
        )?
        .mode,
        Mode::Instrument {
            args: vec!["--role".into(), "test".into()],
            command: vec!["program".into(), "--arg".into()]
        }
    );
    Ok(())
}

#[test]
fn accepts_repeat_modes_and_reports_invalid_values() -> Result<(), String> {
    let host = PureHost::default();
    for (args, mode) in [
        (["--repeat", "until-fail:3"], RepeatMode::UntilFail),
        (["--repeat", "until-pass:3"], RepeatMode::UntilPass),
        (["--repeat", "after-timeout:3"], RepeatMode::AfterTimeout),
        (["--repeat-until-fail", "3"], RepeatMode::UntilFail),
    ] {
        assert_eq!(
            parse_args(&host, &args)?.execution.repeat,
            Some(TestRepeat { mode, count: 3 })
        );
    }
    for (args, error) in [
        (
            vec!["--repeat", "bad:1"],
            "'--repeat' given invalid value 'bad:1'",
        ),
        (
            vec!["--repeat-until-fail", "bad"],
            "'--repeat-until-fail' given non-integer value 'bad'",
        ),
        (
            vec!["--repeat=until-fail:2", "--repeat=until-pass:1"],
            "At most one '--repeat' option may be used.",
        ),
        (
            vec!["--no-tests=action"],
            "'--no-tests=' given unknown value 'action'",
        ),
        (
            vec!["--show-only=unknown"],
            "'--show-only=' given unknown value 'unknown'",
        ),
        (vec!["-I"], "Invalid value used with -I"),
        (
            vec!["--parallel", "-1"],
            "'--parallel' given invalid value '-1'",
        ),
    ] {
        assert_eq!(parse_args(&host, &args).err().as_deref(), Some(error));
    }
    assert_eq!(
        parse_args(&host, &["--parallel", "0"])?.options.jobs,
        usize::MAX
    );
    assert!(parse_args(&host, &["-j"])?.options.jobs >= 2);
    assert_eq!(
        parse_args(&host, &["--test-load=bad", "--schedule-random-seed=bad"])?.test_load,
        None
    );
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "Invalid value for 'Test Load' : bad\nInvalid value for '--schedule-random-seed': bad\n"
    );
    Ok(())
}

#[test]
fn resolves_v13_presets_and_applies_cli_overrides() -> Result<(), String> {
    let host = PureHost::default();
    host.env_set(b"UNSET", Some(b"parent"));
    host.seed_file(Path::new("/work/CMakePresets.json"), br#"{
      "version": 13,
      "configurePresets": [{
        "name": "cfg", "generator": "Ninja", "binaryDir": "${sourceDir}/build",
        "environment": { "FROM_CONFIGURE": "1" }
      }],
      "testPresets": [{
        "name": "test", "configurePreset": "cfg", "configuration": "Debug",
        "environment": { "CTEST_OUTPUT_ON_FAILURE": "ON", "UNSET": null },
        "filter": {
          "include": { "name": "^test", "label": "smoke", "index": { "start": 2, "stride": 3 }, "useUnion": true },
          "exclude": { "name": "slow", "label": "network", "fixtures": { "setup": "setup" } }
        },
        "execution": {
          "jobs": 2, "stopOnFailure": true, "scheduleRandom": true, "testLoad": 4,
          "noTestsAction": "error", "timeout": 15, "showOnly": "json-v1",
          "repeat": { "mode": "until-pass", "count": 3 }, "testPassthroughArguments": ["preset"]
        },
        "output": { "outputJUnitFile": "preset.xml", "outputLogFile": "preset.log", "verbosity": "extra", "labelSummary": true }
      }]
    }"#).map_err(|error| error.to_string())?;
    let invocation = parse_args(
        &host,
        &[
            "--preset=test",
            "-C",
            "Release",
            "-E",
            "manual",
            "-L",
            "quick",
            "--parallel",
            "7",
            "--no-tests=ignore",
            "--output-junit",
            "cli.xml",
            "--no-label-summary",
            "--schedule-random-seed",
            "3",
        ],
    )?;
    assert_eq!(invocation.options.test_dir, Path::new("/work/build"));
    assert_eq!(invocation.options.config, Some("Release".into()));
    assert_eq!(invocation.options.include, Some("^test".into()));
    assert_eq!(invocation.options.exclude, Some("manual".into()));
    assert_eq!(invocation.options.include_labels, ["smoke", "quick"]);
    assert_eq!(invocation.options.exclude_labels, ["network"]);
    assert_eq!(invocation.options.jobs, 7);
    assert!(invocation.options.stop_on_failure);
    assert!(invocation.options.ignore_no_tests);
    assert!(invocation.options.output_on_failure);
    assert_eq!(invocation.options.timeout, Some(Duration::from_secs(15)));
    assert_eq!(
        invocation.selection.indices,
        Some(IndexSource::Preset(TestIndex::Range(TestIndexRange {
            start: Some(2),
            stride: Some(3),
            ..TestIndexRange::default()
        })))
    );
    assert_eq!(
        invocation.selection.exclude_fixture_setup,
        Some("setup".into())
    );
    assert!(invocation.selection.union);
    assert!(invocation.selection.schedule_random);
    assert_eq!(invocation.selection.random_seed, Some(3));
    assert_eq!(invocation.test_load, Some(4));
    assert_eq!(invocation.output.output_junit_file, Some("cli.xml".into()));
    assert_eq!(invocation.output.output_log_file, Some("preset.log".into()));
    assert_eq!(invocation.output.label_summary, Some(false));
    assert_eq!(invocation.output.verbosity, Some(TestVerbosity::Extra));
    assert_eq!(invocation.execution.show_only, Some(ShowOnly::JsonV1));
    assert_eq!(
        invocation.execution.repeat,
        Some(TestRepeat {
            mode: RepeatMode::UntilPass,
            count: 3
        })
    );
    assert_eq!(
        invocation.execution.test_passthrough_arguments,
        Some(vec!["preset".into()])
    );
    assert_eq!(
        invocation.environment.get(b"FROM_CONFIGURE".as_slice()),
        Some(&Some("1".into()))
    );
    assert_eq!(invocation.environment.get(b"UNSET".as_slice()), Some(&None));
    let overridden = parse_args(
        &host,
        &["--preset", "test", "--test-dir", "local", "--", "cli"],
    )?;
    assert_eq!(overridden.options.test_dir, Path::new("/work/local"));
    assert_eq!(
        overridden.execution.test_passthrough_arguments,
        Some(vec!["preset".into(), "cli".into()])
    );
    Ok(())
}

#[test]
fn help_lists_required_options_and_every_long_flag_is_accepted_by_parser() {
    let host = std::sync::Arc::new(PureHost::default());
    let argv = [BString::from("ctest"), BString::from("--help")];
    assert_eq!(crate::main(&argv, host.clone()), 0);
    let help =
        String::from_utf8_lossy(&host.stream_bytes(rcmake_host::Stream::Stdout)).into_owned();
    for required in [
        "--tests-regex",
        "--output-on-failure",
        "--test-dir",
        "--preset",
        "-R",
        "-E",
        "-L",
        "-j",
        "--rerun-failed",
    ] {
        assert!(help.contains(required), "missing {required} in:\n{help}");
    }
    let mut long_flags = std::collections::BTreeSet::new();
    for line in help.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('-') {
            continue;
        }
        let lhs = trimmed.split('=').next().unwrap_or(trimmed);
        for part in lhs.split(',') {
            let token = part.split_whitespace().next().unwrap_or_default();
            let flag = token.split(['=', '[']).next().unwrap_or_default().trim();
            if flag.starts_with("--") && flag.len() > 2 {
                long_flags.insert(flag.to_owned());
            }
        }
    }
    assert!(!long_flags.is_empty());
    for flag in long_flags {
        let result = parse_args(host.as_ref(), &[flag.as_str(), "dummy"]);
        if let Err(err) = result {
            assert!(
                !err.contains("Unknown argument"),
                "documented flag {flag} rejected as unknown: {err}"
            );
        }
    }
}
