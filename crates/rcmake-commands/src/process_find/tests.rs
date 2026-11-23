use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{
    CommandSpec, Host, HostError, HostResult, Input, Output, ProcessResult, PureHost, RunOutput,
    Stream,
};
use rcmake_model::CacheType;

use super::register;

const SCRIPT: &str = "/work/check.cmake";

fn setup() -> (Arc<PureHost>, Interp) {
    let host = Arc::new(PureHost::default());
    let interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Script,
            cmake_root: "/share/cmake".into(),
            cmake_command: "/bin/cmake".into(),
            cwd: "/work".into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        register,
    );
    (host, interp)
}

fn run(host: &PureHost, interp: &mut Interp, script: &str) -> HostResult<i32> {
    host.seed_file(Path::new(SCRIPT), script.as_bytes())?;
    Ok(interp.run_script(Path::new(SCRIPT)).exit_code)
}

fn var(interp: &mut Interp, name: &[u8]) -> BString {
    interp.cx().var_or_empty(name)
}

#[test]
fn execute_pipeline_with_timeout_and_split_capture() -> HostResult<()> {
    let (host, mut interp) = setup();
    host.env_set(b"BASE", Some(b"base"));
    host.set_command_handler(|_, req, echo| {
        assert_eq!(
            req.pipeline,
            vec![
                CommandSpec {
                    program: "produce".into(),
                    args: vec!["two words".into()]
                },
                CommandSpec {
                    program: "consume".into(),
                    args: vec![]
                },
            ]
        );
        assert_eq!(req.cwd, Path::new("/work/sub"));
        assert_eq!(req.stdin, Input::File("/work/sub/in.txt".into()));
        assert_eq!(req.timeout, Some(Duration::from_millis(125)));
        assert_eq!(req.stdout, Output::Capture);
        assert_eq!(req.stderr, Output::Capture);
        assert_eq!(req.env, vec![("BASE".into(), "base".into())]);
        assert!(req.tee);
        echo(Stream::Stdout, b"first\r\nsecond\0 \t\n");
        echo(Stream::Stderr, b" stopped \x0b\n");
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(2), ProcessResult::TimedOut],
            stdout: b"first\r\nsecond\0 \t\n".to_vec(),
            stderr: b" stopped \x0b\n".to_vec(),
        })
    })?;
    let script = r#"
execute_process(COMMAND produce "two words" COMMAND consume
  WORKING_DIRECTORY sub INPUT_FILE in.txt TIMEOUT 0.125
  OUTPUT_VARIABLE OUT ERROR_VARIABLE ERR ECHO_OUTPUT_VARIABLE
  OUTPUT_STRIP_TRAILING_WHITESPACE ERROR_STRIP_TRAILING_WHITESPACE
  RESULT_VARIABLE LAST RESULTS_VARIABLE ALL ENCODING UTF-8)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"OUT"), "first\nsecond");
    assert_eq!(var(&mut interp, b"ERR"), " stopped");
    assert_eq!(
        var(&mut interp, b"LAST"),
        "Process terminated due to timeout"
    );
    assert_eq!(
        var(&mut interp, b"ALL"),
        "2;Process terminated due to timeout"
    );
    assert_eq!(
        host.stream_bytes(Stream::Stdout).as_slice(),
        b"first\r\nsecond\0 \t\n"
    );
    assert!(host.stream_bytes(Stream::Stderr).is_empty());
    Ok(())
}

#[test]
fn execute_shared_output_preserves_write_order() -> HostResult<()> {
    let (host, mut interp) = setup();
    interp
        .cx()
        .set_var(b"CMAKE_EXECUTE_PROCESS_COMMAND_ECHO", "STDERR".into());
    host.set_command_handler(|_, req, echo| {
        assert_eq!(req.stdout, Output::Capture);
        assert_eq!(req.stderr, Output::SharedWithStdout);
        assert!(req.tee);
        for bytes in [b"out1\n", b"err1\n", b"out2\n"] {
            echo(Stream::Stdout, bytes);
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: b"out1\nerr1\nout2\n".to_vec(),
            ..RunOutput::default()
        })
    })?;
    let script = r#"
execute_process(COMMAND produce "" "a'b"
  OUTPUT_VARIABLE MERGED ERROR_VARIABLE MERGED ECHO_OUTPUT_VARIABLE ECHO_ERROR_VARIABLE
  OUTPUT_STRIP_TRAILING_WHITESPACE)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"MERGED"), "out1\nerr1\nout2");
    assert_eq!(host.stream_bytes(Stream::Stdout), "out1\nerr1\nout2\n");
    assert_eq!(host.stream_bytes(Stream::Stderr), "'produce' '' 'a'b'\n");
    Ok(())
}

