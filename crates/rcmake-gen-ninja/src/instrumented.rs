use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::BString;
use rcmake_host::path;
use rcmake_resolve::{BuildStep, GenerationResult, Invocation};

use crate::commands::Commands;
use crate::syntax::{Ninja, shell_word};

type Rule = (&'static [u8], &'static [u8], bool);

const COMPILE_RULE: Rule = (b"COMPILE", b"COMPILE_INSTRUMENTED", false);
const COMPILE_DEPFILE_RULE: Rule = (b"COMPILE_DEPFILE", b"COMPILE_DEPFILE_INSTRUMENTED", true);
const SCANNED_RULE: Rule = (
    b"CXX_COMPILE_SCANNED",
    b"CXX_COMPILE_SCANNED_INSTRUMENTED",
    false,
);
const SCANNED_DEPFILE_RULE: Rule = (
    b"CXX_COMPILE_SCANNED_DEPFILE",
    b"CXX_COMPILE_SCANNED_DEPFILE_INSTRUMENTED",
    true,
);

const RULES: [Rule; 4] = [
    COMPILE_RULE,
    COMPILE_DEPFILE_RULE,
    SCANNED_RULE,
    SCANNED_DEPFILE_RULE,
];

fn arguments(invocation: &Invocation) -> Option<(&BString, &[BString])> {
    let [program, flag, args @ ..] = invocation.argv.as_slice() else {
        return None;
    };
    (invocation.working_dir.is_none()
        && flag == b"--instrument"
        && args.first().is_some_and(|arg| arg == b"--command-type"))
    .then_some((program, args))
}

pub(crate) fn ordinary_rule(scanned: bool, depfile: bool) -> &'static [u8] {
    match (scanned, depfile) {
        (false, false) => b"COMPILE",
        (false, true) => b"COMPILE_DEPFILE",
        (true, false) => b"CXX_COMPILE_SCANNED",
        (true, true) => b"CXX_COMPILE_SCANNED_DEPFILE",
    }
}

pub(crate) fn compile(
    invocation: &Invocation,
    scanned: bool,
    depfile: bool,
) -> (&'static [u8], Option<Vec<u8>>) {
    let (ordinary, instrumented, _) = match (scanned, depfile) {
        (false, false) => COMPILE_RULE,
        (false, true) => COMPILE_DEPFILE_RULE,
        (true, false) => SCANNED_RULE,
        (true, true) => SCANNED_DEPFILE_RULE,
    };
    let Some((_, args)) = arguments(invocation) else {
        return (ordinary, None);
    };
    let mut command = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index != 0 {
            command.push(b' ');
        }
        shell_word(arg, &mut command);
    }
    (instrumented, Some(command))
}

const LAUNCHED_COMPILE_RULE: Rule = (b"COMPILE", b"COMPILE_LAUNCHED", false);
const LAUNCHED_DEPFILE_RULE: Rule = (b"COMPILE_DEPFILE", b"COMPILE_DEPFILE_LAUNCHED", true);
const LAUNCHED_SCANNED_RULE: Rule = (
    b"CXX_COMPILE_SCANNED",
    b"CXX_COMPILE_SCANNED_LAUNCHED",
    false,
);
const LAUNCHED_SCANNED_DEPFILE_RULE: Rule = (
    b"CXX_COMPILE_SCANNED_DEPFILE",
    b"CXX_COMPILE_SCANNED_DEPFILE_LAUNCHED",
    true,
);

const LAUNCHED_RULES: [Rule; 4] = [
    LAUNCHED_COMPILE_RULE,
    LAUNCHED_DEPFILE_RULE,
    LAUNCHED_SCANNED_RULE,
    LAUNCHED_SCANNED_DEPFILE_RULE,
];

