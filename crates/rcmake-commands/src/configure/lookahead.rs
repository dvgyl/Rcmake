//! Find consecutive and `foreach()` probe candidates without evaluating a listfile.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, list};
use rcmake_eval::{Cx, ForeachLookahead};
use rcmake_expr::{ArgumentBudget, ExpandEnv, ExpandedArg, expand_args};
#[cfg(test)]
use rcmake_json::{FieldError, FromJson, ToJson, Value, deny_unknown, object, req};
#[cfg(test)]
use rcmake_store::Digest;
use rcmake_syntax::CommandInvocation;
#[cfg(test)]
use rcmake_syntax::{ArgKind, Argument};

const MAX_LOOKAHEAD: usize = 64;
const MAX_SCANNED_COMMANDS: usize = 256;
const MAX_SOURCE_BYTES: usize = 1 << 20;
const MAX_EXPANDED_ARGS: usize = 256;
const MAX_EXPANDED_BYTES: usize = 2 * 1024 * 1024;
const MAX_FOREACH_BINDING_SLOTS: usize = 512;
const MAX_FOREACH_BINDING_BYTES: usize = 256 * 1024;

/// Caller-scope variables written or unset by stock `CheckIncludeFile.cmake`
/// (including `Internal/CheckCommon.cmake`).
const CHECK_INCLUDE_FILE_WRITES: &[&[u8]] = &[
    b"CHECK_INCLUDE_FILE_VAR",
    b"_CIF_SOURCE_CONTENT",
    b"_CIF_EXTRA_CMAKE_ARGUMENTS",
    b"_CIF_INCLUDE_DIRECTORIES",
    b"_CIF_ADD_LINK_OPTIONS",
    b"_CIF_ADD_LINK_LIBRARIES",
    b"_CIF_LINK_DIRECTORIES",
    b"_CIF_CMP0075",
    b"_CIF_CMP0075_WARNED",
    b"CMAKE_REQUIRED_FLAGS",
];

/// Caller-scope variables written or unset by stock `CheckSymbolExists.cmake`
/// (including `Internal/CheckCommon.cmake`).
const CHECK_SYMBOL_EXISTS_WRITES: &[&[u8]] = &[
    b"_CSE_SOURCE",
    b"_CSE_CHECK_NON_MACRO",
    b"_CSE_EXTRA_CMAKE_ARGUMENTS",
    b"_CSE_INCLUDE_DIRECTORIES",
    b"_CSE_ADD_LINK_OPTIONS",
    b"_CSE_ADD_LINK_LIBRARIES",
    b"_CSE_LINK_DIRECTORIES",
    b"_tc_config",
    b"CMAKE_REQUIRED_FLAGS",
];

/// Caller-scope variables written or unset by stock `CheckFunctionExists.cmake`
/// (including `Internal/CheckCommon.cmake`).
const CHECK_FUNCTION_EXISTS_WRITES: &[&[u8]] = &[
    b"_cfe_source",
    b"_CFE_EXTRA_CMAKE_ARGUMENTS",
    b"_CFE_INCLUDE_DIRECTORIES",
    b"_CFE_ADD_LINK_OPTIONS",
    b"_CFE_ADD_LINK_LIBRARIES",
    b"_CFE_LINK_DIRECTORIES",
    b"_CFE_CMP0075",
    b"_CFE_CMP0075_WARNED",
    b"MACRO_CHECK_FUNCTION_DEFINITIONS",
    b"CMAKE_REQUIRED_FLAGS",
];

/// Probe calls for which the evaluator can consult its call-site index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeKind {
    /// Check for a C include file.
    IncludeFile,
    /// Check a C compiler flag.
    CCompilerFlag,
    /// Check a C++ compiler flag.
    CxxCompilerFlag,
    /// Check whether a C source snippet compiles.
    CSourceCompiles,
    /// Check whether a C++ source snippet compiles.
    CxxSourceCompiles,
    /// Check whether a C symbol is available.
    SymbolExists,
    /// Check whether a C function is available.
    FunctionExists,
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
            Self::CSourceCompiles => Some("CheckCSourceCompiles.cmake"),
            Self::CxxSourceCompiles => Some("CheckCXXSourceCompiles.cmake"),
            Self::SymbolExists => Some("CheckSymbolExists.cmake"),
            Self::FunctionExists => Some("CheckFunctionExists.cmake"),
            Self::TryCompile => None,
        }
    }
}

/// Read-only variable and command-dispatch view used by side-effect-free speculative lookahead.
pub(crate) trait VarLookup {
    /// Look up a normal variable (falling back to cache) without marking it used.
    fn var(&self, name: &[u8]) -> Option<&[u8]>;
    /// Look up a cache variable directly without marking it used.
    fn cache_var(&self, name: &[u8]) -> Option<&[u8]>;
    /// Return whether lowercase command `name` resolves to its built-in implementation
    /// (not overridden by a user `function()` or `macro()`).
    fn is_builtin(&self, name: &[u8]) -> bool;
}

impl VarLookup for Cx<'_> {
    fn var(&self, name: &[u8]) -> Option<&[u8]> {
        self.peek_var(name).map(|value| value.as_slice())
    }

    fn cache_var(&self, name: &[u8]) -> Option<&[u8]> {
        self.peek_cache_var(name).map(|value| value.as_slice())
    }

    fn is_builtin(&self, name: &[u8]) -> bool {
        Cx::is_builtin(self, name)
    }
}

/// Resolved probe payload ready for speculative scheduling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExpandedProbeSpec {
    /// Direct `try_compile` with `SOURCE_FROM_CONTENT` or `SOURCE_FROM_VAR`.
    TryCompile {
        /// Relative source file name.
        filename: BString,
        /// Exact source bytes.
        content: BString,
    },
    /// Two-argument stock `check_include_file(<header> <result>)`.
    IncludeFile {
        /// Header file name to include.
        header: BString,
    },
    /// Stock `check_c_compiler_flag(<flag> <result>)`.
    CCompilerFlag {
        /// Compiler flag string.
        flag: BString,
    },
    /// Stock `check_cxx_compiler_flag(<flag> <result>)`.
    CxxCompilerFlag {
        /// Compiler flag string.
        flag: BString,
    },
    /// Stock `check_c_source_compiles(<source> <result> [FAIL_REGEX ...])`.
    CSourceCompiles {
        /// C source code string.
        source: BString,
    },
    /// Stock `check_cxx_source_compiles(<source> <result> [FAIL_REGEX ...])`.
    CxxSourceCompiles {
        /// C++ source code string.
        source: BString,
    },
    /// Stock `check_symbol_exists(<symbol> <headers> <result>)`.
    SymbolExists {
        /// Symbol name.
        symbol: BString,
        /// Semicolon-separated header list.
        headers: BString,
    },
    /// Stock `check_function_exists(<function> <result>)`.
    FunctionExists {
        /// Function name.
        function: BString,
    },
}

/// One expanded, dependency-checked probe candidate in evaluator order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExpandedProbeCandidate {
    /// The expected built-in or stock module command.
    pub(crate) kind: ProbeKind,
    /// The name of the result binding.
    pub(crate) result: BString,
    /// Resolved inputs for this probe.
    pub(crate) spec: ExpandedProbeSpec,
}

#[derive(Clone, Debug)]
struct LocalBinding {
    /// `Some(value)` for a defined local variable; `None` for an unset `ZIP_LISTS` loop variable.
    value: Option<BString>,
    /// `true` iff the value depends strictly on present loop variables and literals,
    /// never on any variable read from the live `base` environment.
    loop_pure: bool,
}

struct PureExpansion {
    args: Vec<ExpandedArg>,
    arg_loop_pure: Vec<bool>,
    reads: BTreeSet<BString>,
    external_reads: BTreeSet<BString>,
    cache_reads: BTreeSet<BString>,
}

struct ProbeEffects {
    reads: Vec<BString>,
    external_reads: Vec<BString>,
    cache_reads: Vec<BString>,
    writes: Vec<BString>,
}

