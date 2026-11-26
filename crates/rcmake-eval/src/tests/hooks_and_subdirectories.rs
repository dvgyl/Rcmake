use super::{Out, new_interp, run, run_configure, run_configure_interp};
use crate::RunMode;
use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_host::Stream;
use rcmake_model::DirId;
use std::path::PathBuf;

#[test]
fn variable_watch_reports_each_access_kind() {
    let o = run(concat!(
        "function(cb var access value file stack)\n",
        "  message(\"${var} ${access} [${value}]\")\n",
        "endfunction()\n",
        "variable_watch(v cb)\n",
        "set(v 1)\n",
        "set(x \"${v}\")\n",
        "unset(v)\n",
        "set(y \"${v}\")\n",
        "function(f)\n",
        "  set(v 2 PARENT_SCOPE)\n",
        "endfunction()\n",
        "f()\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "v MODIFIED_ACCESS [1]\n",
            "v READ_ACCESS [1]\n",
            "v REMOVED_ACCESS []\n",
            "v UNKNOWN_READ_ACCESS []\n",
            "v MODIFIED_ACCESS [2]\n",
        )
    );
}

#[test]
fn variable_watch_passes_list_file_and_stack() {
    let o = run(concat!(
        "function(cb var access value file stack)\n",
        "  message(\"${file}|${stack}\")\n",
        "endfunction()\n",
        "variable_watch(v cb)\n",
        "set(v 1)\n",
    ));
    assert_eq!(o.stderr, "/work/main.cmake|;/work/main.cmake\n");
}

