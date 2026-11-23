use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, list};
use rcmake_diag::{DiagOptions, PolicyId, PolicyStatus};
use rcmake_eval::{CacheDefine, Interp, InterpConfig, LogLevel, RunMode};
use rcmake_host::{Follow, Host, PureHost, ReadRange, Stream};
use rcmake_model::CacheType;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn context(mode: RunMode) -> (Arc<PureHost>, Interp) {
    let cwd = PathBuf::from("/work");
    let exe = PathBuf::from("/bin/cmake");
    let host = Arc::new(PureHost::new(cwd.clone(), exe.clone()));
    let interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode,
            cmake_root: "/share/cmake-4.5".into(),
            cmake_command: exe,
            cwd,
            limits: Default::default(),
            diag: DiagOptions::default(),
            defines: vec![CacheDefine {
                name: "chosen".into(),
                ty: Some(CacheType::String),
                value: "initial".into(),
            }],
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    (host, interp)
}

fn run(host: &PureHost, interp: &mut Interp, script: &str, code: i32) -> TestResult {
    let path = Path::new("/work/check.cmake");
    host.seed_file(path, script.as_bytes())?;
    assert_eq!(
        interp.run_script(path).exit_code,
        code,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

fn configure(host: &PureHost, interp: &mut Interp, script: &str, code: i32) -> TestResult {
    let source = Path::new("/work/src");
    let binary = Path::new("/work/build");
    host.seed_file(&source.join("CMakeLists.txt"), script.as_bytes())?;
    assert_eq!(
        interp.run_configure(source, binary, &[]).exit_code,
        code,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

fn assert_var(interp: &mut Interp, name: &str, value: Option<&str>) {
    assert_eq!(
        interp.cx().var(name.as_bytes()),
        value.map(BString::from),
        "{name}"
    );
}

#[test]
fn policy_versions_and_scopes() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    interp
        .cx()
        .set_var(b"CMAKE_POLICY_VERSION_MINIMUM", "3.31".into());
    run(
        &host,
        &mut interp,
        r#"
        cmake_minimum_required(VERSION 3.1...3.10 FATAL_ERROR)
        cmake_policy(GET CMP0174 minimum_policy)
        cmake_policy(GET CMP0219 unset_policy)
        cmake_policy(PUSH)
        cmake_policy(SET CMP0174 OLD)
        cmake_policy(GET CMP0174 changed_policy)
        cmake_policy(POP)
        cmake_policy(GET CMP0174 restored_policy)
        cmake_policy(VERSION 3.5...4.5)
        cmake_policy(GET CMP0219 range_policy)
    "#,
        0,
    )?;
    assert_var(&mut interp, "CMAKE_MINIMUM_REQUIRED_VERSION", Some("3.1"));
    assert_var(&mut interp, "minimum_policy", Some("NEW"));
    assert_var(&mut interp, "unset_policy", Some(""));
    assert_var(&mut interp, "changed_policy", Some("OLD"));
    assert_var(&mut interp, "restored_policy", Some("NEW"));
    assert_var(&mut interp, "range_policy", Some("NEW"));
    assert_eq!(interp.cx().policy(PolicyId::new(219)), PolicyStatus::New);
    Ok(())
}

#[test]
fn policy_errors_stop_the_script() -> TestResult {
    for (script, fragment) in [
        (
            "cmake_minimum_required(VERSION 3.4)",
            "Compatibility with CMake < 3.5 has been removed from CMake.",
        ),
        (
            "cmake_minimum_required(VERSION 4.5.2)",
            "CMake 4.5.2 or higher is required.",
        ),
        (
            "cmake_policy(VERSION 4.5...3.10)",
            "specifies a later minimum than maximum.",
        ),
        (
            "cmake_policy(SET CMP0001 OLD)",
            "Policy CMP0001 may not be set to OLD behavior",
        ),
        (
            "cmake_policy(GET CMP9999 value)",
            "GET given policy \"CMP9999\"",
        ),
        ("cmake_policy(POP)", "POP without matching PUSH"),
    ] {
        let (host, mut interp) = context(RunMode::Script);
        run(&host, &mut interp, &format!("{script}\nmessage(never)"), 1)?;
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains(fragment), "{stderr}");
        assert!(!stderr.contains("never"), "{stderr}");
    }
    Ok(())
}

#[test]
fn diagnostic_actions_and_scopes() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    run(
        &host,
        &mut interp,
        r#"
        cmake_diagnostic(SET CMD_AUTHOR IGNORE RECURSE)
        cmake_diagnostic(GET CMD_POLICY initial)
        cmake_diagnostic(PUSH)
        cmake_diagnostic(PROMOTE CMD_AUTHOR SEND_ERROR NO_RECURSE)
        cmake_diagnostic(GET CMD_POLICY unchanged)
        cmake_diagnostic(PROMOTE CMD_AUTHOR WARN)
        cmake_diagnostic(GET CMD_AUTHOR parent)
        cmake_diagnostic(GET CMD_POLICY child)
        cmake_diagnostic(DEMOTE CMD_AUTHOR IGNORE)
        cmake_diagnostic(GET CMD_POLICY demoted)
        cmake_diagnostic(POP)
        cmake_diagnostic(GET CMD_AUTHOR restored)
    "#,
        0,
    )?;
    for (name, value) in [
        ("initial", "IGNORE"),
        ("unchanged", "IGNORE"),
        ("parent", "SEND_ERROR"),
        ("child", "WARN"),
        ("demoted", "IGNORE"),
        ("restored", "IGNORE"),
    ] {
        assert_var(&mut interp, name, Some(value));
    }
    let (host, mut interp) = context(RunMode::Script);
    run(&host, &mut interp, "cmake_diagnostic(POP)", 1)?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("POP without matching PUSH")
    );
    Ok(())
}

