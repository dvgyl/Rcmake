use std::error::Error;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rcmake_host::{
    Host, HostError, Output, ProcessResult, PureHost, RunOutput, RunRequest, Stream,
};

use crate::{CtestError, CtestOptions, run};

type TestResult = Result<(), Box<dyn Error>>;

fn host(script: &str) -> Result<Arc<PureHost>, HostError> {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/CTestTestfile.cmake"), script.as_bytes())?;
    Ok(host)
}

fn output(status: ProcessResult, stdout: &[u8], stderr: &[u8]) -> RunOutput {
    RunOutput {
        results: vec![status],
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    }
}

fn commands(host: &PureHost) -> Result<Arc<Mutex<Vec<RunRequest>>>, HostError> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    host.set_command_handler(move |_, request, _| {
        seen.lock()
            .map_err(|_| HostError::unsupported("Cannot record command."))?
            .push(request.clone());
        Ok(output(ProcessResult::Exited(0), b"", b""))
    })?;
    Ok(requests)
}

#[test]
fn reads_subdirectories_and_interprets_the_test_files() -> TestResult {
    let host = host(
        r#"
        set(prefix root)
        add_test(before echo ${prefix})
        subdirs(child missing)
        add_test(after echo ${prefix})
    "#,
    )?;
    host.seed_file(
        Path::new("/work/child/CTestTestfile.cmake"),
        br#"
        include(helper.cmake)
        if(CTEST_CONFIGURATION_TYPE MATCHES "^[Dd]ebug$")
            foreach(arg IN ITEMS first second)
                add_test([=[child;test]=] "echo" "${arg}" "${prefix}")
            endforeach()
        elseif(CTEST_CONFIGURATION_TYPE STREQUAL Release)
            add_test(release echo)
        else()
            add_test(other echo)
        endif()
        subdirs(nested)
    "#,
    )?;
    host.seed_file(Path::new("/work/child/helper.cmake"), b"set(prefix child)")?;
    host.seed_file(
        Path::new("/work/child/nested/CTestTestfile.cmake"),
        b"add_test(nested echo)",
    )?;
    let requests = commands(&host)?;
    let options = CtestOptions {
        config: Some("Debug".into()),
        ..CtestOptions::default()
    };

    let outcome = run(host.clone(), &options)?;
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(
        outcome.passed,
        ["before", "child;test", "child;test", "nested", "after"]
    );
    let requests = requests.lock().map_err(|_| "Cannot read commands.")?;
    let [before, first, second, nested, after] = requests.as_slice() else {
        return Err("Expected five commands.".into());
    };
    assert_eq!(
        before.pipeline.first().map(|cmd| &cmd.args),
        Some(&vec!["root".into()])
    );
    assert_eq!(
        first.pipeline.first().map(|cmd| &cmd.args),
        Some(&vec!["first".into(), "child".into()])
    );
    assert_eq!(
        second.pipeline.first().map(|cmd| &cmd.args),
        Some(&vec!["second".into(), "child".into()])
    );
    assert_eq!(nested.cwd, Path::new("/work/child/nested"));
    assert_eq!(after.cwd, Path::new("/work"));
    assert_eq!(
        after.pipeline.first().map(|cmd| &cmd.args),
        Some(&vec!["child".into()])
    );
    Ok(())
}

#[test]
fn filters_by_name_label_and_configuration() -> TestResult {
    let host = host(
        r#"
        add_test(other run)
        add_test(unit-slow run)
        add_test(unit-release run)
        add_test(unit-fast run)
        add_test(unit-excluded run)
        add_test(unit-unlabeled run)
        set_tests_properties(unit-slow PROPERTIES LABELS "fast;network;integration")
        set_tests_properties(unit-release PROPERTIES LABELS "fast;unit" CONFIGURATIONS Release)
        set_tests_properties(unit-fast PROPERTIES LABELS "fast;unit" CONFIGURATIONS "Debug;RelWithDebInfo")
        set_tests_properties(unit-excluded PROPERTIES LABELS "fast;unit")
    "#,
    )?;
    let requests = commands(&host)?;
    let options = CtestOptions {
        config: Some("debug".into()),
        include: Some("^unit-".into()),
        exclude: Some("-excluded$".into()),
        include_labels: vec!["^fast$".into()],
        exclude_labels: vec!["^network$".into(), "^integration$".into()],
        ..CtestOptions::default()
    };
    let outcome = run(host.clone(), &options)?;
    assert_eq!(outcome.passed, ["unit-fast"]);
    assert_eq!(
        requests.lock().map_err(|_| "Cannot read commands.")?.len(),
        1
    );
    let stdout = host.stream_bytes(Stream::Stdout);
    assert!(stdout.to_string().contains("1/1 Test #4: unit-fast"));
    assert!(stdout.to_string().contains("100% tests passed out of 1"));

    let options = CtestOptions {
        include: Some("[".into()),
        ..options
    };
    assert!(matches!(run(host, &options), Err(CtestError::BadRegex(_))));
    Ok(())
}

