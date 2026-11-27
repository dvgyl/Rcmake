//! Variable expansion scanner and `if()`/`elseif()` condition operand analyzer.
//!
//! Uses CMake's exact escape and namespace rules (`${...}`, `$ENV{...}`, `$CACHE{...}`),
//! skips [`ArgKind::Bracket`], handles escaped `\$`, and never evaluates expressions.

#![forbid(unsafe_code)]

use core::ops::Range;

use rcmake_base::BString;
use rcmake_syntax::{ArgKind, Argument};

/// Variable namespace referenced by an expansion expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VarNamespace {
    Normal,
    Cache,
    Env,
}

/// One literal variable reference extracted from an argument's raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VarRef {
    pub(crate) namespace: VarNamespace,
    pub(crate) name: BString,
    /// Full `${NAME}` / `$ENV{NAME}` / `$CACHE{NAME}` span in raw source bytes.
    pub(crate) span: Range<usize>,
}

/// Result of scanning an argument for variable expansions.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArgExpansions {
    /// Literal variable references (`${FOO}`, `$ENV{BAR}`, `$CACHE{BAZ}`).
    pub(crate) refs: Vec<VarRef>,
    /// `true` if the argument contains any active expansion (literal or dynamic nested).
    pub(crate) has_active_expansion: bool,
    /// `true` if the argument contains a nested/dynamic variable reference such as `${A_${B}}`.
    pub(crate) has_dynamic_ref: bool,
}

const MAX_EXPANSION_DEPTH: usize = 64;

/// Scan raw bytes of `arg` in `src` for variable references (`${...}`, `$ENV{...}`, `$CACHE{...}`).
///
/// Bracket arguments are verbatim and return an empty [`ArgExpansions`].
pub(crate) fn scan_arg_expansions(arg: &Argument, src: &[u8]) -> ArgExpansions {
    let mut out = ArgExpansions::default();
    if arg.kind == ArgKind::Bracket {
        return out;
    }
    let (base, slice) = match arg.kind {
        ArgKind::Quoted => {
            let start = arg.span.start.saturating_add(1);
            let end = arg.span.end.saturating_sub(1);
            if start >= end {
                return out;
            }
            (start, src.get(start..end).unwrap_or_default())
        }
        ArgKind::Unquoted => (
            arg.span.start,
            src.get(arg.span.clone()).unwrap_or_default(),
        ),
        ArgKind::Bracket => return out,
    };

    if !slice.contains(&b'$') {
        return out;
    }

    struct OpenFrame {
        namespace: VarNamespace,
        full_start: usize,
        name_start: usize,
        literal_valid: bool,
        has_nested: bool,
    }

    let mut stack: Vec<OpenFrame> = Vec::new();
    let mut idx = 0usize;

    while idx < slice.len() {
        let Some(&byte) = slice.get(idx) else {
            break;
        };
        match byte {
            b'\\' => {
                // Skip escaped byte (and for quoted continuations, `\r\n` or `\n`).
                if let Some(frame) = stack.last_mut() {
                    frame.literal_valid = false;
                }
                if arg.kind == ArgKind::Quoted && slice.get(idx + 1..idx + 3) == Some(b"\r\n") {
                    idx += 3;
                } else {
                    idx = (idx + 2).min(slice.len());
                }
            }
            b'$' => {
                let tail = slice.get(idx + 1..).unwrap_or_default();
                let opened = if tail.starts_with(b"{") {
                    Some((VarNamespace::Normal, 2usize))
                } else if tail.starts_with(b"ENV{") {
                    Some((VarNamespace::Env, 5usize))
                } else if tail.starts_with(b"CACHE{") {
                    Some((VarNamespace::Cache, 7usize))
                } else {
                    None
                };
                if let Some((namespace, prefix_len)) = opened {
                    out.has_active_expansion = true;
                    if let Some(parent) = stack.last_mut() {
                        parent.has_nested = true;
                        parent.literal_valid = false;
                    }
                    if stack.len() < MAX_EXPANSION_DEPTH {
                        stack.push(OpenFrame {
                            namespace,
                            full_start: base + idx,
                            name_start: idx + prefix_len,
                            literal_valid: true,
                            has_nested: false,
                        });
                    } else {
                        out.has_dynamic_ref = true;
                    }
                    idx += prefix_len;
                } else {
                    if let Some(frame) = stack.last_mut() {
                        frame.literal_valid = false;
                    }
                    idx += 1;
                }
            }
            b'}' if !stack.is_empty() => {
                if let Some(frame) = stack.pop() {
                    let name_bytes = slice.get(frame.name_start..idx).unwrap_or_default();
                    if frame.literal_valid && !frame.has_nested && !name_bytes.is_empty() {
                        out.refs.push(VarRef {
                            namespace: frame.namespace,
                            name: BString::from(name_bytes),
                            span: frame.full_start..(base + idx + 1),
                        });
                    } else {
                        out.has_dynamic_ref = true;
                    }
                }
                idx += 1;
            }
            _ => {
                if let Some(frame) = stack.last_mut()
                    && !is_var_name_byte(byte)
                {
                    frame.literal_valid = false;
                }
                idx += 1;
            }
        }
    }

    if !stack.is_empty() {
        out.has_dynamic_ref = true;
    }

    out
}