#[test]
fn regular_argument_parsing() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    interp.cx().set_var(b"p_UNUSED", "stale".into());
    run(
        &host,
        &mut interp,
        r#"
        cmake_minimum_required(VERSION 4.5)
        cmake_parse_arguments(p "FAST;SAFE" "DEST;UNUSED" LIST
            first FAST "DEST;a\;b" LIST c d LIST "" DEST LIST e)
    "#,
        0,
    )?;
    for (name, value) in [
        ("p_FAST", "TRUE"),
        ("p_SAFE", "FALSE"),
        ("p_DEST", "a;b"),
        ("p_LIST", "c;d;e"),
        ("p_UNPARSED_ARGUMENTS", "first"),
        ("p_KEYWORDS_MISSING_VALUES", "DEST;LIST"),
    ] {
        assert_var(&mut interp, name, Some(value));
    }
    assert_var(&mut interp, "p_UNUSED", None);
    Ok(())
}

#[test]
fn parse_argv_and_argn_preserve_arguments() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    run(
        &host,
        &mut interp,
        r#"
        cmake_minimum_required(VERSION 4.5)
        function(f named)
          cmake_parse_arguments(PARSE_ARGV 0 v "" "ONE;EMPTY" "MANY;NONE")
          cmake_parse_arguments(PARSE_ARGN n "" "ONE;EMPTY" "MANY;NONE")
          return(PROPAGATE v_ONE v_MANY v_UNPARSED_ARGUMENTS v_KEYWORDS_MISSING_VALUES
                           n_ONE n_EMPTY n_MANY n_NONE n_UNPARSED_ARGUMENTS n_KEYWORDS_MISSING_VALUES)
        endfunction()
        f("formal;name" "unknown;value" ONE "a;b" EMPTY "" MANY "c;d" "" "e\;f" NONE)
    "#,
        0,
    )?;
    assert_var(&mut interp, "v_ONE", Some("a;b"));
    assert_var(&mut interp, "v_MANY", Some(r"c\;d;;e\\;f"));
    assert_var(
        &mut interp,
        "v_UNPARSED_ARGUMENTS",
        Some(r"formal\;name;unknown\;value"),
    );
    assert_var(&mut interp, "v_KEYWORDS_MISSING_VALUES", Some("NONE"));
    assert_var(&mut interp, "n_ONE", Some("a;b"));
    assert_var(&mut interp, "n_EMPTY", Some(""));
    assert_var(&mut interp, "n_MANY", Some(r"c\;d;;e\\;f"));
    assert_var(&mut interp, "n_NONE", None);
    assert_var(&mut interp, "n_UNPARSED_ARGUMENTS", Some(r"unknown\;value"));
    assert_var(&mut interp, "n_KEYWORDS_MISSING_VALUES", Some("NONE"));
    Ok(())
}