fn launched_rule_name(base: &'static [u8], slot: usize) -> Cow<'static, [u8]> {
    if slot == 0 {
        Cow::Borrowed(base)
    } else {
        let mut name = Vec::with_capacity(base.len() + 12);
        name.extend_from_slice(base);
        name.push(b'_');
        name.extend_from_slice(slot.to_string().as_bytes());
        Cow::Owned(name)
    }
}

pub(crate) struct LaunchedCompile {
    pub(crate) rule: Cow<'static, [u8]>,
    pub(crate) launch_args: BString,
    pub(crate) launcher: Option<BString>,
    pub(crate) command: Vec<u8>,
}

/// Scan the next POSIX shell word starting at `pos` in `input`.
///
/// Returns `(start, end, unquoted, quoted)` where `start..end` is the raw token
/// range in `input`, `unquoted` is the decoded word, and `quoted` records whether
/// any quoting or backslash escaping was used in the token.
fn next_shell_token(input: &[u8], mut pos: usize) -> Option<(usize, usize, Vec<u8>, bool)> {
    while let Some(&byte) = input.get(pos) {
        if byte.is_ascii_whitespace() {
            pos += 1;
        } else {
            break;
        }
    }
    if pos >= input.len() {
        return None;
    }
    let start = pos;
    let mut out = Vec::new();
    let mut quoted = false;
    let mut in_single = false;
    let mut in_double = false;
    while let Some(&byte) = input.get(pos) {
        if in_single {
            if byte == b'\'' {
                in_single = false;
            } else {
                out.push(byte);
            }
            pos += 1;
        } else if in_double {
            match byte {
                b'"' => {
                    in_double = false;
                    pos += 1;
                }
                b'\\' => {
                    if let Some(&next) = input.get(pos + 1) {
                        if matches!(next, b'"' | b'\\' | b'$' | b'`' | b'\n') {
                            out.push(next);
                        } else {
                            out.push(b'\\');
                            out.push(next);
                        }
                        pos += 2;
                    } else {
                        return None;
                    }
                }
                _ => {
                    out.push(byte);
                    pos += 1;
                }
            }
        } else {
            match byte {
                b if b.is_ascii_whitespace() => break,
                b'\'' => {
                    quoted = true;
                    in_single = true;
                    pos += 1;
                }
                b'"' => {
                    quoted = true;
                    in_double = true;
                    pos += 1;
                }
                b'\\' => {
                    quoted = true;
                    let &next = input.get(pos + 1)?;
                    out.push(next);
                    pos += 2;
                }
                _ => {
                    out.push(byte);
                    pos += 1;
                }
            }
        }
    }
    if in_single || in_double {
        return None;
    }
    Some((start, pos, out, quoted))
}

/// Parse the first shell word ONLY when it is a pure literal whose double-quoted
/// re-emission (`push_double_quoted_ninja`) is guaranteed to evaluate to the exact
/// same bytes under POSIX `sh`.
///
/// Rejects any unquoted shell expansion or metacharacter (`$`, `` ` ``, `~`, glob
/// characters, redirections, grouping, separators, `=`, control chars) and rejects
/// unescaped `$` or `` ` `` inside double quotes.
fn parse_literal_program(input: &[u8]) -> Option<(usize, BString)> {
    let mut pos = 0usize;
    while let Some(&byte) = input.get(pos) {
        if byte.is_ascii_whitespace() {
            pos += 1;
        } else {
            break;
        }
    }
    if pos >= input.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(&byte) = input.get(pos) {
        if matches!(byte, b'\n' | b'\r' | b'\0') {
            return None;
        }
        if in_single {
            if byte == b'\'' {
                in_single = false;
            } else {
                out.push(byte);
            }
            pos += 1;
        } else if in_double {
            match byte {
                b'"' => {
                    in_double = false;
                    pos += 1;
                }
                b'$' | b'`' => return None,
                b'\\' => {
                    let &next = input.get(pos + 1)?;
                    if matches!(next, b'\n' | b'\r' | b'\0') {
                        return None;
                    }
                    if matches!(next, b'"' | b'\\' | b'$' | b'`') {
                        out.push(next);
                    } else {
                        out.push(b'\\');
                        out.push(next);
                    }
                    pos += 2;
                }
                _ => {
                    out.push(byte);
                    pos += 1;
                }
            }
        } else {
            match byte {
                b if b.is_ascii_whitespace() => break,
                b'\'' => {
                    in_single = true;
                    pos += 1;
                }
                b'"' => {
                    in_double = true;
                    pos += 1;
                }
                b'\\' => {
                    let &next = input.get(pos + 1)?;
                    if matches!(next, b'\n' | b'\r' | b'\0') {
                        return None;
                    }
                    out.push(next);
                    pos += 2;
                }
                b'$' | b'`' | b'~' | b'*' | b'?' | b'[' | b']' | b'(' | b')' | b'{' | b'}'
                | b';' | b'|' | b'&' | b'<' | b'>' | b'!' | b'#' | b'=' => return None,
                _ => {
                    out.push(byte);
                    pos += 1;
                }
            }
        }
    }
    if in_single || in_double || out.is_empty() {
        return None;
    }
    Some((pos, BString::from(out)))
}

fn launch_parts(invocation: &Invocation) -> Option<(BString, &[u8], &[u8])> {
    if invocation.working_dir.is_some() {
        return None;
    }
    let shell = invocation.shell.as_slice();
    let (after_prog, program) = parse_literal_program(shell)?;
    let (_, after_flag, flag, _) = next_shell_token(shell, after_prog)?;
    if flag.as_slice() != b"--launch" {
        return None;
    }
    let mut cursor = after_flag;
    while let Some((tok_start, tok_end, tok_value, tok_quoted)) = next_shell_token(shell, cursor) {
        if !tok_quoted && tok_value.as_slice() == b"--" {
            let launch_args = shell.get(after_flag..tok_start)?.trim_ascii();
            let command = shell.get(tok_end..)?.trim_ascii_start();
            if command.is_empty() {
                return None;
            }
            return Some((program, launch_args, command));
        }
        cursor = tok_end;
    }
    None
}

fn compile_step_invocation<'a>(
    graph: &'a rcmake_resolve::BuildGraph,
    step_index: usize,
    step: &'a BuildStep,
) -> Option<&'a Invocation> {
    match step {
        BuildStep::Compile { invocation, .. } => Some(invocation),
        BuildStep::Custom(custom)
            if graph.swift_actions.contains_key(&step_index) && custom.invocations.len() == 1 =>
        {
            custom.invocations.first()
        }
        _ => None,
    }
}

