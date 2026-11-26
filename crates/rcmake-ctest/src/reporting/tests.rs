use std::error::Error;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(unix)]
use rcmake_host::{CommandSpec, Input, NativeHost, Output, ReadRange, WriteMode};
use rcmake_host::{ProcessResult, PureHost, RunOutput};

use super::*;

type TestResult = Result<(), Box<dyn Error>>;

fn invoke(host: Arc<dyn Host>, args: &[BString]) -> i32 {
    let argv: Vec<_> = std::iter::once("ctest".into())
        .chain(args.iter().cloned())
        .collect();
    crate::main(&argv, host)
}

#[test]
fn lists_tests_and_filtered_labels_and_summarizes_processor_time() -> TestResult {
    let make_host = || -> rcmake_host::HostResult<Arc<PureHost>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/CTestTestfile.cmake"),
            br#"
            add_test(a runner)
            add_test(b runner)
            set_tests_properties(a PROPERTIES LABELS "z;group" PROCESSORS 2)
            set_tests_properties(b PROPERTIES LABELS "group;other" DISABLED ON)
        "#,
        )?;
        host.seed_file(Path::new("/work/runner"), b"")?;
        host.set_command_handler(|host, _, _| {
            host.sleep(Duration::from_millis(150));
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        Ok(host)
    };
    let host = make_host()?;
    assert_eq!(invoke(host.clone(), &["-N".into()]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        "Test project /work\n  Test #1: a\n  Test #2: b (Disabled)\n\nTotal Tests: 2\n"
    );
    let host = make_host()?;
    assert_eq!(
        invoke(
            host.clone(),
            &["--print-labels".into(), "-R".into(), "^b$".into()]
        ),
        0
    );
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        "Test project /work\nAll Labels:\n  group\n  other\n"
    );
    let host = make_host()?;
    assert_eq!(invoke(host.clone(), &[]), 0);
    let stdout = host.stream_bytes(Stream::Stdout).to_string();
    assert!(stdout.contains("Label Time Summary:\ngroup    =   0.30 sec*proc (2 tests)"));
    let host = make_host()?;
    assert_eq!(invoke(host.clone(), &["--no-label-summary".into()]), 0);
    assert!(
        !host
            .stream_bytes(Stream::Stdout)
            .to_string()
            .contains("Label Time Summary")
    );
    Ok(())
}

#[test]
fn verbose_runs_print_the_configuration_fixture_and_graph_lines() -> TestResult {
    let make_host = || -> rcmake_host::HostResult<Arc<PureHost>> {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/CTestTestfile.cmake"),
            br#"
            add_test(setup runner)
            add_test(user runner)
            set_tests_properties(setup PROPERTIES FIXTURES_SETUP fx)
            set_tests_properties(user PROPERTIES FIXTURES_REQUIRED fx)
        "#,
        )?;
        host.seed_file(Path::new("/work/runner"), b"")?;
        host.set_command_handler(|_, _, _| {
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        Ok(host)
    };
    let host = make_host()?;
    let args = ["-V".into(), "-R".into(), "^user$".into()];
    assert_eq!(invoke(host.clone(), &args), 0);
    let stdout = host.stream_bytes(Stream::Stdout).to_string();
    assert!(
        stdout.starts_with(
            "UpdateCTestConfiguration  from :/work/DartConfiguration.tcl\n\
             Test project /work\n\
             Constructing a list of tests\n\
             Done constructing a list of tests\n\
             Updating test list for fixtures\n\
             Added setup test setup required by fixture fx\n\
             Added 1 tests to meet fixture requirements\n\
             Checking test dependency graph...\n\
             Checking test dependency graph end\n\
             test 1\n"
        ),
        "{stdout}"
    );

    // `-N` lists the tests without the dependency graph lines. An existing
    // configuration file also gets a "Parse Config file" line.
    let host = make_host()?;
    host.seed_file(Path::new("/work/DartConfiguration.tcl"), b"")?;
    assert_eq!(invoke(host.clone(), &["-N".into(), "-V".into()]), 0);
    let stdout = host.stream_bytes(Stream::Stdout).to_string();
    assert!(
        stdout.starts_with(
            "UpdateCTestConfiguration  from :/work/DartConfiguration.tcl\n\
             Parse Config file:/work/DartConfiguration.tcl\n\
             Test project /work\n"
        ),
        "{stdout}"
    );
    assert!(stdout.contains("Added 0 tests to meet fixture requirements\n"));
    assert!(!stdout.contains("Checking test dependency graph"));
    assert!(stdout.contains("Done constructing a list of tests\n"));

    // Without a test file, CTest does not write the "Done constructing" line.
    let host = Arc::new(PureHost::default());
    host.create_dir_all(Path::new("/work"))?;
    invoke(host.clone(), &["-N".into(), "-V".into()]);
    let stdout = host.stream_bytes(Stream::Stdout).to_string();
    assert!(
        stdout.contains("Constructing a list of tests\nUpdating test list for fixtures\n"),
        "{stdout}"
    );
    Ok(())
}

#[cfg(unix)]
struct Fixture<'a> {
    host: &'a dyn Host,
    dir: PathBuf,
}

#[cfg(unix)]
impl Drop for Fixture<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.dir);
    }
}

