use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::{BString, ByteSlice as _};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{
    CommandSpec, Host, HostError, HostResult, Input, NativeHost, Output, ProcessResult, PureHost,
    ReadRange, RunOutput, Stream,
};
use rcmake_model::CacheType;

use super::register;

const SCRIPT: &str = "/work/check.cmake";

fn setup() -> (Arc<PureHost>, Interp) {
    setup_with(register)
}

fn setup_with(register: fn(&mut rcmake_eval::Registry)) -> (Arc<PureHost>, Interp) {
    let host = Arc::new(PureHost::default());
    let interp = Interp::new(
        InterpConfig::new(
            host.clone(),
            RunMode::Script,
            "/share/cmake",
            "/bin/cmake",
            "/work",
        ),
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
fn find_library_searches_frameworks_in_the_requested_order() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    for directory in [
        "/work/local/Panel.framework",
        "/work/local/Panel.xcframework",
        "/work/system/Other.framework",
    ] {
        host.create_dir_all(Path::new(directory))?;
    }
    host.seed_file(Path::new("/work/local/libPanel.a"), b"")?;
    host.seed_file(Path::new("/work/local/Program"), b"")?;
    let script = r#"
set(CMAKE_FRAMEWORK_PATH /work/local)
set(CMAKE_SYSTEM_FRAMEWORK_PATH /work/system)
set(CMAKE_FIND_LIBRARY_PREFIXES lib)
set(CMAKE_FIND_LIBRARY_SUFFIXES .a)
function(accept_framework status item)
  if(item MATCHES "[.]xcframework$")
    set(${status} FALSE PARENT_SCOPE)
  endif()
endfunction()
set(CMAKE_FIND_FRAMEWORK FIRST)
find_library(FIRST Panel)
find_library(VALIDATED Panel VALIDATOR accept_framework)
find_library(NAMES_FIRST NAMES Other Panel)
find_library(DIRS_FIRST NAMES Other Panel NAMES_PER_DIR)
find_library(NO_DEFAULT Panel NO_DEFAULT_PATH)
set(CMAKE_FIND_FRAMEWORK LAST)
find_library(LAST Panel)
find_library(FALLBACK Other)
set(CMAKE_FIND_FRAMEWORK ONLY)
find_library(ONLY Panel)
find_library(PLAIN Program)
find_library(NO_SYSTEM Other NO_CMAKE_SYSTEM_PATH)
set(CMAKE_FIND_FRAMEWORK NEVER)
find_library(NEVER Panel)
find_library(NO_FRAMEWORK Other)
find_library(NO_SUFFIX Program)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    for (name, expected) in [
        ("FIRST", "/work/local/Panel.xcframework"),
        ("VALIDATED", "/work/local/Panel.framework"),
        ("NAMES_FIRST", "/work/system/Other.framework"),
        ("DIRS_FIRST", "/work/local/Panel.xcframework"),
        ("NO_DEFAULT", "NO_DEFAULT-NOTFOUND"),
        ("LAST", "/work/local/libPanel.a"),
        ("FALLBACK", "/work/system/Other.framework"),
        ("ONLY", "/work/local/Panel.xcframework"),
        ("PLAIN", "PLAIN-NOTFOUND"),
        ("NO_SYSTEM", "NO_SYSTEM-NOTFOUND"),
        ("NEVER", "/work/local/libPanel.a"),
        ("NO_FRAMEWORK", "NO_FRAMEWORK-NOTFOUND"),
        ("NO_SUFFIX", "NO_SUFFIX-NOTFOUND"),
    ] {
        assert_eq!(var(&mut interp, name.as_bytes()), expected, "{name}");
    }
    Ok(())
}

#[test]
fn find_library_searches_framework_paths_under_the_find_root() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    for directory in [
        "/work/sdk/System/Library/Frameworks/Panel.framework",
        "/System/Library/Frameworks/Panel.framework",
        "/Library/Frameworks/Other.framework",
    ] {
        host.create_dir_all(Path::new(directory))?;
    }
    let script = r#"
set(CMAKE_FIND_FRAMEWORK ONLY)
set(CMAKE_FIND_ROOT_PATH /work/sdk)
set(CMAKE_SYSTEM_FRAMEWORK_PATH /Library/Frameworks /System/Library/Frameworks)
find_library(ROOTED Panel)
find_library(HOST Panel NO_CMAKE_FIND_ROOT_PATH)
find_library(ONLY_ROOT Other ONLY_CMAKE_FIND_ROOT_PATH)
find_library(ALREADY_ROOTED Panel HINTS /work/sdk/System/Library/Frameworks NO_DEFAULT_PATH)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY NEVER)
find_library(MODE Panel)
find_library(BOTH Panel CMAKE_FIND_ROOT_PATH_BOTH)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    for (name, expected) in [
        (
            "ROOTED",
            "/work/sdk/System/Library/Frameworks/Panel.framework",
        ),
        ("HOST", "/System/Library/Frameworks/Panel.framework"),
        ("ONLY_ROOT", "ONLY_ROOT-NOTFOUND"),
        (
            "ALREADY_ROOTED",
            "/work/sdk/System/Library/Frameworks/Panel.framework",
        ),
        ("MODE", "/System/Library/Frameworks/Panel.framework"),
        (
            "BOTH",
            "/work/sdk/System/Library/Frameworks/Panel.framework",
        ),
    ] {
        assert_eq!(var(&mut interp, name.as_bytes()), expected, "{name}");
    }
    Ok(())
}

#[test]
fn find_path_returns_the_search_directory_for_a_nested_file() -> HostResult<()> {
    let (host, mut interp) = setup();
    host.seed_file(Path::new("/work/inc/sub/h.h"), b"")?;
    assert_eq!(
        run(
            &host,
            &mut interp,
            "find_path(HP sub/h.h PATHS inc NO_DEFAULT_PATH)",
        )?,
        0
    );
    assert_eq!(var(&mut interp, b"HP"), "/work/inc");
    Ok(())
}

