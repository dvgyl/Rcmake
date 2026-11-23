//! Evaluator tests.
//!
//! `rcmake-expr` is implemented in parallel, so these tests install a small test
//! frontend (variable expansion and a condition subset) through the private
//! [`Frontend`] seam. All other behavior runs through the real evaluator.

use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::list::{Empty, split_into};
use rcmake_base::{BString, Limits, truth};
use rcmake_diag::{DiagAction, DiagCategory, DiagOptions, PolicyId, PolicyStatus};
use rcmake_expr::{CondEnv, CondError, ExpandEnv, ExpandError, ExpandedArg};
use rcmake_host::{PureHost, Stream};
use rcmake_syntax::{ArgKind, Argument};

use rcmake_model::DirId;

use crate::{
    CmdError, CmdFlags, CmdResult, Cx, Flow, Frontend, IncludeOptions, Interp, InterpConfig,
    Registry, RunMode,
};

// ---------------------------------------------------------------------------------------
// Test frontend.
// ---------------------------------------------------------------------------------------

fn err(msg: &str) -> ExpandError {
    ExpandError {
        message: msg.into(),
    }
}

fn expand_until(
    src: &[u8],
    pos: &mut usize,
    env: &mut dyn ExpandEnv,
    in_var: bool,
) -> Result<Vec<u8>, ExpandError> {
    let mut out = Vec::new();
    while *pos < src.len() {
        let b = src[*pos];
        if in_var && b == b'}' {
            *pos += 1;
            return Ok(out);
        }
        if b == b'\\' {
            match src.get(*pos + 1).copied() {
                Some(b'n') => out.push(b'\n'),
                Some(b't') => out.push(b'\t'),
                Some(b'r') => out.push(b'\r'),
                Some(b';') => out.extend_from_slice(b"\\;"),
                Some(
                    c @ (b'\\' | b'"' | b'$' | b'(' | b')' | b'#' | b' ' | b'@' | b'[' | b']'
                    | b'{' | b'}' | b'<' | b'>' | b'^'),
                ) => out.push(c),
                Some(c) => {
                    return Err(err(&format!(
                        "Invalid character escape '\\{}'.",
                        char::from(c)
                    )));
                }
                None => out.push(b'\\'),
            }
            *pos += 2;
            continue;
        }
        if b == b'$' {
            let rest = &src[*pos..];
            let (skip, kind) = if rest.starts_with(b"${") {
                (2, 0)
            } else if rest.starts_with(b"$ENV{") {
                (5, 1)
            } else if rest.starts_with(b"$CACHE{") {
                (7, 2)
            } else {
                (0, 3)
            };
            if kind < 3 {
                *pos += skip;
                let name = expand_until(src, pos, env, true)?;
                let value = match kind {
                    0 if name == b"CMAKE_CURRENT_LIST_LINE" => env
                        .var(&name)
                        .or_else(|| Some(BString::from(env.current_line().to_string()))),
                    0 => env.var(&name),
                    1 => env.env_var(&name),
                    _ => env.cache_var(&name),
                };
                out.extend_from_slice(&value.unwrap_or_default());
                continue;
            }
        }
        out.push(b);
        *pos += 1;
    }
    if in_var {
        return Err(err("unterminated variable reference"));
    }
    Ok(out)
}

fn t_expand_string(src: &[u8], env: &mut dyn ExpandEnv) -> Result<BString, ExpandError> {
    let mut pos = 0;
    expand_until(src, &mut pos, env, false).map(BString::from)
}

fn t_expand(
    args: &[Argument],
    env: &mut dyn ExpandEnv,
    out: &mut Vec<ExpandedArg>,
) -> Result<(), ExpandError> {
    for a in args {
        let mk = |value: BString| ExpandedArg {
            value,
            kind: a.kind,
            line: a.line,
        };
        match a.kind {
            ArgKind::Bracket => out.push(mk(a.text.clone())),
            ArgKind::Quoted => out.push(mk(t_expand_string(&a.text, env)?)),
            ArgKind::Unquoted => {
                let v = t_expand_string(&a.text, env)?;
                let mut parts = Vec::new();
                split_into(&v, Empty::Drop, &mut parts);
                out.extend(parts.into_iter().map(mk));
            }
        }
    }
    Ok(())
}

