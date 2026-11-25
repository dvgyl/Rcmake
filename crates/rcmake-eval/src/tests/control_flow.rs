use super::{new_interp, run};
use crate::RunMode;
use rcmake_host::{Host, NativeHost, Stream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn assert_empty_loop_hits_budget(script: &'static [u8]) {
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut interp, _) = new_interp(&[], RunMode::Script);
        let host = Arc::new(NativeHost::new().expect("native host"));
        let budget = Duration::from_millis(50);
        interp.config.host = host.clone();
        interp.config.limits.wall_budget = Some(budget);
        interp.started = host.monotonic();
        assert!(interp.eval_code(script).is_ok(), "valid script");
        assert!(
            interp.fatal_occurred(),
            "the empty loop must exhaust the budget"
        );
        assert!(host.monotonic() - interp.started > budget);
        send.send(()).expect("send test result");
    });
    match receive.recv_timeout(Duration::from_secs(5)) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            worker.join().expect("run budget test");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("the empty loop did not stop within five seconds");
        }
    }
}

#[test]
fn empty_while_stops_at_wall_budget() {
    assert_empty_loop_hits_budget(b"while(TRUE)\nendwhile()");
}

#[test]
fn empty_foreach_range_stops_at_wall_budget() {
    assert_empty_loop_hits_budget(b"foreach(i RANGE 1000000000)\nendforeach()");
}

#[test]
fn return_propagate_requires_cmp0140_new() {
    let o = run(concat!(
        "function(f)\n  set(a 1)\n  return(PROPAGATE a)\n  message(\"reached\")\nendfunction()\n",
        "f()\n",
        "message(\"a=[${a}]\")\n",
        "cmake_policy(SET CMP0140 NEW)\n",
        "function(g)\n  set(b 2)\n  block()\n    set(b 3)\n    return(PROPAGATE b)\n  endblock()\n  message(\"not reached\")\nendfunction()\n",
        "g()\n",
        "message(\"b=[${b}]\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert!(o.stderr.contains("Policy CMP0140 is not set"));
    assert!(o.stderr.ends_with("reached\na=[]\nb=[3]\n"), "{}", o.stderr);
}

#[test]
fn return_rejects_unknown_argument_under_new() {
    let o = run("cmake_policy(SET CMP0140 NEW)\nreturn(FOO)\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("return called with unsupported argument \"FOO\"")
    );
}

#[test]
fn break_and_continue_outside_loops_fail() {
    let o = run("break()\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("A BREAK command was found outside of a proper FOREACH or WHILE loop scope.")
    );
    let o =
        run("foreach(i 1)\n  function(f)\n    continue()\n  endfunction()\n  f()\nendforeach()\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("A CONTINUE command was found outside of a proper")
    );
}

#[test]
fn if_elseif_else_choose_one_branch() {
    let o = run(concat!(
        "set(v 2)\n",
        "if(v EQUAL 1)\n  message(one)\nelseif(v EQUAL 2)\n  message(two)\nelseif(TRUE)\n  message(three)\nelse()\n  message(other)\nendif()\n",
        "if(FALSE)\nelse()\n  message(else)\nendif()\n",
        "if(TRUE)\n  if(FALSE)\n    message(no)\n  endif()\n  message(nested)\nendif()\n",
    ));
    assert_eq!(o.stderr, "two\nelse\nnested\n");
}

#[test]
fn if_condition_error_is_fatal_and_lists_arguments() {
    let o = run("if(a b c d)\nendif()\nmessage(after)\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at main.cmake:1 (if):\n",
            "  if given arguments:\n\n",
            "    \"a\" \"b\" \"c\" \"d\"\n\n",
            "  Unknown arguments specified\n\n\n",
        )
    );
}

#[test]
fn block_end_mismatch_warns_at_file_level() {
    let o = run("function(f)\nendfunction(g)\n");
    assert_eq!(o.exit, 0);
    assert!(o.stderr.contains("in main.cmake:\n"), "{}", o.stderr);
    assert!(o.stderr.contains("/work/main.cmake:1 (function)"));
    assert!(o.stderr.contains("/work/main.cmake:2 (endfunction)"));
    assert!(o.stderr.contains("with mis-matching arguments."));
}