#[test]
fn find_program_requires_a_regular_executable_file_and_supports_both_loop_orders() -> HostResult<()>
{
    let (host, mut interp) = setup();
    if cfg!(windows) {
        host.create_dir_all(Path::new("/work/one/first"))?;
    } else {
        host.seed_file(Path::new("/work/one/first"), b"")?;
    }
    host.create_dir_all(Path::new("/work/one/only_dir"))?;
    for file in ["/work/one/second", "/work/two/first"] {
        host.seed_file(Path::new(file), b"")?;
        host.set_permissions(Path::new(file), 0o711)?;
    }
    assert!(
        interp
            .cx()
            .set_policy(PolicyId::new(109), PolicyStatus::New)
            .is_ok()
    );
    let absolute = if cfg!(windows) {
        "C:/work/two/first"
    } else {
        "/work/two/first"
    };
    host.seed_file(Path::new(absolute), b"")?;
    host.set_permissions(Path::new(absolute), 0o755)?;
    let script = format!(
        r#"
find_program(NAMES_FIRST NAMES only_dir first second PATHS one two NO_DEFAULT_PATH)
find_program(DIRS_FIRST NAMES only_dir first second PATHS one two NAMES_PER_DIR NO_DEFAULT_PATH)
find_program(ABSOLUTE {absolute} NO_DEFAULT_PATH)
"#
    );
    assert_eq!(run(&host, &mut interp, &script)?, 0);
    assert_eq!(var(&mut interp, b"NAMES_FIRST"), "/work/two/first");
    assert_eq!(var(&mut interp, b"DIRS_FIRST"), "/work/one/second");
    assert_eq!(var(&mut interp, b"ABSOLUTE"), absolute);
    Ok(())
}

#[test]
#[cfg(windows)]
fn find_program_tries_windows_executable_suffixes() -> HostResult<()> {
    let (host, mut interp) = setup();
    host.env_set(b"PATH", Some(b"C:/tools"));
    for name in ["ninja.exe", "tool.com", "tool.exe", "tool", "plain"] {
        host.seed_file(&Path::new("C:/tools").join(name), b"")?;
    }
    let script = "
find_program(NINJA ninja NAMES_PER_DIR)
find_program(TOOL tool)
find_program(PLAIN plain)
";
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"NINJA"), "C:/tools/ninja.exe");
    assert_eq!(var(&mut interp, b"TOOL"), "C:/tools/tool.com");
    assert_eq!(var(&mut interp, b"PLAIN"), "C:/tools/plain");
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
    host.env_set(
        b"PATH",
        Some(if cfg!(windows) {
            b"/missing;/env"
        } else {
            b"/missing:/env"
        }),
    );
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
        ("HINT", "/prefix/bin/tool"),
        ("PREFIX", "/prefix/bin/tool"),
        ("CMAKE_ENV", "/env-prefix/bin/tool"),
        ("SYSTEM_ENV", "/env/tool"),
        ("EXPLICIT", "/usr/local/bin/tool"),
        ("SYSTEM", "/usr/local/bin/tool"),
        ("DISABLED", "DISABLED-NOTFOUND"),
        ("NO_DEFAULTS", "/no-defaults/tool"),
    ] {
        assert_eq!(var(&mut interp, name.as_bytes()), value);
    }
    Ok(())
}

#[test]
fn find_install_prefix_option_uses_platform_records() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    for file in [
        "/install/lib/libinstalled.a",
        "/staging/lib/libstaged.a",
        "/original/lib/liboriginal.a",
        "/independent/lib/libmanual.a",
    ] {
        host.seed_file(Path::new(file), b"")?;
    }
    let script = r#"
set(CMAKE_INSTALL_PREFIX /install)
set(CMAKE_STAGING_PREFIX /staging/)
set(CMAKE_SYSTEM_PREFIX_PATH /independent;/original;/staging/)
set(_CMAKE_SYSTEM_PREFIX_PATH_INSTALL_PREFIX_VALUE /original)
set(_CMAKE_SYSTEM_PREFIX_PATH_INSTALL_PREFIX_COUNT 1)
set(_CMAKE_SYSTEM_PREFIX_PATH_STAGING_PREFIX_VALUE /staging/)
set(_CMAKE_SYSTEM_PREFIX_PATH_STAGING_PREFIX_COUNT 1)
set(CMAKE_FIND_USE_INSTALL_PREFIX FALSE)
find_library(DISABLED_INSTALL original)
find_library(DISABLED_STAGING staged)
find_library(MANUAL manual)
find_library(EXPLICIT original PATHS /original/lib NO_DEFAULT_PATH)
set(CMAKE_FIND_USE_INSTALL_PREFIX ON)
find_library(ENABLED_INSTALL original)
find_library(ENABLED_STAGING staged)
find_library(CHANGED_INSTALL installed)
find_library(COMMAND_DISABLED original NO_CMAKE_INSTALL_PREFIX)
set(CMAKE_SYSTEM_PREFIX_PATH /original;/original;/staging/)
set(_CMAKE_SYSTEM_PREFIX_PATH_INSTALL_PREFIX_COUNT 2)
find_library(DUPLICATE original NO_CMAKE_INSTALL_PREFIX)
set(CMAKE_SYSTEM_PREFIX_PATH /independent;/staging/;/staging/)
set(_CMAKE_SYSTEM_PREFIX_PATH_INSTALL_PREFIX_VALUE /staging/)
set(_CMAKE_SYSTEM_PREFIX_PATH_STAGING_PREFIX_COUNT 2)
set(CMAKE_FIND_USE_INSTALL_PREFIX OFF)
find_library(SHARED_PREFIX staged)
set(CMAKE_SYSTEM_PREFIX_PATH /independent)
set(CMAKE_FIND_NO_INSTALL_PREFIX ON)
unset(CMAKE_FIND_USE_INSTALL_PREFIX)
find_library(OMITTED installed)
set(CMAKE_FIND_USE_INSTALL_PREFIX ON)
find_library(ADDED_INSTALL installed)
find_library(ADDED_STAGING staged)
find_library(NO_SYSTEM installed NO_CMAKE_SYSTEM_PATH)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    for (name, value) in [
        ("DISABLED_INSTALL", "DISABLED_INSTALL-NOTFOUND"),
        ("DISABLED_STAGING", "DISABLED_STAGING-NOTFOUND"),
        ("MANUAL", "/independent/lib/libmanual.a"),
        ("EXPLICIT", "/original/lib/liboriginal.a"),
        ("ENABLED_INSTALL", "/original/lib/liboriginal.a"),
        ("ENABLED_STAGING", "/staging/lib/libstaged.a"),
        ("CHANGED_INSTALL", "CHANGED_INSTALL-NOTFOUND"),
        ("COMMAND_DISABLED", "COMMAND_DISABLED-NOTFOUND"),
        ("DUPLICATE", "/original/lib/liboriginal.a"),
        ("SHARED_PREFIX", "SHARED_PREFIX-NOTFOUND"),
        ("OMITTED", "OMITTED-NOTFOUND"),
        ("ADDED_INSTALL", "/install/lib/libinstalled.a"),
        ("ADDED_STAGING", "/staging/lib/libstaged.a"),
        ("NO_SYSTEM", "NO_SYSTEM-NOTFOUND"),
    ] {
        assert_eq!(var(&mut interp, name.as_bytes()), value, "{name}");
    }
    Ok(())
}

