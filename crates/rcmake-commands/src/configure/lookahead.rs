//! Find consecutive probe candidates without evaluating a listfile.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

#[cfg(test)]
use rcmake_store::Digest;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};
#[cfg(test)]
use serde::{Deserialize, Serialize};

const MAX_LOOKAHEAD: usize = 64;

/// Probe calls for which the evaluator can consult its call-site index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeKind {
    /// Check for a C include file.
    IncludeFile,
    /// Check a C compiler flag.
    CCompilerFlag,
    /// Check a C++ compiler flag.
    CxxCompilerFlag,
    /// Check whether a C symbol is available.
    SymbolExists,
    /// Compile an embedded source file.
    TryCompile,
}

impl ProbeKind {
    /// Stock module that defines this command. The direct probe is a built-in.
    #[cfg(test)]
    pub(crate) fn defining_module(self) -> Option<&'static str> {
        match self {
            Self::IncludeFile => Some("CheckIncludeFile.cmake"),
            Self::CCompilerFlag => Some("CheckCCompilerFlag.cmake"),
            Self::CxxCompilerFlag => Some("CheckCXXCompilerFlag.cmake"),
            Self::SymbolExists => Some("CheckSymbolExists.cmake"),
            Self::TryCompile => None,
        }
    }
}

/// One syntactic candidate in the original command order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeCandidate<'a> {
    /// The position in the batch input, including the current command when supplied.
    pub(crate) index: usize,
    /// The command with its raw argument delimiters.
    pub(crate) command: &'a CommandInvocation,
    /// The expected built-in or stock module command.
    pub(crate) kind: ProbeKind,
    /// The name of the result binding.
    pub(crate) result: &'a [u8],
}

/// Find a consecutive batch with literal arguments and independent inputs.
///
/// An observer can pass its current command before the remaining commands.
/// The predicate must verify which definition will run, the defining module
/// path and digest, the wrapper guard, and dependencies on preceding results.
/// For a built-in, verify that the builtin still owns the command name.
/// Evaluate each wrapper at its original command position and use its actual key.
pub(crate) fn lookahead<'a>(
    current: Option<&'a CommandInvocation>,
    remaining: &'a [CommandInvocation],
    mut verify: impl FnMut(ProbeKind, &CommandInvocation, &BTreeSet<&'a [u8]>) -> bool,
) -> Vec<ProbeCandidate<'a>> {
    let mut results = BTreeSet::new();
    let mut batch = Vec::new();
    for (index, command) in current
        .into_iter()
        .chain(remaining)
        .take(MAX_LOOKAHEAD)
        .enumerate()
    {
        let Some((kind, result)) = candidate(command) else {
            break;
        };
        if results.contains(result) || !verify(kind, command, &results) {
            break;
        }
        results.insert(result);
        batch.push(ProbeCandidate {
            index,
            command,
            kind,
            result,
        });
    }
    if batch.len() < 2 {
        batch.clear();
    }
    batch
}

fn candidate(command: &CommandInvocation) -> Option<(ProbeKind, &[u8])> {
    if !command.args.iter().all(is_literal) {
        return None;
    }
    let (kind, result) = match (command.lower.as_slice(), command.args.as_slice()) {
        (b"check_include_file", [_, result] | [_, result, _]) => (ProbeKind::IncludeFile, result),
        (b"check_c_compiler_flag", [_, result]) => (ProbeKind::CCompilerFlag, result),
        (b"check_cxx_compiler_flag", [_, result]) => (ProbeKind::CxxCompilerFlag, result),
        (b"check_symbol_exists", [_, _, result]) => (ProbeKind::SymbolExists, result),
        (b"try_compile", [result, source, _, _, options @ ..])
            if source.text == "SOURCE_FROM_CONTENT"
                && options
                    .iter()
                    .all(|arg| matches!(arg.text.as_slice(), b"NO_CACHE" | b"NO_LOG")) =>
        {
            (ProbeKind::TryCompile, result)
        }
        _ => return None,
    };
    let result = result.text.as_slice();
    valid_result(result).then_some((kind, result))
}

fn is_literal(arg: &Argument) -> bool {
    arg.kind == ArgKind::Bracket
        || !arg.text.iter().any(|byte| {
            matches!(byte, b'$' | b'\\')
                || (arg.kind == ArgKind::Unquoted && matches!(byte, b';' | b'(' | b')'))
        })
}

fn valid_result(name: &[u8]) -> bool {
    let Some((&first, rest)) = name.split_first() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && !name.starts_with(b"CMAKE_")
        && !name.starts_with(b"ARGV")
        && !matches!(name, b"ARGC" | b"ARGN")
}

/// One recipe from the previous configure, in call order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg(test)]
pub(crate) struct HistoryProbe {
    /// Probe language as bytes.
    pub(crate) language: Vec<u8>,
    /// Project recipe digest before the current toolchain and seed are applied.
    pub(crate) recipe: Digest,
}

