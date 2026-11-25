//! Script-level tests through the public command registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::{CacheDefine, Interp, InterpConfig, RunMode};
use rcmake_host::{Host, PureHost, Stream};
use rcmake_model::CacheType;

struct Script {
    interp: Interp,
    host: Arc<PureHost>,
}

impl Script {
    fn new(defines: Vec<CacheDefine>) -> Self {
        let host = Arc::new(PureHost::default());
        let interp = Interp::new(
            InterpConfig {
                host: host.clone(),
                mode: RunMode::Script,
                cmake_root: PathBuf::from("/opt/cmake/share/cmake-4.5"),
                cmake_command: PathBuf::from("/usr/local/bin/cmake"),
                cwd: PathBuf::from("/work"),
                limits: Default::default(),
                diag: Default::default(),
                defines,
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        Self { interp, host }
    }

    fn run(&mut self, script: &str) -> i32 {
        self.run_bytes(script.as_bytes())
    }

    fn run_bytes(&mut self, script: &[u8]) -> i32 {
        let seed = self.host.seed_file(Path::new("/work/test.cmake"), script);
        assert!(seed.is_ok(), "{seed:?}");
        self.interp
            .run_script(Path::new("/work/test.cmake"))
            .exit_code
    }

    fn policy(&mut self, number: u16, status: rcmake_diag::PolicyStatus) {
        let result = self
            .interp
            .cx()
            .set_policy(rcmake_diag::PolicyId::new(number), status);
        assert!(result.is_ok(), "{result:?}");
    }

    fn success(&mut self, script: &str) {
        let exit = self.run(script);
        assert_eq!(exit, 0, "{}", self.stderr());
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.host.stream_bytes(Stream::Stderr)).into_owned()
    }

    fn var(&mut self, name: &str, expected: impl AsRef<[u8]>) {
        let actual = self.interp.cx().var(name.as_bytes());
        assert_eq!(
            actual.as_ref().map(|value| value.as_slice()),
            Some(expected.as_ref()),
            "{name}: {actual:?}"
        );
    }

    fn absent(&mut self, name: &str) {
        assert_eq!(self.interp.cx().var(name.as_bytes()), None, "{name}");
    }
}

#[test]
fn variables_environment_and_parent_scope() {
    let mut state = Script::new(Vec::new());
    state.success(
        r#"
set(words a "" c)
set(empty "")
set(gone old)
set(gone)
set(parent old)
set(to_remove old)
function(update)
  set(parent new value PARENT_SCOPE)
  unset(to_remove PARENT_SCOPE)
  set(raised "${parent}" PARENT_SCOPE)
  set(local hidden)
endfunction()
update()
set(ENV{RCMAKE_VAR_TEST} first ignored)
set(env_before "$ENV{RCMAKE_VAR_TEST}")
set(ENV{RCMAKE_VAR_TEST} "")
set(env_after "$ENV{RCMAKE_VAR_TEST}")
set(ENV{RCMAKE_VAR_TEST} again)
unset(ENV{RCMAKE_VAR_TEST})
"#,
    );
    state.var("words", b"a;;c");
    state.var("empty", b"");
    state.var("parent", b"new;value");
    state.var("raised", b"old");
    state.var("env_before", b"first");
    state.var("env_after", b"");
    state.absent("gone");
    state.absent("to_remove");
    state.absent("local");
    assert_eq!(state.host.env_get(b"RCMAKE_VAR_TEST"), None);
    assert!(state.stderr().contains("Only the first value argument"));
}

#[test]
fn cache_assignment_policies_and_options() {
    use rcmake_diag::PolicyStatus::{New, Old};

    let defines = vec![
        CacheDefine {
            name: "path_define".into(),
            ty: None,
            value: "relative;dir/../other".into(),
        },
        CacheDefine {
            name: "bool_define".into(),
            ty: None,
            value: "raw".into(),
        },
    ];
    let mut state = Script::new(defines);
    state.policy(77, New);
    state.policy(102, New);
    state.policy(126, New);
    state.success(
        r#"
set(normal current)
set(normal cached CACHE STRING "help")
set(normal ignored CACHE STRING "ignored")
set(path_define ignored CACHE PATH "path help")
set(forced first CACHE STRING "first")
set(forced second CACHE STRING "second" FORCE)
set(forced internal CACHE INTERNAL "third")
set(CACHE{modern} TYPE STRING HELP "modern help" VALUE one two)
set(CACHE{modern} FORCE VALUE three FORCE)
set(ordinary a b FORCE)
set(remove gone CACHE BOOL "remove")
unset(remove CACHE)
set(remove_again gone CACHE BOOL "remove")
unset(CACHE{remove_again})
option(normal "unused help" OFF)
option(bool_define "bool help" ON)
option(default_option "help")
option(on_option "help" yes)
option(unusual "help" 2)
mark_as_advanced(CLEAR normalized_missing)
mark_as_advanced(CLEAR normal)
mark_as_advanced(normal)
mark_as_advanced(FORCE path_define)
"#,
    );
    state.var("normal", b"current");
    state.var("path_define", b"/work/relative;/work/other");
    state.var("bool_define", b"raw");
    state.var("forced", b"internal");
    state.var("modern", b"three;FORCE");
    state.var("default_option", b"OFF");
    state.var("on_option", b"ON");
    state.var("unusual", b"OFF");
    state.var("ordinary", b"a;b;FORCE");
    state.absent("remove");
    state.absent("remove_again");
    state.absent("normalized_missing");
    let mut cx = state.interp.cx();
    let cache = cx.cache();
    assert_eq!(
        cache
            .get(b"normal")
            .map(|e| (&e.value, &e.help, e.advanced)),
        Some((&BString::from("cached"), &BString::from("help"), false))
    );
    assert_eq!(
        cache.get(b"normal").and_then(|e| e.props.get(b"ADVANCED")),
        Some(&BString::from("0"))
    );
    assert_eq!(
        cache.get(b"bool_define").map(|e| e.ty),
        Some(CacheType::Bool)
    );
    assert!(cache.get(b"path_define").is_some_and(|e| e.advanced));

    let mut legacy = Script::new(Vec::new());
    legacy.policy(77, Old);
    legacy.policy(102, Old);
    legacy.policy(126, Old);
    legacy.success(
        r#"
set(a masked)
set(a cached CACHE STRING "help")
set(a still_masked)
set(a ignored CACHE STRING "help")
set(b masked)
set(b forced CACHE STRING "help" FORCE)
set(c masked)
option(c "help" ON)
mark_as_advanced(legacy_missing)
mark_as_advanced(CLEAR placeholder_set placeholder_option)
set(placeholder_set new CACHE STRING "help")
option(placeholder_option "help" ON)
"#,
    );
    legacy.var("a", b"still_masked");
    legacy.var("b", b"forced");
    legacy.var("c", b"ON");
    legacy.var("placeholder_set", b"new");
    legacy.var("placeholder_option", b"ON");
    assert!(
        legacy
            .interp
            .cx()
            .cache()
            .get(b"legacy_missing")
            .is_some_and(|e| e.advanced && e.ty == CacheType::Uninitialized)
    );
}

#[test]
fn separate_arguments_modes_and_program_lookup() {
    let mut state = Script::new(Vec::new());
    assert!(
        state
            .host
            .seed_file(Path::new("/work/bin/print"), b"")
            .is_ok()
    );
    assert!(
        state
            .host
            .set_permissions(Path::new("/work/bin/print"), 0o755)
            .is_ok()
    );
    state.host.env_set(b"PATH", Some(b"/work/bin"));
    state.success(r#"
set(legacy " a b ; c\td ")
separate_arguments(legacy)
separate_arguments(unknown_legacy)
separate_arguments(unix UNIX_COMMAND [=[a "" '' b\ c "d\q" 'e\f' "a;b" tail\]=])
separate_arguments(windows WINDOWS_COMMAND [=[a "" "a""b" 'c d' x\y "two words" \"lit "one\\" last]=])
separate_arguments(native NATIVE_COMMAND "a b")
separate_arguments(program UNIX_COMMAND PROGRAM [=[ print  "two words"  a;b ]=])
separate_arguments(split UNIX_COMMAND PROGRAM SEPARATE_ARGS [=[print "a;b" ""]=])
separate_arguments(direct UNIX_COMMAND PROGRAM [=[./bin/print trailing]=])
separate_arguments(missing UNIX_COMMAND PROGRAM "missing-command-xyz -arg")
separate_arguments(no_input UNIX_COMMAND)
"#);
    state.var("legacy", b";a;b;;;c\td;");
    state.absent("unknown_legacy");
    state.var("unix", br"a;;;b c;dq;ef;a\;b;tail");
    state.var("windows", br#"a;;ab;'c;d';x\y;two words;"lit;one\;last"#);
    state.var("native", b"a;b");
    state.var("program", br#"/work/bin/print;  "two words"  a;b "#);
    state.var("split", br"/work/bin/print;a\;b;");
    state.var("direct", b"./bin/print; trailing");
    state.var("missing", b"");
    state.var("no_input", b"");
}

#[cfg(unix)]
#[test]
fn program_with_raw_arguments_uses_native_quoting() {
    let mut state = Script::new(Vec::new());
    assert!(
        state
            .host
            .seed_file(Path::new("/work/bin/printer"), b"")
            .is_ok()
    );
    state.success(
        r#"
separate_arguments(raw WINDOWS_COMMAND PROGRAM [=['./bin/printer' "two words"]=])
separate_arguments(split WINDOWS_COMMAND PROGRAM SEPARATE_ARGS [=['./bin/printer' "two words"]=])
"#,
    );
    state.var("raw", br#"./bin/printer; "two words""#);
    state.var("split", b"");
}

#[test]
fn list_queries_and_mutations() {
    let mut state = Script::new(Vec::new());
    state.success(
        r#"
set(xs [=[a;;[b;c];x\;y;z]=])
list(LENGTH xs length)
list(GET xs -1 1 2 3 got)
list(GET absent nonnumeric undefined_get)
list(JOIN xs " | " joined)
list(SUBLIST xs 2 -1 sub)
list(SUBLIST xs 0 40 clamped)
list(SUBLIST missing -99 10 undefined_sub)
list(FIND xs [=[x;y]=] found)
list(FIND xs absent missed)
list(APPEND xs "app;ended")
list(PREPEND xs pre)
set(preserved "${xs}")
list(INSERT xs -1 marker)
list(REMOVE_ITEM xs marker "y")
list(REMOVE_AT xs -1 0 0)
list(REVERSE xs)
list(PREPEND xs z a)
list(REMOVE_DUPLICATES xs)
list(POP_BACK xs last before_last)
list(POP_FRONT xs first)
list(POP_FRONT xs)
list(APPEND no_elements)
list(INSERT created 0 new)
list(POP_BACK created picked missing_output)
"#,
    );
    state.var("length", b"5");
    state.var("got", b"z;;[b;c];x;y");
    state.var("undefined_get", b"NOTFOUND");
    state.var("joined", b"a |  | [b;c] | x;y | z");
    state.var("sub", b"[b;c];x;y;z");
    state.var("clamped", b"a;;[b;c];x;y;z");
    state.var("undefined_sub", b"");
    state.var("found", b"3");
    state.var("missed", b"-1");
    state.var("preserved", br"pre;a;;[b;c];x\;y;z;app;ended");
    state.var("first", b"z");
    state.var("last", b"");
    state.var("before_last", b"[b;c]");
    state.var("xs", b"app;x");
    state.var("picked", b"new");
    state.var("created", b"");
    state.absent("missing_output");
    state.absent("no_elements");
}

#[test]
fn list_get_streams_indices_in_argument_order() {
    let mut state = Script::new(Vec::new());
    state.success(
        r"
set(xs [=[zero;;[two;inner];x\;y;last]=])
list(GET xs 3 0 3 1 selected)
list(GET xs -1 -4 -5 negative)
list(GET xs 2 single)
",
    );
    state.var("selected", b"x;y;zero;x;y;");
    state.var("negative", b"last;;zero");
    state.var("single", b"[two;inner]");

    for (indices, diagnostic) in [
        ("10 5", "index: 10 out of range (-5, 4)"),
        ("-6", "index: -6 out of range (-5, 4)"),
        ("5 invalid", "index: 5 out of range (-5, 4)"),
    ] {
        let mut state = Script::new(Vec::new());
        let exit = state.run(&format!("set(xs a;b;c;d;e)\nlist(GET xs {indices} result)"));
        assert_eq!(exit, 1);
        assert!(state.stderr().contains(diagnostic), "{}", state.stderr());
    }
}

#[test]
fn list_index_policy_applies_before_bounds_checks() {
    let mut state = Script::new(Vec::new());
    state.success("set(xs a;b;c)\nlist(GET xs 1word result)");
    state.var("result", b"b");
    assert!(state.stderr().contains("Invalid list index \"1word\"."));

    let mut state = Script::new(Vec::new());
    state.policy(121, rcmake_diag::PolicyStatus::Old);
    state.success("set(xs a;b;c)\nlist(REMOVE_AT xs word)");
    state.var("xs", b"b;c");
    assert!(state.stderr().is_empty());

    let mut state = Script::new(Vec::new());
    state.policy(121, rcmake_diag::PolicyStatus::New);
    let exit = state.run("set(xs a;b;c)\nlist(GET xs 9 word result)");
    assert_eq!(exit, 1);
    assert!(state.stderr().contains("index: word is not a valid index"));
}

#[test]
fn filter_regex_and_predicate() {
    let mut state = Script::new(Vec::new());
    state.success(
        r#"
function(select_element element result)
  if(NOT ARGC EQUAL 2)
    message(FATAL_ERROR "Expected two callback arguments")
  endif()
  if(element STREQUAL "x;y" OR element STREQUAL "b")
    set("${result}" YES PARENT_SCOPE)
  else()
    set("${result}" 2 PARENT_SCOPE)
  endif()
endfunction()
set(xs [=[a;x\;y;b;abc;ccc]=])
set(copy "${xs}")
set(regex "${xs}")
list(FILTER regex EXCLUDE REGEX "^a")
list(FILTER xs INCLUDE PREDICATE select_element)
list(FILTER copy EXCLUDE PREDICATE select_element)
list(FILTER unknown INCLUDE PREDICATE missing)
"#,
    );
    state.var("xs", b"x;y;b");
    state.var("regex", b"x;y;b;ccc");
    state.var("copy", b"a;abc;ccc");
    state.absent("unknown");
    state.absent("_cmake_predicate_out_0_");
}

#[test]
fn sort_modes_and_ties() {
    let mut state = Script::new(Vec::new());
    state.success(r#"
set(string_sort Z a B b A)
list(SORT string_sort CASE INSENSITIVE ORDER DESCENDING)
set(base z/a / y/B x/a dir/)
list(SORT base COMPARE FILE_BASENAME CASE INSENSITIVE)
set(natural a01 a001 a1 a02 a0 a00 a010 a10 a9 A2 a2 a99999999999999999999999 a9999999999999999999999)
list(SORT natural COMPARE NATURAL CASE INSENSITIVE)
set(default z a Z A)
list(SORT default)
set(fraction 001a 0010 1 1x 10 01 0000 00 " 9")
list(SORT fraction COMPARE NATURAL)
"#);
    state.var("string_sort", b"Z;B;b;a;A");
    state.var("base", b"/;dir/;z/a;x/a;y/B");
    state.var("natural", b"a001;a00;a01;a010;a02;a0;a1;A2;a2;a9;a10;a9999999999999999999999;a99999999999999999999999");
    state.var("default", b"A;Z;a;z");
    state.var("fraction", b" 9;0000;0010;001a;00;01;1;1x;10");
}

#[test]
fn sort_callback_respects_case_and_order() {
    let mut state = Script::new(Vec::new());
    state.success(
        r#"
function(version_less left right out)
  if(left VERSION_LESS right)
    set(${out} ON PARENT_SCOPE)
  else()
    set(${out} OFF PARENT_SCOPE)
  endif()
endfunction()
function(text_less left right out)
  if(left STRLESS right)
    set(${out} ON PARENT_SCOPE)
  else()
    set(${out} OFF PARENT_SCOPE)
  endif()
endfunction()
set(versions 3.1 1.2 2.0 1.10)
list(SORT versions COMPARATOR version_less)
set(letters a b a B C)
list(SORT letters COMPARATOR text_less CASE INSENSITIVE ORDER DESCENDING)
"#,
    );
    state.var("versions", b"1.2;1.10;2.0;3.1");
    state.var("letters", b"C;b;B;a;a");
}

#[test]
fn transform_callback_selects_each_element_before_applying() {
    let mut state = Script::new(Vec::new());
    state.success(
        r#"
function(select_element value out)
  if(value STREQUAL "one" OR previous STREQUAL "one")
    set(${out} ON PARENT_SCOPE)
  else()
    set(${out} OFF PARENT_SCOPE)
  endif()
endfunction()
function(transform_element value out)
  set(${out} "!${value}" PARENT_SCOPE)
  set(previous "${value}" PARENT_SCOPE)
endfunction()
set(values one two three)
list(TRANSFORM values APPLY transform_element PREDICATE select_element OUTPUT_VARIABLE result)
"#,
    );
    state.var("values", b"one;two;three");
    state.var("result", b"!one;!two;three");
    state.var("previous", b"two");
}

#[test]
fn list_and_argument_parsing_preserve_non_utf8_bytes() {
    let mut state = Script::new(Vec::new());
    let exit = state.run_bytes(
        b"set(xs [=[\xffa;\x80B]=])
          list(GET xs 1 0 reversed)
          list(TRANSFORM xs TOUPPER)
          separate_arguments(words UNIX_COMMAND [=[\xff \x80]=])
        ",
    );
    assert_eq!(exit, 0, "{}", state.stderr());
    state.var("reversed", b"\x80B;\xffa");
    state.var("xs", b"\xffA;\x80B");
    state.var("words", b"\xff;\x80");
}

#[test]
fn callback_errors_stop_the_list_operation() {
    for (script, expected) in [
        (
            "macro(m item out)\nendmacro()\nset(x a)\nlist(FILTER x INCLUDE PREDICATE m)",
            "macro \"m\" may not be used here; define it as a function() instead.",
        ),
        (
            "function(m item out)\nendfunction()\nset(x a)\nlist(TRANSFORM x APPLY m)",
            "function \"m\" did not set the output variable.",
        ),
        (
            "function(m a b out)\nset(${out} ON PARENT_SCOPE)\nendfunction()\nset(x a a)\nlist(SORT x COMPARATOR m)",
            "The comparator returned TRUE for both (a, b) and (b, a).",
        ),
        (
            "set(x a)\nlist(TRANSFORM x TOUPPER PREDICATE missing)",
            "selector PREDICATE: unknown function \"missing\".",
        ),
        (
            "set(x a)\nlist(SORT x COMPARE STRING COMPARATOR m)",
            "option \"COMPARATOR\" is incompatible with \"COMPARE\".",
        ),
    ] {
        let mut state = Script::new(Vec::new());
        assert_eq!(state.run(script), 1, "{script}");
        let stderr = state.stderr();
        let normalized = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains(expected), "{stderr}");
        assert!(state.interp.cx().var_defined(b"x"));
    }
}

#[test]
fn transform_actions_selectors_and_captures() {
    use rcmake_diag::PolicyStatus::New;

    let mut state = Script::new(Vec::new());
    state.policy(186, New);
    state.success(
        r#"
set(xs "  Ab ;Cd; ab ;Eff")
list(TRANSFORM xs STRIP OUTPUT_VARIABLE trimmed)
list(TRANSFORM trimmed TOLOWER AT 0 -1)
list(TRANSFORM trimmed TOUPPER FOR 1 -1 2)
list(TRANSFORM trimmed APPEND ! REGEX "^[aA]")
list(TRANSFORM trimmed PREPEND / AT 0 0)
list(TRANSFORM trimmed REPLACE "(C)(D)" [=[\2\1\n]=] AT 1 OUTPUT_VARIABLE replaced)
set(genex [=[a$<A:$<B>>b$<unfinished;c]=])
list(TRANSFORM genex GENEX_STRIP)
set(zero aaa bbb)
list(TRANSFORM zero REPLACE "a*" x)
set(retry x y)
list(TRANSFORM retry REPLACE "^|x" z)
set(anchor aaa)
list(TRANSFORM anchor REPLACE "^a" x)
list(TRANSFORM absent STRIP AT 4 OUTPUT_VARIABLE no_input)
"#,
    );
    state.var("xs", b"  Ab ;Cd; ab ;Eff");
    state.var("trimmed", b"//ab!;CD;ab!;EFF");
    state.var("replaced", b"//ab!;DC\n;ab!;EFF");
    state.var("genex", b"ab$<unfinished;c");
    state.var("zero", b"xx;xbxbxbx");
    state.var("retry", b"zz;zy");
    state.var("anchor", b"xaa");
    state.var("no_input", b"");
    state.absent("absent");

    let mut legacy = Script::new(Vec::new());
    legacy.success(
        r#"
set(anchor aaa)
list(TRANSFORM anchor REPLACE "^a" x)
set(group ab)
list(TRANSFORM group REPLACE "(a)(b)" [=[\2\1]=])
"#,
    );
    legacy.var("anchor", b"xxx");
    legacy.var("group", b"ba");
    legacy.var("CMAKE_MATCH_0", b"ab");
    legacy.var("CMAKE_MATCH_1", b"a");
}

#[test]
fn errors_match_oracle_status_text() {
    for (source, expected) in [
        ("set()", "set called with incorrect number of arguments"),
        (
            "unset(x NO)",
            "unset called with an invalid second argument",
        ),
        (
            "option(a b c d)",
            "option called with incorrect number of arguments: a b c d",
        ),
        (
            "mark_as_advanced()",
            "mark_as_advanced called with incorrect number of arguments",
        ),
        (
            "set(a CACHE)",
            "set given invalid arguments for CACHE mode: missing type and docstring",
        ),
        (
            "separate_arguments()",
            "separate_arguments must be given at least one argument.",
        ),
        (
            "separate_arguments(a UNIX_COMMAND x y)",
            "separate_arguments given unexpected argument(s)",
        ),
        (
            "separate_arguments(a UNIX_COMMAND SEPARATE_ARGS a)",
            "separate_arguments `SEPARATE_ARGS` option requires `PROGRAM' option",
        ),
        ("list()", "list must be called with at least two arguments."),
        ("list(BAD a)", "list does not recognize sub-command BAD"),
        (
            "list(LENGTH a)",
            "list sub-command LENGTH requires two arguments.",
        ),
        (
            "list(GET a o)",
            "list sub-command GET requires at least three arguments.",
        ),
        (
            "list(JOIN a o)",
            "list sub-command JOIN requires three arguments (2 found).",
        ),
        (
            "list(SUBLIST a 0 o)",
            "list sub-command SUBLIST requires four arguments (3 found).",
        ),
        (
            "list(FIND a x)",
            "list sub-command FIND requires three arguments.",
        ),
        (
            "list(INSERT a 0)",
            "list sub-command INSERT requires at least three arguments.",
        ),
        (
            "list(REMOVE_AT a)",
            "list sub-command REMOVE_AT requires at least two arguments.",
        ),
        (
            "list(REMOVE_AT a 0 1)",
            "list index: a, 0, 1 out of range (0, 0)",
        ),
        (
            "list(REVERSE a o)",
            "list sub-command REVERSE only takes one argument.",
        ),
        (
            "list(REMOVE_DUPLICATES a o)",
            "list sub-command REMOVE_DUPLICATES only takes one argument.",
        ),
        ("set(a \"\")\nlist(GET a 0 o)", "list GET given empty list"),
        (
            "cmake_policy(SET CMP0121 NEW)\nset(a x y)\nlist(GET a 1.x o)",
            "list index: 1.x is not a valid index",
        ),
        (
            "set(a x y)\nlist(GET a -3 o)",
            "list index: -3 out of range (-2, 1)",
        ),
        (
            "set(a x y)\nlist(INSERT a 3 z)",
            "list index: 3 out of range (-2, 2)",
        ),
        (
            "set(a x y)\nlist(SUBLIST a 2 0 o)",
            "list begin index: 2 is out of range 0 - 1",
        ),
        (
            "set(a x y)\nlist(SUBLIST a 0 -2 o)",
            "list length: -2 should be -1 or greater",
        ),
        (
            "list(FILTER a)",
            "list sub-command FILTER requires an operator to be specified.",
        ),
        (
            "list(FILTER a INCLUDE)",
            "list sub-command FILTER requires a mode to be specified.",
        ),
        (
            "set(a x)\nlist(FILTER a INCLUDE REGEX)",
            "list sub-command FILTER, mode REGEX requires five arguments.",
        ),
        (
            "set(a x)\nlist(FILTER a INCLUDE PREDICATE missing)",
            "list sub-command FILTER, mode PREDICATE: unknown function \"missing\".",
        ),
        (
            "set(a x)\nlist(FILTER a INCLUDE REGEX \"(\")",
            "list sub-command FILTER, mode REGEX failed to compile regex \"(\".",
        ),
        (
            "list(SORT a COMPARE)",
            "list sub-command SORT missing argument for option \"COMPARE\".",
        ),
        (
            "list(SORT a COMPARE size)",
            "list sub-command SORT value \"size\" for option \"COMPARE\" is invalid.",
        ),
        (
            "list(SORT a CASE SENSITIVE CASE INSENSITIVE)",
            "list sub-command SORT option \"CASE\" has been specified multiple times.",
        ),
        (
            "list(TRANSFORM a)",
            "list sub-command TRANSFORM requires an action to be specified.",
        ),
        (
            "list(TRANSFORM a UNKNOWN)",
            "list sub-command TRANSFORM, UNKNOWN invalid action.",
        ),
        (
            "list(TRANSFORM a REPLACE x)",
            "list sub-command TRANSFORM, action REPLACE expects 2 argument(s).",
        ),
        (
            "set(a x)\nlist(TRANSFORM a STRIP AT -2)",
            "list sub-command TRANSFORM, selector AT, index: -1 out of range (-1, 0).",
        ),
        (
            "set(a x)\nlist(TRANSFORM a STRIP FOR 0 0 -1)",
            "list sub-command TRANSFORM, selector FOR expects positive numeric value for <step>.",
        ),
        (
            "set(a x y)\nlist(TRANSFORM a STRIP FOR 1 0)",
            "list sub-command TRANSFORM, selector FOR expects <start> to be no greater than <stop> (1 > 0)",
        ),
        (
            "list(TRANSFORM a STRIP OUTPUT_VARIABLE)",
            "list sub-command TRANSFORM, OUTPUT_VARIABLE expects variable name argument.",
        ),
        (
            "set(a x)\nlist(TRANSFORM a REPLACE x [=[\x5c1]=])",
            "list sub-command TRANSFORM, action REPLACE: replace expression \"\x5c1\" contains an out-of-range escape for regex \"x\".",
        ),
    ] {
        let mut state = Script::new(Vec::new());
        assert_eq!(state.run(source), 1, "{source}");
        let stderr = state.stderr();
        let body = stderr
            .lines()
            .skip(1)
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(body, expected, "{source}");
    }
}