#[test]
fn find_skips_candidates_under_ignored_prefixes() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    for file in [
        "/work/blocked/sub/header.h",
        "/work/blocked/sub/libexample.a",
        "/work/system/sub/header.h",
        "/work/system/sub/tool",
        "/work/blocked-extra/header.h",
    ] {
        host.seed_file(Path::new(file), b"")?;
    }
    host.set_permissions(Path::new("/work/system/sub/tool"), 0o755)?;
    let script = r#"
set(CMAKE_IGNORE_PREFIX_PATH "/work/blocked/")
set(CMAKE_SYSTEM_IGNORE_PREFIX_PATH "/work/system")
find_file(DIRECT header.h HINTS blocked/sub system/sub blocked-extra NO_DEFAULT_PATH)
find_file(NESTED blocked/sub/header.h PATHS . NO_DEFAULT_PATH)
find_path(INCLUDE system/sub/header.h PATHS . NO_DEFAULT_PATH)
find_library(LIBRARY blocked/sub/example PATHS . NO_DEFAULT_PATH)
find_program(PROGRAM /work/system/sub/tool NO_DEFAULT_PATH)
unset(CMAKE_IGNORE_PREFIX_PATH)
set(CMAKE_IGNORE_PATH "/work/blocked")
find_file(EXACT_IGNORE header.h PATHS blocked PATH_SUFFIXES sub NO_DEFAULT_PATH)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    for (name, value) in [
        ("DIRECT", "/work/blocked-extra/header.h"),
        ("NESTED", "NESTED-NOTFOUND"),
        ("INCLUDE", "INCLUDE-NOTFOUND"),
        ("LIBRARY", "LIBRARY-NOTFOUND"),
        ("PROGRAM", "PROGRAM-NOTFOUND"),
        ("EXACT_IGNORE", "/work/blocked/sub/header.h"),
    ] {
        assert_eq!(var(&mut interp, name.as_bytes()), value, "{name}");
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

#[test]
fn find_package_loads_module_and_restores_request_variables() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    host.seed_file(
        Path::new("/work/modules/FindWidget.cmake"),
        br#"
set(request "${Widget_FIND_VERSION}|${Widget_FIND_VERSION_RANGE}|${Widget_FIND_VERSION_RANGE_MAX}|${Widget_FIND_VERSION_MAX_MINOR}|${Widget_FIND_COMPONENTS}|${Widget_FIND_REQUIRED_core}|${Widget_FIND_REQUIRED_extra}|${Widget_FIND_QUIETLY}|${Widget_FIND_REQUIRED}")
set(WIDGET_FOUND TRUE)
"#,
    )?;
    let script = r#"
set(CMAKE_MODULE_PATH /work/modules)
set(Widget_FIND_VERSION saved)
find_package(Widget 1.2...<2.3 QUIET REQUIRED MODULE COMPONENTS core OPTIONAL_COMPONENTS extra)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(
        var(&mut interp, b"request"),
        "1.2|1.2...<2.3|EXCLUDE|3|core;extra|1|0|1|1"
    );
    assert_eq!(var(&mut interp, b"Widget_FIND_VERSION"), "saved");
    assert!(!interp.cx().var_defined(b"Widget_FIND_REQUIRED_extra"));
    assert!(interp.cx().var_is_on(b"Widget_FOUND"));
    Ok(())
}

fn setup_package() -> (Arc<PureHost>, Interp) {
    let host = Arc::new(PureHost::default());
    let interp = package_interp(host.clone(), RunMode::Script);
    (host, interp)
}

fn package_interp(host: Arc<PureHost>, mode: RunMode) -> Interp {
    Interp::new(
        InterpConfig::new(host, mode, "/share/cmake", "/bin/cmake", "/work"),
        crate::register_all,
    )
}

#[test]
fn find_package_checks_config_versions_in_separate_scopes() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    for (folder, version, flags) in [
        ("bad", "9.1", "set(PACKAGE_VERSION_UNSUITABLE TRUE)"),
        ("compatible", "1.7.5-rc1", ""),
        ("exact", "1.2", "set(PACKAGE_VERSION_EXACT TRUE)"),
    ] {
        let dir = format!("/work/{folder}/lib/cmake/Widget");
        host.seed_file(
            &Path::new(&dir).join("WidgetConfig.cmake"),
            format!("set(loaded {folder})").as_bytes(),
        )?;
        let script = format!(
            r#"
if(DEFINED PACKAGE_VERSION OR DEFINED version_local)
  message(FATAL_ERROR "Version state leaked")
endif()
if(NOT PACKAGE_FIND_NAME STREQUAL Widget OR NOT PACKAGE_FIND_VERSION_MAJOR EQUAL 1)
  message(FATAL_ERROR "Incorrect version request")
endif()
set(version_local TRUE)
set(PACKAGE_VERSION {version})
set(PACKAGE_VERSION_COMPATIBLE TRUE)
{flags}
"#
        );
        host.seed_file(
            &Path::new(&dir).join("WidgetConfigVersion.cmake"),
            script.as_bytes(),
        )?;
    }
    let script = r#"
set(PACKAGE_VERSION saved)
find_package(Widget 1.2 EXACT CONFIG REQUIRED NO_DEFAULT_PATH PATHS bad compatible exact)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(var(&mut interp, b"loaded"), "exact");
    assert_eq!(var(&mut interp, b"Widget_VERSION"), "1.2");
    assert_eq!(var(&mut interp, b"Widget_VERSION_COUNT"), "2");
    assert_eq!(var(&mut interp, b"Widget_VERSION_PATCH"), "0");
    assert_eq!(
        var(&mut interp, b"Widget_CONSIDERED_VERSIONS"),
        "9.1;1.7.5-rc1;1.2"
    );
    assert_eq!(var(&mut interp, b"PACKAGE_VERSION"), "saved");
    let mut cx = interp.cx();
    assert!(!cx.var_defined(b"version_local"));
    assert!(!cx.var_defined(b"PACKAGE_FIND_NAME"));
    assert_eq!(
        cx.cache()
            .get(b"Widget_DIR")
            .map(|entry| (&entry.value, entry.ty)),
        Some((&"/work/exact/lib/cmake/Widget".into(), CacheType::Path))
    );
    Ok(())
}

