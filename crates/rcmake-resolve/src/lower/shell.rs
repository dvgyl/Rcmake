//! Process arguments and POSIX shell command lines.

use rcmake_base::{BString, list, shell::quote};
use rcmake_model::{DirId, ProjectModel};

use super::{definition, expand_rule};
use crate::Invocation;

pub(super) fn join_words(words: impl IntoIterator<Item = BString>) -> BString {
    let mut result = BString::default();
    for word in words.into_iter().filter(|word| !word.is_empty()) {
        if !result.is_empty() {
            result.push(b' ');
        }
        result.extend_from_slice(&word);
    }
    result
}

pub(super) fn direct(argv: Vec<BString>, working_dir: Option<BString>) -> Invocation {
    let shell = join_words(argv.iter().map(|arg| quote(arg)));
    Invocation {
        argv,
        shell,
        working_dir,
    }
}

pub(super) fn command(shell: BString) -> Invocation {
    let argv = simple_args(&shell)
        .filter(|args| {
            args.first()
                .is_some_and(|program| !is_shell_prefix(program))
        })
        .unwrap_or_else(|| vec!["sh".into(), "-c".into(), shell.clone()]);
    Invocation {
        argv,
        shell,
        working_dir: None,
    }
}

fn is_shell_prefix(program: &[u8]) -> bool {
    matches!(
        program,
        b"!" | b"." | b":" | b"cd" | b"exec" | b"exit" | b"export" | b"set" | b"umask" | b"unset"
    ) || program.iter().position(|&b| b == b'=').is_some_and(|end| {
        program.get(..end).is_some_and(|name| {
            name.first()
                .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
                && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        })
    })
}

/// Read a simple shell command. Require the shell when an expansion or operator is present.
pub(super) fn simple_args(shell: &[u8]) -> Option<Vec<BString>> {
    let mut args = Vec::new();
    let mut word = BString::default();
    let mut active = false;
    let mut delimiter = None;
    let mut bytes = shell.iter().copied();
    while let Some(byte) = bytes.next() {
        match (delimiter, byte) {
            (Some(b'\''), b'\'') | (Some(b'"'), b'"') => delimiter = None,
            (Some(b'\''), _) => word.push(byte),
            (None, b'\'' | b'"') => {
                delimiter = Some(byte);
                active = true;
            }
            (_, b'\\') => {
                let next = bytes.next()?;
                if next != b'\n' {
                    if delimiter.is_some() && !matches!(next, b'$' | 0x60 | 0x5c | b'"') {
                        word.push(byte);
                    }
                    word.push(next);
                    active = true;
                }
            }
            (_, b'$' | b'`')
            | (None, b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' | b'*' | b'?' | b'[' | b'\n') =>
            {
                return None;
            }
            (None, b'#' | b'~') if !active => return None,
            (None, b' ' | b'\t') => {
                if active {
                    args.push(std::mem::take(&mut word));
                    active = false;
                }
            }
            _ => {
                word.push(byte);
                active = true;
            }
        }
    }
    if delimiter.is_some() {
        return None;
    }
    if active {
        args.push(word);
    }
    Some(args)
}

pub(super) fn rule_invocations(
    model: &ProjectModel,
    dir: DirId,
    template: &[u8],
    lookup: impl Fn(&[u8]) -> Option<BString>,
) -> Vec<Invocation> {
    list::split(template, list::Empty::Drop)
        .into_iter()
        .map(|rule| {
            expand_rule(&rule, |key| {
                lookup(key).or_else(|| {
                    let value = definition(model, dir, key).map(|v| v.as_slice());
                    match key {
                        b"CMAKE_AR" => Some(quote(value.unwrap_or(b"ar"))),
                        b"CMAKE_RANLIB" => Some(quote(value.unwrap_or(b"ranlib"))),
                        b"CMAKE_COMMAND" => Some(quote(value.unwrap_or(b"cmake"))),
                        key if key.starts_with(b"CMAKE_") && key.ends_with(b"_COMPILER") => {
                            Some(quote(value.unwrap_or({
                                if matches!(key, b"CMAKE_CXX_COMPILER" | b"CMAKE_OBJCXX_COMPILER") {
                                    b"c++"
                                } else {
                                    b"cc"
                                }
                            })))
                        }
                        _ => value.map(BString::from),
                    }
                })
            })
        })
        .filter(|shell| shell.iter().any(|b| !b.is_ascii_whitespace()))
        .map(command)
        .collect()
}
