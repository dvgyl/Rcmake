use super::{ROOT, TEST_FRONTEND, new_interp, register_test, run, t_noop, t_set};
use crate::{CmdFlags, Interp, InterpConfig, RunMode};
use rcmake_base::BString;
use rcmake_host::{PureHost, Stream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[test]
fn script_mode_variables() {
    let o = run(concat!(
        "message(\"argc=${CMAKE_ARGC} a0=${CMAKE_ARGV0} a1=${CMAKE_ARGV1} a2=${CMAKE_ARGV2}\")\n",
        "message(\"smf=${CMAKE_SCRIPT_MODE_FILE}\")\n",
        "message(\"clf=${CMAKE_CURRENT_LIST_FILE} cld=${CMAKE_CURRENT_LIST_DIR} cll=${CMAKE_CURRENT_LIST_LINE}\")\n",
        "message(\"csd=${CMAKE_CURRENT_SOURCE_DIR} cbd=${CMAKE_CURRENT_BINARY_DIR}\")\n",
        "message(\"ver=${CMAKE_VERSION} ${CMAKE_MAJOR_VERSION}.${CMAKE_MINOR_VERSION}.${CMAKE_PATCH_VERSION} t=${CMAKE_TWEAK_VERSION}\")\n",
        "message(\"root=${CMAKE_ROOT} cmd=${CMAKE_COMMAND} ctest=${CMAKE_CTEST_COMMAND}\")\n",
        "message(\"host=${CMAKE_HOST_SYSTEM_NAME} unix=${CMAKE_HOST_UNIX} U=${UNIX} win=[${WIN32}] plf=[${CMAKE_PARENT_LIST_FILE}]\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "argc=3 a0=/usr/local/bin/cmake a1=-P a2=main.cmake\n",
            "smf=/work/main.cmake\n",
            "clf=/work/main.cmake cld=/work cll=3\n",
            "csd=/work cbd=/work\n",
            "ver=4.5.1 4.5.1 t=0\n",
            "root=/opt/cmake/share/cmake-4.5 cmd=/usr/local/bin/cmake ctest=/usr/local/bin/ctest\n",
            "host=PureOS unix=1 U=1 win=[] plf=[]\n",
        )
    );
}

#[test]
fn cli_defines_are_cache_entries() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        &PathBuf::from("/work/main.cmake"),
        b"message(\"X=${X} c=$CACHE{X}\")\nif(DEFINED X)\nmessage(def)\nendif()\n",
    )
    .unwrap();
    let mut config = InterpConfig::new(
        host.clone(),
        RunMode::Script,
        ROOT,
        "/usr/local/bin/cmake",
        "/work",
    );
    config.defines = vec![crate::CacheDefine {
        name: "X".into(),
        ty: None,
        value: "1".into(),
    }];
    let mut interp = Interp::new(config, register_test);
    interp.frontend = TEST_FRONTEND;
    assert_eq!(interp.run_script(&PathBuf::from("main.cmake")).exit_code, 0);
    assert_eq!(host.stream_bytes(Stream::Stderr), "X=1 c=1\ndef\n");
}