#[test]
fn evaluates_output_exit_codes_and_abnormal_process_results() -> TestResult {
    let host = host(
        r#"
        add_test(inverted fail)
        add_test(unexpected-success zero)
        add_test(required-match fail)
        add_test(missing-match zero)
        add_test(forbidden-match zero)
        add_test(regex-skip zero)
        add_test(return-skip skip)
        add_test(signal signal)
        add_test(timeout timeout)
        add_test(disabled fail)
        set_tests_properties(inverted unexpected-success signal timeout PROPERTIES WILL_FAIL YES)
        set_tests_properties(required-match PROPERTIES PASS_REGULAR_EXPRESSION "missing;warning")
        set_tests_properties(missing-match PROPERTIES PASS_REGULAR_EXPRESSION missing)
        set_tests_properties(forbidden-match PROPERTIES PASS_REGULAR_EXPRESSION diagnostic FAIL_REGULAR_EXPRESSION warning)
        set_tests_properties(regex-skip PROPERTIES SKIP_REGULAR_EXPRESSION warning FAIL_REGULAR_EXPRESSION warning)
        set_tests_properties(return-skip PROPERTIES SKIP_RETURN_CODE 77)
        set_tests_properties(signal PROPERTIES PASS_REGULAR_EXPRESSION warning)
        set_tests_properties(timeout PROPERTIES TIMEOUT 0.25 PASS_REGULAR_EXPRESSION warning)
        set_tests_properties(disabled PROPERTIES DISABLED ON)
    "#,
    )?;
    host.set_command_handler(|_, request, _| {
        if request.stderr != Output::SharedWithStdout {
            return Err(HostError::unsupported("Expected one output stream."));
        }
        let program = request.pipeline.first().map(|cmd| cmd.program.as_slice());
        let result = match program {
            Some(b"fail") => ProcessResult::Exited(1),
            Some(b"skip") => ProcessResult::Exited(77),
            Some(b"signal") => ProcessResult::Signaled(11),
            Some(b"timeout") if request.timeout == Some(Duration::from_millis(250)) => {
                ProcessResult::TimedOut
            }
            _ if request.timeout == Some(Duration::from_secs(2)) => ProcessResult::Exited(0),
            _ => return Err(HostError::unsupported("Incorrect timeout.")),
        };
        Ok(output(result, b"diagnostic\n", b"warning\n"))
    })?;
    let options = CtestOptions {
        timeout: Some(Duration::from_secs(2)),
        ..CtestOptions::default()
    };
    let outcome = run(host.clone(), &options)?;
    assert_eq!(outcome.exit_code, 8);
    assert_eq!(outcome.passed, ["inverted", "required-match"]);
    assert_eq!(
        outcome.failed,
        [
            "unexpected-success",
            "missing-match",
            "forbidden-match",
            "signal",
            "timeout"
        ]
    );
    let text = host.stream_bytes(Stream::Stdout).to_string();
    assert!(text.contains("***Timeout"));
    assert!(text.contains("***Not Run (Disabled)"));
    assert_eq!(text.matches("***Skipped").count(), 2);
    assert!(!text.contains("\ndiagnostic\n"));
    Ok(())
}