#[test]
fn find_package_passes_range_to_config_version_file() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    for (dir, version) in [
        ("Widget-2.0", "2.0"),
        ("Widget-1.10", "1.10"),
        ("Widget-1.2", "1.2"),
    ] {
        let dir = Path::new("/work/prefix").join(dir);
        host.seed_file(&dir.join("widget-config.cmake"), b"")?;
        let script = format!(
            r#"
if(NOT PACKAGE_FIND_VERSION STREQUAL "1.2"
   OR NOT PACKAGE_FIND_VERSION_MIN STREQUAL "1.2"
   OR NOT PACKAGE_FIND_VERSION_MAX STREQUAL "2.0"
   OR NOT PACKAGE_FIND_VERSION_RANGE STREQUAL "1.2...<2.0"
   OR NOT PACKAGE_FIND_VERSION_RANGE_MIN STREQUAL "INCLUDE"
   OR NOT PACKAGE_FIND_VERSION_RANGE_MAX STREQUAL "EXCLUDE")
  message(FATAL_ERROR "Incorrect version range")
endif()
set(PACKAGE_VERSION {version})
if(PACKAGE_VERSION VERSION_GREATER_EQUAL PACKAGE_FIND_VERSION_MIN
   AND PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION_MAX)
  set(PACKAGE_VERSION_COMPATIBLE TRUE)
endif()
"#
        );
        host.seed_file(&dir.join("widget-config-version.cmake"), script.as_bytes())?;
    }
    let exit = run(
        &host,
        &mut interp,
        "find_package(Widget 1.2...<2.0 CONFIG REQUIRED NO_DEFAULT_PATH PATHS prefix)",
    )?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(var(&mut interp, b"Widget_VERSION"), "1.10");
    assert_eq!(var(&mut interp, b"Widget_CONSIDERED_VERSIONS"), "2.0;1.10");
    assert_eq!(
        var(&mut interp, b"Widget_CONFIG"),
        "/work/prefix/Widget-1.10/widget-config.cmake"
    );
    Ok(())
}

#[test]
fn find_package_uses_config_preference_redirects_and_provider() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    host.seed_file(
        Path::new("/share/cmake/Modules/FindWidget.cmake"),
        b"set(loaded module)\nset(Widget_FOUND TRUE)",
    )?;
    for (dir, source) in [("config", "config"), ("redirect", "redirect")] {
        host.seed_file(
            &Path::new("/work").join(dir).join("WidgetConfig.cmake"),
            format!("set(loaded {source})").as_bytes(),
        )?;
    }
    let script = r#"
set(Widget_DIR /work/config)
find_package(Widget REQUIRED)
set(first "${loaded}")
set(CMAKE_FIND_PACKAGE_PREFER_CONFIG TRUE)
find_package(Widget REQUIRED)
set(second "${loaded}")
set(CMAKE_FIND_PACKAGE_REDIRECTS_DIR /work/redirect)
find_package(Widget MODULE REQUIRED)
set(third "${loaded}")

macro(provide method package_name)
  set(provided "${method}|${package_name}|${ARGN}")
  if("${package_name}" STREQUAL "Remote")
    set(Remote_FOUND TRUE)
  endif()
endmacro()
set_property(GLOBAL PROPERTY _RCMAKE_FIND_PACKAGE_PROVIDER provide)
set(CMAKE_REQUIRE_FIND_PACKAGE_Remote ON)
find_package(Remote 1 OPTIONAL COMPONENTS runtime)
find_package(Skipped CONFIG QUIET BYPASS_PROVIDER)
get_property(packages GLOBAL PROPERTY PACKAGES_FOUND)
get_property(missing GLOBAL PROPERTY PACKAGES_NOT_FOUND)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    for (name, expected) in [
        (b"first".as_slice(), "module"),
        (b"second", "config"),
        (b"third", "redirect"),
        (
            b"provided",
            "FIND_PACKAGE|Remote|1;OPTIONAL;COMPONENTS;runtime",
        ),
        (b"packages", "Widget;Remote"),
        (b"missing", "Skipped"),
    ] {
        assert_eq!(var(&mut interp, name), expected);
    }
    Ok(())
}

#[test]
fn find_package_handles_disabled_optional_and_required_packages() -> HostResult<()> {
    for (script, code, found, message) in [
        ("find_package(Missing CONFIG QUIET)", 0, Some("0"), ""),
        ("find_package(Missing CONFIG OPTIONAL)", 0, Some("0"), ""),
        (
            "find_package(Missing CONFIG REQUIRED)",
            1,
            Some("0"),
            "Could not find a package configuration file provided by \"Missing\"",
        ),
        (
            "set(CMAKE_REQUIRE_FIND_PACKAGE_Missing ON)\nfind_package(Missing CONFIG OPTIONAL)",
            1,
            Some("0"),
            "Could not find a package configuration file provided by \"Missing\"",
        ),
        (
            "set(CMAKE_DISABLE_FIND_PACKAGE_Missing ON)\nfind_package(Missing)",
            0,
            None,
            "",
        ),
        (
            "set(CMAKE_DISABLE_FIND_PACKAGE_Missing ON)\nfind_package(Missing REQUIRED)",
            1,
            None,
            "A REQUIRED package cannot",
        ),
    ] {
        let (host, mut interp) = setup_package();
        assert_eq!(run(&host, &mut interp, script)?, code, "{script}");
        assert_eq!(
            interp.cx().var(b"Missing_FOUND"),
            found.map(BString::from),
            "{script}"
        );
        let stderr = host.stream_bytes(Stream::Stderr);
        if message.is_empty() {
            assert!(stderr.is_empty(), "{stderr}");
        } else {
            assert!(stderr.to_str_lossy().contains(message), "{stderr}");
        }
    }
    Ok(())
}

#[test]
fn find_package_searches_nested_roots_and_uses_renamed_configs() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    host.seed_file(
        Path::new("/work/modules/FindOuter.cmake"),
        b"find_package(Inner CONFIG NAMES Alternate CONFIGS special.cmake PATH_SUFFIXES extra REQUIRED)\nset(Outer_FOUND TRUE)",
    )?;
    host.seed_file(
        Path::new("/sysroot/opt/share/alternate-2/extra/special.cmake"),
        b"set(nested \"${CMAKE_FIND_PACKAGE_NAME}\")",
    )?;
    let script = r#"
cmake_minimum_required(VERSION 4.5)
set(CMAKE_MODULE_PATH /work/modules)
set(Outer_ROOT /opt)
set(CMAKE_FIND_ROOT_PATH /sysroot)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
find_package(Outer REQUIRED)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(var(&mut interp, b"nested"), "Inner");
    assert_eq!(
        var(&mut interp, b"Inner_CONFIG"),
        "/sysroot/opt/share/alternate-2/extra/special.cmake"
    );
    assert!(!interp.cx().var_defined(b"CMAKE_FIND_PACKAGE_NAME"));
    Ok(())
}