fn deref(a: &ExpandedArg, env: &mut dyn CondEnv) -> BString {
    if a.kind == ArgKind::Unquoted && env.is_defined(&a.value) {
        env.var(&a.value).unwrap_or_default()
    } else {
        a.value.clone()
    }
}

fn t_atom(args: &[ExpandedArg], env: &mut dyn CondEnv) -> Result<bool, CondError> {
    let word = |i: usize| args.get(i).map(|a| a.value.as_slice());
    match args.len() {
        0 => Ok(false),
        _ if word(0) == Some(b"NOT") => Ok(!t_atom(&args[1..], env)?),
        2 if word(0) == Some(b"DEFINED") => Ok(env.is_defined(&args[1].value)),
        2 if word(0) == Some(b"COMMAND") => Ok(env.command_exists(&args[1].value)),
        2 if word(0) == Some(b"POLICY") => Ok(env.policy_known(&args[1].value)),
        3 => {
            let l = deref(&args[0], env);
            let r = deref(&args[2], env);
            let num = |v: &BString| std::str::from_utf8(v).ok()?.parse::<i64>().ok();
            match args[1].value.as_slice() {
                b"STREQUAL" => Ok(l == r),
                b"EQUAL" => Ok(num(&l).is_some() && num(&l) == num(&r)),
                b"LESS" => Ok(matches!((num(&l), num(&r)), (Some(a), Some(b)) if a < b)),
                b"GREATER" => Ok(matches!((num(&l), num(&r)), (Some(a), Some(b)) if a > b)),
                _ => Err(CondError {
                    message: "Unknown arguments specified".into(),
                }),
            }
        }
        1 => {
            let a = &args[0];
            if let Some(c) = truth::if_constant(&a.value) {
                return Ok(c);
            }
            if a.kind != ArgKind::Unquoted || !env.is_defined(&a.value) {
                return Ok(false);
            }
            let v = env.var(&a.value).unwrap_or_default();
            Ok(!truth::is_off(&v))
        }
        _ => Err(CondError {
            message: "Unknown arguments specified".into(),
        }),
    }
}

fn t_cond(args: &[ExpandedArg], env: &mut dyn CondEnv) -> Result<bool, CondError> {
    let is_kw = |a: &ExpandedArg, kw: &[u8]| a.kind == ArgKind::Unquoted && a.value == kw;
    if let Some(p) = args.iter().position(|a| is_kw(a, b"OR")) {
        let l = t_cond(&args[..p], env)?;
        let r = t_cond(&args[p + 1..], env)?;
        return Ok(l || r);
    }
    if let Some(p) = args.iter().position(|a| is_kw(a, b"AND")) {
        let l = t_cond(&args[..p], env)?;
        let r = t_cond(&args[p + 1..], env)?;
        return Ok(l && r);
    }
    t_atom(args, env)
}

const TEST_FRONTEND: Frontend = Frontend {
    expand: t_expand,
    expand_string: t_expand_string,
    cond: t_cond,
};

// ---------------------------------------------------------------------------------------
// Test-only commands (the real ones arrive in Wave 2).
// ---------------------------------------------------------------------------------------

fn vals(args: &[ExpandedArg]) -> Vec<BString> {
    args.iter().map(|a| a.value.clone()).collect()
}

fn t_set(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let mut v = vals(args);
    let Some(name) = v.first().cloned() else {
        return Err(CmdError::Status(
            "called with incorrect number of arguments".into(),
        ));
    };
    v.remove(0);
    let parent = v.last().is_some_and(|l| l.as_slice() == b"PARENT_SCOPE");
    if parent {
        v.pop();
    }
    let value = (!v.is_empty()).then(|| rcmake_base::list::join(v.iter()));
    match (parent, value) {
        (true, value) => cx.set_parent_scope(&name, value),
        (false, Some(value)) => cx.set_var(&name, value),
        (false, None) => cx.unset_var(&name),
    }
    Ok(Flow::Normal)
}

