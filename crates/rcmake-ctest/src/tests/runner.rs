use std::error::Error;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, HostError, HostResult, ProcessResult, PureHost, RunOutput, Stream, path};
use rcmake_json::Value;

use crate::{discovery, main, state};

type TestResult = Result<(), Box<dyn Error>>;

fn tree(script: &str) -> HostResult<Arc<PureHost>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/CTestTestfile.cmake"), script.as_bytes())?;
    Ok(host)
}

fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
    let argv: Vec<_> = std::iter::once("ctest")
        .chain(args.iter().copied())
        .map(BString::from)
        .collect();
    main(&argv, host.clone())
}

fn output(status: ProcessResult, stdout: &[u8]) -> RunOutput {
    RunOutput {
        results: vec![status],
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}

#[test]
fn reports_the_text_version_after_options() {
    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["--quiet", "--version"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        concat!(
            "ctest version 4.5.1\nRcmake ",
            env!("CARGO_PKG_VERSION"),
            "\n"
        )
    );
}

#[test]
fn evaluates_generated_test_includes() -> TestResult {
    let host = tree("include(\"/work/test_include.cmake\")\n")?;
    host.seed_file(
        Path::new("/work/test_include.cmake"),
        br#"
        function(register prefix)
            execute_process(COMMAND list-cases OUTPUT_VARIABLE cases)
            string(STRIP "${cases}" cases)
            foreach(case IN LISTS cases)
                add_test("${prefix}-${case}" run-case "${case}")
                set_tests_properties("${prefix}-${case}" PROPERTIES LABELS included)
            endforeach()
        endfunction()
        register(generated)
    "#,
    )?;
    host.set_command_handler(|_, request, _| {
        assert_eq!(request.pipeline[0].program, "list-cases");
        Ok(output(ProcessResult::Exited(0), b"one;two\n"))
    })?;
    assert_eq!(invoke(&host, &["-N", "-L", "^included$"]), 0);
    let stdout = String::from_utf8(host.stream_bytes(Stream::Stdout).to_vec())?;
    assert!(stdout.contains("Test #1: generated-one"));
    assert!(stdout.contains("Test #2: generated-two"));
    Ok(())
}

#[test]
fn discovers_tests_in_show_only_mode() -> TestResult {
    let host = tree(
        r#"
        discover_tests(
            COMMAND test-binary base
            DISCOVERY_ARGS --list
            DISCOVERY_MATCH "case:([0-9]+)"
            DISCOVERY_PROPERTIES ENVIRONMENT "TYPE=listing"
            TEST_NAME [[case-\1]]
            TEST_ARGS [[--id=\1]] [[\0]]
            TEST_PROPERTIES LABELS [[group-\1]]
            BUILD_DEPENDS /work/bin
            TEST_LIST names
        )
        foreach(test IN LISTS names)
            set_tests_properties("${test}" PROPERTIES RESOURCE_LOCK db)
        endforeach()
    "#,
    )?;
    host.set_command_handler(|_, request, _| {
        assert_eq!(request.pipeline[0].program, "test-binary");
        assert_eq!(request.pipeline[0].args, ["base", "--list"]);
        assert_eq!(request.env, [("TYPE".into(), "listing".into())]);
        Ok(output(
            ProcessResult::Exited(0),
            b"case:3\r\nnoise case:4\ncase:8\n",
        ))
    })?;
    let tests = discovery::load(host.clone(), Path::new("/work"), None)?;
    assert_eq!(tests.len(), 2);
    assert_eq!(tests[0].name, "case-3");
    assert_eq!(tests[0].command.args, ["base", "--id=3", "case:3"]);
    assert_eq!(tests[0].list(b"LABELS"), ["group-3"]);
    assert_eq!(
        tests[0].property(b"_CMAKE_TEST_BUILD_DEPENDS"),
        b"/work/bin"
    );
    assert_eq!(tests[1].property(b"RESOURCE_LOCK"), b"db");
    assert_eq!(invoke(&host, &["--show-only=json-v1"]), 0);
    let stdout = host.stream_bytes(Stream::Stdout);
    let json = rcmake_json::parse(&stdout, &Default::default())?;
    assert!(matches!(json, Value::Object(_)));
    Ok(())
}

#[test]
fn repeats_until_the_selected_result() -> TestResult {
    for (mode, first, second, code) in [
        (
            "until-fail:3",
            ProcessResult::Exited(0),
            ProcessResult::Exited(9),
            8,
        ),
        (
            "until-pass:3",
            ProcessResult::Exited(9),
            ProcessResult::Exited(0),
            0,
        ),
        (
            "after-timeout:3",
            ProcessResult::TimedOut,
            ProcessResult::Exited(0),
            0,
        ),
    ] {
        let host = tree("add_test(test runner)")?;
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        host.set_command_handler(move |_, _, _| {
            let status = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                &first
            } else {
                &second
            };
            Ok(output(status.clone(), b"attempt output"))
        })?;
        assert_eq!(invoke(&host, &["--repeat", mode]), code);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let log = state::last_log(host.as_ref(), Path::new("/work"))?.ok_or("Missing test log")?;
        assert_eq!(String::from_utf8(log)?.matches("attempt output").count(), 2);
    }
    Ok(())
}