/// Find a straight-line batch of independent probes, expanding `${var}` arguments and
/// allowing pure built-in local `set`/`string` derivations between probes.
///
/// Stops immediately on any command outside the pure built-in derivation / probe subset,
/// any read of an earlier probe's result or macro-written caller-scope variable, any
/// duplicate result variable, any write to `CMAKE_*` / `ARGV*` / `CACHE` / `PARENT_SCOPE`,
/// any bare `set(X)` unset, or any `$ENV{...}` / line-dependent expansion.
pub(crate) fn lookahead_expanded(
    base: &impl VarLookup,
    current: Option<&CommandInvocation>,
    remaining: &[CommandInvocation],
    mut verify: impl FnMut(ProbeKind, &CommandInvocation, &BTreeSet<BString>) -> bool,
) -> Vec<ExpandedProbeCandidate> {
    let mut locals: BTreeMap<BString, LocalBinding> = BTreeMap::new();
    let mut probe_written: BTreeSet<BString> = BTreeSet::new();
    let mut probe_results: BTreeSet<BString> = BTreeSet::new();
    let mut saw_include_file = false;
    let mut saw_symbol_exists = false;
    let mut saw_function_exists = false;
    let mut batch: Vec<ExpandedProbeCandidate> = Vec::new();
    let empty_protected: BTreeSet<BString> = BTreeSet::new();
    let commands: Vec<&CommandInvocation> = current
        .into_iter()
        .chain(remaining)
        .take(MAX_SCANNED_COMMANDS)
        .collect();
    let mut open_not_defined_ifs = 0usize;
    let mut index = 0usize;

    while let Some(&command) = commands.get(index) {
        if batch.len() >= MAX_LOOKAHEAD {
            break;
        }
        // Support simple `if(NOT DEFINED <var>) ... endif()` guards (no `else`/`elseif`)
        // around subsequent probes, matching the dominant pattern in protobuf/qtbase/llvm.
        if !(current.is_some() && index == 0) {
            if command.lower.as_slice() == b"endif" && open_not_defined_ifs > 0 {
                open_not_defined_ifs -= 1;
                index += 1;
                continue;
            }
            if command.lower.as_slice() == b"if" {
                let Some(expanded) = expand_pure(command, base, &locals) else {
                    break;
                };
                if expanded.reads.iter().any(|var| {
                    probe_written.contains(var)
                        || macro_prefix_conflict(
                            var,
                            saw_include_file,
                            saw_symbol_exists,
                            saw_function_exists,
                        )
                }) || expanded.cache_reads.iter().any(|var| {
                    probe_written.contains(var)
                        || macro_prefix_conflict(
                            var,
                            saw_include_file,
                            saw_symbol_exists,
                            saw_function_exists,
                        )
                }) {
                    break;
                }
                let Some(end_idx) = simple_if_endif(&commands, index) else {
                    break;
                };
                let Some(cond_var) = parse_not_defined_guard(&expanded.args) else {
                    break;
                };
                if probe_written.contains(cond_var)
                    || macro_prefix_conflict(
                        cond_var,
                        saw_include_file,
                        saw_symbol_exists,
                        saw_function_exists,
                    )
                {
                    break;
                }
                let is_defined = match locals.get(cond_var) {
                    Some(LocalBinding { value: Some(_), .. }) => true,
                    Some(LocalBinding { value: None, .. }) => base.cache_var(cond_var).is_some(),
                    None => base.var(cond_var).is_some(),
                };
                if is_defined {
                    index = end_idx + 1;
                } else {
                    open_not_defined_ifs += 1;
                    index += 1;
                }
                continue;
            }
        }
        let Some(expanded) = expand_pure(command, base, &locals) else {
            break;
        };
        if expanded.reads.iter().any(|var| {
            probe_written.contains(var)
                || macro_prefix_conflict(
                    var,
                    saw_include_file,
                    saw_symbol_exists,
                    saw_function_exists,
                )
        }) || expanded.cache_reads.iter().any(|var| {
            probe_written.contains(var)
                || macro_prefix_conflict(
                    var,
                    saw_include_file,
                    saw_symbol_exists,
                    saw_function_exists,
                )
        }) {
            break;
        }
        let mut effects = ProbeEffects {
            reads: Vec::new(),
            external_reads: Vec::new(),
            cache_reads: Vec::new(),
            writes: Vec::new(),
        };
        if let Some((kind, result, spec)) = classify_expanded_probe(
            command,
            &expanded.args,
            &expanded.arg_loop_pure,
            false,
            base,
            &locals,
            &empty_protected,
            &mut effects,
        ) {
            if effects.reads.iter().any(|var| {
                probe_written.contains(var)
                    || macro_prefix_conflict(
                        var,
                        saw_include_file,
                        saw_symbol_exists,
                        saw_function_exists,
                    )
            }) || effects.cache_reads.iter().any(|var| {
                probe_written.contains(var)
                    || macro_prefix_conflict(
                        var,
                        saw_include_file,
                        saw_symbol_exists,
                        saw_function_exists,
                    )
            }) || probe_results.contains(&result)
                || probe_written.contains(&result)
                || macro_prefix_conflict(
                    &result,
                    saw_include_file,
                    saw_symbol_exists,
                    saw_function_exists,
                )
                || effects
                    .writes
                    .iter()
                    .any(|var| probe_results.contains(var) || var == &result)
                || !verify(kind, command, &probe_results)
            {
                break;
            }
            if kind == ProbeKind::IncludeFile {
                saw_include_file = true;
            } else if kind == ProbeKind::SymbolExists {
                saw_symbol_exists = true;
            } else if kind == ProbeKind::FunctionExists {
                saw_function_exists = true;
            }
            locals.remove(&result);
            probe_results.insert(result.clone());
            probe_written.insert(result.clone());
            for written in effects.writes {
                locals.remove(&written);
                probe_written.insert(written);
            }
            batch.push(ExpandedProbeCandidate { kind, result, spec });
            index += 1;
            continue;
        }
        // Index 0 (`current`, when provided) must itself be an admitted probe.
        if current.is_some() && index == 0 {
            break;
        }
        let Some((target, binding)) = eval_pure_derivation(
            command,
            &expanded.args,
            &expanded.arg_loop_pure,
            false,
            base,
            &empty_protected,
        ) else {
            break;
        };
        if probe_written.contains(&target)
            || macro_prefix_conflict(
                &target,
                saw_include_file,
                saw_symbol_exists,
                saw_function_exists,
            )
        {
            break;
        }
        locals.insert(target, binding);
        index += 1;
    }
    if batch.len() < 2 {
        batch.clear();
    }
    batch
}