fn t_unset(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let v = vals(args);
    match v.as_slice() {
        [name] => cx.unset_var(name),
        [name, p] if p.as_slice() == b"PARENT_SCOPE" => cx.set_parent_scope(name, None),
        _ => return Err(CmdError::Status("bad args".into())),
    }
    Ok(Flow::Normal)
}

fn t_policy(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let v = vals(args);
    match v
        .iter()
        .map(|s| s.as_slice())
        .collect::<Vec<_>>()
        .as_slice()
    {
        [b"PUSH"] => cx.push_policy_scope(),
        [b"POP"] => cx.pop_policy_scope()?,
        [b"SET", id, status] => {
            let id = PolicyId::parse(id).ok_or(CmdError::Status("bad policy".into()))?;
            let status = if *status == b"NEW" {
                PolicyStatus::New
            } else {
                PolicyStatus::Old
            };
            cx.set_policy(id, status)
                .map_err(|e| CmdError::Status(e.to_string().into()))?;
        }
        [b"GET", id, var] => {
            let id = PolicyId::parse(id).ok_or(CmdError::Status("bad policy".into()))?;
            let text = match cx.policy(id) {
                PolicyStatus::New => "NEW",
                PolicyStatus::Old => "OLD",
                PolicyStatus::Warn => "",
            };
            cx.set_var(var, text.into());
        }
        _ => return Err(CmdError::Status("bad args".into())),
    }
    Ok(Flow::Normal)
}

fn t_diagnostic(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let v = vals(args);
    match v
        .iter()
        .map(|s| s.as_slice())
        .collect::<Vec<_>>()
        .as_slice()
    {
        [b"PUSH"] => cx.push_diag_scope(),
        [b"POP"] => cx.pop_diag_scope()?,
        [b"SET", cat, action] => {
            let cat = DiagCategory::parse(cat).ok_or(CmdError::Status("bad cat".into()))?;
            let action = DiagAction::parse(action).ok_or(CmdError::Status("bad act".into()))?;
            cx.update_diag_state(|s| s.set(cat, action, true));
        }
        _ => return Err(CmdError::Status("bad args".into())),
    }
    Ok(Flow::Normal)
}

fn t_inc(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let name = args.first().map(|a| a.value.clone()).unwrap_or_default();
    let n: i64 = std::str::from_utf8(&cx.var_or_empty(&name))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    cx.set_var(&name, (n + 1).to_string().into());
    Ok(Flow::Normal)
}

fn t_noop(_: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    Ok(Flow::Normal)
}

fn t_language(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    match args.first().map(|a| a.value.as_slice()) {
        Some(b"EVAL") => {
            let code: Vec<u8> = args.iter().skip(2).flat_map(|a| a.value.to_vec()).collect();
            cx.eval_code(&code)
        }
        Some(b"CALL") => {
            let name = args.get(1).map(|a| a.value.clone()).unwrap_or_default();
            cx.call_command(&name, args.get(2..).unwrap_or_default().to_vec())
        }
        _ => Err(CmdError::Status("bad args".into())),
    }
}

fn t_include_file(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let path = args.first().map(|a| a.value.clone()).unwrap_or_default();
    let optional = args
        .get(1)
        .is_some_and(|a| a.value.as_slice() == b"OPTIONAL");
    cx.include_file(
        &rcmake_host::path::to_native(&path),
        IncludeOptions {
            optional,
            ..IncludeOptions::default()
        },
    )
}

fn t_variable_watch(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let var = args.first().map(|a| a.value.clone()).unwrap_or_default();
    cx.add_variable_watch(&var, args.get(1).map(|a| a.value.as_slice()));
    Ok(Flow::Normal)
}

/// `test_defer(<id> <command> <args>...)`: queue `<command>` with raw arguments.
fn t_defer(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    let raw = cx.raw_args().to_vec();
    let head = cx.expand_raw_args(raw.get(..2).unwrap_or_default())?;
    let (Some(id), Some(command)) = (head.first(), head.get(1)) else {
        return Err(CmdError::Status("bad args".into()));
    };
    let rest = raw.get(2..).unwrap_or_default();
    if cx.defer_call(id.value.clone(), &command.value, rest) {
        Ok(Flow::Normal)
    } else {
        Err(CmdError::Status("no defer queue".into()))
    }
}