#[test]
fn repeats_fixture_lifecycles_with_the_selected_mode() -> TestResult {
    for (mode, expected) in [
        (
            "AROUND_EACH_REPEAT",
            &["setup", "run", "clean", "setup", "run", "clean", "after"][..],
        ),
        (
            "AROUND_ALL_REPEATS",
            &["setup", "run", "run", "clean", "after", "after"][..],
        ),
        (
            "EACH_TEST_SEPARATELY",
            &[
                "setup", "setup", "run", "run", "clean", "clean", "after", "after",
            ][..],
        ),
    ] {
        let host = tree(&format!(
            r#"
            add_test(setup runner setup)
            add_test(run runner run)
            add_test(clean runner clean)
            add_test(after runner after)
            set_tests_properties(setup PROPERTIES FIXTURES_SETUP db _CMAKE_DEFAULT_FIXTURE_REPEAT_MODE {mode})
            set_tests_properties(run PROPERTIES FIXTURES_REQUIRED db)
            set_tests_properties(clean PROPERTIES FIXTURES_CLEANUP db)
            set_tests_properties(after PROPERTIES DEPENDS run)
        "#
        ))?;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        host.set_command_handler(move |_, request, _| {
            seen.lock()
                .map_err(|_| HostError::unsupported("Cannot record command."))?
                .push(request.pipeline[0].args[0].clone());
            let code = if mode == "AROUND_EACH_REPEAT" && request.pipeline[0].args == ["after"] {
                1
            } else {
                0
            };
            Ok(output(ProcessResult::Exited(code), b""))
        })?;
        assert_eq!(
            invoke(&host, &["--repeat", "until-fail:2"]),
            i32::from(mode == "AROUND_EACH_REPEAT") * 8
        );
        assert_eq!(
            calls
                .lock()
                .map_err(|_| "Cannot read commands.")?
                .as_slice(),
            expected
        );
    }
    Ok(())
}