#[test]
fn parse_empty_single_value_uses_policy() -> TestResult {
    for (policy, result) in [("", None), ("OLD", None), ("NEW", Some(""))] {
        let (host, mut interp) = context(RunMode::Script);
        let setup = if policy.is_empty() {
            "cmake_policy(VERSION 3.30)".to_owned()
        } else {
            format!("cmake_policy(SET CMP0174 {policy})")
        };
        run(
            &host,
            &mut interp,
            &format!(
                r#"
            {setup}
            cmake_policy(SET CMP0140 NEW)
            function(f)
              cmake_parse_arguments(PARSE_ARGN p "" "EMPTY;MISSING" "")
              return(PROPAGATE p_EMPTY p_MISSING p_KEYWORDS_MISSING_VALUES)
            endfunction()
            f(EMPTY "" MISSING)
        "#
            ),
            0,
        )?;
        assert_var(&mut interp, "p_EMPTY", result);
        assert_var(&mut interp, "p_MISSING", result);
        assert_var(&mut interp, "p_KEYWORDS_MISSING_VALUES", Some("MISSING"));
        let warning = host
            .stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Policy CMP0174");
        assert_eq!(warning, policy.is_empty());
    }
    Ok(())
}

#[test]
fn parse_modes_validate_the_signature() -> TestResult {
    for (script, message) in [
        (
            r#"cmake_parse_arguments(PARSE_ARGN 0 p "" "" "")"#,
            "exactly 5 arguments",
        ),
        (
            r#"cmake_parse_arguments(PARSE_ARGV -1 p "" "" "")"#,
            "index '-1' is not an unsigned integer",
        ),
        (
            r#"cmake_parse_arguments(PARSE_ARGN p "" "" "")"#,
            "ARGNC='' that is not an unsigned integer",
        ),
    ] {
        let (host, mut interp) = context(RunMode::Script);
        run(&host, &mut interp, script, 1)?;
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains(message), "{stderr}");
    }
    Ok(())
}

#[test]
fn configure_file_expands_templates_and_preserves_mtime() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    let source = Path::new("/work/config.in");
    let output = Path::new("/work/sub/config.out");
    host.seed_file(source, b"#cmakedefine01 ENABLE\r\n@NAME@ ${NAME}\nlast")?;
    host.set_permissions(source, 0o751)?;
    interp.cx().set_var(b"ENABLE", "ON".into());
    interp.cx().set_var(b"NAME", "a\"b".into());
    let script = "configure_file(config.in sub/config.out @ONLY ESCAPE_QUOTES NEWLINE_STYLE CRLF)";
    run(&host, &mut interp, script, 0)?;
    let read = |path: &Path| host.read(path, ReadRange::All { limit: 1024 });
    assert_eq!(
        read(output)?,
        b"#define ENABLE 1\r\na\\\"b ${NAME}\r\nlast\r\n"
    );
    let first = host.metadata(output, Follow::Yes)?;
    assert_eq!(first.mode, 0o751);
    run(&host, &mut interp, script, 0)?;
    assert_eq!(host.metadata(output, Follow::Yes)?.mtime, first.mtime);
    interp.cx().set_var(b"NAME", "changed".into());
    run(&host, &mut interp, script, 0)?;
    assert!(host.metadata(output, Follow::Yes)?.mtime > first.mtime);

    host.create_dir_all(Path::new("/work/copy"))?;
    run(
        &host,
        &mut interp,
        "configure_file(config.in copy COPYONLY NO_SOURCE_PERMISSIONS)",
        0,
    )?;
    let copied = Path::new("/work/copy/config.in");
    assert_eq!(read(copied)?, read(source)?);
    assert_eq!(host.metadata(copied, Follow::Yes)?.mode, 0o644);

    run(
        &host,
        &mut interp,
        "configure_file(config.in explicit FILE_PERMISSIONS OWNER_READ GROUP_READ NEWLINE_STYLE LF)",
        0,
    )?;
    let explicit = Path::new("/work/explicit");
    assert_eq!(
        read(explicit)?,
        b"#define ENABLE 1\nchanged changed\nlast\n"
    );
    assert_eq!(host.metadata(explicit, Follow::Yes)?.mode, 0o440);
    Ok(())
}