#[test]
fn find_package_runs_vendored_threads_and_zlib_modules() -> HostResult<()> {
    let host = Arc::new(PureHost::default());
    let native = NativeHost::new()?;
    let modules = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/cmake-4.5/Modules");
    for module in [
        "CheckCSourceCompiles.cmake",
        "CheckIncludeFile.cmake",
        "CheckLibraryExists.cmake",
        "FindPackageHandleStandardArgs.cmake",
        "FindPackageMessage.cmake",
        "FindThreads.cmake",
        "FindZLIB.cmake",
        "SelectLibraryConfigurations.cmake",
        "Internal/CheckCommon.cmake",
        "Internal/CheckSourceCompiles.cmake",
    ] {
        let data = native.read(&modules.join(module), ReadRange::All { limit: 128 * 1024 })?;
        host.seed_file(&Path::new("/share/cmake/Modules").join(module), &data)?;
    }
    let (root, library, platform) = if cfg!(windows) {
        (
            "C:/work/zlib",
            "zlibstatic.lib",
            "set(WIN32 TRUE)\nset(CMAKE_FIND_LIBRARY_PREFIXES [[]])",
        )
    } else {
        ("/work/zlib", "libz.a", "")
    };
    let root = Path::new(root);
    let library = root.join("lib").join(library);
    host.seed_file(
        &root.join("include/zlib.h"),
        b"#define ZLIB_VERSION \"1.3.1\"\n",
    )?;
    host.seed_file(&library, b"")?;
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        format!(
            r#"
cmake_minimum_required(VERSION 4.5)
project(Packages NONE)
set(CMAKE_C_COMPILER_LOADED TRUE)
set(CMAKE_HAVE_LIBC_PTHREAD TRUE)
find_package(Threads REQUIRED)
{platform}
set(ZLIB_ROOT "{root}")
set(ZLIB_USE_STATIC_LIBS TRUE)
find_package(ZLIB 1.3 REQUIRED)
if(NOT TARGET Threads::Threads OR NOT TARGET ZLIB::ZLIB)
  message(FATAL_ERROR "An imported target is missing")
endif()
"#,
            root = root.display()
        )
        .as_bytes(),
    )?;
    let mut interp = package_interp(host.clone(), RunMode::Configure);
    let outcome = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert!(interp.cx().var_is_on(b"Threads_FOUND"));
    assert!(interp.cx().var_is_on(b"ZLIB_FOUND"));
    assert_eq!(var(&mut interp, b"ZLIB_VERSION"), "1.3.1");
    let mut cx = interp.cx();
    let model = cx.project();
    let zlib = model
        .find_target(rcmake_model::DirId::ROOT, b"ZLIB::ZLIB")
        .and_then(|id| model.target(id))
        .ok_or_else(|| rcmake_host::HostError::unsupported("Missing imported ZLIB target."))?;
    let location = zlib
        .props
        .get(b"IMPORTED_LOCATION_RELEASE")
        .or_else(|| zlib.props.get(b"IMPORTED_LOCATION"));
    assert_eq!(
        location.map(|value| rcmake_host::path::to_native(value)),
        Some(library)
    );
    Ok(())
}

#[test]
fn find_package_scopes_policies_unless_requested_otherwise() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    host.seed_file(
        Path::new("/share/cmake/Modules/FindWidget.cmake"),
        b"cmake_policy(SET CMP0074 OLD)\nset(Widget_FOUND TRUE)",
    )?;
    let script = r#"
cmake_minimum_required(VERSION 4.5)
find_package(Widget MODULE REQUIRED)
cmake_policy(GET CMP0074 scoped)
find_package(Widget MODULE REQUIRED NO_POLICY_SCOPE)
cmake_policy(GET CMP0074 unscoped)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(var(&mut interp, b"scoped"), "NEW");
    assert_eq!(var(&mut interp, b"unscoped"), "OLD");
    Ok(())
}

#[test]
fn find_package_global_promotes_targets_from_a_subdirectory() -> HostResult<()> {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Packages NONE)\nadd_subdirectory(child)",
    )?;
    host.seed_file(
        Path::new("/work/child/CMakeLists.txt"),
        b"find_package(Widget CONFIG GLOBAL REQUIRED NO_DEFAULT_PATH PATHS /work/prefix)",
    )?;
    host.seed_file(
        Path::new("/work/prefix/WidgetConfig.cmake"),
        b"add_library(Widget::Widget INTERFACE IMPORTED)",
    )?;
    let mut interp = package_interp(host.clone(), RunMode::Configure);
    let outcome = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert!(
        interp
            .cx()
            .project()
            .find_target(rcmake_model::DirId::ROOT, b"Widget::Widget")
            .is_some()
    );
    Ok(())
}

#[test]
fn pkg_config_extract_prefers_uninstalled_and_preserves_exact_version() -> HostResult<()> {
    let (host, mut interp) = setup();
    let search = if cfg!(windows) {
        b"/first;/second"
    } else {
        b"/first:/second"
    };
    host.env_set(b"PKG_CONFIG_PATH", Some(search));
    interp
        .cx()
        .set_var(b"CMAKE_PKG_CONFIG_PC_SYSROOT_DIR", "/sys root".into());
    host.seed_file(
        Path::new("/first/demo.pc"),
        b"Name: Installed\nDescription: Test package\nVersion: 1",
    )?;
    host.seed_file(
        Path::new("/second/demo-uninstalled.pc"),
        br#"prefix=/prefix
Name: Uninstalled
Description: Test package
Version: 01
Cflags: -I${prefix}/include -DTEST
Libs: -L${prefix}/lib -ldemo
Libs.private: -lm
Requires: core >= 1, extra
"#,
    )?;
    assert_eq!(
        run(
            &host,
            &mut interp,
            r#"
cmake_pkg_config(EXTRACT demo 1 REQUIRED)
cmake_pkg_config(EXTRACT demo 1 EXACT QUIET)
cmake_pkg_config(POPULATE demo REQUIRED DISABLE_UNINSTALLED ON)
cmake_pkg_config(IMPORT demo REQUIRED NAME renamed)
"#,
        )?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(var(&mut interp, b"CMAKE_PKG_CONFIG_NAME"), "Uninstalled");
    assert_eq!(var(&mut interp, b"CMAKE_PKG_CONFIG_VERSION"), "01");
    assert_eq!(
        var(&mut interp, b"CMAKE_PKG_CONFIG_INCLUDES"),
        "-I/sys root/prefix/include"
    );
    assert_eq!(
        var(&mut interp, b"CMAKE_PKG_CONFIG_LIBNAMES_PRIVATE"),
        "-lm"
    );
    assert_eq!(var(&mut interp, b"CMAKE_PKG_CONFIG_REQUIRES"), "core;extra");
    assert_eq!(var(&mut interp, b"PKGCONFIG_demo_FOUND"), "TRUE");
    assert!(
        interp
            .cx()
            .project()
            .find_target(rcmake_model::DirId::ROOT, b"PkgConfig::renamed")
            .is_some()
    );
    assert!(host.stream_bytes(Stream::Stderr).is_empty());
    Ok(())
}

#[test]
fn find_registry_paths_expand_each_view_before_globbing() -> HostResult<()> {
    use rcmake_host::{RegistryRoot::CurrentUser, RegistryView::*};

    let (host, mut interp) = setup_with(crate::register_all);
    host.set_registry_host_view(Bits64)?;
    for (view, root) in [(Bits32, "/work/sdk32"), (Bits64, "/work/sdk64")] {
        host.seed_registry_value(CurrentUser, b"Software/Tools", None, view, root.as_bytes())?;
        host.seed_registry_value(CurrentUser, b"Software/Tools", Some(b"bin"), view, b"bin")?;
        host.seed_file(&Path::new(root).join("v1/bin/probe"), b"")?;
    }
    let script = r#"
cmake_policy(SET CMP0134 NEW)
set(CMAKE_SIZEOF_VOID_P 4)
find_file(first NAMES probe NO_DEFAULT_PATH
  HINTS "[HKCU/Software/Tools;(default)]/*/[{!}HKEY_CURRENT_USER/Software/Tools!bin]"
  REGISTRY_VIEW BOTH)
find_file(second NAMES probe NO_DEFAULT_PATH
  PATHS "[HKCU/Software/Tools]/*/bin" REGISTRY_VIEW HOST)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"first"), "/work/sdk32/v1/bin/probe");
    assert_eq!(var(&mut interp, b"second"), "/work/sdk64/v1/bin/probe");
    Ok(())
}