#[cfg(unix)]
fn oracle(
    host: &dyn Host,
    dir: &Path,
    executable: &BString,
    args: &[BString],
) -> rcmake_host::HostResult<RunOutput> {
    host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: executable.clone(),
                args: args.to_vec(),
            }],
            cwd: dir.to_path_buf(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(10)),
            tee: false,
        },
        &mut |_, _| {},
    )
}

#[cfg(unix)]
fn stable_junit(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.starts_with("\ttime=") && !line.starts_with("\ttimestamp="))
        .map(|line| {
            if let Some((start, rest)) = line.split_once(" time=\"")
                && let Some((_, end)) = rest.split_once('"')
            {
                return format!("{start}{end}");
            }
            line.to_owned()
        })
        .collect()
}

#[cfg(unix)]
fn stable_cdash(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().skip_while(|line| !line.contains("<Testing>"));
    let mut stable = Vec::new();
    while let Some(line) = lines.next() {
        if line.contains("DateTime>") || line.contains("TestTime>") {
            continue;
        }
        stable.push(line.to_owned());
        if line.contains("name=\"Execution Time\"") {
            lines.next();
        }
    }
    stable
}

#[test]
#[cfg(unix)]
fn reports_match_ctest_451() -> TestResult {
    let host = Arc::new(NativeHost::new()?);
    let Some(executable) = host.env_get(b"RCMAKE_CTEST_COMPARISON") else {
        return Ok(());
    };
    if host
        .metadata(&path::to_native(&executable), Follow::Yes)
        .is_err()
    {
        return Ok(());
    }
    let version = oracle(
        host.as_ref(),
        &host.initial_cwd(),
        &executable,
        &["--version".into()],
    )?;
    assert!(version.stdout.starts_with(b"ctest version 4.5.1\n"));
    let system = host.system();
    let fixture = Fixture {
        host: host.as_ref(),
        dir: std::env::temp_dir().join(format!(
            "rcmake-ctest-report-{}-{}",
            system.pid,
            host.now().nanos
        )),
    };
    let dir = &fixture.dir;
    host.create_dir_all(dir)?;
    let read = |path: &Path| host.read(path, ReadRange::All { limit: 256 * 1024 });
    host.write(
        &dir.join("CTestConfiguration.ini"),
        b"Site: local\nBuildName: report fixture\n",
        WriteMode::CreateNew,
    )?;
    host.write(&dir.join("CTestTestfile.cmake"), br#"
        add_test([=[pass<&"]=] /bin/echo [=[<output> & "quoted"]=])
        add_test(fail /usr/bin/false)
        add_test(disabled /usr/bin/true)
        add_test(missing /nonexistent/rcmake-ctest-fixture)
        add_test(skipped /usr/bin/false)
        set_tests_properties([=[pass<&"]=] PROPERTIES LABELS "z;group;group" PROCESSORS 2 RUN_SERIAL OFF TIMEOUT 5 CUSTOM "a;b" _BACKTRACE_TRIPLES "/fixture/CMakeLists.txt;7;add_test;/fixture/CMakeLists.txt;0;")
        set_tests_properties(disabled PROPERTIES DISABLED YES)
        set_tests_properties(skipped PROPERTIES SKIP_RETURN_CODE 1)
    "#, WriteMode::CreateNew)?;
    let args = ["--test-dir".into(), path::from_native(dir)];
    let options = rcmake_json::ReadOptions::default();
    for mode in ["--show-only=json-v1", "--show-only=json-v1-raw"] {
        let argv = [args.as_slice(), &[mode.into(), "-C".into(), "Debug".into()]].concat();
        let expected = oracle(host.as_ref(), dir, &executable, &argv)?;
        assert_eq!(expected.results, [ProcessResult::Exited(0)]);
        let actual = Arc::new(rcmake_host::PureHost::new(
            dir.to_path_buf(),
            dir.join("ctest"),
        ));
        actual.seed_file(
            &dir.join("CTestTestfile.cmake"),
            &read(&dir.join("CTestTestfile.cmake"))?,
        )?;
        for program in ["/bin/echo", "/usr/bin/false", "/usr/bin/true"] {
            actual.seed_file(Path::new(program), b"")?;
        }
        assert_eq!(invoke(actual.clone(), &argv), 0);
        let expected = rcmake_json::parse(&expected.stdout, &options)?;
        let actual = rcmake_json::parse(&actual.stream_bytes(Stream::Stdout), &options)?;
        assert_eq!(actual, expected, "{mode}");
    }

    let argv = [
        args.as_slice(),
        &[
            "-Q".into(),
            "-T".into(),
            "Test".into(),
            "--output-junit".into(),
            "junit.xml".into(),
        ],
    ]
    .concat();
    let result = oracle(host.as_ref(), dir, &executable, &argv)?;
    assert_eq!(result.results, [ProcessResult::Exited(8)]);
    let junit = dir.join("junit.xml");
    let tag = crate::dart::Tag::read(host.as_ref(), dir)?;
    let cdash = dir.join("Testing").join(tag.stamp()).join("Test.xml");
    let expected_junit = stable_junit(&read(&junit)?);
    let expected_cdash = stable_cdash(&read(&cdash)?);
    assert_eq!(invoke(host.clone(), &argv), 8);
    assert_eq!(stable_junit(&read(&junit)?), expected_junit);
    assert_eq!(stable_cdash(&read(&cdash)?), expected_cdash);
    Ok(())
}