#[test]
fn configure_file_rejects_incompatible_options() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    host.seed_file(Path::new("/work/in"), b"old")?;
    run(
        &host,
        &mut interp,
        "configure_file(in out COPYONLY NEWLINE_STYLE LF)",
        1,
    )?;
    assert!(host.metadata(Path::new("/work/out"), Follow::Yes).is_err());
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("COPYONLY could not be used")
    );
    Ok(())
}

#[test]
fn filename_components_paths_program_and_cache() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    host.seed_file(Path::new("/tools/my tool"), b"")?;
    host.set_permissions(Path::new("/tools/my tool"), 0o755)?;
    host.env_set(b"PATH", Some(b"/tools"));
    host.seed_file(Path::new("/real/file"), b"")?;
    host.symlink(Path::new("/real"), Path::new("/work/link"))?;
    run(
        &host,
        &mut interp,
        r#"
        get_filename_component(dir "path//archive.tar.gz" DIRECTORY)
        get_filename_component(path "path/archive.tar.gz" PATH)
        get_filename_component(name "path/archive.tar.gz" NAME CACHE)
        get_filename_component(name "other.txt" NAME CACHE)
        get_filename_component(ext "path/archive.tar.gz" EXT)
        get_filename_component(last_ext "path/archive.tar.gz" LAST_EXT)
        get_filename_component(stem "path/archive.tar.gz" NAME_WE)
        get_filename_component(last_stem "path/archive.tar.gz" NAME_WLE)
        get_filename_component(abs "../archive.tar.gz" ABSOLUTE BASE_DIR /some/sub)
        get_filename_component(real "link/file" REALPATH)
        get_filename_component(program [=["my tool" -flag  x]=] PROGRAM PROGRAM_ARGS params)
    "#,
        0,
    )?;
    for (name, value) in [
        ("dir", "path"),
        ("path", "path"),
        ("name", "archive.tar.gz"),
        ("ext", ".tar.gz"),
        ("last_ext", ".gz"),
        ("stem", "archive"),
        ("last_stem", "archive.tar"),
        ("abs", "/some/archive.tar.gz"),
        ("real", "/real/file"),
        ("program", "/tools/my tool"),
        ("params", " -flag  x"),
    ] {
        assert_var(&mut interp, name, Some(value));
    }
    assert_eq!(
        interp.cx().cache().get(b"name").map(|entry| entry.ty),
        Some(CacheType::String)
    );
    Ok(())
}

#[test]
fn properties_use_scope_and_query_modes() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    run(
        &host,
        &mut interp,
        r#"
        define_property(DIRECTORY PROPERTY flavor INHERITED BRIEF_DOCS "Brief" " text" FULL_DOCS "Full text")
        set_property(GLOBAL PROPERTY flavor vanilla)
        get_property(inherited DIRECTORY PROPERTY flavor)
        set_property(DIRECTORY APPEND PROPERTY flavor mint)
        set_property(DIRECTORY APPEND_STRING PROPERTY flavor "-chip")
        get_property(local DIRECTORY PROPERTY flavor)
        get_property(defined DIRECTORY PROPERTY flavor DEFINED)
        get_property(is_set DIRECTORY PROPERTY flavor SET)
        get_property(brief DIRECTORY PROPERTY flavor BRIEF_DOCS)
        get_property(full DIRECTORY PROPERTY flavor FULL_DOCS)
        get_property(missing GLOBAL PROPERTY absent)
        get_property(missing_docs GLOBAL PROPERTY absent BRIEF_DOCS)
        get_cmake_property(global flavor)
        get_cmake_property(unknown absent)
        get_cmake_property(cache_names CACHE_VARIABLES)
        set_property(CACHE chosen PROPERTY STRINGS first second)
        set_property(CACHE chosen APPEND PROPERTY STRINGS third)
        set_property(CACHE chosen PROPERTY ADVANCED TRUE)
        set_property(CACHE chosen APPEND_STRING PROPERTY VALUE "-updated")
        get_property(strings CACHE chosen PROPERTY STRINGS)
        get_property(advanced CACHE chosen PROPERTY ADVANCED)
        get_property(chosen_value CACHE chosen PROPERTY VALUE)
        set_property(DIRECTORY PROPERTY flavor)
        get_property(inherited_again DIRECTORY PROPERTY flavor)
    "#,
        0,
    )?;
    for (name, value) in [
        ("inherited", "vanilla"),
        ("local", "mint-chip"),
        ("defined", "1"),
        ("is_set", "1"),
        ("brief", "Brief text"),
        ("full", "Full text"),
        ("missing_docs", "NOTFOUND"),
        ("global", "vanilla"),
        ("unknown", "NOTFOUND"),
        ("cache_names", "chosen"),
        ("strings", "first;second;third"),
        ("advanced", "TRUE"),
        ("chosen_value", "initial-updated"),
        ("inherited_again", "vanilla"),
    ] {
        assert_var(&mut interp, name, Some(value));
    }
    assert_var(&mut interp, "missing", None);
    Ok(())
}