/// `test_defer_query(<var> [CANCEL <id>] [GET <id>])`: store the queued call identifiers
/// (or the call `<id>`) in `<var>`.
fn t_defer_query(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let v = vals(args);
    let var = v.first().cloned().unwrap_or_default();
    let value = match v.get(1).map(|s| s.as_slice()) {
        Some(b"CANCEL") => {
            cx.defer_cancel_call(v.get(2).map_or(&b""[..], |s| s.as_slice()));
            cx.defer_get_call_ids()
        }
        Some(b"GET") => cx.defer_get_call(v.get(2).map_or(&b""[..], |s| s.as_slice())),
        _ => cx.defer_get_call_ids(),
    };
    match value {
        Some(items) => cx.set_var(&var, rcmake_base::list::join(items.iter())),
        None => cx.set_var(&var, "inactive".into()),
    }
    Ok(Flow::Normal)
}

/// `test_subdir(<src> <bin> [EXCLUDE_FROM_ALL] [SYSTEM])`: run a subdirectory.
fn t_subdir(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let v = vals(args);
    let Some(src) = v.first() else {
        return Err(CmdError::Status("bad args".into()));
    };
    let bin = v
        .get(1)
        .filter(|b| !matches!(b.as_slice(), b"EXCLUDE_FROM_ALL" | b"SYSTEM"))
        .unwrap_or(src);
    let flag = |name: &[u8]| v.iter().skip(2).any(|a| a.as_slice() == name);
    let options = crate::SubdirOptions {
        exclude_from_all: flag(b"EXCLUDE_FROM_ALL"),
        system: flag(b"SYSTEM"),
    };
    let src = rcmake_host::path::to_native(src);
    let bin = rcmake_host::path::to_native(bin);
    cx.run_subdirectory(&src, &bin, options)?;
    Ok(Flow::Normal)
}

fn register_test(reg: &mut Registry) {
    let s = CmdFlags { scriptable: true };
    reg.add("test_subdir", t_subdir, CmdFlags { scriptable: false });
    reg.add("variable_watch", t_variable_watch, s);
    reg.add_raw("test_defer", t_defer, s);
    reg.add("test_defer_query", t_defer_query, s);
    reg.add("set", t_set, s);
    reg.add("unset", t_unset, s);
    reg.add("cmake_policy", t_policy, s);
    reg.add("cmake_diagnostic", t_diagnostic, s);
    reg.add("test_inc", t_inc, s);
    reg.add("test_noop", t_noop, s);
    reg.add("cmake_language", t_language, s);
    reg.add("test_include_file", t_include_file, s);
    reg.add("add_executable", t_noop, CmdFlags { scriptable: false });
}

// ---------------------------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------------------------

const ROOT: &str = "/opt/cmake/share/cmake-4.5";

struct Out {
    exit: i32,
    stdout: String,
    stderr: String,
}

fn new_interp(files: &[(&str, &str)], mode: RunMode) -> (Interp, Arc<PureHost>) {
    let host = Arc::new(PureHost::new(
        PathBuf::from("/work"),
        PathBuf::from("/usr/local/bin/cmake"),
    ));
    for (path, text) in files {
        host.seed_file(&PathBuf::from(path), text.as_bytes())
            .expect("seed file");
    }
    let config = InterpConfig {
        host: host.clone(),
        mode,
        cmake_root: PathBuf::from(ROOT),
        cmake_command: PathBuf::from("/usr/local/bin/cmake"),
        cwd: PathBuf::from("/work"),
        limits: Limits::default(),
        diag: DiagOptions::default(),
        defines: Vec::new(),
        script_args: vec![
            "/usr/local/bin/cmake".into(),
            "-P".into(),
            "main.cmake".into(),
        ],
    };
    let mut interp = Interp::new(config, register_test);
    interp.frontend = TEST_FRONTEND;
    (interp, host)
}

fn run_files(main: &str, files: &[(&str, &str)]) -> Out {
    let mut all = vec![("/work/main.cmake", main)];
    all.extend_from_slice(files);
    let (mut interp, host) = new_interp(&all, RunMode::Script);
    let outcome = interp.run_script(&PathBuf::from("main.cmake"));
    Out {
        exit: outcome.exit_code,
        stdout: String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)).into_owned(),
        stderr: String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned(),
    }
}