#[test]
fn execute_redirects_files_and_discards_quiet_streams() -> HostResult<()> {
    let (host, mut interp) = setup();
    host.set_command_handler(|_, req, _| {
        assert_eq!(req.cwd, Path::new("/work"));
        assert_eq!(
            req.stdout,
            Output::File {
                path: "/work/out.txt".into(),
                append: false
            }
        );
        assert_eq!(req.stderr, Output::Null);
        assert!(!req.tee);
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let script = "execute_process(COMMAND produce OUTPUT_FILE out.txt OUTPUT_QUIET OUTPUT_VARIABLE OUT ERROR_QUIET ERROR_VARIABLE ERR)";
    interp.cx().set_var(b"OUT", "old".into());
    interp.cx().set_var(b"ERR", "old".into());
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"OUT"), "");
    assert_eq!(var(&mut interp, b"ERR"), "");

    host.set_command_handler(|_, req, _| {
        assert_eq!(
            req.stdout,
            Output::File {
                path: "/work/both.txt".into(),
                append: false
            }
        );
        assert_eq!(req.stderr, Output::SharedWithStdout);
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(
        run(
            &host,
            &mut interp,
            "execute_process(COMMAND produce OUTPUT_FILE both.txt ERROR_FILE both.txt)"
        )?,
        0
    );
    Ok(())
}

#[test]
fn execute_fatal_modes_report_failures() -> HostResult<()> {
    for (mode, results, expected) in [
        (
            "ANY",
            vec![
                ProcessResult::Exited(3),
                ProcessResult::Exited(0),
                ProcessResult::Signaled(15),
            ],
            Some(
                "execute_process failed command indexes:\n\n    1: \"Child return code: 3\"\n    3: \"Abnormal exit with child return code: Subprocess terminated\"",
            ),
        ),
        (
            "LAST",
            vec![
                ProcessResult::Exited(3),
                ProcessResult::Exited(0),
                ProcessResult::Exited(7),
            ],
            Some("execute_process last command failed"),
        ),
        (
            "LAST",
            vec![
                ProcessResult::Exited(3),
                ProcessResult::Exited(0),
                ProcessResult::Signaled(15),
            ],
            Some("execute_process Abnormal exit: Subprocess terminated"),
        ),
        (
            "LAST",
            vec![
                ProcessResult::Exited(0),
                ProcessResult::TimedOut,
                ProcessResult::TimedOut,
            ],
            Some("execute_process Process terminated due to timeout"),
        ),
        (
            "LAST",
            vec![
                ProcessResult::Exited(3),
                ProcessResult::Exited(0),
                ProcessResult::Exited(0),
            ],
            None,
        ),
        (
            "NONE",
            vec![
                ProcessResult::Exited(3),
                ProcessResult::Exited(0),
                ProcessResult::Exited(7),
            ],
            None,
        ),
    ] {
        let (host, mut interp) = setup();
        host.set_command_handler(move |_, _, _| {
            Ok(RunOutput {
                results: results.clone(),
                ..RunOutput::default()
            })
        })?;
        let script = format!(
            "execute_process(COMMAND one COMMAND two COMMAND three COMMAND_ERROR_IS_FATAL {mode})"
        );
        assert_eq!(
            run(&host, &mut interp, &script)?,
            i32::from(expected.is_some())
        );
        if let Some(expected) = expected {
            let diagnostic = host.stream_bytes(Stream::Stderr);
            let rendered = String::from_utf8_lossy(&diagnostic);
            let mut lines = rendered.lines().filter(|line| !line.is_empty());
            for expected_line in expected.lines().filter(|line| !line.is_empty()) {
                assert!(
                    lines.any(|line| line.trim() == expected_line.trim()),
                    "{diagnostic}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn execute_result_variables_override_the_fatal_default() -> HostResult<()> {
    let (host, mut interp) = setup();
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            results: vec![
                ProcessResult::Exited(7),
                ProcessResult::Signaled(15),
                ProcessResult::SpawnFailed(HostError::unsupported("file is missing")),
            ],
            ..RunOutput::default()
        })
    })?;
    interp.cx().set_var(
        b"CMAKE_EXECUTE_PROCESS_COMMAND_ERROR_IS_FATAL",
        "ANY".into(),
    );
    let script = "execute_process(COMMAND one COMMAND two COMMAND three RESULT_VARIABLE LAST RESULTS_VARIABLE ALL)";
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"LAST"), "file is missing");
    assert_eq!(
        var(&mut interp, b"ALL"),
        "7;Subprocess terminated;file is missing"
    );
    Ok(())
}