#[test]
fn parent_scope_raise_then_set_keeps_child_view() {
    let o = run(concat!(
        "set(a outer)\n",
        "function(f)\n",
        "  set(a changed PARENT_SCOPE)\n",
        "  message(\"in f: ${a}\")\n",
        "  set(b new PARENT_SCOPE)\n",
        "  if(DEFINED b)\n",
        "    message(\"b visible in f\")\n",
        "  endif()\n",
        "  unset(a PARENT_SCOPE)\n",
        "endfunction()\n",
        "f()\n",
        "message(\"a=[${a}] b=[${b}]\")\n",
        "if(DEFINED a)\n",
        "  message(\"a defined\")\n",
        "endif()\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "in f: outer\na=[] b=[new]\n");
}

#[test]
fn parent_scope_at_top_level_warns() {
    let o = run("set(X 1 PARENT_SCOPE)\nmessage(after)\n");
    assert_eq!(o.exit, 0);
    assert!(
        o.stderr
            .contains("at main.cmake:1 (set):\n  Cannot set \"X\": current scope has no parent.\n"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.ends_with("after\n"));
}

#[test]
fn function_binds_arguments_and_isolates_scope() {
    let o = run(concat!(
        "function(f a b)\n",
        "  message(\"a=${a} b=${b} argc=${ARGC} argnc=${ARGNC} argv=${ARGV} argn=${ARGN} v0=${ARGV0} v2=${ARGV2}\")\n",
        "  message(\"fn=${CMAKE_CURRENT_FUNCTION} file=${CMAKE_CURRENT_FUNCTION_LIST_FILE} dir=${CMAKE_CURRENT_FUNCTION_LIST_DIR} line=${CMAKE_CURRENT_FUNCTION_LIST_LINE}\")\n",
        "  set(local 1)\n",
        "endfunction()\n",
        "F(x y z w)\n",
        "message(\"local=[${local}] argc=[${ARGC}]\")\n",
        "f(only)\n",
    ));
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "a=x b=y argc=4 argnc=2 argv=x;y;z;w argn=z;w v0=x v2=z\n",
            "fn=f file=/work/main.cmake dir=/work line=1\n",
            "local=[] argc=[]\n",
            "CMake Error at main.cmake:8 (f):\n",
            "  f Function invoked with incorrect arguments for function named: f\n\n\n",
        )
    );
}

#[test]
fn redefinition_keeps_one_previous_level() {
    let o = run(concat!(
        "function(f)\n  message(f1)\nendfunction()\n",
        "function(f)\n  message(f2)\n  _f()\nendfunction()\n",
        "f()\n",
        "function(message)\n  _message(\"wrapped ${ARGV}\")\nendfunction()\n",
        "message(hi)\n",
        "if(COMMAND _f AND COMMAND _message)\n  _message(both)\nendif()\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "f2\nf1\nwrapped hi\nboth\n");
}

#[test]
fn flow_control_commands_cannot_be_overridden() {
    let o = run("function(IF)\nendfunction()\nmessage(after)\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("Built-in flow control command \"if\" cannot be overridden.")
    );
    assert!(!o.stderr.contains("after"));
}

#[test]
fn macro_substitutes_textually_in_caller_scope() {
    let o = run(concat!(
        "macro(m a)\n",
        "  message(\"a=[${a}] argc=${ARGC} argv=[${ARGV}] argn=[${ARGN}] v0=${ARGV0} v1=${ARGV1}\")\n",
        "  message([[bracket ${a}]])\n",
        "  set(from_macro ${a})\n",
        "endmacro()\n",
        "m(x z)\n",
        "message(\"from_macro=${from_macro}\")\n",
        "m()\n",
    ));
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "a=[x] argc=2 argv=[x;z] argn=[z] v0=x v1=z\n",
            "bracket ${a}\n",
            "from_macro=x\n",
            "CMake Error at main.cmake:8 (m):\n",
            "  m Macro invoked with incorrect arguments for macro named: m\n\n\n",
        )
    );
}