fn run(main: &str) -> Out {
    run_files(main, &[])
}

/// Configure `/work/src` (its `CMakeLists.txt` is `lists`) into `/work/build`.
fn run_configure(lists: &str, files: &[(&str, &str)]) -> Out {
    run_configure_interp(lists, files).0
}

/// [`run_configure`] that also returns the interpreter for model checks.
fn run_configure_interp(lists: &str, files: &[(&str, &str)]) -> (Out, Interp) {
    let mut all = vec![("/work/src/CMakeLists.txt", lists)];
    all.extend_from_slice(files);
    let (mut interp, host) = new_interp(&all, RunMode::Configure);
    let outcome = interp.run_configure(
        &PathBuf::from("/work/src"),
        &PathBuf::from("/work/build"),
        &[],
    );
    let out = Out {
        exit: outcome.exit_code,
        stdout: String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)).into_owned(),
        stderr: String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned(),
    };
    (out, interp)
}

/// Run on a thread with a large stack (deep recursion tests).
fn run_big_stack(main: &'static str) -> Out {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run(main))
        .expect("spawn")
        .join()
        .expect("join")
}

// ---------------------------------------------------------------------------------------
// message()
// ---------------------------------------------------------------------------------------

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
    // Under CMP0218 WARN, CMAKE_WARN_DEPRECATED decides, not CMD_AUTHOR (oracle).
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

// ---------------------------------------------------------------------------------------
// Commands, errors, recursion.
// ---------------------------------------------------------------------------------------

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
fn not_a_file_and_parse_errors() {
    let (mut interp, host) = new_interp(&[], RunMode::Script);
    let outcome = interp.run_script(&PathBuf::from("/work/nope.cmake"));
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "CMake Error: Not a file: /work/nope.cmake\nCMake Error: Error processing file: /work/nope.cmake\n"
    );

    let o = run("message(a)\nendif()\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at main.cmake:2 (endif):\n",
            "  Flow control statements are not properly nested.\n\n\n",
            "CMake Error: Error processing file: /work/main.cmake\n",
        )
    );
}

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
    let config = InterpConfig {
        host: host.clone(),
        mode: RunMode::Script,
        cmake_root: PathBuf::from(ROOT),
        cmake_command: PathBuf::from("/usr/local/bin/cmake"),
        cwd: PathBuf::from("/work"),
        limits: Limits::default(),
        diag: DiagOptions::default(),
        defines: vec![crate::CacheDefine {
            name: "X".into(),
            ty: None,
            value: "1".into(),
        }],
        script_args: Vec::new(),
    };
    let mut interp = Interp::new(config, register_test);
    interp.frontend = TEST_FRONTEND;
    assert_eq!(interp.run_script(&PathBuf::from("main.cmake")).exit_code, 0);
    assert_eq!(host.stream_bytes(Stream::Stderr), "X=1 c=1\ndef\n");
}

// ---------------------------------------------------------------------------------------
// Variable scopes.
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Macros.
// ---------------------------------------------------------------------------------------

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
    // OLD behavior: the substituted text is decoded again.
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

// ---------------------------------------------------------------------------------------
// return(), break(), continue().
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// if / foreach / while.
// ---------------------------------------------------------------------------------------

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
    // Unknown commands are fatal in every mode.
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

// ---------------------------------------------------------------------------------------
// block().
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Policy and diagnostic stacks.
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// include() and include_guard().
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Cx API used by Wave 2 command families.
// ---------------------------------------------------------------------------------------

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
    let config = InterpConfig {
        host: host.clone(),
        mode: RunMode::Script,
        cmake_root: PathBuf::from(ROOT),
        cmake_command: PathBuf::from("/usr/local/bin/cmake"),
        cwd: PathBuf::from("/work"),
        limits: Limits::default(),
        diag: DiagOptions::default(),
        defines: Vec::new(),
        script_args: Vec::new(),
    };
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

// ---------------------------------------------------------------------------------------
// Opener failures, parse errors, and expansion errors.
// ---------------------------------------------------------------------------------------