#[test]
fn finishes_seeded_fixture_dependency_graphs_with_repeats() -> TestResult {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;

    for seed in 1..=32_u32 {
        let mut state = seed;
        let mut choose = |limit| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state % limit
        };
        let (repeat, first_result) = match seed % 3 {
            0 => ("until-fail", ProcessResult::Exited(0)),
            1 => ("until-pass", ProcessResult::Exited(1)),
            _ => ("after-timeout", ProcessResult::TimedOut),
        };
        let repeat = format!("{repeat}:{}", 2 + choose(3));
        let mut script = String::new();
        let group_count = 2 + choose(3);
        let mut test_count = 0;
        for group in 0..group_count {
            let mode = match choose(3) {
                0 => "AROUND_EACH_REPEAT",
                1 => "AROUND_ALL_REPEATS",
                _ => "EACH_TEST_SEPARATELY",
            };
            writeln!(script, "add_test(s{group} runner s{group})")?;
            writeln!(script, "add_test(c{group} runner c{group})")?;
            writeln!(
                script,
                "set_tests_properties(s{group} PROPERTIES FIXTURES_SETUP f{group} FIXTURE_REPEAT_MODE {mode})\n\
                 set_tests_properties(c{group} PROPERTIES FIXTURES_CLEANUP f{group})"
            )?;
            if group > 0 && choose(2) == 0 {
                writeln!(
                    script,
                    "set_tests_properties(s{group} PROPERTIES DEPENDS c{})",
                    choose(group)
                )?;
            }
            let members = 2 + choose(4);
            let retry_member = choose(members);
            for member in 0..members {
                let name = format!("g{group}t{member}");
                let marker = if member == retry_member { " retry" } else { "" };
                writeln!(script, "add_test({name} runner {name}{marker})")?;
                writeln!(
                    script,
                    "set_tests_properties({name} PROPERTIES FIXTURES_REQUIRED f{group})"
                )?;
                if member > 0 {
                    writeln!(
                        script,
                        "set_tests_properties({name} PROPERTIES DEPENDS g{group}t{})",
                        choose(member)
                    )?;
                }
            }
            test_count += members + 2;
        }
        let host = tree(&script)?;
        let retries = Mutex::new(BTreeSet::new());
        host.set_command_handler(move |_, request, _| {
            let [command] = request.pipeline.as_slice() else {
                return Err(HostError::unsupported("Expected one test command"));
            };
            let result = if let [name, _] = command.args.as_slice()
                && retries
                    .lock()
                    .map_err(|_| HostError::unsupported("Cannot record test attempts"))?
                    .insert(name.clone())
            {
                first_result.clone()
            } else {
                ProcessResult::Exited(0)
            };
            Ok(output(result, b""))
        })?;
        assert_eq!(
            invoke(
                &host,
                &[
                    "--parallel",
                    "4",
                    "--schedule-random",
                    "--schedule-random-seed",
                    &seed.to_string(),
                    "--repeat",
                    &repeat,
                    "--output-junit",
                    "results.xml"
                ],
            ),
            0,
            "seed {seed}, repeat {repeat}: {}",
            host.stream_bytes(Stream::Stderr)
        );
        let report = state::read_optional(host.as_ref(), Path::new("/work/results.xml"))?
            .ok_or("Missing test report")?;
        assert_eq!(
            String::from_utf8(report)?.matches("<testcase ").count(),
            test_count as usize,
            "seed {seed}, repeat {repeat}"
        );
    }
    Ok(())
}

#[test]
fn reruns_all_when_the_failure_log_is_empty_and_keeps_previous_failures() -> TestResult {
    let host = tree("add_test(first runner first)\nadd_test(second runner second)")?;
    let failing = Arc::new(AtomicBool::new(true));
    let fail = failing.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    host.set_command_handler(move |_, request, _| {
        let name = &request.pipeline[0].args[0];
        seen.lock()
            .map_err(|_| HostError::unsupported("Cannot record command."))?
            .push(name.clone());
        let failure = name == "second" && fail.load(Ordering::SeqCst);
        Ok(output(ProcessResult::Exited(i32::from(failure)), b""))
    })?;
    assert_eq!(invoke(&host, &["--rerun-failed", "-R", "no-match"]), 8);
    failing.store(false, Ordering::SeqCst);
    assert_eq!(invoke(&host, &["--rerun-failed"]), 0);
    assert_eq!(invoke(&host, &["--rerun-failed"]), 0);
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "Cannot read commands.")?
            .as_slice(),
        ["first", "second", "second", "second"]
    );
    assert!(state::read_stamp(host.as_ref(), Path::new("/work"), b"second")?.is_some());
    Ok(())
}