/// Find independent probes from upcoming iterations of an active `foreach()` loop.
///
/// Rejects the entire loop (returning an empty vector) if:
/// - `frame.scope_depth() != current_scope_depth` (avoiding inner-function variable shadowing),
/// - any command in the loop body is outside the strict built-in pure derivation / probe
///   whitelist (no control flow such as `if`, `foreach`, `while`, `break`, `continue`,
///   `return`, no overridden `set`/`string`, no bare `set(X)` unset, and no impure commands),
/// - any command writes to a loop variable, `CMAKE_*`, `ARGV*`, `CACHE`, or `PARENT_SCOPE`,
/// - any write target depends on the live outer `base` state rather than solely on loop
///   variables and literals (ensuring mid-iteration live replay produces the exact same
///   write set as a loop-entry snapshot),
/// - any command reads a variable from `base` (or cache) that is written anywhere in the
///   loop body (including stock macro caller-scope writes like `CHECK_INCLUDE_FILE_VAR`
///   and `_CIF_*`),
/// - any command reads a variable mutated by an earlier iteration (loop-carried state) or by
///   an earlier probe in the same or an earlier iteration,
/// - any two probes across iterations share a result variable name, or
/// - any expansion accesses `$ENV{...}` or `CMAKE_CURRENT_LIST_LINE`.
///
/// Returned candidates belong strictly to upcoming iterations (after the currently executing
/// iteration), in exact evaluator order, capped at `MAX_LOOKAHEAD`.
pub(crate) fn lookahead_foreach(
    base: &impl VarLookup,
    current_scope_depth: usize,
    frame: &ForeachLookahead<'_>,
    mut verify: impl FnMut(ProbeKind, &CommandInvocation, &BTreeSet<BString>) -> bool,
) -> Vec<ExpandedProbeCandidate> {
    if frame.scope_depth() != current_scope_depth {
        return Vec::new();
    }
    let body = frame.body();
    let vars = frame.vars();
    if body.is_empty()
        || vars.is_empty()
        || body.len() > MAX_SCANNED_COMMANDS
        || vars.len() > MAX_FOREACH_BINDING_SLOTS
        || frame.remaining_len() == 0
    {
        return Vec::new();
    }
    let mut loop_vars = BTreeSet::new();
    let mut var_name_bytes = 0usize;
    for var in vars {
        var_name_bytes = var_name_bytes.saturating_add(var.len());
        if var_name_bytes > MAX_FOREACH_BINDING_BYTES || !valid_local_var(var) {
            return Vec::new();
        }
        loop_vars.insert(var.clone());
    }

    let mut accumulated_writes: BTreeSet<BString> = BTreeSet::new();
    let mut all_reads: BTreeSet<BString> = BTreeSet::new();
    let mut all_external_reads: BTreeSet<BString> = BTreeSet::new();
    let mut all_cache_reads: BTreeSet<BString> = BTreeSet::new();
    let mut probe_results: BTreeSet<BString> = BTreeSet::new();
    let mut saw_include_file = false;
    let mut saw_symbol_exists = false;
    let mut saw_function_exists = false;
    let mut upcoming: Vec<ExpandedProbeCandidate> = Vec::new();

    let ok = frame.visit_iterations(
        MAX_LOOKAHEAD,
        MAX_FOREACH_BINDING_SLOTS,
        MAX_FOREACH_BINDING_BYTES,
        |iter_index, values| {
            use std::ops::ControlFlow;

            if iter_index > 0 && upcoming.len() >= MAX_LOOKAHEAD {
                return ControlFlow::Break(true);
            }
            if values.len() != vars.len() {
                return ControlFlow::Break(false);
            }
            // Each iteration starts with ONLY its own loop-variable bindings. Any variable
            // written in an earlier iteration is absent from `iter_locals`, so reading it
            // before assigning it in this iteration is detected as loop-carried mutation.
            let mut iter_locals: BTreeMap<BString, LocalBinding> = BTreeMap::new();
            for (var, &value) in vars.iter().zip(values) {
                let loop_pure = value.is_some();
                iter_locals.insert(
                    var.clone(),
                    LocalBinding {
                        value: value.map(BString::from),
                        loop_pure,
                    },
                );
            }
            let mut iter_probe_writes: BTreeSet<BString> = BTreeSet::new();
            let mut iter_saw_probe = false;

            for command in body {
                let Some(expanded) = expand_pure(command, base, &iter_locals) else {
                    return ControlFlow::Break(false);
                };
                // Reject reads of any variable written by a probe in this iteration, any
                // variable written in an earlier iteration that was not reassigned earlier in
                // this iteration, or any stock-macro private prefix variable.
                for read in &expanded.reads {
                    if iter_probe_writes.contains(read)
                        || (!iter_locals.contains_key(read) && accumulated_writes.contains(read))
                        || macro_prefix_conflict(
                            read,
                            saw_include_file,
                            saw_symbol_exists,
                            saw_function_exists,
                        )
                    {
                        return ControlFlow::Break(false);
                    }
                }
                for read in &expanded.cache_reads {
                    if iter_probe_writes.contains(read)
                        || accumulated_writes.contains(read)
                        || probe_results.contains(read)
                        || macro_prefix_conflict(
                            read,
                            saw_include_file,
                            saw_symbol_exists,
                            saw_function_exists,
                        )
                    {
                        return ControlFlow::Break(false);
                    }
                }
                all_reads.extend(expanded.reads.iter().cloned());
                all_external_reads.extend(expanded.external_reads.iter().cloned());
                all_cache_reads.extend(expanded.cache_reads.iter().cloned());

                let mut effects = ProbeEffects {
                    reads: Vec::new(),
                    external_reads: Vec::new(),
                    cache_reads: Vec::new(),
                    writes: Vec::new(),
                };
                if let Some((kind, result, spec)) = classify_expanded_probe(
                    command,
                    &expanded.args,
                    &expanded.arg_loop_pure,
                    true,
                    base,
                    &iter_locals,
                    &loop_vars,
                    &mut effects,
                ) {
                    for read in &effects.reads {
                        if iter_probe_writes.contains(read)
                            || (!iter_locals.contains_key(read)
                                && accumulated_writes.contains(read))
                            || macro_prefix_conflict(
                                read,
                                saw_include_file,
                                saw_symbol_exists,
                                saw_function_exists,
                            )
                        {
                            return ControlFlow::Break(false);
                        }
                    }
                    for read in &effects.cache_reads {
                        if iter_probe_writes.contains(read)
                            || accumulated_writes.contains(read)
                            || probe_results.contains(read)
                            || macro_prefix_conflict(
                                read,
                                saw_include_file,
                                saw_symbol_exists,
                                saw_function_exists,
                            )
                        {
                            return ControlFlow::Break(false);
                        }
                    }
                    all_reads.extend(effects.reads.iter().cloned());
                    all_external_reads.extend(effects.external_reads.iter().cloned());
                    all_cache_reads.extend(effects.cache_reads.iter().cloned());

                    if kind == ProbeKind::IncludeFile && !saw_include_file {
                        saw_include_file = true;
                        if has_macro_prefix_history(
                            &all_reads,
                            &all_cache_reads,
                            &loop_vars,
                            &accumulated_writes,
                            b"_CIF_",
                        ) {
                            return ControlFlow::Break(false);
                        }
                    } else if kind == ProbeKind::SymbolExists && !saw_symbol_exists {
                        saw_symbol_exists = true;
                        if has_macro_prefix_history(
                            &all_reads,
                            &all_cache_reads,
                            &loop_vars,
                            &accumulated_writes,
                            b"_CSE_",
                        ) {
                            return ControlFlow::Break(false);
                        }
                    } else if kind == ProbeKind::FunctionExists && !saw_function_exists {
                        saw_function_exists = true;
                        if has_macro_prefix_history(
                            &all_reads,
                            &all_cache_reads,
                            &loop_vars,
                            &accumulated_writes,
                            b"_CFE_",
                        ) || has_macro_prefix_history(
                            &all_reads,
                            &all_cache_reads,
                            &loop_vars,
                            &accumulated_writes,
                            b"_cfe_source",
                        ) {
                            return ControlFlow::Break(false);
                        }
                    }

                    if probe_results.contains(&result)
                        || iter_probe_writes.contains(&result)
                        || all_external_reads.contains(&result)
                        || all_cache_reads.contains(&result)
                        || macro_prefix_conflict(
                            &result,
                            saw_include_file,
                            saw_symbol_exists,
                            saw_function_exists,
                        )
                        || effects.writes.iter().any(|var| {
                            probe_results.contains(var)
                                || var == &result
                                || loop_vars.contains(var)
                                || all_external_reads.contains(var)
                                || all_cache_reads.contains(var)
                        })
                        || !verify(kind, command, &probe_results)
                    {
                        return ControlFlow::Break(false);
                    }
                    iter_saw_probe = true;
                    iter_locals.remove(&result);
                    probe_results.insert(result.clone());
                    iter_probe_writes.insert(result.clone());
                    accumulated_writes.insert(result.clone());
                    for written in effects.writes {
                        iter_locals.remove(&written);
                        iter_probe_writes.insert(written.clone());
                        accumulated_writes.insert(written);
                    }
                    if iter_index > 0 && upcoming.len() < MAX_LOOKAHEAD {
                        upcoming.push(ExpandedProbeCandidate { kind, result, spec });
                    }
                    continue;
                }

                let Some((target, binding)) = eval_pure_derivation(
                    command,
                    &expanded.args,
                    &expanded.arg_loop_pure,
                    true,
                    base,
                    &loop_vars,
                ) else {
                    return ControlFlow::Break(false);
                };
                if probe_results.contains(&target)
                    || iter_probe_writes.contains(&target)
                    || all_external_reads.contains(&target)
                    || all_cache_reads.contains(&target)
                    || macro_prefix_conflict(
                        &target,
                        saw_include_file,
                        saw_symbol_exists,
                        saw_function_exists,
                    )
                {
                    return ControlFlow::Break(false);
                }
                iter_locals.insert(target.clone(), binding);
                accumulated_writes.insert(target);
            }

            if !iter_saw_probe {
                return ControlFlow::Break(false);
            }
            ControlFlow::Continue(())
        },
    );

    if ok { upcoming } else { Vec::new() }
}

fn macro_prefix_conflict(
    name: &[u8],
    include_file: bool,
    symbol_exists: bool,
    function_exists: bool,
) -> bool {
    (include_file && (name.starts_with(b"_CIF_") || name == b"CHECK_INCLUDE_FILE_VAR"))
        || (symbol_exists && name.starts_with(b"_CSE_"))
        || (function_exists
            && (name.starts_with(b"_CFE_")
                || name == b"_cfe_source"
                || name == b"MACRO_CHECK_FUNCTION_DEFINITIONS"))
}

fn parse_not_defined_guard(args: &[ExpandedArg]) -> Option<&[u8]> {
    let [not_kw, def_kw, var] = args else {
        return None;
    };
    if not_kw.value.as_slice() != b"NOT" || def_kw.value.as_slice() != b"DEFINED" {
        return None;
    }
    let name = var.value.as_slice();
    if name.is_empty()
        || name.starts_with(b"ENV{")
        || name.starts_with(b"CACHE{")
        || name.starts_with(b"POLICY")
        || !valid_result(name)
    {
        return None;
    }
    Some(name)
}

/// Find the matching `endif()` for `if()` at `start` when the block has no `else`/`elseif`.
fn simple_if_endif(commands: &[&CommandInvocation], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, cmd) in commands.get(start + 1..)?.iter().enumerate() {
        match cmd.lower.as_slice() {
            b"if" => depth += 1,
            b"else" | b"elseif" if depth == 0 => return None,
            b"endif" => {
                if depth == 0 {
                    return Some(start + 1 + offset);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn has_macro_prefix_history(
    all_reads: &BTreeSet<BString>,
    all_cache_reads: &BTreeSet<BString>,
    loop_vars: &BTreeSet<BString>,
    accumulated_writes: &BTreeSet<BString>,
    prefix: &[u8],
) -> bool {
    all_reads
        .iter()
        .chain(all_cache_reads)
        .chain(loop_vars)
        .chain(accumulated_writes)
        .any(|var| var.starts_with(prefix))
}

struct SnapshotExpandEnv<'a, V: VarLookup + ?Sized> {
    base: &'a V,
    locals: &'a BTreeMap<BString, LocalBinding>,
    read_vars: BTreeSet<BString>,
    external_reads: BTreeSet<BString>,
    read_cache_vars: BTreeSet<BString>,
    used_non_loop_pure: bool,
    tainted: bool,
    line: u32,
    used_bytes: usize,
}

impl<V: VarLookup + ?Sized> ExpandEnv for SnapshotExpandEnv<'_, V> {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        if matches!(
            name,
            b"CMAKE_CURRENT_LIST_LINE" | b"CMAKE_CURRENT_LIST_FILE" | b"CMAKE_CURRENT_LIST_DIR"
        ) {
            self.tainted = true;
            return None;
        }
        let key = BString::from(name);
        self.read_vars.insert(key.clone());
        match self.locals.get(name) {
            Some(LocalBinding {
                value: Some(value),
                loop_pure,
            }) => {
                if !*loop_pure {
                    self.used_non_loop_pure = true;
                }
                Some(value.clone())
            }
            Some(LocalBinding { value: None, .. }) => {
                // Unset `ZIP_LISTS` loop variable falls back to cache in `base`.
                self.used_non_loop_pure = true;
                self.read_cache_vars.insert(key);
                self.base.cache_var(name).map(BString::from)
            }
            None => {
                self.used_non_loop_pure = true;
                self.external_reads.insert(key);
                self.base.var(name).map(BString::from)
            }
        }
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.used_non_loop_pure = true;
        self.read_cache_vars.insert(BString::from(name));
        self.base.cache_var(name).map(BString::from)
    }

    fn env_var(&mut self, _name: &[u8]) -> Option<BString> {
        self.tainted = true;
        None
    }

    fn current_line(&self) -> u32 {
        self.line
    }

    fn current_list_file(&mut self) -> Option<BString> {
        self.tainted = true;
        None
    }

    fn argument_budget(&self) -> ArgumentBudget {
        let mut budget = ArgumentBudget::new(MAX_EXPANDED_ARGS, MAX_EXPANDED_BYTES);
        budget.used_bytes = self.used_bytes;
        budget
    }
}