#[test]
fn sets_the_working_directory_and_isolates_environment_changes() -> TestResult {
    let host = host(
        r#"
        add_test(first ./local-run "" "a;b")
        set_tests_properties(first PROPERTIES
            WORKING_DIRECTORY "run"
            ENVIRONMENT "VAR=a;PATH=/base;LIST=a"
            ENVIRONMENT_MODIFICATION "VAR=string_append:b;VAR=reset:;VAR=string_prepend:c;PATH=path_list_append:/b;LIST=cmake_list_prepend:z;DROP=unset:;NEW=set:v")
        add_test(second echo)
    "#,
    )?;
    host.seed_file(Path::new("/work/run/local-run"), b"")?;
    host.env_set(b"VAR", Some(b"host"));
    host.env_set(b"DROP", Some(b"present"));
    let requests = commands(&host)?;
    assert_eq!(run(host.clone(), &CtestOptions::default())?.exit_code, 0);
    let requests = requests.lock().map_err(|_| "Cannot read commands.")?;
    let [first, second] = requests.as_slice() else {
        return Err("Expected two commands.".into());
    };
    assert_eq!(first.cwd, Path::new("/work/run"));
    let command = first.pipeline.first().ok_or("Expected a command.")?;
    assert_eq!(
        rcmake_host::path::to_native(&command.program),
        Path::new("/work/run/local-run")
    );
    assert_eq!(command.args, ["", "a;b"]);
    assert_eq!(
        first.env,
        vec![
            ("LIST".into(), "z;a".into()),
            ("NEW".into(), "v".into()),
            (
                "PATH".into(),
                if host.system().sysname == "Windows" {
                    "/base;/b"
                } else {
                    "/base:/b"
                }
                .into()
            ),
            ("VAR".into(), "ca".into()),
        ]
    );
    assert_eq!(
        second.env,
        vec![
            ("DROP".into(), "present".into()),
            ("VAR".into(), "host".into())
        ]
    );
    assert_eq!(host.env_get(b"VAR"), Some("host".into()));
    Ok(())
}

#[test]
fn adds_fixture_tests_and_cleans_up_after_setup_failure() -> TestResult {
    for stop_on_failure in [false, true] {
        let host = host(
            r#"
            add_test(use-db use)
            add_test(teardown clean)
            add_test(setup setup)
            set_tests_properties(use-db PROPERTIES FIXTURES_REQUIRED db)
            set_tests_properties(teardown PROPERTIES FIXTURES_CLEANUP db)
            set_tests_properties(setup PROPERTIES FIXTURES_SETUP db)
        "#,
        )?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let called = seen.clone();
        host.set_command_handler(move |_, request, _| {
            let command = request
                .pipeline
                .first()
                .ok_or_else(|| HostError::unsupported("No command."))?;
            called
                .lock()
                .map_err(|_| HostError::unsupported("Cannot record command."))?
                .push(command.program.clone());
            Ok(output(
                ProcessResult::Exited(i32::from(command.program == "setup")),
                b"",
                b"",
            ))
        })?;
        let options = CtestOptions {
            include: Some("^use-db$".into()),
            jobs: 2,
            stop_on_failure,
            ..CtestOptions::default()
        };
        let outcome = run(host.clone(), &options)?;
        let seen = seen.lock().map_err(|_| "Cannot read commands.")?;
        assert_eq!(outcome.exit_code, 8);
        if stop_on_failure {
            assert_eq!(*seen, ["setup"]);
            assert!(outcome.passed.is_empty());
            assert_eq!(outcome.failed, ["setup"]);
        } else {
            assert_eq!(*seen, ["setup", "clean"]);
            assert_eq!(outcome.passed, ["teardown"]);
            assert_eq!(outcome.failed, ["use-db", "setup"]);
            assert!(
                host.stream_bytes(Stream::Stdout)
                    .to_string()
                    .contains("Failed test dependencies: setup")
            );
        }
    }
    Ok(())
}

#[test]
fn orders_tests_after_fixture_setup_and_named_dependencies() -> TestResult {
    let host = host(
        r#"
        add_test(clean clean)
        add_test(use-db use)
        add_test(other independent)
        add_test(setup setup)
        set_tests_properties(clean PROPERTIES FIXTURES_CLEANUP db)
        set_tests_properties(use-db PROPERTIES FIXTURES_REQUIRED db DEPENDS other)
        set_tests_properties(setup PROPERTIES FIXTURES_SETUP db)
    "#,
    )?;
    let requests = commands(&host)?;
    let outcome = run(host, &CtestOptions::default())?;
    assert_eq!(outcome.exit_code, 0);
    let requests = requests.lock().map_err(|_| "Cannot read commands.")?;
    let sequence: Vec<_> = requests
        .iter()
        .filter_map(|req| req.pipeline.first())
        .map(|cmd| cmd.program.as_slice())
        .collect();
    assert_eq!(
        sequence,
        [b"independent".as_slice(), b"setup", b"use", b"clean"]
    );
    Ok(())
}