#[test]
fn limits_workers_by_processor_slots_and_exclusive_properties() -> TestResult {
    let host = tree(
        r#"
        add_test(a runner 2 db)
        add_test(b runner 1 db)
        add_test(c runner 1 serial)
        add_test(d runner 2 free)
        set_tests_properties(a PROPERTIES PROCESSORS 2 RESOURCE_LOCK db)
        set_tests_properties(b PROPERTIES RESOURCE_LOCK db)
        set_tests_properties(c PROPERTIES RUN_SERIAL ON)
        set_tests_properties(d PROPERTIES PROCESSORS 2)
    "#,
    )?;
    let active = Arc::new(Mutex::new((0, false, false)));
    host.set_command_handler(move |_, request, _| {
        let command = &request.pipeline[0];
        let slots: usize = command.args[0].to_string().parse().unwrap_or_default();
        let lock = command.args[1] == "db";
        let serial = command.args[1] == "serial";
        {
            let mut state = active
                .lock()
                .map_err(|_| HostError::unsupported("Cannot record command."))?;
            assert!(state.0 + slots <= 3);
            assert!(!(state.1 && lock));
            assert!(!state.2);
            assert!(!serial || state.0 == 0);
            state.0 += slots;
            state.1 |= lock;
            state.2 |= serial;
        }
        std::thread::sleep(Duration::from_millis(20));
        let mut state = active
            .lock()
            .map_err(|_| HostError::unsupported("Cannot record command."))?;
        state.0 -= slots;
        state.1 &= !lock;
        state.2 &= !serial;
        Ok(output(ProcessResult::Exited(0), b""))
    })?;
    assert_eq!(invoke(&host, &["--parallel", "3"]), 0);
    Ok(())
}

#[test]
fn test_load_waits_until_the_stop_time() -> TestResult {
    let host = tree("add_test(test runner)")?;
    host.seed_file(Path::new("/proc/loadavg"), b"4.1 4.0 4.0 1/2 10\n")?;
    let started = Arc::new(AtomicBool::new(false));
    let seen = started.clone();
    host.set_command_handler(move |_, _, _| {
        seen.store(true, Ordering::SeqCst);
        Ok(output(ProcessResult::Exited(0), b""))
    })?;
    assert_eq!(
        invoke(&host, &["--test-load", "5", "--stop-time", "22:13:22"]),
        0
    );
    assert_eq!(host.monotonic().as_secs(), 2);
    assert!(!started.load(Ordering::SeqCst));
    Ok(())
}

#[test]
fn stop_time_caps_the_process_timeout() -> TestResult {
    let host = tree("add_test(test runner)\nadd_test(late runner)")?;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    host.set_command_handler(move |host, request, _| {
        let timeout = request
            .timeout
            .ok_or_else(|| HostError::unsupported("Missing timeout."))?;
        assert!((Duration::from_millis(1999)..=Duration::from_secs(2)).contains(&timeout));
        seen.fetch_add(1, Ordering::SeqCst);
        host.sleep(timeout);
        Ok(output(ProcessResult::TimedOut, b""))
    })?;
    assert_eq!(
        invoke(
            &host,
            &["--stop-time", "22:13:22", "--repeat", "after-timeout:3"]
        ),
        8
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn calls_post_ctest_after_the_test() -> TestResult {
    let host = tree("add_test(test runner)")?;
    host.seed_file(
        Path::new("/work/.cmake/instrumentation/v1/query/test.json"),
        br#"{"version":1,"hooks":["postCTest"],"callbacks":["callback"]}"#,
    )?;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    host.set_command_handler(move |host, request, _| {
        let command = &request.pipeline[0];
        seen.lock()
            .map_err(|_| HostError::unsupported("Cannot record command."))?
            .push(command.program.clone());
        if command.program == "callback" {
            let file = command
                .args
                .first()
                .ok_or_else(|| HostError::unsupported("Missing index file."))?;
            let index = host.read(
                &path::to_native(file),
                rcmake_host::ReadRange::All { limit: 4096 },
            )?;
            assert!(String::from_utf8_lossy(&index).contains("postCTest"));
        }
        Ok(output(ProcessResult::Exited(0), b""))
    })?;
    assert_eq!(invoke(&host, &[]), 0);
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "Cannot read commands.")?
            .as_slice(),
        ["runner", "callback"]
    );
    Ok(())
}

#[test]
fn passes_the_instrumented_command_exit_code_through() -> TestResult {
    let host = tree("add_test(other unexpected)")?;
    host.set_command_handler(|_, request, _| {
        assert_eq!(request.pipeline[0].program, "instrumented");
        assert_eq!(request.pipeline[0].args, ["--flag"]);
        Ok(output(ProcessResult::Exited(23), b""))
    })?;
    assert_eq!(
        invoke(
            &host,
            &[
                "--instrument",
                "--command-type",
                "test",
                "--",
                "instrumented",
                "--flag"
            ]
        ),
        23
    );
    Ok(())
}