/// Input for the next configure's speculative jobs.
///
/// The evaluator resolves the recipe digest and recomputes the key with the
/// current toolchain, seed, and arguments before either use or publication.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg(test)]
pub(crate) struct ProbeHistory {
    /// Previous calls in evaluator order.
    pub(crate) probes: Vec<HistoryProbe>,
}

#[cfg(test)]
impl ProbeHistory {
    /// Return the previous recipes for one language in call order.
    pub(crate) fn for_language<'a>(
        &'a self,
        language: &'a [u8],
    ) -> impl Iterator<Item = Digest> + 'a {
        self.probes.iter().filter_map(move |probe| {
            (probe.language.as_slice() == language).then_some(probe.recipe)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_base::Limits;
    use rcmake_diag::{SourceMap, source::SourceKind};
    use rcmake_syntax::{ListFile, parse_list_file};

    use super::*;

    fn parse(input: &[u8]) -> Result<ListFile, Box<dyn std::error::Error>> {
        let mut sources = SourceMap::default();
        let kind = SourceKind::File;
        let source = sources
            .add("lookahead.cmake".into(), kind, Arc::from(input))
            .ok_or("Cannot register the test source.")?;
        Ok(parse_list_file(input, source, kind, &Limits::default())?)
    }

    #[test]
    fn returns_only_the_first_straight_line_batch() -> Result<(), Box<dyn std::error::Error>> {
        let file = parse(
            br#"check_include_file(stdio.h HAVE_STDIO)
check_c_compiler_flag("-Wall" HAVE_WALL)
check_cxx_compiler_flag("-Wall" HAVE_CXX_WALL)
check_symbol_exists(printf "stdio.h;stdlib.h" HAVE_PRINTF)
try_compile(HAVE_MAIN SOURCE_FROM_CONTENT probe.c [[int main(void) { return 0; }]] NO_CACHE)
set(CMAKE_REQUIRED_FLAGS "-Werror")
check_include_file(errno.h HAVE_ERRNO)"#,
        )?;
        let batch = lookahead(None, &file.commands, |_, _, _| true);
        assert_eq!(
            batch
                .iter()
                .map(|probe| (probe.index, probe.kind))
                .collect::<Vec<_>>(),
            [
                (0, ProbeKind::IncludeFile),
                (1, ProbeKind::CCompilerFlag),
                (2, ProbeKind::CxxCompilerFlag),
                (3, ProbeKind::SymbolExists),
                (4, ProbeKind::TryCompile),
            ]
        );
        assert_eq!(
            batch.last().map(|probe| probe.result),
            Some(b"HAVE_MAIN".as_slice())
        );
        Ok(())
    }

    #[test]
    fn stops_on_dynamic_calls_reused_results_and_persistent_builds()
    -> Result<(), Box<dyn std::error::Error>> {
        for command in [
            "check_include_file(\"${HEADER}\" HAVE_NEW)",
            "check_include_file(stddef.h HAVE_OLD)",
            "check_include_file(stddef.h CMAKE_REQUIRED_FLAGS)",
            "try_compile(HAVE_NEW /build /source)",
            "try_compile(HAVE_NEW SOURCE_FROM_CONTENT probe.c [[int x;]] COPY_FILE out)",
            "user_wrapper(HAVE_NEW)",
        ] {
            let file = parse(format!("check_include_file(stdio.h HAVE_OLD)\n{command}\ncheck_include_file(errno.h HAVE_ERRNO)").as_bytes())?;
            assert!(
                lookahead(None, &file.commands, |_, _, _| true).is_empty(),
                "{command}"
            );
        }
        Ok(())
    }

    #[test]
    fn includes_the_first_observed_call_and_stops_at_replaced_wrappers()
    -> Result<(), Box<dyn std::error::Error>> {
        let file = parse(
            b"check_include_file(stdio.h HAVE_STDIO)\ncheck_include_file(errno.h HAVE_ERRNO)",
        )?;
        let Some((current, remaining)) = file.commands.split_first() else {
            return Err("Expected a current command.".into());
        };
        let batch = lookahead(Some(current), remaining, |kind, _, earlier| {
            assert_eq!(kind.defining_module(), Some("CheckIncludeFile.cmake"));
            assert!(earlier.len() <= 1);
            true
        });
        assert_eq!(batch.len(), 2);
        assert!(lookahead(Some(current), remaining, |_, _, earlier| earlier.is_empty()).is_empty());
        Ok(())
    }

    #[test]
    fn selects_history_for_the_completed_language_in_order() {
        let first = Digest::hash(b"first");
        let other = Digest::hash(b"other");
        let last = Digest::hash(b"last");
        let history = ProbeHistory {
            probes: vec![
                HistoryProbe {
                    language: b"C".to_vec(),
                    recipe: first,
                },
                HistoryProbe {
                    language: b"CXX".to_vec(),
                    recipe: other,
                },
                HistoryProbe {
                    language: b"C".to_vec(),
                    recipe: last,
                },
            ],
        };
        assert_eq!(
            history.for_language(b"C").collect::<Vec<_>>(),
            [first, last]
        );
    }
}