#[test]
fn execute_environment_changes_apply_only_to_children() -> HostResult<()> {
    let (host, mut interp) = setup();
    host.env_set(b"DROP", Some(b"present"));
    host.set_command_handler(|_, req, _| {
        assert_eq!(req.env, vec![("VALUE".into(), "new;next".into())]);
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let script = r#"
execute_process(COMMAND inspect
  ENVIRONMENT VALUE=old VALUE=new
  ENVIRONMENT_MODIFICATION VALUE=string_append:temp VALUE=reset: VALUE=cmake_list_append:next DROP=unset:)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert!(host.env_get(b"VALUE").is_none());
    assert_eq!(host.env_get(b"DROP"), Some("present".into()));
    Ok(())
}

#[test]
fn finds_files_directories_and_libraries() -> HostResult<()> {
    let (host, mut interp) = setup();
    for file in [
        "/work/tree/header.h",
        "/work/tree/suffix/header.h",
        "/work/libs/libone.a",
        "/work/libs/libtwo.so",
        "/work/custom/pretool.mod",
    ] {
        host.seed_file(Path::new(file), b"")?;
    }
    let script = r#"
find_file(HEADER header.h tree NO_DEFAULT_PATH DOC "The header")
find_path(INCLUDE NAMES header.h HINTS tree PATH_SUFFIXES suffix NO_DEFAULT_PATH NO_CACHE)
find_library(STATIC NAMES one HINTS libs NO_DEFAULT_PATH)
find_library(EXACT NAMES libtwo.so HINTS libs NO_DEFAULT_PATH)
find_library(CUSTOM NAMES tool PATHS custom NO_DEFAULT_PATH)
"#;
    interp
        .cx()
        .set_var(b"CMAKE_FIND_LIBRARY_PREFIXES", "lib;pre".into());
    interp
        .cx()
        .set_var(b"CMAKE_FIND_LIBRARY_SUFFIXES", ".a;.mod".into());
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"HEADER"), "/work/tree/header.h");
    assert_eq!(var(&mut interp, b"INCLUDE"), "/work/tree/suffix/");
    assert_eq!(var(&mut interp, b"STATIC"), "/work/libs/libone.a");
    assert_eq!(var(&mut interp, b"EXACT"), "/work/libs/libtwo.so");
    assert_eq!(var(&mut interp, b"CUSTOM"), "/work/custom/pretool.mod");
    let mut cx = interp.cx();
    assert!(!cx.cache().contains(b"INCLUDE"));
    assert_eq!(
        cx.cache()
            .get(b"HEADER")
            .map(|entry| (&entry.help, entry.ty)),
        Some((&"The header".into(), CacheType::Filepath))
    );
    Ok(())
}

#[test]
fn find_program_requires_a_regular_executable_file_and_supports_both_loop_orders() -> HostResult<()>
{
    let (host, mut interp) = setup();
    host.seed_file(Path::new("/work/one/first"), b"")?;
    host.create_dir_all(Path::new("/work/one/only_dir"))?;
    for file in ["/work/one/second", "/work/two/first"] {
        host.seed_file(Path::new(file), b"")?;
        host.set_permissions(Path::new(file), 0o711)?;
    }
    let script = r#"
find_program(NAMES_FIRST NAMES only_dir first second PATHS one two NO_DEFAULT_PATH)
find_program(DIRS_FIRST NAMES only_dir first second PATHS one two NAMES_PER_DIR NO_DEFAULT_PATH)
find_program(ABSOLUTE /work/two/first NO_DEFAULT_PATH)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"NAMES_FIRST"), "/work/two/first");
    assert_eq!(var(&mut interp, b"DIRS_FIRST"), "/work/one/second");
    assert_eq!(var(&mut interp, b"ABSOLUTE"), "/work/two/first");
    Ok(())
}