fn expand_pure(
    command: &CommandInvocation,
    base: &impl VarLookup,
    locals: &BTreeMap<BString, LocalBinding>,
) -> Option<PureExpansion> {
    let mut env = SnapshotExpandEnv {
        base,
        locals,
        read_vars: BTreeSet::new(),
        external_reads: BTreeSet::new(),
        read_cache_vars: BTreeSet::new(),
        used_non_loop_pure: false,
        tainted: false,
        line: command.line,
        used_bytes: 0,
    };
    let mut args = Vec::new();
    let mut arg_loop_pure = Vec::new();
    for raw_arg in &command.args {
        let before = args.len();
        env.used_non_loop_pure = false;
        if expand_args(core::slice::from_ref(raw_arg), &mut env, &mut args).is_err() || env.tainted
        {
            return None;
        }
        let pure = !env.used_non_loop_pure;
        for added in args.get(before..).unwrap_or_default() {
            env.used_bytes = env.used_bytes.saturating_add(added.value.len());
            arg_loop_pure.push(pure);
        }
    }
    Some(PureExpansion {
        args,
        arg_loop_pure,
        reads: env.read_vars,
        external_reads: env.external_reads,
        cache_reads: env.read_cache_vars,
    })
}

fn lookup_snapshot(
    name: &[u8],
    base: &impl VarLookup,
    locals: &BTreeMap<BString, LocalBinding>,
    effects: &mut ProbeEffects,
) -> Option<BString> {
    let key = BString::from(name);
    effects.reads.push(key.clone());
    match locals.get(name) {
        Some(LocalBinding {
            value: Some(value), ..
        }) => Some(value.clone()),
        Some(LocalBinding { value: None, .. }) => {
            effects.cache_reads.push(key);
            base.cache_var(name).map(BString::from)
        }
        None => {
            effects.external_reads.push(key);
            base.var(name).map(BString::from)
        }
    }
}

fn arg_is_loop_pure(arg_loop_pure: &[bool], index: usize, required: bool) -> bool {
    !required || arg_loop_pure.get(index).copied().unwrap_or(false)
}

#[allow(clippy::too_many_arguments)]
fn classify_expanded_probe(
    command: &CommandInvocation,
    args: &[ExpandedArg],
    arg_loop_pure: &[bool],
    require_loop_pure_target: bool,
    base: &impl VarLookup,
    locals: &BTreeMap<BString, LocalBinding>,
    protected: &BTreeSet<BString>,
    effects: &mut ProbeEffects,
) -> Option<(ProbeKind, BString, ExpandedProbeSpec)> {
    match command.lower.as_slice() {
        b"check_include_file" => {
            let [header, result] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target) {
                return None;
            }
            let result = result.value.as_slice();
            if header.value.is_empty()
                || !valid_result(result)
                || protected.contains(result)
                || result == b"CHECK_INCLUDE_FILE_VAR"
                || result.starts_with(b"_CIF_")
            {
                return None;
            }
            for &written in CHECK_INCLUDE_FILE_WRITES {
                effects.writes.push(BString::from(written));
            }
            Some((
                ProbeKind::IncludeFile,
                BString::from(result),
                ExpandedProbeSpec::IncludeFile {
                    header: header.value.clone(),
                },
            ))
        }
        b"check_c_compiler_flag" => {
            let [flag, result] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target) {
                return None;
            }
            let result = result.value.as_slice();
            if !valid_result(result) || protected.contains(result) {
                return None;
            }
            Some((
                ProbeKind::CCompilerFlag,
                BString::from(result),
                ExpandedProbeSpec::CCompilerFlag {
                    flag: flag.value.clone(),
                },
            ))
        }
        b"check_cxx_compiler_flag" => {
            let [flag, result] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target) {
                return None;
            }
            let result = result.value.as_slice();
            if !valid_result(result) || protected.contains(result) {
                return None;
            }
            Some((
                ProbeKind::CxxCompilerFlag,
                BString::from(result),
                ExpandedProbeSpec::CxxCompilerFlag {
                    flag: flag.value.clone(),
                },
            ))
        }
        b"check_c_source_compiles" | b"check_cxx_source_compiles" => {
            let [source, result, rest @ ..] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target) {
                return None;
            }
            if source.value.len() > MAX_SOURCE_BYTES {
                return None;
            }
            let result = result.value.as_slice();
            if !valid_result(result) || protected.contains(result) {
                return None;
            }
            if !rest.is_empty()
                && (rest.first()?.value.as_slice() != b"FAIL_REGEX"
                    || rest.len() < 2
                    || rest
                        .iter()
                        .any(|arg| matches!(arg.value.as_slice(), b"SRC_EXT" | b"OUTPUT_VARIABLE")))
            {
                return None;
            }
            let (kind, spec) = if command.lower.as_slice() == b"check_cxx_source_compiles" {
                (
                    ProbeKind::CxxSourceCompiles,
                    ExpandedProbeSpec::CxxSourceCompiles {
                        source: source.value.clone(),
                    },
                )
            } else {
                (
                    ProbeKind::CSourceCompiles,
                    ExpandedProbeSpec::CSourceCompiles {
                        source: source.value.clone(),
                    },
                )
            };
            Some((kind, BString::from(result), spec))
        }
        b"check_function_exists" => {
            let [function, result] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target) {
                return None;
            }
            let result = result.value.as_slice();
            if function.value.is_empty()
                || !valid_result(result)
                || protected.contains(result)
                || result.starts_with(b"_CFE_")
                || result == b"_cfe_source"
                || result == b"MACRO_CHECK_FUNCTION_DEFINITIONS"
            {
                return None;
            }
            for &written in CHECK_FUNCTION_EXISTS_WRITES {
                effects.writes.push(BString::from(written));
            }
            Some((
                ProbeKind::FunctionExists,
                BString::from(result),
                ExpandedProbeSpec::FunctionExists {
                    function: function.value.clone(),
                },
            ))
        }
        b"check_symbol_exists" => {
            let [symbol, headers, result] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 2, require_loop_pure_target) {
                return None;
            }
            let result = result.value.as_slice();
            if symbol.value.is_empty()
                || !valid_result(result)
                || protected.contains(result)
                || result.starts_with(b"_CSE_")
            {
                return None;
            }
            for &written in CHECK_SYMBOL_EXISTS_WRITES {
                effects.writes.push(BString::from(written));
            }
            Some((
                ProbeKind::SymbolExists,
                BString::from(result),
                ExpandedProbeSpec::SymbolExists {
                    symbol: symbol.value.clone(),
                    headers: headers.value.clone(),
                },
            ))
        }
        b"try_compile" => {
            let [result, mode, filename, payload, rest @ ..] = args else {
                return None;
            };
            if !arg_is_loop_pure(arg_loop_pure, 0, require_loop_pure_target)
                || !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target)
            {
                return None;
            }
            let result = result.value.as_slice();
            if !valid_result(result)
                || protected.contains(result)
                || !valid_source_filename(&filename.value)
            {
                return None;
            }
            let content = match mode.value.as_slice() {
                b"SOURCE_FROM_CONTENT" => payload.value.clone(),
                b"SOURCE_FROM_VAR" => {
                    let var = payload.value.as_slice();
                    if var.is_empty()
                        || matches!(
                            var,
                            b"CMAKE_CURRENT_LIST_LINE"
                                | b"CMAKE_CURRENT_LIST_FILE"
                                | b"CMAKE_CURRENT_LIST_DIR"
                        )
                    {
                        return None;
                    }
                    lookup_snapshot(var, base, locals, effects).unwrap_or_default()
                }
                _ => return None,
            };
            if content.len() > MAX_SOURCE_BYTES {
                return None;
            }
            let mut idx = 0usize;
            while let Some(opt) = rest.get(idx) {
                if !arg_is_loop_pure(arg_loop_pure, 4 + idx, require_loop_pure_target) {
                    return None;
                }
                match opt.value.as_slice() {
                    b"NO_CACHE" | b"NO_LOG" => {
                        idx += 1;
                    }
                    b"OUTPUT_VARIABLE" | b"COMPILE_OUTPUT_VARIABLE" => {
                        let out_arg = rest.get(idx + 1)?;
                        if !arg_is_loop_pure(arg_loop_pure, 4 + idx + 1, require_loop_pure_target) {
                            return None;
                        }
                        let out_var = out_arg.value.as_slice();
                        if !valid_local_var(out_var) || protected.contains(out_var) {
                            return None;
                        }
                        effects.writes.push(BString::from(out_var));
                        idx += 2;
                    }
                    _ => return None,
                }
            }
            Some((
                ProbeKind::TryCompile,
                BString::from(result),
                ExpandedProbeSpec::TryCompile {
                    filename: filename.value.clone(),
                    content,
                },
            ))
        }
        _ => None,
    }
}