fn is_var_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'.' | b'+' | b'-')
}

/// Role of an unquoted expanding operand inside `if()` / `elseif()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FragileRole {
    /// Operand to a binary comparison/match/list or unary operator.
    ComparisonOrUnary,
    /// Standalone truthiness test such as `if(${VAR})` where quoting changes CMP0054 semantics.
    StandaloneTruthTest,
}

/// Detected fragile unquoted variable expansion inside `if()` / `elseif()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FragileExpansion {
    pub(crate) span: Range<usize>,
    pub(crate) arg_text: BString,
    pub(crate) role: FragileRole,
}

/// Guard information extracted from an `if()` / `elseif()` condition.
#[derive(Clone, Debug, Default)]
pub(crate) struct ConditionAnalysis {
    /// Variables proven `DEFINED` when the condition evaluates to `true`.
    pub(crate) defined_when_true: Vec<BString>,
    /// Variables proven `DEFINED` when the condition evaluates to `false` (for `else()`).
    pub(crate) defined_when_false: Vec<BString>,
    /// `true` if this condition guards an unset/empty `CMAKE_BUILD_TYPE` initialization.
    pub(crate) guards_build_type_init: bool,
    /// `true` if the `true` branch is proven single-config (`NOT <multi_config>` / `NOT CMAKE_CONFIGURATION_TYPES`).
    pub(crate) single_config_when_true: bool,
    /// `true` if the `else` branch is proven single-config (`if(<multi_config>) ... else()`).
    pub(crate) single_config_when_false: bool,
    /// Spans of comparison operands that read `CMAKE_BUILD_TYPE` against a non-empty value.
    pub(crate) build_type_comparisons: Vec<Range<usize>>,
    /// Unquoted expanding operands that are fragile in `if()` / `elseif()`.
    pub(crate) fragile_expansions: Vec<FragileExpansion>,
}

