use super::{ROOT, TEST_FRONTEND, register_test, run, run_big_stack};
use crate::{CmdFlags, CmdResult, Cx, Flow, Interp, InterpConfig, Registry, RunMode};
use rcmake_expr::ExpandedArg;
use rcmake_host::{PureHost, Stream};
use std::path::PathBuf;
use std::sync::Arc;

#[test]
fn message_routes_status_to_stdout_and_notice_to_stderr() {
    let o = run(concat!(
        "message(STATUS \"st\")\n",
        "message(\"plain\")\n",
        "message(NOTICE \"notice\")\n",
        "message(VERBOSE \"hidden\")\n",
        "message(STATUS a b \"c;d\")\n",
        "message(FOO x)\n",
    ));
    assert_eq!(o.exit, 0);
    assert_eq!(o.stdout, "-- st\n-- abc;d\n");
    assert_eq!(o.stderr, "plain\nnotice\nFOOx\n");
}

#[test]
fn message_indent_context_and_checks_match_oracle() {
    let o = run(concat!(
        "set(CMAKE_MESSAGE_INDENT \"  \" \"> \")\n",
        "set(CMAKE_MESSAGE_CONTEXT a b)\n",
        "set(CMAKE_MESSAGE_CONTEXT_SHOW ON)\n",
        "message(STATUS \"indented\")\n",
        "message(\"plain2\")\n",
        "message(CHECK_START \"chk\")\n",
        "message(CHECK_PASS \"ok\")\n",
        "message(CHECK_START \"chk2\")\n",
        "message(CHECK_START \"inner\")\n",
        "message(CHECK_FAIL \"bad\")\n",
        "message(CHECK_PASS \"ok2\")\n",
        "message(STATUS)\n",
    ));
    assert_eq!(
        o.stdout,
        concat!(
            "-- [a.b]   > indented\n",
            "-- [a.b]   > chk\n",
            "-- [a.b]   > chk - ok\n",
            "-- [a.b]   > chk2\n",
            "-- [a.b]   > inner\n",
            "-- [a.b]   > inner - bad\n",
            "-- [a.b]   > chk2 - ok2\n",
            "-- [a.b]   > \n",
        )
    );
    assert_eq!(o.stderr, "[a.b]   > plain2\n");
}

#[test]
fn message_log_level_variable_enables_verbose() {
    let o = run("set(CMAKE_MESSAGE_LOG_LEVEL debug)\nmessage(DEBUG d)\nmessage(TRACE t)\n");
    assert_eq!(o.stdout, "-- d\n");
}

#[test]
fn message_warning_and_send_error_continue_fatal_error_stops() {
    let o = run(concat!(
        "message(WARNING \"w1\" \"w2\")\n",
        "message(SEND_ERROR \"se\")\n",
        "message(\"after\")\n",
        "message(FATAL_ERROR \"fe\")\n",
        "message(\"not reached\")\n",
    ));
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Warning at main.cmake:1 (message):\n  w1w2\n\n\n",
            "CMake Error at main.cmake:2 (message):\n  se\n\n\n",
            "after\n",
            "CMake Error at main.cmake:4 (message):\n  fe\n\n\n",
        )
    );
}

#[test]
fn message_author_warning_respects_diagnostic_state() {
    let o = run(concat!(
        "message(AUTHOR_WARNING \"aw\")\n",
        "cmake_diagnostic(SET CMD_AUTHOR IGNORE)\n",
        "message(AUTHOR_WARNING \"hidden\")\n",
        "message(DEPRECATION \"dep-shown\")\n",
    ));
    assert_eq!(o.exit, 0);
    assert!(
        o.stderr.contains("at main.cmake:1 (message):\n  aw\n"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains("hidden"));
    assert!(
        o.stderr
            .contains("at main.cmake:4 (message):\n  dep-shown\n")
    );
}

#[test]
fn message_check_pass_without_start_warns() {
    let o = run("message(CHECK_PASS \"nostart\")\n");
    assert_eq!(o.exit, 0);
    assert!(o.stderr.contains("Ignored CHECK_PASS without CHECK_START"));
}

#[test]
fn message_without_arguments_uses_call_site_spelling() {
    let o = run("MESSAGE()\nmessage(\"after\")\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        "CMake Error at main.cmake:1 (MESSAGE):\n  MESSAGE called with incorrect number of arguments\n\n\n"
    );
}

#[test]
fn unknown_command_is_fatal_with_suggestion() {
    let o = run("foo_unknown(a)\nmessage(after)\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        "CMake Error at main.cmake:1 (foo_unknown):\n  Unknown CMake command \"foo_unknown\".\n\n\n"
    );
    let o = run("mesage(x)\n");
    assert!(
        o.stderr
            .contains("Unknown CMake command \"mesage\".  Did you mean: \"message\"?")
    );
}

#[test]
fn unscriptable_command_fails_in_script_mode() {
    let o = run("add_executable(x y)\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("add_executable command is not scriptable")
    );
}

#[test]
fn recursion_limit_from_variable_matches_oracle_stack() {
    let o = run(concat!(
        "function(rec)\n",
        "  test_noop()\n",
        "  rec()\n",
        "endfunction()\n",
        "set(CMAKE_MAXIMUM_RECURSION_DEPTH 5)\n",
        "rec()\n",
    ));
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at main.cmake:2 (test_noop):\n",
            "  Maximum recursion depth of 5 exceeded\n",
            "Call Stack (most recent call first):\n",
            "  main.cmake:3 (rec)\n",
            "  main.cmake:3 (rec)\n",
            "  main.cmake:3 (rec)\n",
            "  main.cmake:3 (rec)\n",
            "  main.cmake:6 (rec)\n\n\n",
        )
    );
}

#[test]
fn default_recursion_limit_is_1000() {
    let o = run_big_stack("function(rec)\n  rec()\nendfunction()\nrec()\n");
    assert_eq!(o.exit, 1);
    assert!(o.stderr.starts_with(
        "CMake Error at main.cmake:2 (rec):\n  Maximum recursion depth of 1000 exceeded\n"
    ));
    assert_eq!(o.stderr.matches("main.cmake:2 (rec)\n").count(), 999);
}

#[test]
fn command_lookup_is_case_insensitive_and_frames_keep_spelling() {
    let o = run("Message(WARNING x)\n");
    assert!(
        o.stderr
            .starts_with("CMake Warning at main.cmake:1 (Message):")
    );
}

#[test]
fn issue_marks_command_failed_and_fatal_in_script_mode() {
    fn t_fail(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
        cx.issue(
            rcmake_diag::MessageKind::Error,
            rcmake_diag::Effect::SetError,
            None,
            "boom".into(),
        );
        Ok(Flow::Normal)
    }
    fn reg(r: &mut Registry) {
        register_test(r);
        r.add("t_fail", t_fail, CmdFlags { scriptable: true });
    }
    let host = Arc::new(PureHost::default());
    host.seed_file(
        &PathBuf::from("/work/main.cmake"),
        b"t_fail()\nmessage(after)\n",
    )
    .unwrap();
    let config = InterpConfig::new(
        host.clone(),
        RunMode::Script,
        ROOT,
        "/usr/local/bin/cmake",
        "/work",
    );
    let mut interp = Interp::new(config, reg);
    interp.frontend = TEST_FRONTEND;
    assert_eq!(interp.run_script(&PathBuf::from("main.cmake")).exit_code, 1);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "CMake Error at main.cmake:1 (t_fail):\n  boom\n\n\n"
    );
    assert!(interp.fatal_occurred());
    assert!(interp.diag_engine().error_occurred());
}