#[test]
fn define_property_is_unavailable_in_script_mode() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    run(
        &host,
        &mut interp,
        "define_property(GLOBAL PROPERTY test)",
        1,
    )?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("command is not scriptable")
    );
    Ok(())
}

#[test]
fn host_information_uses_the_host_and_sysroot() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    host.seed_file(
        Path::new("/target/etc/os-release"),
        b"NAME='Target OS'\nID=target\nVERSION_ID=\"2.1\"\n",
    )?;
    host.seed_file(Path::new("/etc/os-release"), b"ID=host\n")?;
    host.env_set(b"LANG", Some(b"en_GB.UTF-8"));
    interp.cx().set_var(b"CMAKE_SYSROOT", "/target".into());
    run(
        &host,
        &mut interp,
        r#"
        cmake_minimum_required(VERSION 4.5)
        cmake_host_system_information(RESULT identity QUERY NUMBER_OF_LOGICAL_CORES
          NUMBER_OF_PHYSICAL_CORES HOSTNAME OS_NAME OS_RELEASE OS_VERSION OS_PLATFORM IS_64BIT LOCALE_CHARSET)
        cmake_host_system_information(RESULT host QUERY DISTRIB_ID)
        cmake_host_system_information(RESULT distro QUERY FROM_SYSROOT ON DISTRIB_INFO)
    "#,
        0,
    )?;
    assert_var(
        &mut interp,
        "identity",
        Some("4;4;purehost;PureOS;1.0.0;PureHost;arm64;1;UTF-8"),
    );
    assert_var(&mut interp, "host", Some("host"));
    assert_var(
        &mut interp,
        "distro",
        Some("distro_ID;distro_NAME;distro_VERSION_ID"),
    );
    assert_var(&mut interp, "distro_NAME", Some("Target OS"));
    assert_var(&mut interp, "distro_VERSION_ID", Some("2.1"));
    Ok(())
}

#[test]
fn language_call_eval_and_log_level() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    interp.set_log_level(Some(LogLevel::Debug));
    run(
        &host,
        &mut interp,
        r#"
        cmake_minimum_required(VERSION 4.5)
        function(report arg)
          message(STATUS "reported: ${arg}")
        endfunction()
        cmake_language(CALL RePoRt "one;two")
        cmake_language(EVAL CODE "cmake_policy(GET" "CMP0174 evaluated)")
        cmake_language(GET_MESSAGE_LOG_LEVEL level)
    "#,
        0,
    )?;
    assert_var(&mut interp, "evaluated", Some("NEW"));
    assert_var(&mut interp, "level", Some("DEBUG"));
    assert!(
        host.stream_bytes(Stream::Stdout)
            .to_string()
            .contains("-- reported: one;two")
    );
    Ok(())
}

#[test]
fn language_rejects_unknown_meta_operation_and_structural_commands() -> TestResult {
    for (script, error) in [
        (
            "cmake_language(GET_COMMAND_LINE command)",
            "unknown meta-operation",
        ),
        (
            "cmake_language(CALL if TRUE)",
            "invalid command specified: if",
        ),
    ] {
        let (host, mut interp) = context(RunMode::Script);
        run(&host, &mut interp, script, 1)?;
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains(error)
        );
    }
    Ok(())
}