/// Analyze the operand roles, guards, and fragile expansions of an `if()` / `elseif()` call.
pub(crate) fn analyze_if_condition(
    args: &[Argument],
    src: &[u8],
    multi_config_vars: &std::collections::BTreeSet<BString>,
) -> ConditionAnalysis {
    let mut analysis = ConditionAnalysis::default();
    if args.is_empty() {
        return analysis;
    }

    // Track whether the top-level condition has any `OR` at paren depth 0.
    let mut has_top_or = false;
    let mut paren_depth = 0usize;
    for arg in args {
        if arg.kind == ArgKind::Unquoted {
            match arg.text.as_slice() {
                b"(" => paren_depth += 1,
                b")" => paren_depth = paren_depth.saturating_sub(1),
                b"OR" if paren_depth == 0 => has_top_or = true,
                _ => {}
            }
        }
    }

    // Split top-level `AND`-separated clauses (only when there is no top-level `OR`)
    // to extract sound conjunction guards.
    if !has_top_or {
        let mut clause_start = 0usize;
        let mut depth = 0usize;
        for (idx, arg) in args.iter().enumerate() {
            if arg.kind == ArgKind::Unquoted {
                match arg.text.as_slice() {
                    b"(" => depth += 1,
                    b")" => depth = depth.saturating_sub(1),
                    b"AND" if depth == 0 => {
                        if let Some(clause) = args.get(clause_start..idx) {
                            extract_clause_guards(clause, multi_config_vars, &mut analysis);
                        }
                        clause_start = idx + 1;
                    }
                    _ => {}
                }
            }
        }
        if let Some(clause) = args.get(clause_start..) {
            extract_clause_guards(clause, multi_config_vars, &mut analysis);
        }
    } else {
        // Even with top-level OR, a single-clause `if(NOT CMAKE_BUILD_TYPE OR ...)` can still
        // guard build-type init if every disjunct checks CMAKE_BUILD_TYPE unset/empty.
        let mut all_build_type_empty = true;
        let mut clause_start = 0usize;
        let mut depth = 0usize;
        let mut saw_clause = false;
        for (idx, arg) in args.iter().enumerate() {
            if arg.kind == ArgKind::Unquoted {
                match arg.text.as_slice() {
                    b"(" => depth += 1,
                    b")" => depth = depth.saturating_sub(1),
                    b"OR" if depth == 0 => {
                        if let Some(clause) = args.get(clause_start..idx) {
                            saw_clause = true;
                            all_build_type_empty &= is_build_type_unset_clause(clause);
                        }
                        clause_start = idx + 1;
                    }
                    _ => {}
                }
            }
        }
        if let Some(clause) = args.get(clause_start..) {
            saw_clause = true;
            all_build_type_empty &= is_build_type_unset_clause(clause);
        }
        if saw_clause && all_build_type_empty {
            analysis.guards_build_type_init = true;
        }
    }

    // Walk tokens to identify unary/binary operand roles, CMAKE_BUILD_TYPE comparisons,
    // and fragile unquoted expanding operands.
    let mut i = 0usize;
    while i < args.len() {
        let Some(arg) = args.get(i) else {
            break;
        };

        // Skip parentheses and boolean connectives.
        if arg.kind == ArgKind::Unquoted
            && matches!(arg.text.as_slice(), b"(" | b")" | b"AND" | b"OR" | b"NOT")
        {
            i += 1;
            continue;
        }

        // Unary operators (`DEFINED`, `EXISTS`, `TARGET`, etc.).
        if arg.kind == ArgKind::Unquoted && is_unary_op(arg.text.as_slice()) {
            let op = arg.text.as_slice();
            if let Some(operand) = args.get(i + 1) {
                if op != b"DEFINED" {
                    check_fragile(operand, src, FragileRole::ComparisonOrUnary, &mut analysis);
                }
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        // Binary operators (`<left> <BINOP> <right>`).
        if let (Some(op_arg), Some(right)) = (args.get(i + 1), args.get(i + 2))
            && op_arg.kind == ArgKind::Unquoted
            && is_binary_op(op_arg.text.as_slice())
        {
            let left = arg;
            let op = op_arg.text.as_slice();

            // Check single-config-condition on CMAKE_BUILD_TYPE comparisons (excluding empty-string checks).
            let left_is_bt = reads_build_type(left, src);
            let right_is_bt = reads_build_type(right, src);
            let comparing_empty = (left_is_bt && is_empty_string_operand(right))
                || (right_is_bt && is_empty_string_operand(left));
            if !comparing_empty {
                if left_is_bt {
                    analysis.build_type_comparisons.push(left.span.clone());
                } else if right_is_bt && op != b"IN_LIST" {
                    analysis.build_type_comparisons.push(right.span.clone());
                }
            }

            check_fragile(left, src, FragileRole::ComparisonOrUnary, &mut analysis);
            check_fragile(right, src, FragileRole::ComparisonOrUnary, &mut analysis);
            i += 3;
            continue;
        }

        // Standalone operand (truthiness test).
        check_fragile(arg, src, FragileRole::StandaloneTruthTest, &mut analysis);
        i += 1;
    }

    analysis
}

fn check_fragile(arg: &Argument, src: &[u8], role: FragileRole, analysis: &mut ConditionAnalysis) {
    if arg.kind != ArgKind::Unquoted {
        return;
    }
    let exp = scan_arg_expansions(arg, src);
    if exp.has_active_expansion {
        analysis.fragile_expansions.push(FragileExpansion {
            span: arg.span.clone(),
            arg_text: arg.text.clone(),
            role,
        });
    }
}

fn strip_outer_parens(mut clause: &[Argument]) -> &[Argument] {
    while clause.len() >= 2
        && clause
            .first()
            .is_some_and(|a| a.kind == ArgKind::Unquoted && a.text.as_slice() == b"(")
        && clause
            .last()
            .is_some_and(|a| a.kind == ArgKind::Unquoted && a.text.as_slice() == b")")
    {
        // Verify the leading '(' actually matches the trailing ')'.
        let mut depth = 0usize;
        let mut matches_end = true;
        for (idx, a) in clause.iter().enumerate() {
            if a.kind == ArgKind::Unquoted {
                if a.text.as_slice() == b"(" {
                    depth += 1;
                } else if a.text.as_slice() == b")" {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && idx + 1 < clause.len() {
                        matches_end = false;
                        break;
                    }
                }
            }
        }
        if !matches_end || depth != 0 {
            break;
        }
        clause = clause.get(1..clause.len() - 1).unwrap_or(clause);
    }
    clause
}

fn extract_clause_guards(
    clause: &[Argument],
    multi_config_vars: &std::collections::BTreeSet<BString>,
    analysis: &mut ConditionAnalysis,
) {
    let clause = strip_outer_parens(clause);
    if is_build_type_unset_clause(clause) {
        analysis.guards_build_type_init = true;
    }

    match clause {
        // `DEFINED VAR`
        [op, target] if op.kind == ArgKind::Unquoted && op.text.as_slice() == b"DEFINED" => {
            if let Some(name) = extract_defined_var_name(target.text.as_slice()) {
                analysis.defined_when_true.push(name);
            }
        }
        // `NOT DEFINED VAR`
        [not_op, op, target]
            if not_op.kind == ArgKind::Unquoted
                && not_op.text.as_slice() == b"NOT"
                && op.kind == ArgKind::Unquoted
                && op.text.as_slice() == b"DEFINED" =>
        {
            if let Some(name) = extract_defined_var_name(target.text.as_slice()) {
                if name.as_slice() == b"CMAKE_CONFIGURATION_TYPES" {
                    analysis.single_config_when_true = true;
                }
                analysis.defined_when_false.push(name);
            }
        }
        // `NOT VAR`
        [not_op, var]
            if not_op.kind == ArgKind::Unquoted
                && not_op.text.as_slice() == b"NOT"
                && var.kind == ArgKind::Unquoted =>
        {
            let name = var.text.as_slice();
            if name == b"CMAKE_CONFIGURATION_TYPES"
                || name == b"GENERATOR_IS_MULTI_CONFIG"
                || multi_config_vars.contains(name)
            {
                analysis.single_config_when_true = true;
            }
            if is_plain_var_ident(name) {
                analysis.defined_when_false.push(var.text.clone());
            }
        }
        // Bare `VAR`
        [var] if var.kind == ArgKind::Unquoted => {
            let name = var.text.as_slice();
            if name == b"CMAKE_CONFIGURATION_TYPES"
                || name == b"GENERATOR_IS_MULTI_CONFIG"
                || multi_config_vars.contains(name)
            {
                analysis.single_config_when_false = true;
            }
            if is_plain_var_ident(name) {
                analysis.defined_when_true.push(var.text.clone());
            }
        }
        // `CMAKE_GENERATOR STREQUAL "Ninja"` / `"Unix Makefiles"` (not Multi-Config)
        [left, op, right]
            if op.kind == ArgKind::Unquoted
                && op.text.as_slice() == b"STREQUAL"
                && (left.text.as_slice() == b"CMAKE_GENERATOR"
                    || left.text.as_slice() == b"${CMAKE_GENERATOR}")
                && matches!(
                    right.text.as_slice(),
                    b"Ninja" | b"Unix Makefiles" | b"MinGW Makefiles" | b"NMake Makefiles"
                ) =>
        {
            analysis.single_config_when_true = true;
        }
        _ => {}
    }
}

fn is_build_type_unset_clause(clause: &[Argument]) -> bool {
    let clause = strip_outer_parens(clause);
    match clause {
        [not_op, var]
            if not_op.kind == ArgKind::Unquoted
                && not_op.text.as_slice() == b"NOT"
                && (var.text.as_slice() == b"CMAKE_BUILD_TYPE"
                    || var.text.as_slice() == b"${CMAKE_BUILD_TYPE}") =>
        {
            true
        }
        [not_op, def_op, var]
            if not_op.kind == ArgKind::Unquoted
                && not_op.text.as_slice() == b"NOT"
                && def_op.kind == ArgKind::Unquoted
                && def_op.text.as_slice() == b"DEFINED"
                && matches!(
                    var.text.as_slice(),
                    b"CMAKE_BUILD_TYPE" | b"CACHE{CMAKE_BUILD_TYPE}"
                ) =>
        {
            true
        }
        [left, op, right]
            if op.kind == ArgKind::Unquoted
                && op.text.as_slice() == b"STREQUAL"
                && ((is_build_type_token(left) && is_empty_string_operand(right))
                    || (is_build_type_token(right) && is_empty_string_operand(left))) =>
        {
            true
        }
        _ => false,
    }
}

fn is_build_type_token(arg: &Argument) -> bool {
    arg.kind != ArgKind::Bracket
        && (arg.text.as_slice() == b"CMAKE_BUILD_TYPE"
            || arg.text.as_slice() == b"${CMAKE_BUILD_TYPE}"
            || arg.text.as_slice() == b"$CACHE{CMAKE_BUILD_TYPE}")
}

fn reads_build_type(arg: &Argument, src: &[u8]) -> bool {
    if arg.kind == ArgKind::Bracket {
        return false;
    }
    if arg.kind == ArgKind::Unquoted && arg.text.as_slice() == b"CMAKE_BUILD_TYPE" {
        return true;
    }
    let exp = scan_arg_expansions(arg, src);
    exp.refs.iter().any(|r| {
        matches!(r.namespace, VarNamespace::Normal | VarNamespace::Cache)
            && r.name.as_slice() == b"CMAKE_BUILD_TYPE"
    })
}

fn is_empty_string_operand(arg: &Argument) -> bool {
    arg.kind == ArgKind::Quoted && arg.text.is_empty()
}

fn extract_defined_var_name(raw: &[u8]) -> Option<BString> {
    if let Some(rest) = raw
        .strip_prefix(b"CACHE{")
        .and_then(|s| s.strip_suffix(b"}"))
    {
        return (!rest.is_empty()).then(|| BString::from(rest));
    }
    if raw.starts_with(b"ENV{") {
        return None;
    }
    if !raw.is_empty() && raw.iter().all(|&b| is_var_name_byte(b)) {
        Some(BString::from(raw))
    } else {
        None
    }
}

fn is_plain_var_ident(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.iter().all(|&b| is_var_name_byte(b))
        && !matches!(
            bytes,
            b"ON" | b"OFF" | b"TRUE" | b"FALSE" | b"YES" | b"NO" | b"Y" | b"N" | b"0" | b"1"
        )
}

fn is_unary_op(op: &[u8]) -> bool {
    matches!(
        op,
        b"DEFINED"
            | b"COMMAND"
            | b"POLICY"
            | b"DIAGNOSTIC"
            | b"TARGET"
            | b"TEST"
            | b"RULE"
            | b"IS_ABSOLUTE"
            | b"EXISTS"
            | b"IS_READABLE"
            | b"IS_WRITABLE"
            | b"IS_EXECUTABLE"
            | b"IS_DIRECTORY"
            | b"IS_SYMLINK"
    )
}

fn is_binary_op(op: &[u8]) -> bool {
    matches!(
        op,
        b"EQUAL"
            | b"LESS"
            | b"GREATER"
            | b"LESS_EQUAL"
            | b"GREATER_EQUAL"
            | b"STREQUAL"
            | b"STRLESS"
            | b"STRGREATER"
            | b"STRLESS_EQUAL"
            | b"STRGREATER_EQUAL"
            | b"VERSION_EQUAL"
            | b"VERSION_LESS"
            | b"VERSION_GREATER"
            | b"VERSION_LESS_EQUAL"
            | b"VERSION_GREATER_EQUAL"
            | b"MATCHES"
            | b"IN_LIST"
            | b"IS_NEWER_THAN"
            | b"PATH_EQUAL"
            | b"PATH_IS_PREFIX"
    )
}