fn eval_pure_derivation(
    command: &CommandInvocation,
    args: &[ExpandedArg],
    arg_loop_pure: &[bool],
    require_loop_pure_target: bool,
    base: &impl VarLookup,
    protected: &BTreeSet<BString>,
) -> Option<(BString, LocalBinding)> {
    let lower = command.lower.as_slice();
    if !base.is_builtin(lower) {
        return None;
    }
    match lower {
        b"set" => {
            let (target, values) = args.split_first()?;
            // Do not admit bare `set(X)` (1-arg unset); treat it conservatively like `unset(X)`.
            if values.is_empty() || !arg_is_loop_pure(arg_loop_pure, 0, require_loop_pure_target) {
                return None;
            }
            let target_bytes = target.value.as_slice();
            if !valid_local_var(target_bytes) || protected.contains(target_bytes) {
                return None;
            }
            if values
                .iter()
                .any(|arg| matches!(arg.value.as_slice(), b"CACHE" | b"PARENT_SCOPE" | b"FORCE"))
            {
                return None;
            }
            let joined = list::join(values.iter().map(|arg| arg.value.as_slice()));
            if joined.len() > MAX_SOURCE_BYTES {
                return None;
            }
            let loop_pure = arg_loop_pure
                .get(1..)
                .is_some_and(|flags| flags.iter().all(|&flag| flag));
            Some((
                BString::from(target_bytes),
                LocalBinding {
                    value: Some(joined),
                    loop_pure,
                },
            ))
        }
        b"string" => {
            let (subcommand, rest) = args.split_first()?;
            if !arg_is_loop_pure(arg_loop_pure, 0, require_loop_pure_target) {
                return None;
            }
            match subcommand.value.as_slice() {
                b"TOUPPER" => {
                    let [input, out] = rest else {
                        return None;
                    };
                    if !arg_is_loop_pure(arg_loop_pure, 2, require_loop_pure_target) {
                        return None;
                    }
                    let out_bytes = out.value.as_slice();
                    if !valid_local_var(out_bytes) || protected.contains(out_bytes) {
                        return None;
                    }
                    Some((
                        BString::from(out_bytes),
                        LocalBinding {
                            value: Some(input.value.to_ascii_uppercase().into()),
                            loop_pure: arg_loop_pure.get(1).copied().unwrap_or(false),
                        },
                    ))
                }
                b"TOLOWER" => {
                    let [input, out] = rest else {
                        return None;
                    };
                    if !arg_is_loop_pure(arg_loop_pure, 2, require_loop_pure_target) {
                        return None;
                    }
                    let out_bytes = out.value.as_slice();
                    if !valid_local_var(out_bytes) || protected.contains(out_bytes) {
                        return None;
                    }
                    Some((
                        BString::from(out_bytes),
                        LocalBinding {
                            value: Some(input.value.to_ascii_lowercase().into()),
                            loop_pure: arg_loop_pure.get(1).copied().unwrap_or(false),
                        },
                    ))
                }
                b"STRIP" => {
                    let [input, out] = rest else {
                        return None;
                    };
                    if !arg_is_loop_pure(arg_loop_pure, 2, require_loop_pure_target) {
                        return None;
                    }
                    let out_bytes = out.value.as_slice();
                    if !valid_local_var(out_bytes) || protected.contains(out_bytes) {
                        return None;
                    }
                    Some((
                        BString::from(out_bytes),
                        LocalBinding {
                            value: Some(strip_cmake_whitespace(&input.value).into()),
                            loop_pure: arg_loop_pure.get(1).copied().unwrap_or(false),
                        },
                    ))
                }
                b"CONCAT" => {
                    let (out, inputs) = rest.split_first()?;
                    if !arg_is_loop_pure(arg_loop_pure, 1, require_loop_pure_target) {
                        return None;
                    }
                    let out_bytes = out.value.as_slice();
                    if !valid_local_var(out_bytes) || protected.contains(out_bytes) {
                        return None;
                    }
                    let mut total = 0usize;
                    for piece in inputs {
                        total = total.checked_add(piece.value.len())?;
                        if total > MAX_SOURCE_BYTES {
                            return None;
                        }
                    }
                    let mut buf = Vec::with_capacity(total);
                    for piece in inputs {
                        buf.extend_from_slice(&piece.value);
                    }
                    let loop_pure = arg_loop_pure
                        .get(2..)
                        .is_some_and(|flags| flags.iter().all(|&flag| flag));
                    Some((
                        BString::from(out_bytes),
                        LocalBinding {
                            value: Some(buf.into()),
                            loop_pure,
                        },
                    ))
                }
                b"MAKE_C_IDENTIFIER" => {
                    let [input, out] = rest else {
                        return None;
                    };
                    if !arg_is_loop_pure(arg_loop_pure, 2, require_loop_pure_target) {
                        return None;
                    }
                    let out_bytes = out.value.as_slice();
                    if !valid_local_var(out_bytes) || protected.contains(out_bytes) {
                        return None;
                    }
                    let ident = make_c_identifier(&input.value)?;
                    Some((
                        BString::from(out_bytes),
                        LocalBinding {
                            value: Some(ident),
                            loop_pure: arg_loop_pure.get(1).copied().unwrap_or(false),
                        },
                    ))
                }
                b"REPLACE" => {
                    let [needle, replacement, out, inputs @ ..] = rest else {
                        return None;
                    };
                    if inputs.is_empty()
                        || !arg_is_loop_pure(arg_loop_pure, 3, require_loop_pure_target)
                    {
                        return None;
                    }
                    let out_bytes = out.value.as_slice();
                    if needle.value.is_empty()
                        || !valid_local_var(out_bytes)
                        || protected.contains(out_bytes)
                    {
                        return None;
                    }
                    let mut input = Vec::new();
                    for piece in inputs {
                        if input.len().saturating_add(piece.value.len()) > MAX_SOURCE_BYTES {
                            return None;
                        }
                        input.extend_from_slice(&piece.value);
                    }
                    let replaced = replace_bytes(&input, &needle.value, &replacement.value)?;
                    let loop_pure = arg_loop_pure.get(1).copied().unwrap_or(false)
                        && arg_loop_pure.get(2).copied().unwrap_or(false)
                        && arg_loop_pure
                            .get(4..)
                            .is_some_and(|flags| flags.iter().all(|&flag| flag));
                    Some((
                        BString::from(out_bytes),
                        LocalBinding {
                            value: Some(replaced),
                            loop_pure,
                        },
                    ))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn strip_cmake_whitespace(bytes: &[u8]) -> &[u8] {
    let is_space = |b: &u8| matches!(*b, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c);
    let start = bytes
        .iter()
        .position(|b| !is_space(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_space(b))
        .map_or(start, |i| i.saturating_add(1));
    bytes.get(start..end).unwrap_or_default()
}

fn make_c_identifier(input: &[u8]) -> Option<BString> {
    if input.is_empty() || input.iter().any(|b| !b.is_ascii()) {
        return None;
    }
    let mut out = Vec::with_capacity(input.len().saturating_add(1));
    if input.first().is_some_and(u8::is_ascii_digit) {
        out.push(b'_');
    }
    for &byte in input {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            out.push(byte);
        } else {
            out.push(b'_');
        }
    }
    Some(out.into())
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Option<BString> {
    let mut out = Vec::new();
    let mut rest = haystack;
    while !rest.is_empty() {
        if let Some(pos) = rest.windows(needle.len()).position(|w| w == needle) {
            let prefix = rest.get(..pos)?;
            if out
                .len()
                .saturating_add(prefix.len())
                .saturating_add(replacement.len())
                > MAX_SOURCE_BYTES
            {
                return None;
            }
            out.extend_from_slice(prefix);
            out.extend_from_slice(replacement);
            rest = rest.get(pos.saturating_add(needle.len())..)?;
        } else {
            if out.len().saturating_add(rest.len()) > MAX_SOURCE_BYTES {
                return None;
            }
            out.extend_from_slice(rest);
            break;
        }
    }
    Some(out.into())
}

fn valid_source_filename(name: &[u8]) -> bool {
    !name.is_empty()
        && !name.contains(&b'/')
        && !name.contains(&b'\\')
        && !name.contains(&0)
        && name != b"."
        && name != b".."
        && !name.eq_ignore_ascii_case(b"CMakeLists.txt")
}

/// One syntactic candidate in the original command order.
#[cfg(test)]
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
#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn is_literal(arg: &Argument) -> bool {
    arg.kind == ArgKind::Bracket
        || !arg.text.iter().any(|byte| {
            matches!(byte, b'$' | b'\\')
                || (arg.kind == ArgKind::Unquoted && matches!(byte, b';' | b'(' | b')'))
        })
}

fn valid_local_var(name: &[u8]) -> bool {
    valid_result(name)
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
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct HistoryProbe {
    /// Probe language as bytes.
    pub(crate) language: Vec<u8>,
    /// Project recipe digest before the current toolchain and seed are applied.
    pub(crate) recipe: Digest,
}

#[cfg(test)]
impl ToJson for HistoryProbe {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("language", self.language.to_json()),
            ("recipe", self.recipe.to_json()),
        ])
    }
}

#[cfg(test)]
impl FromJson for HistoryProbe {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(obj, &["language", "recipe"])?;
        Ok(Self {
            language: req(obj, "language")?,
            recipe: req(obj, "recipe")?,
        })
    }
}

/// Input for the next configure's speculative jobs.
///
/// The evaluator resolves the recipe digest and recomputes the key with the
/// current toolchain, seed, and arguments before either use or publication.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct ProbeHistory {
    /// Previous calls in evaluator order.
    pub(crate) probes: Vec<HistoryProbe>,
}

#[cfg(test)]
impl ToJson for ProbeHistory {
    fn to_json(&self) -> Value {
        Value::object_from([("probes", self.probes.to_json())])
    }
}

#[cfg(test)]
impl FromJson for ProbeHistory {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(obj, &["probes"])?;
        Ok(Self {
            probes: req(obj, "probes")?,
        })
    }
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
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use rcmake_base::Limits;
    use rcmake_diag::{SourceMap, source::SourceKind};
    use rcmake_eval::{CmdFlags, Flow, Interp, InterpConfig, RunMode};
    use rcmake_host::PureHost;
    use rcmake_syntax::{ListFile, parse_list_file};

    use super::*;

    #[derive(Default)]
    struct TestVars {
        vars: BTreeMap<BString, BString>,
        cache: BTreeMap<BString, BString>,
        overridden: BTreeSet<BString>,
    }

    impl VarLookup for TestVars {
        fn var(&self, name: &[u8]) -> Option<&[u8]> {
            self.vars
                .get(name)
                .or_else(|| self.cache.get(name))
                .map(|value| value.as_slice())
        }

        fn cache_var(&self, name: &[u8]) -> Option<&[u8]> {
            self.cache.get(name).map(|value| value.as_slice())
        }

        fn is_builtin(&self, name: &[u8]) -> bool {
            !self.overridden.contains(name)
        }
    }

    fn parse(input: &[u8]) -> Result<ListFile, Box<dyn std::error::Error>> {
        let mut sources = SourceMap::default();
        let kind = SourceKind::File;
        let source = sources
            .add("lookahead.cmake".into(), kind, Arc::from(input))
            .ok_or("Cannot register the test source.")?;
        Ok(parse_list_file(input, source, kind, &Limits::default())?)
    }

    fn eval_foreach_capture(
        script: &str,
    ) -> Result<Vec<ExpandedProbeCandidate>, Box<dyn std::error::Error>> {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = Arc::new(PureHost::new(root.clone(), root.join("rcmake")));
        host.seed_file(&root.join("script.cmake"), script.as_bytes())?;
        let captured: Arc<Mutex<Option<Vec<ExpandedProbeCandidate>>>> = Arc::new(Mutex::new(None));
        let capture_hook = Arc::clone(&captured);

        let config = InterpConfig::new(
            host,
            RunMode::Script,
            root.join("cmake"),
            root.join("rcmake"),
            root.clone(),
        );
        let mut interp = Interp::new(config, |registry| {
            crate::register_all(registry);
            let flags = CmdFlags { scriptable: true };
            registry.add(
                "try_compile",
                |cx, _| {
                    let cx_ref: &Cx<'_> = cx;
                    if let Some(sink) =
                        cx_ref.session_data::<Arc<Mutex<Option<Vec<ExpandedProbeCandidate>>>>>()
                        && let Ok(mut guard) = sink.lock()
                        && guard.is_none()
                        && let Some(frame) = cx_ref.foreach_lookahead()
                    {
                        let found = lookahead_foreach(
                            cx_ref,
                            cx_ref.var_scope_depth(),
                            &frame,
                            |_, _, _| true,
                        );
                        *guard = Some(found);
                    }
                    Ok(Flow::Normal)
                },
                flags,
            );
            for probe_cmd in [
                "check_include_file",
                "check_function_exists",
                "check_c_source_compiles",
                "check_cxx_source_compiles",
            ] {
                registry.add(
                    probe_cmd,
                    |cx, _| {
                        let cx_ref: &Cx<'_> = cx;
                        if let Some(sink) =
                            cx_ref.session_data::<Arc<Mutex<Option<Vec<ExpandedProbeCandidate>>>>>()
                            && let Ok(mut guard) = sink.lock()
                            && guard.is_none()
                            && let Some(frame) = cx_ref.foreach_lookahead()
                        {
                            let found = lookahead_foreach(
                                cx_ref,
                                cx_ref.var_scope_depth(),
                                &frame,
                                |_, _, _| true,
                            );
                            *guard = Some(found);
                        }
                        Ok(Flow::Normal)
                    },
                    flags,
                );
            }
        });
        interp.add_session_data(capture_hook);
        let outcome = interp.run_script(&root.join("script.cmake"));
        assert_eq!(outcome.exit_code, 0);
        let result = captured
            .lock()
            .map_err(|_| "lock poisoned")?
            .take()
            .unwrap_or_default();
        Ok(result)
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
            let file = parse(
                format!(
                    "check_include_file(stdio.h HAVE_OLD)\n{command}\ncheck_include_file(errno.h HAVE_ERRNO)"
                )
                .as_bytes(),
            )?;
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
    fn expanded_straight_line_resolves_vars_and_source_from_var()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut vars = TestVars::default();
        vars.vars
            .insert("BODY".into(), "int main(void) { return 0; }".into());
        vars.vars.insert("PREFIX".into(), "HAVE".into());
        let file = parse(
            br#"try_compile(${PREFIX}_ONE SOURCE_FROM_VAR one.c BODY NO_CACHE)
set(NEXT_SRC "int main(void) { return 1; }")
string(TOUPPER "two" TWO_UP)
try_compile(${PREFIX}_${TWO_UP} SOURCE_FROM_CONTENT two.c "${NEXT_SRC}" NO_CACHE OUTPUT_VARIABLE OUT_TWO)
try_compile(READS_EARLIER SOURCE_FROM_CONTENT three.c "${HAVE_ONE}" NO_CACHE)"#,
        )?;
        let (current, remaining) = file
            .commands
            .split_first()
            .ok_or("Missing first command.")?;
        let batch = lookahead_expanded(&vars, Some(current), remaining, |_, _, _| true);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].result, "HAVE_ONE");
        assert_eq!(
            batch[0].spec,
            ExpandedProbeSpec::TryCompile {
                filename: "one.c".into(),
                content: "int main(void) { return 0; }".into(),
            }
        );
        assert_eq!(batch[1].result, "HAVE_TWO");
        assert_eq!(
            batch[1].spec,
            ExpandedProbeSpec::TryCompile {
                filename: "two.c".into(),
                content: "int main(void) { return 1; }".into(),
            }
        );
        Ok(())
    }

    #[test]
    fn expanded_straight_line_stops_on_env_or_cmake_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let vars = TestVars::default();
        let file = parse(
            br#"try_compile(FIRST SOURCE_FROM_CONTENT a.c "int main(){return 0;}" NO_CACHE)
set(CMAKE_REQUIRED_FLAGS "-Werror")
try_compile(SECOND SOURCE_FROM_CONTENT b.c "int main(){return 1;}" NO_CACHE)"#,
        )?;
        let (current, remaining) = file
            .commands
            .split_first()
            .ok_or("Missing first command.")?;
        assert!(lookahead_expanded(&vars, Some(current), remaining, |_, _, _| true).is_empty());

        let env_file = parse(
            br#"try_compile(FIRST SOURCE_FROM_CONTENT a.c "int main(){return 0;}" NO_CACHE)
try_compile(SECOND SOURCE_FROM_CONTENT b.c "$ENV{DYNAMIC_SRC}" NO_CACHE)"#,
        )?;
        let (current, remaining) = env_file
            .commands
            .split_first()
            .ok_or("Missing first command.")?;
        assert!(lookahead_expanded(&vars, Some(current), remaining, |_, _, _| true).is_empty());
        Ok(())
    }

    #[test]
    fn foreach_items_expands_pure_string_derivations_and_probes()
    -> Result<(), Box<dyn std::error::Error>> {
        let found = eval_foreach_capture(
            r##"
foreach(hdr IN ITEMS sys/types.h stdint.h unistd.h)
  string(MAKE_C_IDENTIFIER "${hdr}" ident)
  string(TOUPPER "${ident}" upper)
  set(src "#include <${hdr}>\nint main(void) { return 0; }\n")
  try_compile(HAVE_${upper} SOURCE_FROM_VAR "check_${ident}.c" src NO_CACHE NO_LOG)
endforeach()
"##,
        )?;
        assert_eq!(
            found,
            vec![
                ExpandedProbeCandidate {
                    kind: ProbeKind::TryCompile,
                    result: "HAVE_STDINT_H".into(),
                    spec: ExpandedProbeSpec::TryCompile {
                        filename: "check_stdint_h.c".into(),
                        content: "#include <stdint.h>\nint main(void) { return 0; }\n".into(),
                    },
                },
                ExpandedProbeCandidate {
                    kind: ProbeKind::TryCompile,
                    result: "HAVE_UNISTD_H".into(),
                    spec: ExpandedProbeSpec::TryCompile {
                        filename: "check_unistd_h.c".into(),
                        content: "#include <unistd.h>\nint main(void) { return 0; }\n".into(),
                    },
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn foreach_range_and_zip_lists_expand_cleanly() -> Result<(), Box<dyn std::error::Error>> {
        let range_found = eval_foreach_capture(
            r#"
foreach(idx RANGE 1 3)
  try_compile(HAVE_${idx} SOURCE_FROM_CONTENT "p_${idx}.c" "int main(void) { return ${idx}; }" NO_CACHE)
endforeach()
"#,
        )?;
        assert_eq!(
            range_found
                .iter()
                .map(|c| c.result.clone())
                .collect::<Vec<_>>(),
            vec![BString::from("HAVE_2"), BString::from("HAVE_3")]
        );

        let zip_found = eval_foreach_capture(
            r#"
set(HEADERS stdio.h stdlib.h errno.h)
set(NAMES HAVE_STDIO HAVE_STDLIB HAVE_ERRNO)
foreach(hdr var IN ZIP_LISTS HEADERS NAMES)
  check_include_file("${hdr}" "${var}")
endforeach()
"#,
        )?;
        assert_eq!(
            zip_found,
            vec![
                ExpandedProbeCandidate {
                    kind: ProbeKind::IncludeFile,
                    result: "HAVE_STDLIB".into(),
                    spec: ExpandedProbeSpec::IncludeFile {
                        header: "stdlib.h".into(),
                    },
                },
                ExpandedProbeCandidate {
                    kind: ProbeKind::IncludeFile,
                    result: "HAVE_ERRNO".into(),
                    spec: ExpandedProbeSpec::IncludeFile {
                        header: "errno.h".into(),
                    },
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn foreach_rejects_body_mutating_variable_read_by_later_probe()
    -> Result<(), Box<dyn std::error::Error>> {
        for script in [
            // Loop-carried accumulator mutated and read on next iteration:
            r#"
set(ACC "int x = 0;")
foreach(n IN ITEMS a b c)
  set(ACC "${ACC}\nint v_${n} = 1;")
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "${ACC}\nint main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // Mutation after probe read on next iteration:
            r#"
set(SHARED "int main(void){return 0;}")
foreach(n IN ITEMS a b c)
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "${SHARED}" NO_CACHE)
  set(SHARED "int main(void){return 1;}")
endforeach()
"#,
            // Mutates CMAKE_REQUIRED_FLAGS:
            r#"
foreach(n IN ITEMS a b)
  set(CMAKE_REQUIRED_FLAGS "-D${n}")
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // Reads earlier probe result in the same iteration:
            r#"
foreach(n IN ITEMS a b)
  try_compile(FIRST_${n} SOURCE_FROM_CONTENT "${n}1.c" "int main(void){return 0;}" NO_CACHE)
  try_compile(SECOND_${n} SOURCE_FROM_CONTENT "${n}2.c" "/* ${FIRST_${n}} */ int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // Duplicate result variable across iterations:
            r#"
foreach(n IN ITEMS a b)
  try_compile(HAVE_SAME SOURCE_FROM_CONTENT "${n}.c" "int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // Control flow inside loop body:
            r#"
foreach(n IN ITEMS a b)
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "int main(void){return 0;}" NO_CACHE)
  if(HAVE_${n})
    set(OK_${n} 1)
  endif()
endforeach()
"#,
            // Mutates loop variable:
            r#"
foreach(n IN ITEMS a b)
  set(n "fixed")
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // CACHE / PARENT_SCOPE write:
            r#"
foreach(n IN ITEMS a b)
  set(TMP_${n} "1" CACHE INTERNAL "")
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // $ENV{} access:
            r#"
foreach(n IN ITEMS a b)
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "$ENV{BODY}" NO_CACHE)
endforeach()
"#,
            // Reading previous iteration's probe result via fixed name:
            r#"
set(PREV "a")
foreach(n IN ITEMS a b)
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "/* ${HAVE_a} */ int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // Dynamic write target derived from mutable live state (mid-iteration divergence):
            r#"
set(A "A")
foreach(n IN ITEMS a b)
  set(T "${A}")
  set(${T} "X")
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "/* ${T} */ int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            // Probe target derived from outer variable rather than loop variable:
            r#"
set(P "P")
foreach(n IN ITEMS a b)
  try_compile(${P} SOURCE_FROM_CONTENT "${n}1.c" "int main(void){return 0;}" NO_CACHE)
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}2.c" "/* ${P} */ int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
        ] {
            let found = eval_foreach_capture(script)?;
            assert!(found.is_empty(), "expected rejection for script:\n{script}");
        }
        Ok(())
    }

    #[test]
    fn rejects_overridden_set_and_string_builtins() -> Result<(), Box<dyn std::error::Error>> {
        // Straight-line with overridden `set` or `string`:
        let file = parse(
            br#"try_compile(FIRST SOURCE_FROM_CONTENT a.c "int main(){return 0;}" NO_CACHE)
set(BODY "int main(){return 1;}")
string(TOUPPER "second" SUFFIX)
try_compile(HAVE_${SUFFIX} SOURCE_FROM_CONTENT b.c "${BODY}" NO_CACHE)"#,
        )?;
        let (current, remaining) = file
            .commands
            .split_first()
            .ok_or("Missing first command.")?;
        assert_eq!(
            lookahead_expanded(&TestVars::default(), Some(current), remaining, |_, _, _| {
                true
            })
            .len(),
            2
        );
        for overridden in ["set", "string"] {
            let mut vars = TestVars::default();
            vars.overridden.insert(overridden.into());
            assert!(
                lookahead_expanded(&vars, Some(current), remaining, |_, _, _| true).is_empty(),
                "straight-line must reject overridden {overridden}"
            );
        }

        // Foreach with user `function(set)` or `macro(string)`:
        for script in [
            r#"
function(set var val)
  _set(${var} "${val}" PARENT_SCOPE)
endfunction()
foreach(n IN ITEMS a b c)
  set(src "int main(void){return 0;} /* ${n} */")
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "${src}" NO_CACHE)
endforeach()
"#,
            r#"
macro(string op input out)
  _string(${op} "${input}" ${out})
endmacro()
foreach(n IN ITEMS a b c)
  string(TOUPPER "${n}" up)
  try_compile(HAVE_${up} SOURCE_FROM_CONTENT "${n}.c" "int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
        ] {
            let found = eval_foreach_capture(script)?;
            assert!(
                found.is_empty(),
                "foreach must reject overridden built-in:\n{script}"
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_bare_set_unset_in_straight_line_and_foreach()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut vars = TestVars::default();
        vars.vars.insert("TMP".into(), "stale".into());
        for middle in ["set(TMP)", "set(TMP ${UNSET_VALUE})"] {
            let file = parse(
                format!(
                    "try_compile(FIRST SOURCE_FROM_CONTENT a.c \"int main(){{return 0;}}\" NO_CACHE)\n\
                     {middle}\n\
                     try_compile(SECOND SOURCE_FROM_CONTENT b.c \"/* ${{TMP}} */ int main(){{return 0;}}\" NO_CACHE)"
                )
                .as_bytes(),
            )?;
            let (current, remaining) = file
                .commands
                .split_first()
                .ok_or("Missing first command.")?;
            assert!(
                lookahead_expanded(&vars, Some(current), remaining, |_, _, _| true).is_empty(),
                "straight-line must reject bare unset `{middle}`"
            );
        }

        for script in [
            r#"
set(TMP "before")
foreach(n IN ITEMS a b)
  set(TMP)
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "/* ${TMP} */ int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
            r#"
set(TMP "before")
foreach(n IN ITEMS a b)
  set(TMP ${EMPTY_UNSET})
  try_compile(HAVE_${n} SOURCE_FROM_CONTENT "${n}.c" "/* ${TMP} */ int main(void){return 0;}" NO_CACHE)
endforeach()
"#,
        ] {
            let found = eval_foreach_capture(script)?;
            assert!(
                found.is_empty(),
                "foreach must reject bare unset:\n{script}"
            );
        }
        Ok(())
    }

    #[test]
    fn check_include_file_tracks_caller_scope_macro_writes()
    -> Result<(), Box<dyn std::error::Error>> {
        // Straight-line: reading `CHECK_INCLUDE_FILE_VAR` or `_CIF_*` after `check_include_file`
        // must stop the batch.
        for bad_read in [
            "${CHECK_INCLUDE_FILE_VAR}",
            "${_CIF_SOURCE_CONTENT}",
            "${_CIF_EXTRA_CMAKE_ARGUMENTS}",
            "${_CIF_CUSTOM}",
            "${CMAKE_REQUIRED_FLAGS}",
        ] {
            let file = parse(
                format!(
                    "check_include_file(stdio.h HAVE_STDIO)\n\
                     set(COPY \"{bad_read}\")\n\
                     check_include_file(stdlib.h HAVE_STDLIB)"
                )
                .as_bytes(),
            )?;
            let (current, remaining) = file
                .commands
                .split_first()
                .ok_or("Missing first command.")?;
            assert!(
                lookahead_expanded(&TestVars::default(), Some(current), remaining, |_, _, _| {
                    true
                })
                .is_empty(),
                "straight-line must reject read of {bad_read} after check_include_file"
            );
        }

        // Foreach: reading `CHECK_INCLUDE_FILE_VAR` / `_CIF_*` / `CMAKE_REQUIRED_FLAGS` before
        // or after `check_include_file`, or using them as loop/result variables, must reject.
        for script in [
            r#"
foreach(hdr IN ITEMS stdio.h stdlib.h)
  string(MAKE_C_IDENTIFIER "${hdr}" id)
  set(PREV "${CHECK_INCLUDE_FILE_VAR}")
  check_include_file("${hdr}" HAVE_${id})
endforeach()
"#,
            r#"
foreach(hdr IN ITEMS stdio.h stdlib.h)
  string(MAKE_C_IDENTIFIER "${hdr}" id)
  check_include_file("${hdr}" HAVE_${id})
  set(AFTER "${_CIF_SOURCE_CONTENT}")
endforeach()
"#,
            r#"
foreach(hdr IN ITEMS stdio.h stdlib.h)
  string(MAKE_C_IDENTIFIER "${hdr}" id)
  set(FLAGS_COPY "${CMAKE_REQUIRED_FLAGS}")
  check_include_file("${hdr}" HAVE_${id})
endforeach()
"#,
            r#"
foreach(CHECK_INCLUDE_FILE_VAR IN ITEMS stdio.h stdlib.h)
  string(MAKE_C_IDENTIFIER "${CHECK_INCLUDE_FILE_VAR}" id)
  check_include_file("${CHECK_INCLUDE_FILE_VAR}" HAVE_${id})
endforeach()
"#,
            r#"
foreach(_CIF_SOURCE_CONTENT IN ITEMS stdio.h stdlib.h)
  string(MAKE_C_IDENTIFIER "${_CIF_SOURCE_CONTENT}" id)
  check_include_file("${_CIF_SOURCE_CONTENT}" HAVE_${id})
endforeach()
"#,
        ] {
            let found = eval_foreach_capture(script)?;
            assert!(
                found.is_empty(),
                "foreach must reject CheckIncludeFile macro-write conflict:\n{script}"
            );
        }

        // Clean `check_include_file` loop with no reference to macro caller-scope vars still passes.
        let clean = eval_foreach_capture(
            r#"
foreach(hdr IN ITEMS stdio.h stdlib.h errno.h)
  string(MAKE_C_IDENTIFIER "${hdr}" id)
  string(TOUPPER "${id}" up)
  check_include_file("${hdr}" HAVE_${up})
endforeach()
"#,
        )?;
        assert_eq!(clean.len(), 2);
        assert_eq!(clean[0].result, "HAVE_STDLIB_H");
        assert_eq!(clean[1].result, "HAVE_ERRNO_H");
        Ok(())
    }

    #[test]
    fn lookahead_peeks_variables_without_marking_tracked_reads()
    -> Result<(), Box<dyn std::error::Error>> {
        // `FUTURE_BODY` is set in cache/normal state and only peeked by lookahead during
        // iteration 0 (`n = a`), while `USED_NOW` is actually expanded by iteration 0.
        // Here `FUTURE_BODY_b` is NOT expanded during iteration 0's foreground execution of
        // `try_compile` because iteration 0 expands `FUTURE_BODY_a`, yet lookahead peeks
        // `FUTURE_BODY_b` when simulating iteration 1.
        // We break out of the loop right after iteration 0 so `FUTURE_BODY_b` is NEVER read
        // by foreground execution — only peeked by lookahead!
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = Arc::new(PureHost::new(root.clone(), root.join("rcmake")));
        let script = r#"
foreach(n IN ITEMS a b)
  try_compile(HAVE_${n} SOURCE_FROM_VAR "${n}.c" BODY_${n} NO_CACHE)
endforeach()
"#;
        host.seed_file(&root.join("script.cmake"), script.as_bytes())?;
        let captured: Arc<Mutex<Option<Vec<ExpandedProbeCandidate>>>> = Arc::new(Mutex::new(None));
        let capture_hook = Arc::clone(&captured);

        let mut config = InterpConfig::new(
            host,
            RunMode::Script,
            root.join("cmake"),
            root.join("rcmake"),
            root.clone(),
        );
        config.defines.push(rcmake_eval::CacheDefine {
            name: "BODY_a".into(),
            ty: Some(rcmake_model::CacheType::String),
            value: "int main(void){return 0;}".into(),
        });
        config.defines.push(rcmake_eval::CacheDefine {
            name: "BODY_b".into(),
            ty: Some(rcmake_model::CacheType::String),
            value: "int main(void){return 1;}".into(),
        });
        let mut interp = Interp::new(config, |registry| {
            crate::register_all(registry);
            registry.add(
                "try_compile",
                |cx, _| {
                    let cx_ref: &Cx<'_> = cx;
                    if let Some(sink) =
                        cx_ref.session_data::<Arc<Mutex<Option<Vec<ExpandedProbeCandidate>>>>>()
                        && let Ok(mut guard) = sink.lock()
                        && guard.is_none()
                        && let Some(frame) = cx_ref.foreach_lookahead()
                    {
                        let found = lookahead_foreach(
                            cx_ref,
                            cx_ref.var_scope_depth(),
                            &frame,
                            |_, _, _| true,
                        );
                        *guard = Some(found);
                    }
                    // Break after iteration 0 so iteration 1 never executes in foreground.
                    Ok(Flow::Break)
                },
                CmdFlags { scriptable: true },
            );
        });
        interp.add_session_data(capture_hook);
        interp.track_variable_reads([BString::from("BODY_a"), BString::from("BODY_b")]);
        let outcome = interp.run_script(&root.join("script.cmake"));
        assert_eq!(outcome.exit_code, 0);
        let found = captured
            .lock()
            .map_err(|_| "lock poisoned")?
            .take()
            .unwrap_or_default();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].spec,
            ExpandedProbeSpec::TryCompile {
                filename: "b.c".into(),
                content: "int main(void){return 1;}".into(),
            }
        );
        let unread: BTreeSet<BString> = interp.unread_variables().map(BString::from).collect();
        assert!(
            unread.contains(b"BODY_b".as_slice()),
            "peeked variable BODY_b must remain unread after lookahead; unread={unread:?}"
        );
        Ok(())
    }

    #[test]
    fn foreach_bounds_binding_bytes_and_slots_before_copying()
    -> Result<(), Box<dyn std::error::Error>> {
        // Large per-iteration payloads: total bytes exceed `MAX_FOREACH_BINDING_BYTES` (256 KiB),
        // so lookahead stops cleanly within budget instead of cloning all 64 iterations.
        let chunk = "x".repeat(96 * 1024);
        let script = format!(
            "set(L1 \"{chunk}\" \"{chunk}\" \"{chunk}\" \"{chunk}\")\n\
             set(NAMES a b c d)\n\
             foreach(name payload IN ZIP_LISTS NAMES L1)\n\
               try_compile(HAVE_${{name}} SOURCE_FROM_CONTENT \"${{name}}.c\" \"/* ${{payload}} */ int main(void){{return 0;}}\" NO_CACHE)\n\
             endforeach()\n"
        );
        let found = eval_foreach_capture(&script)?;
        // Iteration 0 uses ~96 KiB, iteration 1 uses ~192 KiB (admitted), iteration 2 would hit
        // ~288 KiB > 256 KiB (stopped before cloning).
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].result, "HAVE_b");

        // Wide ZIP_LISTS exceeding slot cap stops within `MAX_FOREACH_BINDING_SLOTS` (512).
        let mut wide = String::new();
        let mut var_names = Vec::new();
        let mut list_names = Vec::new();
        for col in 0..32 {
            let l = format!("COL_{col}");
            wide.push_str(&format!(
                "set({l} a0 a1 a2 a3 a4 a5 a6 a7 a8 a9 a10 a11 a12 a13 a14 a15 a16 a17 a18 a19)\n"
            ));
            var_names.push(format!("v{col}"));
            list_names.push(l);
        }
        wide.push_str(&format!(
            "foreach({} IN ZIP_LISTS {})\n\
               try_compile(HAVE_${{v0}} SOURCE_FROM_CONTENT \"${{v0}}.c\" \"int main(void){{return 0;}}\" NO_CACHE)\n\
             endforeach()\n",
            var_names.join(" "),
            list_names.join(" ")
        ));
        let wide_found = eval_foreach_capture(&wide)?;
        // 32 vars * 16 iterations = 512 slots -> 1 current + 15 upcoming iterations max.
        assert_eq!(wide_found.len(), 15);
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
        assert_eq!(ProbeHistory::from_json(&history.to_json()), Ok(history));
        assert!(
            ProbeHistory::from_json(&Value::object_from([
                ("probes", Value::Array(Vec::new())),
                ("extra", Value::Bool(true)),
            ]))
            .is_err()
        );
    }

    #[test]
    fn check_source_compiles_function_exists_and_not_defined_guards()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1. Straight-line `check_cxx_source_compiles` with `FAIL_REGEX` and
        //    `if(NOT DEFINED ...)` guards (skipping already-defined vars, entering undefined).
        let mut vars = TestVars::default();
        vars.vars
            .insert(b"HAVE_SKIP".as_slice().into(), b"1".as_slice().into());
        let file = parse(
            br#"
check_cxx_source_compiles("int main() { return 0; }" HAVE_A)
if(NOT DEFINED HAVE_SKIP)
  check_cxx_source_compiles("int main() { return 1; }" HAVE_SKIP)
endif()
if(NOT DEFINED HAVE_B)
  check_cxx_source_compiles("int main() { return 2; }" HAVE_B FAIL_REGEX "warning")
endif()
"#,
        )?;
        let (current, remaining) = file
            .commands
            .split_first()
            .ok_or("Missing first command.")?;
        let batch = lookahead_expanded(&vars, Some(current), remaining, |kind, _, _| {
            kind == ProbeKind::CxxSourceCompiles
        });
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].result, "HAVE_A");
        assert_eq!(batch[1].result, "HAVE_B");
        assert_eq!(
            batch[1].spec,
            ExpandedProbeSpec::CxxSourceCompiles {
                source: "int main() { return 2; }".into(),
            }
        );

        // `SRC_EXT` and `OUTPUT_VARIABLE` must be rejected cleanly.
        for bad in [
            "check_c_source_compiles(\"int main(){}\" HAVE_A)\ncheck_c_source_compiles(\"int main(){}\" HAVE_B SRC_EXT .cc)",
            "check_c_source_compiles(\"int main(){}\" HAVE_A)\ncheck_c_source_compiles(\"int main(){}\" HAVE_B OUTPUT_VARIABLE OUT)",
        ] {
            let file = parse(bad.as_bytes())?;
            let (current, remaining) = file
                .commands
                .split_first()
                .ok_or("Missing first command.")?;
            assert!(
                lookahead_expanded(&TestVars::default(), Some(current), remaining, |_, _, _| {
                    true
                })
                .is_empty(),
                "must reject {bad}"
            );
        }

        // 2. `check_function_exists` in `foreach()` plus `_CFE_` write conflict rejection.
        let clean = eval_foreach_capture(
            r#"
foreach(fn IN ITEMS memcpy memmove strlcpy)
  string(TOUPPER "${fn}" up)
  check_function_exists("${fn}" HAVE_${up})
endforeach()
"#,
        )?;
        assert_eq!(clean.len(), 2);
        assert_eq!(clean[0].kind, ProbeKind::FunctionExists);
        assert_eq!(clean[0].result, "HAVE_MEMMOVE");
        assert_eq!(
            clean[0].spec,
            ExpandedProbeSpec::FunctionExists {
                function: "memmove".into(),
            }
        );
        assert_eq!(clean[1].result, "HAVE_STRLCPY");

        let conflict = eval_foreach_capture(
            r#"
foreach(fn IN ITEMS memcpy memmove)
  string(TOUPPER "${fn}" up)
  check_function_exists("${fn}" HAVE_${up})
  set(COPY "${_CFE_EXTRA_CMAKE_ARGUMENTS}")
endforeach()
"#,
        )?;
        assert!(conflict.is_empty());
        Ok(())
    }
}
