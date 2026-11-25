//! Shared fixtures for evaluator tests. The default fixture uses a small test
//! frontend for variable expansion and conditions.

mod control_flow;
mod hooks_and_subdirectories;
mod message_and_errors;
mod oracle_frontend;
mod parse_and_recovery;
mod policies_and_includes;
mod scopes_and_callables;

use crate::{
    CmdError, CmdFlags, CmdResult, Cx, Flow, Frontend, IncludeOptions, Interp, InterpConfig,
    Registry, RunMode,
};
use rcmake_base::list::{Empty, split_into};
use rcmake_base::{BString, truth};
use rcmake_diag::{DiagAction, DiagCategory, PolicyId, PolicyStatus};
use rcmake_expr::{CondEnv, CondError, ExpandEnv, ExpandError, ExpandedArg};
use rcmake_host::{PureHost, Stream};
use rcmake_syntax::{ArgKind, Argument};
use std::path::PathBuf;
use std::sync::Arc;

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
    if cx.defer_call(cx.current_dir_id(), id.value.clone(), &command.value, rest) {
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
            cx.defer_cancel_call(
                cx.current_dir_id(),
                v.get(2).map_or(&b""[..], |s| s.as_slice()),
            );
            cx.defer_get_call_ids(cx.current_dir_id())
        }
        Some(b"GET") => cx.defer_get_call(
            cx.current_dir_id(),
            v.get(2).map_or(&b""[..], |s| s.as_slice()),
        ),
        _ => cx.defer_get_call_ids(cx.current_dir_id()),
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
    let mut config = InterpConfig::new(host.clone(), mode, ROOT, "/usr/local/bin/cmake", "/work");
    config.script_args = vec![
        "/usr/local/bin/cmake".into(),
        "-P".into(),
        "main.cmake".into(),
    ];
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