#[test]
fn variable_watch_without_command_logs_the_access() {
    let o = run("variable_watch(w)\nset(w 2)\n");
    assert!(
        o.stderr
            .contains("  Variable \"w\" was accessed using MODIFIED_ACCESS with value \"2\".\n"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("main.cmake:2 (set):"), "{}", o.stderr);
}

#[test]
fn variable_watch_callback_does_not_recurse() {
    let o = run(concat!(
        "function(cb var access value)\n",
        "  message(cb)\n",
        "  set(v x PARENT_SCOPE)\n",
        "endfunction()\n",
        "variable_watch(v cb)\n",
        "set(v 0)\n",
        "message(\"[${v}]\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "cb\ncb\n[x]\n");
}

#[test]
fn variable_watch_runs_every_watch_once_per_access() {
    let o = run(concat!(
        "function(w1)\n  message(w1)\n  variable_watch(v w2)\nendfunction()\n",
        "function(w2)\n  message(w2)\nendfunction()\n",
        "variable_watch(v w1)\n",
        "variable_watch(v w2)\n",
        "set(x \"${v}\")\n",
    ));
    assert_eq!(o.stderr, "w1\nw2\n");
}

#[test]
fn variable_watch_callback_failure_is_reported() {
    let o = run("variable_watch(v nope)\nset(v 1)\n");
    assert!(
        o.stderr.contains("Unknown CMake command \"nope\"."),
        "{}",
        o.stderr
    );
}

#[test]
fn deferred_calls_run_at_directory_end_with_late_expansion() {
    let o = run_configure(
        concat!(
            "test_defer(a message \"${x} ${CMAKE_CURRENT_LIST_LINE}\")\n",
            "test_defer(b message never)\n",
            "test_defer(c test_defer d message nested)\n",
            "test_defer_query(ids)\n",
            "test_defer_query(call GET a)\n",
            "test_defer_query(left CANCEL b)\n",
            "message(\"${ids}|${call}|${left}\")\n",
            "set(x late)\n",
        ),
        &[],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "a;b;c|message;${x} ${CMAKE_CURRENT_LIST_LINE}|a;c\n",
            "late DEFERRED:a\n",
            "nested\n",
        )
    );
}

#[test]
fn deferred_call_queue_is_inactive_in_script_mode() {
    let o = run("test_defer_query(ids)\nmessage(\"${ids}\")\n");
    assert_eq!(o.stderr, "inactive\n");
}

#[test]
fn deferred_call_error_shows_deferred_frame() {
    let o = run_configure("test_defer(a nope)\n", &[]);
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .starts_with("CMake Error at CMakeLists.txt:1 (nope):\n"),
        "{}",
        o.stderr
    );
}

#[test]
fn subdirectory_copies_variables_and_parent_scope_updates_caller_only() {
    let o = run_configure(
        concat!(
            "set(a parent)
",
            "test_subdir(sub)
",
            "message(\"after a=${a} b=${b} c=${c}\")
",
            "message(\"dirs ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_BINARY_DIR}\")
",
        ),
        &[(
            "/work/src/sub/CMakeLists.txt",
            concat!(
                "message(\"child a=${a}\")
",
                "set(a child)
",
                "set(b up PARENT_SCOPE)
",
                "message(\"child b=[${b}]\")
",
                "message(\"dirs ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_BINARY_DIR}\")
",
                "message(\"list ${CMAKE_CURRENT_LIST_FILE} ${CMAKE_CURRENT_LIST_DIR}\")
",
            ),
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "child a=parent\n",
            "child b=[]\n",
            "dirs /work/src/sub /work/build/sub\n",
            "list /work/src/sub/CMakeLists.txt /work/src/sub\n",
            "after a=parent b=up c=\n",
            "dirs /work/src /work/build\n",
        )
    );
}

#[test]
fn subdirectory_return_propagate_reaches_caller() {
    let o = run_configure(
        "test_subdir(sub)\nmessage(\"r=[${r}]\")\n",
        &[(
            "/work/src/sub/CMakeLists.txt",
            "cmake_policy(SET CMP0140 NEW)\nset(r 7)\nreturn(PROPAGATE r)\nmessage(no)\n",
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "r=[7]\n");
}

#[test]
fn subdirectory_isolates_policies_and_records_final_policies() {
    let (o, interp) = run_configure_interp(
        concat!(
            "cmake_policy(SET CMP0140 OLD)\n",
            "test_subdir(sub out EXCLUDE_FROM_ALL)\n",
            "cmake_policy(GET CMP0140 p)\n",
            "message(\"parent ${p}\")\n",
        ),
        &[
            (
                "/work/src/sub/CMakeLists.txt",
                "cmake_policy(SET CMP0140 NEW)\ninclude(${CMAKE_CURRENT_LIST_DIR}/x.cmake)\n",
            ),
            ("/work/src/sub/x.cmake", ""),
        ],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "parent OLD\n");
    let project = &interp.project;
    let root = project.dir(DirId::ROOT);
    let [child] = root.children.as_slice() else {
        panic!("one child expected");
    };
    let child = project.dir(*child);
    assert_eq!(child.source_dir, "/work/src/sub");
    assert_eq!(child.binary_dir, "/work/build/out");
    assert!(child.exclude_from_all);
    assert_eq!(child.parent, Some(DirId::ROOT));
    let cmp0140 = PolicyId::new(140);
    assert_eq!(
        child.final_policies.map(|p| p.get(cmp0140)),
        Some(PolicyStatus::New)
    );
    assert_eq!(
        root.final_policies.map(|p| p.get(cmp0140)),
        Some(PolicyStatus::Old)
    );
    assert_eq!(
        child.listfiles,
        vec![
            BString::from("/work/src/sub/CMakeLists.txt"),
            BString::from("/work/src/sub/x.cmake"),
        ]
    );
    assert_eq!(
        root.listfiles,
        vec![BString::from("/work/src/CMakeLists.txt")]
    );
}

#[test]
fn subdirectory_runs_its_deferred_calls_before_returning() {
    let o = run_configure(
        concat!(
            "test_defer(p message parent-deferred)\n",
            "test_subdir(sub)\n",
            "message(after-subdir)\n",
        ),
        &[(
            "/work/src/sub/CMakeLists.txt",
            "test_defer(c message child-deferred)\ntest_defer_query(ids)\nmessage(\"child ids ${ids}\")\n",
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        "child ids c\nchild-deferred\nafter-subdir\nparent-deferred\n"
    );
}

#[test]
fn subdirectory_without_listfile_is_an_error() {
    let o = run_configure("test_subdir(missing)\nmessage(next)\n", &[]);
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at CMakeLists.txt:1 (test_subdir):\n",
            "  The source directory\n\n",
            "    /work/src/missing\n\n",
            "  does not contain a CMakeLists.txt file.\n\n\n",
            "next\n",
        )
    );
}

fn run_traced(main: &str, config: crate::TraceConfig) -> Out {
    let (mut interp, host) = new_interp(&[("/work/main.cmake", main)], RunMode::Script);
    interp.set_trace_config(Some(config));
    let outcome = interp.run_script(&PathBuf::from("main.cmake"));
    Out {
        exit: outcome.exit_code,
        stdout: String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)).into_owned(),
        stderr: String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned(),
    }
}

#[test]
fn trace_human_lists_commands_and_end_commands_after_bodies() {
    let o = run_traced(
        "set(a 1)\nif(a)\n  set(b \"${a}\")\nelse()\nendif()\nforeach(i 1 2)\nendforeach()\n",
        crate::TraceConfig::default(),
    );
    assert_eq!(
        o.stderr,
        concat!(
            "/work/main.cmake(1):  set(a 1 )\n",
            "/work/main.cmake(2):  if(a )\n",
            "/work/main.cmake(3):  set(b ${a} )\n",
            "/work/main.cmake(5):  endif()\n",
            "/work/main.cmake(6):  foreach(i 1 2 )\n",
            "/work/main.cmake(7):  endforeach()\n",
        )
    );
}

#[test]
fn trace_expand_and_json_v1() {
    let o = run_traced(
        "set(a \"x\\\"y\x7f\")\nmessage(\"${a}\")\n",
        crate::TraceConfig {
            format: crate::TraceFormat::JsonV1,
            expand: true,
            ..crate::TraceConfig::default()
        },
    );
    let lines: Vec<&str> = o.stderr.lines().collect();
    assert_eq!(
        lines.first().copied(),
        Some("{\"version\":{\"major\":1,\"minor\":2}}")
    );
    let msg = lines.get(2).copied().unwrap_or_default();
    assert!(
        msg.starts_with(
            "{\"args\":[\"x\\\"y\x7f\"],\"cmd\":\"message\",\"file\":\"/work/main.cmake\",\"frame\":1,\"global_frame\":1,\"line\":2,\"time\":"
        ),
        "{msg}"
    );
}

#[test]
fn trace_source_filters_files() {
    let o = run_traced(
        "set(a 1)\n",
        crate::TraceConfig {
            sources: vec!["other.cmake".into()],
            ..crate::TraceConfig::default()
        },
    );
    assert_eq!(o.stderr, "");
}

#[test]
fn variable_watch_stack_lists_files_and_function_scopes() {
    let o = run_configure(
        concat!(
            "function(cb var access value file stack)\n",
            "  message(\"${stack}\")\n",
            "endfunction()\n",
            "variable_watch(z cb)\n",
            "set(z 1)\n",
            "include(inc.cmake)\n",
            "function(g)\n  set(z 3)\nendfunction()\n",
            "g()\n",
        ),
        &[("/work/src/inc.cmake", "set(z 2)\n")],
    );
    assert_eq!(
        o.stderr,
        concat!(
            "/work/src/CMakeLists.txt\n",
            "/work/src/CMakeLists.txt;/work/src/inc.cmake\n",
            "/work/src/CMakeLists.txt;/work/src/CMakeLists.txt\n",
        )
    );
}

#[test]
fn foreach_reads_lists_after_the_watch_callback() {
    let out = run(r#"
function(refresh name access)
  if(access STREQUAL READ_ACCESS)
    set(${name} new again PARENT_SCOPE)
  endif()
endfunction()
set(values old)
variable_watch(values refresh)
foreach(item IN LISTS values)
  message("list=${item}")
endforeach()
set(values old)
foreach(item IN ZIP_LISTS values)
  message("zip=${item_0}")
endforeach()
"#);
    assert_eq!(out.exit, 0, "{}", out.stderr);
    assert_eq!(out.stderr, "list=new\nlist=again\nzip=new\nzip=again\n");
}
