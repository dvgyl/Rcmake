use super::{new_interp, run, run_configure, run_files};
use crate::RunMode;
use rcmake_diag::{PolicyId, PolicyStatus};

#[test]
fn policies_write_through_weak_call_scopes_and_capture_at_definition() {
    let o = run(concat!(
        "function(f)\n  cmake_policy(SET CMP0140 NEW)\nendfunction()\n",
        "f()\n",
        "cmake_policy(GET CMP0140 x)\n",
        "message(\"after f: [${x}]\")\n",
        "cmake_policy(SET CMP0124 NEW)\n",
        "function(g)\n  cmake_policy(GET CMP0124 y)\n  message(\"in g: [${y}]\")\nendfunction()\n",
        "cmake_policy(SET CMP0124 OLD)\n",
        "g()\n",
        "cmake_policy(PUSH)\n",
        "cmake_policy(SET CMP0124 NEW)\n",
        "cmake_policy(POP)\n",
        "cmake_policy(GET CMP0124 z)\n",
        "message(\"after pop: [${z}]\")\n",
    ));
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "after f: [NEW]\nin g: [NEW]\nafter pop: [OLD]\n");
}

#[test]
fn removed_policies_are_always_new() {
    let (mut interp, _) = new_interp(&[], RunMode::Script);
    assert_eq!(interp.cx().policy(PolicyId::new(54)), PolicyStatus::New);
    assert_eq!(interp.cx().policy(PolicyId::new(219)), PolicyStatus::Warn);
}

#[test]
fn policy_pop_without_push_and_unbalanced_push() {
    let o = run("cmake_policy(POP)\n");
    assert_eq!(o.exit, 1);
    assert!(o.stderr.contains("cmake_policy POP without matching PUSH"));

    let o = run_files(
        "include(inc.cmake)\nmessage(after)\n",
        &[("/work/inc.cmake", "cmake_policy(PUSH)\n")],
    );
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr.contains("cmake_policy PUSH without matching POP"),
        "{}",
        o.stderr
    );
}

#[test]
fn diagnostic_scopes_are_independent() {
    let o = run(concat!(
        "block(SCOPE_FOR DIAGNOSTICS)\n",
        "  cmake_diagnostic(SET CMD_AUTHOR IGNORE)\n",
        "  message(AUTHOR_WARNING hidden)\n",
        "endblock()\n",
        "message(AUTHOR_WARNING shown)\n",
        "cmake_diagnostic(PUSH)\n",
        "cmake_diagnostic(SET CMD_AUTHOR IGNORE)\n",
        "cmake_diagnostic(POP)\n",
        "message(AUTHOR_WARNING shown2)\n",
        "cmake_diagnostic(POP)\n",
    ));
    assert_eq!(o.exit, 1);
    assert!(!o.stderr.contains("hidden"));
    assert!(o.stderr.contains("shown\n"));
    assert!(o.stderr.contains("shown2\n"));
    assert!(
        o.stderr
            .contains("cmake_diagnostic POP without matching PUSH")
    );
}

#[test]
fn include_sets_list_file_variables_and_policy_scope() {
    let o = run_files(
        concat!(
            "include(sub/inc.cmake RESULT_VARIABLE r)\n",
            "message(\"r=${r} clf=${CMAKE_CURRENT_LIST_FILE} plf=[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "cmake_policy(GET CMP0124 p)\n",
            "message(\"p=[${p}] from_inc=${from_inc}\")\n",
            "include(sub/inc.cmake NO_POLICY_SCOPE)\n",
            "cmake_policy(GET CMP0124 p)\n",
            "message(\"p=[${p}]\")\n",
        ),
        &[(
            "/work/sub/inc.cmake",
            concat!(
                "message(\"in: ${CMAKE_CURRENT_LIST_FILE} ${CMAKE_CURRENT_LIST_DIR} ${CMAKE_PARENT_LIST_FILE}\")\n",
                "cmake_policy(SET CMP0124 NEW)\n",
                "set(from_inc yes)\n",
                "return()\n",
                "message(\"not reached\")\n",
            ),
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "in: /work/sub/inc.cmake /work/sub /work/main.cmake\n",
            "r=/work/sub/inc.cmake clf=/work/main.cmake plf=[]\n",
            "p=[] from_inc=yes\n",
            "in: /work/sub/inc.cmake /work/sub /work/main.cmake\n",
            "p=[NEW]\n",
        )
    );
}

#[test]
fn include_resolves_modules_and_reports_missing_files() {
    let o = run_files(
        concat!(
            "set(CMAKE_MODULE_PATH /work/cmake)\n",
            "include(Mine)\n",
            "include(Std)\n",
            "include(Nope OPTIONAL RESULT_VARIABLE r)\n",
            "message(\"r=${r}\")\n",
            "include(Nope)\n",
        ),
        &[
            ("/work/cmake/Mine.cmake", "message(mine)\n"),
            (
                "/opt/cmake/share/cmake-4.5/Modules/Std.cmake",
                "message(std)\n",
            ),
        ],
    );
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "mine\nstd\nr=NOTFOUND\n",
            "CMake Error at main.cmake:6 (include):\n",
            "  include could not find requested file:\n\n",
            "    Nope\n\n\n",
        )
    );
}

#[test]
fn include_parse_error_shows_call_stack() {
    let o = run_files(
        "include(bad.cmake)\n",
        &[("/work/bad.cmake", "message(a)\nendif()\n")],
    );
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at bad.cmake:2 (endif):\n",
            "  Flow control statements are not properly nested.\n",
            "Call Stack (most recent call first):\n",
            "  main.cmake:1 (include)\n\n\n",
        )
    );
}

#[test]
fn include_guard_skips_second_inclusion() {
    let o = run_files(
        "include(g.cmake)\ninclude(g.cmake)\ninclude(gg.cmake)\ninclude(gg.cmake)\n",
        &[
            ("/work/g.cmake", "include_guard()\nmessage(once)\n"),
            ("/work/gg.cmake", "include_guard(GLOBAL)\nmessage(global)\n"),
        ],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(o.stderr, "once\nglobal\n");
    let o = run("include_guard(BAD)\n");
    assert!(
        o.stderr
            .contains("include_guard given an invalid scope: BAD")
    );
}

#[test]
fn include_file_api_reports_missing_and_optional() {
    let o =
        run("test_include_file(/work/none.cmake OPTIONAL)\ntest_include_file(/work/none.cmake)\n");
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("test_include_file could not find requested file:\n\n    /work/none.cmake\n")
    );
}

#[test]
fn cmp0198_controls_parent_list_file_in_directory_listfile() {
    let o = run_configure(
        concat!(
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "include(inc.cmake)\n",
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "cmake_policy(SET CMP0198 NEW)\n",
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
            "include(inc.cmake)\n",
            "message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n",
        ),
        &[(
            "/work/src/inc.cmake",
            "message(\"inc [${CMAKE_PARENT_LIST_FILE}]\")\n",
        )],
    );
    assert_eq!(o.exit, 0, "{}", o.stderr);
    assert_eq!(
        o.stderr,
        concat!(
            "[/work/src/CMakeLists.txt]\n",
            "inc [/work/src/CMakeLists.txt]\n",
            "[/work/src/CMakeLists.txt]\n",
            "[]\n",
            "inc [/work/src/CMakeLists.txt]\n",
            "[]\n",
        )
    );
}

#[test]
fn cmp0198_does_not_apply_in_script_mode() {
    let o = run("message(\"[${CMAKE_PARENT_LIST_FILE}]\")\n");
    assert_eq!(o.stderr, "[]\n");
}