#[test]
fn macro_cmp0219_new_preserves_backslashes() {
    let o = run(concat!(
        "cmake_policy(SET CMP0219 NEW)\n",
        "macro(m a)\n",
        "  message(\"${a}|${ARGV}\")\n",
        "endmacro()\n",
        "m(\"p\\\\q\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "p\\q|p\\q\n");
}

#[test]
fn macro_cmp0219_warns_once_per_callee() {
    let o = run(concat!(
        "macro(m a)\n",
        "  message(\"${a}\")\n",
        "endmacro()\n",
        "m(\"p\\\\\\\\q\")\n",
        "m(\"r\\\\\\\\s\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr.matches("Policy CMP0219 is not set").count(), 1);
    assert!(o.stderr.contains("at main.cmake:4 (m):"));
    assert!(
        o.stderr
            .contains("  Command \"m\" called with arguments containing backslashes.\n")
    );
    assert!(o.stderr.contains("   \"p\\\\q\"\n"));
    assert!(o.stderr.contains("   \"p\\\\\\\\q\"\n"));
    assert!(o.stderr.ends_with("p\\q\nr\\s\n"), "{}", o.stderr);
}

#[test]
fn macro_return_exits_enclosing_function() {
    let o = run(concat!(
        "macro(mr)\n  message(\"mr before\")\n  return()\n  message(\"mr after\")\nendmacro()\n",
        "function(h)\n  mr()\n  message(\"h after mr\")\nendfunction()\n",
        "h()\n",
        "message(\"top after h\")\n",
    ));
    assert_eq!(o.stderr, "mr before\ntop after h\n");
}

#[test]
fn eval_code_and_call_command() {
    let o = run(concat!(
        "cmake_language(EVAL CODE [[set(e 1)\nmessage(\"eval ${CMAKE_CURRENT_LIST_FILE} ${CMAKE_CURRENT_LIST_LINE} ${CMAKE_PARENT_LIST_FILE}\")\nmessage(WARNING w)]])\n",
        "message(\"e=${e}\")\n",
        "function(f x)\n  message(\"f got ${x}\")\nendfunction()\n",
        "cmake_language(CALL f \"a;b\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "eval /work/main.cmake:1:EVAL 2 /work/main.cmake\n",
            "CMake Warning at main.cmake:1:EVAL:3 (message):\n  w\n",
            "Call Stack (most recent call first):\n  main.cmake:1 (cmake_language)\n\n\n",
            "e=1\nf got a;b\n",
        )
    );
}

#[test]
fn cx_variable_and_scope_queries() {
    let (mut interp, _) = new_interp(&[], RunMode::Script);
    let mut cx = interp.cx();
    cx.set_var(b"a", "1".into());
    assert!(cx.var_defined(b"a"));
    assert_eq!(cx.binding(b"a"), Some(crate::Binding::Set("1".into())));
    cx.unset_var(b"a");
    assert_eq!(cx.binding(b"a"), Some(crate::Binding::Unset));
    assert!(!cx.var_defined(b"a"));
    assert!(cx.command_exists(b"IF"));
    assert!(cx.command_exists(b"message"));
    assert!(!cx.command_exists(b"nosuch"));
    assert_eq!(cx.current_source_dir(), PathBuf::from("/work"));
    assert!(
        cx.variable_names()
            .contains(&BString::from("CMAKE_VERSION"))
    );
    assert_eq!(cx.log_level(), crate::LogLevel::Status);
}

#[test]
fn match_variables_are_cleared_and_stored() {
    let (mut interp, _) = new_interp(&[], RunMode::Script);
    let mut cx = interp.cx();
    let mut groups = [None; rcmake_regex::MAX_CAPTURE_SLOTS];
    groups[0] = Some((0, 3));
    groups[1] = Some((1, 2));
    let caps = rcmake_regex::Captures { groups };
    cx.set_match_vars(Some((&caps, b"abc")));
    assert_eq!(cx.var(b"CMAKE_MATCH_0"), Some("abc".into()));
    assert_eq!(cx.var(b"CMAKE_MATCH_1"), Some("b".into()));
    assert_eq!(cx.var(b"CMAKE_MATCH_COUNT"), Some("1".into()));
    cx.set_match_vars(None);
    assert_eq!(cx.var(b"CMAKE_MATCH_0"), Some("".into()));
    assert_eq!(cx.var(b"CMAKE_MATCH_COUNT"), Some("0".into()));
}

#[test]
fn registry_add_replaces_earlier_registrations() {
    let mut registry = crate::Registry::default();
    let flags = CmdFlags { scriptable: true };
    registry.add_raw("x_cmd", t_noop, flags);
    registry.add("X_CMD", t_set, CmdFlags { scriptable: false });
    assert!(!registry.takes_raw_args(b"x_cmd"));
    let Some((func, got)) = registry.get(b"x_cmd") else {
        panic!("command is not registered");
    };
    assert!(!got.scriptable);
    assert!(std::ptr::fn_addr_eq(func, t_set as crate::BuiltinFn));
}

#[test]
fn rule_configurator_directory_scope_preserves_command_backtrace() {
    let (mut interp, host) = new_interp(&[], RunMode::Configure);
    let root = Path::new(ROOT);
    host.seed_file(&root.join("CMakeLists.txt"), b"include(defs.cmake)\n")
        .expect("seed CMakeLists.txt");
    host.seed_file(
        &root.join("defs.cmake"),
        concat!(
            "function(inner)\n",
            "  message(SEND_ERROR \"inside inner\")\n",
            "endfunction()\n",
            "function(cfg)\n",
            "  message(SEND_ERROR \"inside cfg\")\n",
            "  cmake_language(CALL inner)\n",
            "endfunction()\n",
            "function(needs_arg required)\n",
            "endfunction()\n",
        )
        .as_bytes(),
    )
    .expect("seed defs.cmake");
    let outcome = interp.run_configure(root, &root.join("build"), &[]);
    assert_eq!(outcome.exit_code, 0);

    let mut cx = interp.cx();
    let status = cx.with_directory_scope(rcmake_model::DirId::ROOT, |cx| {
        cx.global().props.set(
            b"\0RCMAKE_IN_RULE_CONFIGURATOR".as_slice().into(),
            "1".into(),
        );
        let first = cx.call_command_in_current_frame(b"cfg", Vec::new());
        let second = cx.call_command_in_current_frame(b"needs_arg", Vec::new());
        cx.global().props.unset(b"\0RCMAKE_IN_RULE_CONFIGURATOR");
        (first, second)
    });
    assert!(status.0.is_ok());
    assert!(matches!(status.1, Err(crate::CmdError::Reported)));

    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains(
            "CMake Error at defs.cmake:5 (message):\n  inside cfg\nCall Stack (most recent call first):\n  CMakeLists.txt\n"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "CMake Error at defs.cmake:2 (message):\n  inside inner\nCall Stack (most recent call first):\n  defs.cmake:6 (inner)\n  defs.cmake:6 (cmake_language)\n  CMakeLists.txt\n"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "CMake Error in CMakeLists.txt:\n  needs_arg Function invoked with incorrect arguments for function named:\n  needs_arg\n"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("(cfg)"), "{stderr}");
}