#[test]
fn library_search_checks_architecture_paths_in_order() -> HostResult<()> {
    let (host, mut interp) = setup_with(crate::register_all);
    for dir in ["mylib", "mylib64", "mylib64/custom"] {
        host.seed_file(&Path::new("/work").join(dir).join("libtest.a"), b"")?;
    }
    let script = r#"
set(CMAKE_FIND_LIBRARY_PREFIXES lib)
set(CMAKE_FIND_LIBRARY_SUFFIXES .a)
set(CMAKE_SIZEOF_VOID_P 8)
set_property(GLOBAL PROPERTY FIND_LIBRARY_USE_LIB64_PATHS ON)
find_library(first NAMES test NO_DEFAULT_PATH PATHS /work/mylib)
set(CMAKE_FIND_LIBRARY_CUSTOM_LIB_SUFFIX custom)
find_library(second NAMES test NO_DEFAULT_PATH PATHS /work/mylib64)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"first"), "/work/mylib64/libtest.a");
    assert_eq!(
        var(&mut interp, b"second"),
        "/work/mylib64/custom/libtest.a"
    );
    Ok(())
}

#[test]
fn config_package_preserves_true_and_rejects_non_on_values() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    host.seed_file(
        Path::new("/work/prefix/WidgetConfig.cmake"),
        br#"
if(DEFINED called AND NOT Widget_FOUND)
  message(FATAL_ERROR "The previous successful result was lost")
endif()
set(called ON)
"#,
    )?;
    host.seed_file(
        Path::new("/work/prefix/OtherConfig.cmake"),
        b"set(Other_FOUND arbitrary)",
    )?;
    let exit = run(
        &host,
        &mut interp,
        r#"
set(Widget_FOUND arbitrary)
find_package(Widget CONFIG REQUIRED NO_DEFAULT_PATH PATHS /work/prefix)
find_package(Widget CONFIG REQUIRED NO_DEFAULT_PATH PATHS /work/prefix)
find_package(Other QUIET CONFIG NO_DEFAULT_PATH PATHS /work/prefix)
if(Other_FOUND)
  message(FATAL_ERROR "The package set a result that is not true")
endif()
"#,
    )?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    Ok(())
}

#[test]
fn find_path_and_find_file_accept_directory_entries() -> HostResult<()> {
    let (host, mut interp) = setup();
    for directory in [
        "/work/include/sub",
        "/work/lib/libsub.so",
        "/work/frameworks/Foo.framework/Headers/Inner",
    ] {
        host.create_dir_all(Path::new(directory))?;
    }
    for (name, value) in [
        ("CMAKE_FIND_LIBRARY_PREFIXES", "lib"),
        ("CMAKE_FIND_LIBRARY_SUFFIXES", ".so"),
        ("CMAKE_FIND_FRAMEWORK", "FIRST"),
    ] {
        interp.cx().set_var(name.as_bytes(), value.into());
    }
    let script = r#"
find_path(WITH_SLASH NAMES sub/ PATHS /work/include NO_DEFAULT_PATH)
find_path(WITHOUT_SLASH NAMES sub PATHS /work/include NO_DEFAULT_PATH)
find_file(DIRECTORY NAMES sub PATHS /work/include NO_DEFAULT_PATH)
find_library(LIBRARY NAMES sub PATHS /work/lib NO_DEFAULT_PATH)
find_path(FRAMEWORK NAMES Foo/Inner PATHS /work/frameworks NO_DEFAULT_PATH)
"#;
    assert_eq!(run(&host, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"WITH_SLASH"), "/work/include");
    assert_eq!(var(&mut interp, b"WITHOUT_SLASH"), "/work/include");
    assert_eq!(var(&mut interp, b"DIRECTORY"), "/work/include/sub");
    assert_eq!(var(&mut interp, b"LIBRARY"), "LIBRARY-NOTFOUND");
    assert_eq!(
        var(&mut interp, b"FRAMEWORK"),
        "/work/frameworks/Foo.framework"
    );
    Ok(())
}

struct TestHostWrapper {
    inner: Arc<PureHost>,
    case_insensitive: bool,
    read_dir_calls: std::sync::atomic::AtomicUsize,
    system_calls: std::sync::atomic::AtomicUsize,
    run_calls: std::sync::atomic::AtomicUsize,
    trailing_slash_metadata: std::sync::atomic::AtomicUsize,
}