#[test]
fn foreach_items_range_and_restore() {
    let o = run(concat!(
        "foreach(x a b c)\n  message(\"${x}\")\nendforeach()\n",
        "foreach(i RANGE 2)\n  message(\"r${i}\")\nendforeach()\n",
        "foreach(i RANGE 10 0 -4)\n  message(\"d${i}\")\nendforeach()\n",
        "foreach(i RANGE 0 10 3)\n  message(\"s${i}\")\nendforeach()\n",
        "message(\"x=[${x}]\")\n",
        "if(DEFINED x)\n  message(\"x defined (CMP0124 unset)\")\nendif()\n",
        "cmake_policy(SET CMP0124 NEW)\n",
        "foreach(y a)\nendforeach()\n",
        "if(NOT DEFINED y)\n  message(\"y undefined\")\nendif()\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        "a\nb\nc\nr0\nr1\nr2\nd10\nd6\nd2\ns0\ns3\ns6\ns9\nx=[]\nx defined (CMP0124 unset)\ny undefined\n"
    );
}

#[test]
fn foreach_range_errors() {
    let o = run("foreach(i RANGE 5 1 1)\nendforeach()\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("foreach called with incorrect range specification: start 5, stop 1, step 1")
    );
    let o = run("foreach(i RANGE abc)\nendforeach()\n");
    assert!(o.stderr.contains("foreach Invalid integer: 'abc'"));
    let o = run("foreach(i RANGE 99999999999)\nendforeach()\n");
    assert!(
        o.stderr
            .contains("foreach Integer out of range: '99999999999'")
    );
}

#[test]
fn foreach_in_lists_and_items_switch_sections() {
    let o = run(concat!(
        "set(l1 a \"\" b)\n",
        "set(l2 \"\")\n",
        "foreach(x IN LISTS l1 ITEMS i1 LISTS l2 ITEMS i2)\n  message(\"[${x}]\")\nendforeach()\n",
    ));
    assert_eq!(o.stderr, "[a]\n[]\n[b]\n[i1]\n[i2]\n");
}

#[test]
fn foreach_zip_lists_unsets_exhausted_variables() {
    let o = run(concat!(
        "set(A 1 2 3)\n",
        "set(B x \"\")\n",
        "foreach(v IN ZIP_LISTS A B)\n",
        "  if(DEFINED v_1)\n    message(\"${v_0}:[${v_1}]\")\n  else()\n    message(\"${v_0}:exhausted\")\n  endif()\n",
        "endforeach()\n",
        "foreach(p q IN ZIP_LISTS A B)\n  message(\"${p}${q}\")\n  break()\nendforeach()\n",
        "foreach(p q IN ZIP_LISTS A)\nendforeach()\n",
    ));
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "1:[x]\n2:[]\n3:exhausted\n1x\n",
            "CMake Error at main.cmake:14 (foreach):\n  Expected 2 list variables, but given 1\n\n\n",
        )
    );
}

#[test]
fn foreach_in_mode_errors() {
    let o = run("foreach(a b IN LISTS l)\nendforeach()\n");
    assert!(
        o.stderr
            .contains("ITEMS or LISTS require exactly one iteration variable")
    );
    let o = run("foreach(a IN ZIP_LISTS l LISTS m)\nendforeach()\n");
    assert!(
        o.stderr
            .contains("ZIP_LISTS can not be used with LISTS or ITEMS")
    );
    let o = run("foreach(a IN junk)\nendforeach()\n");
    assert!(
        o.stderr.contains("Unknown argument:\n\n    junk\n"),
        "{}",
        o.stderr
    );
}

#[test]
fn while_reevaluates_condition_with_break_and_continue() {
    let o = run(concat!(
        "set(i 0)\n",
        "while(i LESS 10)\n",
        "  test_inc(i)\n",
        "  if(i EQUAL 2)\n    continue()\n  endif()\n",
        "  if(i EQUAL 5)\n    break()\n  endif()\n",
        "  message(\"i=${i}\")\n",
        "endwhile()\n",
        "message(\"end=${i}\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "i=1\ni=3\ni=4\nend=5\n");
}

#[test]
fn loop_body_errors_continue_in_configure_mode() {
    let (mut interp, host) = new_interp(
        &[(
            "/work/main.cmake",
            "foreach(x 1 2)\n  nosuch_cmd_but_continue()\nendforeach()\nmessage(after)\n",
        )],
        RunMode::Configure,
    );
    let outcome = interp.run_script(&PathBuf::from("main.cmake"));
    assert_eq!(outcome.exit_code, 1);
    assert!(
        !host
            .stream_bytes(Stream::Stderr)
            .to_string()
            .contains("after")
    );

    let (mut interp, host) = new_interp(
        &[("/work/main.cmake", "MESSAGE()\nmessage(after)\n")],
        RunMode::Configure,
    );
    assert_eq!(interp.run_script(&PathBuf::from("main.cmake")).exit_code, 1);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .ends_with("after\n")
    );
}

#[test]
fn block_scopes_and_propagate() {
    let o = run(concat!(
        "set(keep 1)\nset(gone 1)\n",
        "block(PROPAGATE out gone)\n",
        "  set(inner 1)\n  set(out 2)\n  unset(gone)\n  set(keep 9)\n",
        "endblock()\n",
        "message(\"inner=[${inner}] out=[${out}] keep=[${keep}]\")\n",
        "if(NOT DEFINED gone)\n  message(\"gone unset\")\nendif()\n",
        "block(SCOPE_FOR POLICIES)\n  set(leak 1)\nendblock()\n",
        "message(\"leak=[${leak}]\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        "inner=[] out=[2] keep=[1]\ngone unset\nleak=[1]\n"
    );
}

#[test]
fn block_propagates_while_unwinding_break() {
    let o = run(concat!(
        "foreach(i 1 2 3)\n",
        "  block(PROPAGATE last)\n",
        "    set(last ${i})\n",
        "    if(i EQUAL 2)\n      break()\n    endif()\n",
        "  endblock()\n",
        "endforeach()\n",
        "message(\"last=${last}\")\n",
    ));
    assert_eq!(o.stderr, "last=2\n");
}

#[test]
fn block_argument_errors() {
    let o = run("block(SCOPE_FOR POLICIES PROPAGATE x)\nendblock()\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("block PROPAGATE cannot be specified without a new scope for VARIABLES")
    );
    let o = run("block(SCOPE_FOR FOO)\nendblock()\n");
    assert!(
        o.stderr
            .contains("block SCOPE_FOR unsupported scope \"FOO\"")
    );
    let o = run("block(BAD)\nendblock()\n");
    assert!(
        o.stderr
            .contains("block called with unsupported argument \"BAD\"")
    );
    let o = run("block(SCOPE_FOR)\nendblock()\n");
    assert!(
        o.stderr
            .contains("Error after keyword \"SCOPE_FOR\":\n\n    missing required value\n"),
        "{}",
        o.stderr
    );
}