#[test]
fn runs_two_independent_tests_at_the_same_time() -> TestResult {
    let host = host(
        "add_test(first first)\nadd_test(second second)\nadd_test(last last)\nset_tests_properties(last PROPERTIES DEPENDS \"first;second\")",
    )?;
    let concurrent = Arc::new((Mutex::new(0), Condvar::new()));
    host.set_command_handler(move |_, request, _| {
        let (count, signal) = &*concurrent;
        let mut count = count
            .lock()
            .map_err(|_| HostError::unsupported("Cannot record concurrency."))?;
        let program = request.pipeline.first().map(|cmd| cmd.program.as_slice());
        if program == Some(b"last") {
            return Ok(output(
                ProcessResult::Exited(i32::from(*count != 2)),
                b"",
                b"",
            ));
        }
        *count += 1;
        signal.notify_all();
        let (_count, range) = signal
            .wait_timeout_while(count, Duration::from_secs(3), |count| *count < 2)
            .map_err(|_| HostError::unsupported("Cannot observe concurrency."))?;
        Ok(output(
            ProcessResult::Exited(i32::from(range.timed_out())),
            b"",
            b"",
        ))
    })?;
    let outcome = run(
        host,
        &CtestOptions {
            jobs: 2,
            ..CtestOptions::default()
        },
    )?;
    assert_eq!(outcome.passed, ["first", "second", "last"]);
    Ok(())
}

#[test]
fn prints_failed_output_and_respects_stop_and_quiet_options() -> TestResult {
    for (verbose, quiet) in [(false, false), (true, false), (true, true)] {
        let host = host("add_test(pass ok)\nadd_test(fail bad)\nadd_test(after ok)")?;
        host.set_command_handler(|_, request, _| {
            if request
                .pipeline
                .first()
                .is_some_and(|cmd| cmd.program == "bad")
            {
                Ok(output(ProcessResult::Exited(4), b"failure details\n", b""))
            } else {
                Ok(output(ProcessResult::Exited(0), b"success details\n", b""))
            }
        })?;
        let options = CtestOptions {
            output_on_failure: !verbose,
            verbose,
            quiet,
            stop_on_failure: true,
            ..CtestOptions::default()
        };
        let outcome = run(host.clone(), &options)?;
        assert_eq!(outcome.exit_code, 8);
        assert_eq!(outcome.passed, ["pass"]);
        assert_eq!(outcome.failed, ["fail"]);
        let stdout = host.stream_bytes(Stream::Stdout).to_string();
        assert_eq!(stdout.contains("failure details"), !quiet);
        assert_eq!(stdout.contains("success details"), verbose && !quiet);
        assert_eq!(stdout.contains("The following tests FAILED:"), !quiet);
        if quiet {
            assert!(host.events().is_empty());
        }
    }
    Ok(())
}

#[test]
fn lists_tests_without_executing_and_handles_an_empty_selection() -> TestResult {
    let host = host("add_test(one run)\nadd_test(two run)")?;
    let requests = commands(&host)?;
    let options = CtestOptions {
        show_only: true,
        include: Some("^two$".into()),
        ..CtestOptions::default()
    };
    assert_eq!(run(host.clone(), &options)?.exit_code, 0);
    let stdout = host.stream_bytes(Stream::Stdout).to_string();
    assert!(stdout.contains("  Test #2: two"));
    assert!(stdout.contains("Total Tests: 1"));
    let options = CtestOptions {
        show_only: false,
        include: Some("absent".into()),
        ..options
    };
    assert_eq!(run(host.clone(), &options)?.exit_code, 0);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("No tests were found!!!")
    );
    let options = CtestOptions {
        error_on_no_tests: true,
        ..options
    };
    assert_eq!(run(host.clone(), &options)?.exit_code, 8);
    assert!(
        requests
            .lock()
            .map_err(|_| "Cannot read commands.")?
            .is_empty()
    );
    Ok(())
}

#[test]
fn rejects_a_dependency_cycle_before_starting_any_test() -> TestResult {
    let host = host(
        r#"
        add_test(a run)
        add_test(b run)
        set_tests_properties(a PROPERTIES DEPENDS b)
        set_tests_properties(b PROPERTIES DEPENDS a)
    "#,
    )?;
    let requests = commands(&host)?;
    assert_eq!(run(host.clone(), &CtestOptions::default())?.exit_code, 8);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("cycle")
    );
    assert!(
        requests
            .lock()
            .map_err(|_| "Cannot read commands.")?
            .is_empty()
    );
    Ok(())
}

mod runner;

#[test]
fn build_and_test_rejects_ctest_passthrough() -> TestResult {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let host = Arc::new(PureHost::new(root.into(), Path::new(root).join("ctest")));
    let argv = [
        "ctest",
        "--build-and-test",
        "source",
        "build",
        "--build-generator",
        "Ninja",
        "--",
    ]
    .map(rcmake_base::BString::from);
    assert_eq!(crate::main(&argv, host.clone()), 1);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "CMake Error: The -- option cannot be used with --build-and-test. Use --build-options or --test-command to forward arguments.\n"
    );
    Ok(())
}