#[test]
fn failed_while_runs_body_and_reports_endwhile() {
    let o = run_configure("while()\nmessage(inside)\nendwhile()\n", &[]);
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at CMakeLists.txt:1 (while):\n",
            "  while called with incorrect number of arguments\n\n\n",
            "inside\n",
            "CMake Error at CMakeLists.txt:3 (endwhile):\n",
            "  endwhile An ENDWHILE command was found outside of a proper WHILE ENDWHILE\n",
            "  structure.  Or its arguments did not match the opening WHILE command.\n\n\n",
        )
    );
}

#[test]
fn failed_function_definition_runs_body_inline() {
    let o = run_configure("function()\nmessage(body)\nendfunction()\n", &[]);
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("function called with incorrect number of arguments")
    );
    assert!(o.stderr.contains("\nbody\n"), "{}", o.stderr);
    assert!(
        o.stderr
            .contains("An ENDFUNCTION command was found outside")
    );
}

#[test]
fn nesting_error_stops_processing_without_include_error() {
    let o = run_configure(
        "include(bad.cmake)\nmessage(after)\n",
        &[("/work/src/bad.cmake", "while(a)\n")],
    );
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at bad.cmake:1 (while):\n",
            "  Flow control statements are not properly nested.\n",
            "Call Stack (most recent call first):\n",
            "  CMakeLists.txt:1 (include)\n\n\n",
        )
    );
}

#[test]
fn missing_close_paren_at_end_of_file_is_not_fatal() {
    let o = run_configure(
        "include(bad.cmake)\nmessage(after)\n",
        &[("/work/src/bad.cmake", "message(a\n")],
    );
    assert_eq!(o.exit, 1);
    assert!(
        o.stderr
            .contains("Function missing ending \")\".  End of file reached.")
    );
    assert!(o.stderr.contains("include could not load requested file:"));
    assert!(o.stderr.ends_with("after\n"), "{}", o.stderr);
}

#[test]
fn non_utf8_bom_is_a_file_level_error() {
    let (mut interp, host) = new_interp(
        &[(
            "/work/src/CMakeLists.txt",
            "include(bom.cmake)\nmessage(after)\n",
        )],
        RunMode::Configure,
    );
    host.seed_file(&PathBuf::from("/work/src/bom.cmake"), b"\xff\xfem\x00")
        .expect("seed file");
    let code = interp
        .run_configure(
            &PathBuf::from("/work/src"),
            &PathBuf::from("/work/build"),
            &[],
        )
        .exit_code;
    let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
    assert_eq!(code, 1);
    assert_eq!(
        stderr,
        concat!(
            "CMake Error in bom.cmake:\n",
            "  File starts with a Byte-Order-Mark that is not UTF-8.\n",
            "Call Stack (most recent call first):\n",
            "  CMakeLists.txt:1 (include)\n\n\n",
            "CMake Error at CMakeLists.txt:1 (include):\n",
            "  include could not load requested file:\n\n",
            "    bom.cmake\n\n\n",
            "after\n",
        )
    );
}

#[test]
fn expansion_error_is_fatal_and_names_the_argument() {
    let o = run("message(ok \"a\\q\" b)\nmessage(after)\n");
    assert_eq!(o.exit, 1);
    assert_eq!(
        o.stderr,
        concat!(
            "CMake Error at main.cmake:1 (message):\n",
            "  Syntax error in cmake code at\n\n",
            "    /work/main.cmake:1\n\n",
            "  when parsing string\n\n",
            "    a\\q\n\n",
            "  Invalid character escape '\\q'.\n\n\n",
        )
    );
}

// ---------------------------------------------------------------------------------------
// CMP0198: CMAKE_PARENT_LIST_FILE in CMakeLists.txt.
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Variable watches.
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Deferred calls.
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Subdirectories.
// ---------------------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------------------
// Tracing.
// ---------------------------------------------------------------------------------------

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
        "set(a \"x\\\"y\")\nmessage(\"${a}\")\n",
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
            "{\"args\":[\"x\\\"y\"],\"cmd\":\"message\",\"file\":\"/work/main.cmake\",\"frame\":1,\"global_frame\":1,\"line\":2,\"time\":"
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
