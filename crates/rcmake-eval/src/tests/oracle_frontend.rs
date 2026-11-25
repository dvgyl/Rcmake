use super::new_interp;
use crate::{Frontend, RunMode};
use rcmake_host::Stream;
use std::path::PathBuf;

#[test]
fn real_expr_frontend_matches_oracle() {
    let (mut interp, host) = new_interp(
        &[(
            "/work/main.cmake",
            concat!(
                "set(l a b)\n",
                "foreach(x IN LISTS l)\n  message(\"x=${x} line=${CMAKE_CURRENT_LIST_LINE}\")\nendforeach()\n",
                "if(DEFINED l AND NOT \"${l}\" STREQUAL \"\")\n  message(ok)\nendif()\n",
                "if(COMMAND message AND POLICY CMP0219)\n  message(cmd)\nendif()\n",
                "function(f)\n  message(\"argn=${ARGN}\")\nendfunction()\n",
                "f(1 2)\n",
                "macro(m a)\n  message(\"${a}\")\nendmacro()\n",
                "cmake_policy(SET CMP0219 NEW)\n",
                "m(\"p\\\\q\")\n",
            ),
        )],
        RunMode::Script,
    );
    interp.frontend = Frontend::default();
    let code = interp.run_script(&PathBuf::from("main.cmake")).exit_code;
    assert_eq!(code, 0);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "x=a line=3\nx=b line=3\nok\ncmd\nargn=1;2\np\\q\n"
    );
}

/// Scripts and outputs recorded from the pinned CMake 4.5.1 oracle (`cmake -P main.cmake`
/// in a directory that is mapped to `/work`). They run with the real `rcmake-expr`
/// frontend and avoid author, deprecation, and policy warnings, whose rendering
/// belongs to `rcmake-diag`.
const ORACLE_CASES: &[(&str, &str, &str, &str, i32)] = &[
    (
        "scopes",
        r#"set(a outer)
function(f)
  set(a changed PARENT_SCOPE)
  message("in f: ${a}")
  set(b new PARENT_SCOPE)
endfunction()
f()
message("a=${a} b=${b}")
block(PROPAGATE p)
  set(p 1)
  set(q 2)
endblock()
message("p=[${p}] q=[${q}]")
"#,
        r#""#,
        r#"in f: outer
a=changed b=new
p=[1] q=[]
"#,
        0,
    ),
    (
        "foreach",
        r#"foreach(i RANGE 3)
  message("i=${i}")
endforeach()
foreach(i RANGE 5 1 -2)
  message("j=${i}")
endforeach()
set(A 1 2 3)
set(B x)
foreach(v IN ZIP_LISTS A B)
  if(DEFINED v_1)
    message("${v_0}=${v_1}")
  else()
    message("${v_0}=none")
  endif()
endforeach()
foreach(x IN ITEMS a b LISTS A)
  if(x STREQUAL "b")
    continue()
  endif()
  message("x=${x}")
endforeach()
"#,
        r#""#,
        r#"i=0
i=1
i=2
i=3
j=5
j=3
j=1
1=x
2=none
3=none
x=a
x=1
x=2
x=3
"#,
        0,
    ),
    (
        "return",
        r#"cmake_policy(SET CMP0140 NEW)
function(g)
  set(r 5)
  return(PROPAGATE r)
  message("no")
endfunction()
g()
message("r=${r}")
"#,
        r#""#,
        r#"r=5
"#,
        0,
    ),
    (
        "macro",
        r#"macro(m a)
  message("a=${a} argc=${ARGC} argv=${ARGV} argn=${ARGN}")
  set(mv "${a}")
endmacro()
m(1 2 3)
message("mv=${mv}")
function(h)
  m(inner)
  message("h mv=${mv}")
endfunction()
h()
message("top mv=${mv}")
"#,
        r#""#,
        r#"a=1 argc=3 argv=1;2;3 argn=2;3
mv=1
a=inner argc=1 argv=inner argn=
h mv=inner
top mv=1
"#,
        0,
    ),
    (
        "fatal",
        r#"message(STATUS "begin")
function(boom)
  message(FATAL_ERROR "bad thing")
endfunction()
boom()
message("unreached")
"#,
        r#"-- begin
"#,
        r#"CMake Error at main.cmake:3 (message):
  bad thing
Call Stack (most recent call first):
  main.cmake:5 (boom)


"#,
        1,
    ),
    (
        "recursion",
        r#"function(rec)
  set(x 1)
  rec()
endfunction()
set(CMAKE_MAXIMUM_RECURSION_DEPTH 3)
rec()
"#,
        r#""#,
        r#"CMake Error at main.cmake:2 (set):
  Maximum recursion depth of 3 exceeded
Call Stack (most recent call first):
  main.cmake:3 (rec)
  main.cmake:3 (rec)
  main.cmake:6 (rec)


"#,
        1,
    ),
    (
        "include",
        r#"set(CMAKE_MODULE_PATH ${CMAKE_CURRENT_LIST_DIR}/mods)
include(Inc RESULT_VARIABLE r)
message("r=${r}")
include(Missing OPTIONAL RESULT_VARIABLE r2)
message("r2=${r2}")
include(Missing)
"#,
        r#""#,
        r#"in /work/mods/Inc.cmake parent=/work/main.cmake
r=/work/mods/Inc.cmake
r2=NOTFOUND
CMake Error at main.cmake:6 (include):
  include could not find requested file:

    Missing


"#,
        1,
    ),
];

#[test]
fn oracle_recorded_scripts_match_with_real_frontend() {
    for (name, script, stdout, stderr, exit) in ORACLE_CASES {
        let (mut interp, host) = new_interp(
            &[
                ("/work/main.cmake", script),
                (
                    "/work/mods/Inc.cmake",
                    "message(\"in ${CMAKE_CURRENT_LIST_FILE} parent=${CMAKE_PARENT_LIST_FILE}\")\ninclude_guard()\n",
                ),
            ],
            RunMode::Script,
        );
        interp.frontend = Frontend::default();
        let code = interp.run_script(&PathBuf::from("main.cmake")).exit_code;
        assert_eq!(host.stream_bytes(Stream::Stdout), *stdout, "{name}: stdout");
        assert_eq!(host.stream_bytes(Stream::Stderr), *stderr, "{name}: stderr");
        assert_eq!(code, *exit, "{name}: exit code");
    }
}