/// Collect every distinct `--launch` program across `result` in deterministic
/// first-seen order, returning both the ordered list and a program-to-slot map.
pub(crate) fn collect_launchers(
    result: &GenerationResult,
) -> (Vec<BString>, BTreeMap<BString, usize>) {
    let mut ordered = Vec::new();
    let mut slots = BTreeMap::new();
    for graph in &result.graphs {
        for (step_index, step) in graph.steps.iter().enumerate() {
            let Some(invocation) = compile_step_invocation(graph, step_index, step) else {
                continue;
            };
            if let Some((program, _, _)) = launch_parts(invocation)
                && !slots.contains_key(&program)
            {
                let slot = ordered.len();
                ordered.push(program.clone());
                slots.insert(program, slot);
            }
        }
    }
    (ordered, slots)
}

pub(crate) fn launched(
    invocation: &Invocation,
    compiler_launcher: Option<&BString>,
    launcher_slots: &BTreeMap<BString, usize>,
    scanned: bool,
    depfile: bool,
) -> Option<LaunchedCompile> {
    let (program, launch_args, rest) = launch_parts(invocation)?;
    let &slot = launcher_slots.get(&program)?;
    let (_, base_rule, _) = match (scanned, depfile) {
        (false, false) => LAUNCHED_COMPILE_RULE,
        (false, true) => LAUNCHED_DEPFILE_RULE,
        (true, false) => LAUNCHED_SCANNED_RULE,
        (true, true) => LAUNCHED_SCANNED_DEPFILE_RULE,
    };
    let rule = launched_rule_name(base_rule, slot);
    let (launcher, command) = match compiler_launcher {
        Some(prefix)
            if !prefix.is_empty()
                && rest
                    .strip_prefix(prefix.as_slice())
                    .is_some_and(|tail| tail.starts_with(b" ")) =>
        {
            let tail = rest
                .strip_prefix(prefix.as_slice())
                .and_then(|tail| tail.strip_prefix(b" "))
                .unwrap_or(rest);
            (Some(prefix.clone()), tail.to_vec())
        }
        _ => (None, rest.to_vec()),
    };
    Some(LaunchedCompile {
        rule,
        launch_args: launch_args.into(),
        launcher,
        command,
    })
}

fn push_double_quoted_ninja(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'"');
    for &byte in bytes {
        if matches!(byte, b'"' | b'\\' | b'$' | b'`') {
            out.push(b'\\');
        }
        if byte == b'$' {
            out.push(b'$');
        }
        out.push(byte);
    }
    out.push(b'"');
}

pub(crate) fn emit_rules(
    ninja: &mut Ninja,
    result: &GenerationResult,
    build_dir: &Path,
    in_build_dir: bool,
    commands: &Commands,
) {
    if let Some(program) = result.graphs.iter().find_map(|graph| {
        graph
            .steps
            .iter()
            .enumerate()
            .find_map(|(step_index, step)| {
                let invocation = compile_step_invocation(graph, step_index, step)?;
                arguments(invocation).map(|(program, _)| program)
            })
    }) {
        for (_, rule, depfile) in RULES {
            ninja.0.extend_from_slice(b"\nrule ");
            ninja.0.extend_from_slice(rule);
            ninja.0.extend_from_slice(b"\n  command = ");
            push_double_quoted_ninja(&mut ninja.0, program.as_slice());
            ninja
                .0
                .extend_from_slice(b" --instrument $COMMAND\n  description = $DESC\n");
            if depfile {
                ninja
                    .0
                    .extend_from_slice(b"  depfile = $DEP_FILE\n  deps = gcc\n");
            }
        }
    }
    let (programs, _) = collect_launchers(result);
    let build_dir_bytes = in_build_dir.then(|| path::from_native(build_dir));
    for (slot, program) in programs.iter().enumerate() {
        let mut command = Vec::new();
        push_double_quoted_ninja(&mut command, program.as_slice());
        command.extend_from_slice(b" --launch $LAUNCH_ARGS -- $LAUNCHER $COMMAND");
        let command = match &build_dir_bytes {
            Some(dir) => commands.in_directory(&command, dir),
            None => command,
        };
        for (_, base_rule, depfile) in LAUNCHED_RULES {
            let rule = launched_rule_name(base_rule, slot);
            ninja.0.extend_from_slice(b"\nrule ");
            ninja.0.extend_from_slice(&rule);
            ninja.0.extend_from_slice(b"\n  command = ");
            ninja.0.extend_from_slice(&command);
            ninja.0.extend_from_slice(b"\n  description = $DESC\n");
            if depfile {
                ninja
                    .0
                    .extend_from_slice(b"  depfile = $DEP_FILE\n  deps = gcc\n");
            }
        }
    }
}