#[test]
fn project_sets_metadata_without_a_compiler() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    configure(
        &host,
        &mut interp,
        r#"
        cmake_minimum_required(VERSION 4.5.1)
        project(Test VERSION 1.2.3 COMPAT_VERSION 1.1 SPDX_LICENSE "MIT OR Apache-2.0"
            DESCRIPTION "Test project" HOMEPAGE_URL "https://example.com" LANGUAGES NONE)
        "#,
        0,
    )?;
    assert_var(&mut interp, "PROJECT_NAME", Some("Test"));
    assert_var(&mut interp, "CMAKE_PROJECT_NAME", Some("Test"));
    assert_var(&mut interp, "CMAKE_SYSTEM_NAME", Some("PureOS"));
    assert_var(&mut interp, "CMAKE_SYSTEM_PROCESSOR", Some("arm64"));
    for prefix in ["PROJECT", "Test"] {
        for (suffix, value) in [
            ("SOURCE_DIR", "/work/src"),
            ("BINARY_DIR", "/work/build"),
            ("IS_TOP_LEVEL", "ON"),
        ] {
            assert_var(&mut interp, &format!("{prefix}_{suffix}"), Some(value));
        }
    }
    for prefix in ["PROJECT", "Test", "CMAKE_PROJECT"] {
        for (suffix, value) in [
            ("VERSION", "1.2.3"),
            ("VERSION_MAJOR", "1"),
            ("VERSION_MINOR", "2"),
            ("VERSION_PATCH", "3"),
            ("VERSION_TWEAK", ""),
            ("COMPAT_VERSION", "1.1"),
            ("SPDX_LICENSE", "MIT OR Apache-2.0"),
            ("DESCRIPTION", "Test project"),
            ("HOMEPAGE_URL", "https://example.com"),
        ] {
            assert_var(&mut interp, &format!("{prefix}_{suffix}"), Some(value));
        }
    }
    Ok(())
}

#[test]
fn project_accepts_legacy_languages_and_clears_version_metadata() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    configure(
        &host,
        &mut interp,
        "project(Test VERSION 001.02 LANGUAGES NONE)\nproject(Other C CXX)\nproject(Other NONE)",
        0,
    )?;
    assert_var(&mut interp, "CMAKE_PROJECT_NAME", Some("Other"));
    assert_var(&mut interp, "PROJECT_VERSION", Some(""));
    assert_var(&mut interp, "CMAKE_PROJECT_VERSION", Some(""));
    assert_var(&mut interp, "Other_VERSION", None);
    assert_var(&mut interp, "Test_VERSION", Some("001.02"));
    Ok(())
}

#[test]
fn project_validates_version_and_language_arguments() -> TestResult {
    for (script, fragment) in [
        (
            "project(Test NONE VERSION 1.2.3)",
            "must use LANGUAGES before",
        ),
        (
            "project(Test VERSION 1.2.3.4.5 LANGUAGES NONE)",
            "VERSION \"1.2.3.4.5\" format invalid.",
        ),
        (
            "project(Test VERSION 1 COMPAT_VERSION 1.1 LANGUAGES NONE)",
            "COMPAT_VERSION must be less than or equal to VERSION",
        ),
    ] {
        let (host, mut interp) = context(RunMode::Configure);
        configure(&host, &mut interp, script, 1)?;
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains(fragment)
        );
    }
    Ok(())
}

#[test]
fn language_exit_stops_execution() -> TestResult {
    for (script, code) in [
        ("cmake_language(EXIT 5)", 5),
        ("cmake_language(EXIT 0)", 0),
        ("cmake_language(EXIT -5)", -5),
        ("cmake_language(EXIT 2147483648)", i32::MIN),
        (r#"cmake_language(EVAL CODE "cmake_language(EXIT 8)")"#, 8),
    ] {
        let (host, mut interp) = context(RunMode::Script);
        run(
            &host,
            &mut interp,
            &format!("{script}\nget_cmake_property(after CMAKE_ROLE)"),
            code,
        )?;
        assert_var(&mut interp, "after", None);
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
    }
    Ok(())
}

#[test]
fn language_exit_reports_invalid_arguments() -> TestResult {
    for (script, error) in [
        ("cmake_language(EXIT)", "EXIT requires one argument"),
        ("cmake_language(EXIT 5 6)", "EXIT requires one argument"),
        (
            "cmake_language(EXIT wrong)",
            "EXIT requires one integral argument, got \"wrong\"",
        ),
        (
            "cmake_language(EXIT 9223372036854775808)",
            "EXIT requires one integral argument, got",
        ),
    ] {
        let (host, mut interp) = context(RunMode::Script);
        run(&host, &mut interp, script, 1)?;
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains(error)
        );
    }
    Ok(())
}

#[test]
fn language_exit_requires_script_mode() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    configure(
        &host,
        &mut interp,
        "project(Test NONE)\ncmake_language(EXIT 5)",
        1,
    )?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("cmake_language EXIT can be used only in SCRIPT mode")
    );
    Ok(())
}