impl TestHostWrapper {
    fn new(inner: Arc<PureHost>, case_insensitive: bool) -> Self {
        Self {
            inner,
            case_insensitive,
            read_dir_calls: std::sync::atomic::AtomicUsize::new(0),
            system_calls: std::sync::atomic::AtomicUsize::new(0),
            run_calls: std::sync::atomic::AtomicUsize::new(0),
            trailing_slash_metadata: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn resolve_case(&self, p: &Path) -> std::path::PathBuf {
        if !self.case_insensitive || self.inner.metadata(p, rcmake_host::Follow::No).is_ok() {
            return p.to_path_buf();
        }
        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            let requested = rcmake_host::path::from_native(Path::new(name));
            if let Ok(entries) = self.inner.read_dir(parent)
                && let Some(actual) = entries
                    .iter()
                    .find(|entry| entry.name.as_slice().eq_ignore_ascii_case(&requested))
            {
                return parent.join(rcmake_host::path::to_native(&actual.name));
            }
        }
        p.to_path_buf()
    }
}

impl Host for TestHostWrapper {
    fn metadata(&self, p: &Path, follow: rcmake_host::Follow) -> HostResult<rcmake_host::Metadata> {
        let raw = p.as_os_str().as_encoded_bytes();
        if raw.len() > 1 && raw.ends_with(b"/") {
            self.trailing_slash_metadata
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let resolved = self.resolve_case(p);
        self.inner.metadata(&resolved, follow)
    }

    fn metadata_fresh(
        &self,
        p: &Path,
        follow: rcmake_host::Follow,
    ) -> HostResult<rcmake_host::Metadata> {
        self.metadata(p, follow)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let resolved = self.resolve_case(p);
        self.inner.read(&resolved, range)
    }

    fn write(&self, p: &Path, data: &[u8], mode: rcmake_host::WriteMode) -> HostResult<()> {
        self.inner.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<rcmake_host::DirEntry>> {
        self.read_dir_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.read_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.inner.create_dir_all(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.inner.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.inner.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.inner.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.inner.rename(from, to)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: rcmake_host::CopyOptions) -> HostResult<()> {
        self.inner.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.inner.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.inner.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<std::path::PathBuf> {
        self.inner.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<std::path::PathBuf> {
        self.inner.canonicalize(p)
    }

    fn canonicalize_fresh(&self, p: &Path) -> HostResult<std::path::PathBuf> {
        self.inner.canonicalize_fresh(p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.inner.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
    ) -> HostResult<()> {
        self.inner.set_times(p, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.inner.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.inner.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.inner.env_all()
    }

    fn run(
        &self,
        req: &rcmake_host::RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.run(req, on_output)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.inner.write_out(stream, bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> rcmake_host::TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> HostResult<std::path::PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> std::path::PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        self.system_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.system()
    }
}

#[test]
fn find_program_strict_hermetic_allows_exact_executable_without_parent_access() -> HostResult<()> {
    use rcmake_eval::{HermeticMode, Services};
    use rcmake_host::path;

    let abs = |p: &str| -> std::path::PathBuf {
        if cfg!(windows) {
            format!("C:{p}").into()
        } else {
            p.into()
        }
    };
    let source = abs("/project");
    let build = abs("/build");
    let external_dir = abs("/external");
    let tool = external_dir.join("tool");
    let cmake_exe = abs("/bin/cmake");

    let inner = Arc::new(PureHost::new(source.clone(), cmake_exe.clone()));
    inner.create_dir_all(&build)?;
    inner.seed_file(&cmake_exe, b"")?;
    inner.set_permissions(&cmake_exe, 0o755)?;
    inner.seed_file(&tool, b"")?;
    inner.set_permissions(&tool, 0o755)?;
    inner.env_set(
        b"RCMAKE_HERMETIC_ALLOW_PATHS",
        Some(&path::from_native(&tool)),
    );
    let script = format!(
        "cmake_minimum_required(VERSION 4.5)\nproject(Audit NONE)\nset(CMAKE_FIND_APPBUNDLE NEVER)\nfind_program(RESULT NAMES tool fallback PATHS \"{}\" NO_DEFAULT_PATH REQUIRED)\n",
        path::from_native_cmake(&external_dir),
    );
    inner.seed_file(&source.join("CMakeLists.txt"), script.as_bytes())?;

    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;
    let runtime = crate::hermetic::HermeticRuntime::new(inner.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create hermetic runtime"))?;
    let mut interp = Interp::new(
        InterpConfig {
            host: runtime.host(),
            mode: RunMode::Configure,
            cmake_root: source.join("data"),
            cmake_command: cmake_exe,
            cwd: source.clone(),
            limits: Default::default(),
            diag: Default::default(),
            services: std::sync::Arc::new(services),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    runtime.install(&mut interp);
    let outcome = interp.run_configure(&source, &build, &[]);
    let diagnostics = inner.stream_bytes(Stream::Stderr);
    assert_eq!(outcome.exit_code, 0, "{diagnostics}");
    assert!(!runtime.denied(), "{diagnostics}");
    assert_eq!(
        var(&mut interp, b"RESULT"),
        path::from_native_cmake(&tool),
        "{diagnostics}"
    );
    Ok(())
}

#[test]
fn find_darwin_hotpath_avoids_eager_read_dir_plutil_and_trailing_slashes() -> HostResult<()> {
    use std::sync::atomic::Ordering;

    let inner = Arc::new(PureHost::default());
    let mut darwin = inner.system();
    darwin.sysname = "Darwin".into();
    inner.set_system(darwin)?;
    inner.seed_file(Path::new("/work/bins/tool"), b"")?;
    inner.set_permissions(Path::new("/work/bins/tool"), 0o755)?;
    inner.seed_file(
        Path::new("/work/apps/Editor.app/Contents/Info.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>EditorBin</string>
</dict>
</plist>
"#,
    )?;
    inner.seed_file(
        Path::new("/work/apps/Editor.app/Contents/MacOS/EditorBin"),
        b"",
    )?;
    inner.set_permissions(
        Path::new("/work/apps/Editor.app/Contents/MacOS/EditorBin"),
        0o755,
    )?;
    inner.seed_file(Path::new("/work/mylib/libdemo.a"), b"")?;

    let host = Arc::new(TestHostWrapper::new(inner.clone(), false));
    let mut interp = Interp::new(
        InterpConfig::new(
            host.clone(),
            RunMode::Script,
            "/share/cmake",
            "/bin/cmake",
            "/work",
        ),
        crate::register_all,
    );
    let script = r#"
find_program(TOOL NAMES missing1 missing2 tool PATHS /work/missing_dir /work/bins NO_DEFAULT_PATH)
find_program(EDITOR NAMES Editor PATHS /work/apps NO_DEFAULT_PATH)
set(CMAKE_FIND_LIBRARY_PREFIXES lib)
set(CMAKE_FIND_LIBRARY_SUFFIXES .a)
set(CMAKE_SIZEOF_VOID_P 8)
set_property(GLOBAL PROPERTY FIND_LIBRARY_USE_LIB64_PATHS ON)
find_library(DEMO NAMES missing demo PATHS /work/missing_lib /work/mylib NO_DEFAULT_PATH)
"#;
    host.read_dir_calls.store(0, Ordering::Relaxed);
    host.run_calls.store(0, Ordering::Relaxed);
    host.trailing_slash_metadata.store(0, Ordering::Relaxed);
    host.system_calls.store(0, Ordering::Relaxed);
    assert_eq!(run(&inner, &mut interp, script)?, 0);
    assert_eq!(var(&mut interp, b"TOOL"), "/work/bins/tool");
    assert_eq!(
        var(&mut interp, b"EDITOR"),
        "/work/apps/Editor.app/Contents/MacOS/EditorBin"
    );
    assert_eq!(var(&mut interp, b"DEMO"), "/work/mylib/libdemo.a");
    assert_eq!(host.read_dir_calls.load(Ordering::Relaxed), 0);
    assert_eq!(host.run_calls.load(Ordering::Relaxed), 0);
    assert_eq!(host.trailing_slash_metadata.load(Ordering::Relaxed), 0);
    assert_eq!(host.system_calls.load(Ordering::Relaxed), 3);
    Ok(())
}

#[test]
fn find_package_warns_on_module_name_case_mismatch() -> HostResult<()> {
    let inner = Arc::new(PureHost::default());
    inner.seed_file(
        Path::new("/work/ModuleNameCaseMismatch/FindCaseModule.cmake"),
        b"set(CASEMODULE_FOUND TRUE)\n",
    )?;
    inner.seed_file(
        Path::new("/work/ModuleNameCaseMismatch.cmake"),
        br#"list(PREPEND CMAKE_MODULE_PATH "${CMAKE_CURRENT_LIST_DIR}/ModuleNameCaseMismatch")
find_package(casemodule MODULE)
if(NOT casemodule_FOUND)
  message(FATAL_ERROR "FindCaseModule.cmake not included")
endif()
"#,
    )?;
    inner.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Case NONE)\ninclude(ModuleNameCaseMismatch.cmake)\n",
    )?;

    let host = Arc::new(TestHostWrapper::new(inner.clone(), true));
    let mut interp = Interp::new(
        InterpConfig::new(
            host.clone(),
            RunMode::Configure,
            "/share/cmake",
            "/bin/cmake",
            "/work",
        ),
        crate::register_all,
    );
    let outcome = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
    let stderr = inner.stream_bytes(Stream::Stderr).to_string();
    assert_eq!(outcome.exit_code, 0, "{stderr}");
    assert_eq!(var(&mut interp, b"casemodule_FOUND"), "TRUE");
    let expected = concat!(
        "CMake Warning (author) at ModuleNameCaseMismatch.cmake:2 (find_package):\n",
        "  The module name\n",
        "\n",
        "    Findcasemodule.cmake\n",
        "\n",
        "  does not match the case of the module file name on disk\n",
        "\n",
        "    /work/ModuleNameCaseMismatch/FindCaseModule.cmake\n",
        "\n",
        "  This may fail on case-sensitive file systems.  Use the module name\n",
        "\n",
        "    FindCaseModule.cmake\n",
        "\n",
        "  instead.\n",
        "Call Stack (most recent call first):\n",
        "  CMakeLists.txt:3 (include)\n",
        "This warning is for project developers.  Use -Wno-author to suppress it.\n",
    );
    assert_eq!(stderr.trim_end(), expected.trim_end());

    // Exact-case module lookup emits no warning.
    let inner_exact = Arc::new(PureHost::default());
    inner_exact.seed_file(
        Path::new("/work/ModuleNameCaseMismatch/FindCaseModule.cmake"),
        b"set(CASEMODULE_FOUND TRUE)\n",
    )?;
    let host_exact = Arc::new(TestHostWrapper::new(inner_exact.clone(), true));
    let mut interp_exact = Interp::new(
        InterpConfig::new(
            host_exact,
            RunMode::Script,
            "/share/cmake",
            "/bin/cmake",
            "/work",
        ),
        crate::register_all,
    );
    assert_eq!(
        run(
            &inner_exact,
            &mut interp_exact,
            "set(CMAKE_MODULE_PATH /work/ModuleNameCaseMismatch)\nfind_package(CaseModule MODULE REQUIRED)\n",
        )?,
        0
    );
    assert!(inner_exact.stream_bytes(Stream::Stderr).is_empty());
    Ok(())
}

#[test]
fn find_package_prefers_bundled_module_when_called_from_cmake_modules() -> HostResult<()> {
    let (host, mut interp) = setup_package();
    host.seed_file(
        Path::new("/work/cmake/FindZLIB.cmake"),
        b"set(ZLIB_ORIGIN user)\nset(ZLIB_FOUND TRUE)\n",
    )?;
    host.seed_file(
        Path::new("/share/cmake/Modules/FindZLIB.cmake"),
        b"set(ZLIB_ORIGIN bundled)\nset(ZLIB_FOUND TRUE)\n",
    )?;
    host.seed_file(
        Path::new("/share/cmake/Modules/FindPNG.cmake"),
        b"find_package(ZLIB MODULE REQUIRED)\nset(PNG_FOUND TRUE)\n",
    )?;
    host.seed_file(
        Path::new("/work/cmake/FindBoost.cmake"),
        b"set(BOOST_ORIGIN user)\nset(Boost_FOUND TRUE)\n",
    )?;
    host.seed_file(
        Path::new("/share/cmake/Modules/FindBoost.cmake"),
        b"set(BOOST_ORIGIN bundled)\nset(Boost_FOUND TRUE)\n",
    )?;
    host.seed_file(
        Path::new("/share/cmake/Modules/FindCaller.cmake"),
        b"find_package(Boost MODULE QUIET)\nset(Caller_FOUND TRUE)\n",
    )?;
    let script = r#"
set(CMAKE_MODULE_PATH /work/cmake)
find_package(PNG MODULE REQUIRED)
set(FROM_BUNDLED "${ZLIB_ORIGIN}")
unset(ZLIB_FOUND)
unset(ZLIB_ORIGIN)
find_package(ZLIB MODULE REQUIRED)
set(FROM_USER "${ZLIB_ORIGIN}")
cmake_policy(SET CMP0167 NEW)
find_package(Caller MODULE REQUIRED)
"#;
    let exit = run(&host, &mut interp, script)?;
    assert_eq!(exit, 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(var(&mut interp, b"FROM_BUNDLED"), "bundled");
    assert_eq!(var(&mut interp, b"FROM_USER"), "user");
    assert_eq!(var(&mut interp, b"BOOST_ORIGIN"), "");
    assert_eq!(var(&mut interp, b"Boost_FOUND"), "0");
    Ok(())
}