#[test]
fn find_search_order_and_disable_options() -> HostResult<()> {
    let (host, mut interp) = setup();
    for file in [
        "/hints/tool",
        "/prefix/bin/tool",
        "/programs/tool",
        "/env-prefix/bin/tool",
        "/env/tool",
        "/paths/tool",
        "/usr/local/bin/tool",
        "/no-defaults/tool",
    ] {
        host.seed_file(Path::new(file), b"")?;
        host.set_permissions(Path::new(file), 0o755)?;
    }
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_PREFIX_PATH", "/prefix".into());
    cx.set_var(b"CMAKE_PROGRAM_PATH", "/programs".into());
    host.env_set(b"CMAKE_PREFIX_PATH", Some(b"/env-prefix"));
    host.env_set(b"PATH", Some(b"/missing:/env"));
    let script = r#"
find_program(HINT tool HINTS /hints PATHS /paths)
find_program(PREFIX tool PATHS /paths)
find_program(CMAKE_ENV tool PATHS /paths NO_CMAKE_PATH)
find_program(SYSTEM_ENV tool PATHS /paths NO_CMAKE_PATH NO_CMAKE_ENVIRONMENT_PATH)
find_program(EXPLICIT tool PATHS /paths NO_CMAKE_PATH NO_CMAKE_ENVIRONMENT_PATH NO_SYSTEM_ENVIRONMENT_PATH)
find_program(SYSTEM tool NO_CMAKE_PATH NO_CMAKE_ENVIRONMENT_PATH NO_SYSTEM_ENVIRONMENT_PATH)
find_program(DISABLED tool NO_CMAKE_PATH NO_CMAKE_ENVIRONMENT_PATH NO_SYSTEM_ENVIRONMENT_PATH NO_CMAKE_SYSTEM_PATH)
find_program(NO_DEFAULTS tool HINTS /no-defaults NO_DEFAULT_PATH)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    for (name, value) in [
        ("HINT", "/hints/tool"),
        ("PREFIX", "/prefix/bin/tool"),
        ("CMAKE_ENV", "/env-prefix/bin/tool"),
        ("SYSTEM_ENV", "/env/tool"),
        ("EXPLICIT", "/paths/tool"),
        ("SYSTEM", "/usr/local/bin/tool"),
        ("DISABLED", "DISABLED-NOTFOUND"),
        ("NO_DEFAULTS", "/no-defaults/tool"),
    ] {
        assert_eq!(var(&mut interp, name.as_bytes()), value);
    }
    Ok(())
}

#[test]
fn find_respects_existing_values_and_retries_notfound() -> HostResult<()> {
    let (host, mut interp) = setup();
    interp.cx().set_var(b"NORMAL", "keep".into());
    interp.cx().set_var(b"EMPTY", "".into());
    let script = r#"
find_file(NORMAL header . NO_DEFAULT_PATH)
find_file(EMPTY header . NO_DEFAULT_PATH)
find_file(CACHED header . NO_DEFAULT_PATH)
find_file(LOCAL header . NO_DEFAULT_PATH NO_CACHE)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"CACHED"), "CACHED-NOTFOUND");
    assert_eq!(var(&mut interp, b"LOCAL"), "LOCAL-NOTFOUND");
    assert!(!interp.cx().cache().contains(b"LOCAL"));

    host.seed_file(Path::new("/work/header"), b"")?;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"CACHED"), "/work/header");
    assert_eq!(var(&mut interp, b"LOCAL"), "/work/header");
    assert_eq!(var(&mut interp, b"NORMAL"), "keep");
    assert_eq!(var(&mut interp, b"EMPTY"), "");
    assert!(!interp.cx().cache().contains(b"NORMAL"));
    assert!(!interp.cx().cache().contains(b"LOCAL"));

    let script = "find_file(CACHED missing NO_DEFAULT_PATH NO_CACHE REQUIRED)";
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"CACHED"), "/work/header");
    Ok(())
}

#[test]
fn find_required_records_the_miss_and_stops_the_script() -> HostResult<()> {
    let (host, mut interp) = setup();
    let script = "find_path(X NAMES one.h two.h NO_DEFAULT_PATH REQUIRED)\nfind_file(AFTER missing NO_DEFAULT_PATH)";
    assert_eq!(run(&host, &mut interp, script)?, 1);
    assert_eq!(var(&mut interp, b"X"), "X-NOTFOUND");
    let mut cx = interp.cx();
    assert_eq!(
        cx.cache().get(b"X").map(|entry| entry.ty),
        Some(CacheType::Path)
    );
    assert!(!cx.var_defined(b"AFTER"));
    let error = host.stream_bytes(Stream::Stderr);
    assert!(
        String::from_utf8_lossy(&error)
            .contains("Could not find X using the following files: one.h, two.h"),
        "{error}"
    );
    Ok(())
}