#[test]
fn role_and_commands_properties_report_interpreter_state() -> TestResult {
    let script = r#"
        function(custom_command)
        endfunction()
        get_property(role GLOBAL PROPERTY CMAKE_ROLE)
        get_cmake_property(other_role CMAKE_ROLE)
        get_cmake_property(commands COMMANDS)
    "#;
    for (mode, role) in [(RunMode::Script, "SCRIPT"), (RunMode::Configure, "PROJECT")] {
        let (host, mut interp) = context(mode);
        match mode {
            RunMode::Script => run(&host, &mut interp, script, 0)?,
            RunMode::Configure => configure(&host, &mut interp, script, 0)?,
        }
        assert_var(&mut interp, "role", Some(role));
        assert_var(&mut interp, "other_role", Some(role));
        let names = interp.cx().registered_command_names();
        assert!(names.iter().any(|name| name == b"custom_command"));
        assert_eq!(interp.cx().var(b"commands"), Some(list::join(names)));
    }
    Ok(())
}

#[test]
fn script_source_properties_support_multiple_files_and_updates() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    run(
        &host,
        &mut interp,
        r#"
        set_property(SOURCE a.c b.c PROPERTY LABEL initial)
        set_property(SOURCE a.c APPEND PROPERTY LABEL next)
        set_property(SOURCE a.c APPEND_STRING PROPERTY LABEL -tail)
        get_property(a SOURCE a.c PROPERTY LABEL)
        get_property(b SOURCE b.c PROPERTY LABEL)
        set_property(SOURCE b.c PROPERTY LABEL)
        get_property(removed SOURCE b.c PROPERTY LABEL SET)
        get_property(missing SOURCE missing.c PROPERTY LABEL)
        "#,
        0,
    )?;
    assert_var(&mut interp, "a", Some("initial;next-tail"));
    assert_var(&mut interp, "b", Some("initial"));
    assert_var(&mut interp, "removed", Some("0"));
    assert_var(&mut interp, "missing", None);
    Ok(())
}

#[test]
fn set_property_reports_a_missing_target() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    run(
        &host,
        &mut interp,
        "set_property(TARGET nope PROPERTY FOO foo)",
        1,
    )?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains("set_property could not find TARGET nope.  Perhaps it has not yet been")
    );
    assert!(stderr.contains("  created."));
    Ok(())
}

#[test]
fn site_name_uses_the_host_and_preserves_existing_values() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    interp.cx().set_var(b"existing", "previous".into());
    run(
        &host,
        &mut interp,
        r#"
        site_name(site)
        get_property(site_type CACHE site PROPERTY TYPE)
        site_name(existing)
        site_name(chosen)
        "#,
        0,
    )?;
    assert_var(&mut interp, "site", Some("purehost"));
    assert_var(&mut interp, "site_type", Some("STRING"));
    assert_var(&mut interp, "existing", Some("previous"));
    assert_var(&mut interp, "chosen", Some("initial"));
    assert!(!interp.cx().cache().contains(b"existing"));
    Ok(())
}

#[test]
fn variable_watch_accepts_an_optional_callback() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    run(
        &host,
        &mut interp,
        "variable_watch(a)\nvariable_watch(b callback)",
        0,
    )?;
    let (host, mut interp) = context(RunMode::Script);
    run(&host, &mut interp, "variable_watch()", 1)?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("variable_watch must be called with at least one argument.")
    );
    Ok(())
}

#[test]
fn project_commands_are_unavailable_in_script_mode() -> TestResult {
    for command in [
        "project",
        "add_executable",
        "add_library",
        "add_subdirectory",
        "add_custom_target",
        "add_custom_command",
        "add_custom_rule",
        "add_dependencies",
        "target_sources",
        "target_include_directories",
        "target_link_libraries",
        "target_compile_definitions",
        "target_compile_options",
        "target_compile_features",
        "target_link_options",
        "target_link_directories",
        "install",
        "export",
        "enable_language",
        "enable_testing",
        "add_test",
        "discover_tests",
    ] {
        let (host, mut interp) = context(RunMode::Script);
        run(&host, &mut interp, &format!("{command}()"), 1)?;
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains(&format!("{command} command is not scriptable"))
        );
    }
    Ok(())
}
