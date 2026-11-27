//! Single-pass static scope analyzer, source-order include walker, and rule emitter.
//!
//! Tracks directory, function, macro, block, loop, and conditional scopes with a
//! 4-valued binding lattice (`Definite`, `Possible`, `Absent`, `Unknown`), def-use
//! chains for `file(GLOB)` variables, `CMAKE_BUILD_TYPE` / `GENERATOR_IS_MULTI_CONFIG`
//! guards, `CMP0054` and version state, and bounded static project `include(...)`
//! resolution preferring all open LSP buffers via [`WorkspaceReader`].

#![forbid(unsafe_code)]

use core::ops::Range;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits, version::Version};
use rcmake_diag::source::{SourceKind, SourceMap};
use rcmake_diag::suggest::{Namespace, best_match_in};
use rcmake_syntax::concrete::{ConcreteListFile, parse_concrete_with_budget};
use rcmake_syntax::{ArgKind, CommandInvocation};

use crate::config::LintConfig;
use crate::discovery::normalize_lexical;
use crate::line_index::LineIndex;
use crate::rules_meta::{INVALID_DIRECTIVE_RULE, RuleId};

use super::directives::DirectiveMap;
use super::expr::{FragileRole, VarNamespace, analyze_if_condition, scan_arg_expansions};
use super::rules::{
    check_old_policy_call, classify_add_definitions, is_builtin_or_external_var,
    lookup_obsolete_command, parse_file_glob, parse_version_arg,
};
use super::{Finding, LintDependencies, LintLimits, WorkspaceReader};

/// Abstract binding state for a variable in the 4-valued lattice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingState {
    /// Definitely bound on all paths reaching this point.
    Definite,
    /// Bound on some, but not all, reaching paths.
    Possible,
    /// Proven absent (explicitly `unset()`) on all reaching paths.
    Absent,
    /// State cannot be statically proven (dynamic command/include/variable).
    Unknown,
}

impl BindingState {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Definite, Self::Definite) => Self::Definite,
            (Self::Absent, Self::Absent) => Self::Absent,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            _ => Self::Possible,
        }
    }
}

/// Active `CMP0054` policy status for `fragile-if-expansion` guidance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmp0054State {
    Unknown,
    Old,
    New,
}

/// Proven execution mode of the current file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutionMode {
    Configure,
    Script,
    Unknown,
}

/// Originating `file(GLOB|GLOB_RECURSE)` site without `CONFIGURE_DEPENDS`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GlobOrigin {
    command_index: usize,
    span: Range<usize>,
    var_name: BString,
    is_recurse: bool,
    /// `true` if this glob originates in the primary file being linted (not an include).
    in_primary_file: bool,
}

/// Abstract scope state propagated in source order through commands and includes.
#[derive(Clone, Debug)]
struct ScopeState {
    bindings: BTreeMap<BString, BindingState>,
    /// Variables explicitly `unset(... CACHE)` in this scope.
    unset_cache_vars: BTreeSet<BString>,
    /// Variables assigned by `get_property(... GLOBAL PROPERTY GENERATOR_IS_MULTI_CONFIG)`.
    multi_config_vars: BTreeSet<BString>,
    /// Active unguarded `file(GLOB)` def-use chains per variable name.
    glob_origins: BTreeMap<BString, Vec<GlobOrigin>>,
    /// Statically known `CMAKE_MODULE_PATH` directories.
    module_paths: Vec<PathBuf>,
    /// Effective `PROJECT_SOURCE_DIR` if a `project()` call was seen.
    project_source_dir: Option<PathBuf>,
    /// Active `CMP0054` state.
    cmp0054: Cmp0054State,
    /// Last seen `cmake_minimum_required` / `cmake_policy` effective version.
    effective_version: Option<Version>,
    /// Depth of enclosing single-config generator guards.
    single_config_guard_depth: usize,
    /// Depth of enclosing unset/empty `CMAKE_BUILD_TYPE` guards.
    build_type_guard_depth: usize,
    /// User-defined macro names (invoking them may dynamically bind variables in caller scope).
    defined_macros: BTreeSet<BString>,
}

impl Default for ScopeState {
    fn default() -> Self {
        Self {
            bindings: BTreeMap::new(),
            unset_cache_vars: BTreeSet::new(),
            multi_config_vars: BTreeSet::new(),
            glob_origins: BTreeMap::new(),
            module_paths: Vec::new(),
            project_source_dir: None,
            cmp0054: Cmp0054State::Unknown,
            effective_version: None,
            single_config_guard_depth: 0,
            build_type_guard_depth: 0,
            defined_macros: BTreeSet::new(),
        }
    }
}

impl ScopeState {
    fn set_var(&mut self, name: BString, state: BindingState, max_vars: usize) {
        if self.bindings.len() < max_vars || self.bindings.contains_key(&name) {
            self.bindings.insert(name.clone(), state);
        }
        // Overwriting or unsetting a variable kills any prior unguarded glob origin on it!
        self.glob_origins.remove(&name);
    }

    fn set_definite(&mut self, name: impl Into<BString>, max_vars: usize) {
        let name = name.into();
        self.unset_cache_vars.remove(&name);
        self.set_var(name, BindingState::Definite, max_vars);
    }

    fn set_absent(&mut self, name: impl Into<BString>, max_vars: usize) {
        self.set_var(name.into(), BindingState::Absent, max_vars);
    }

    /// After an unresolved dynamic effect (`set(${X} ...)`, unresolved `include()`,
    /// `cmake_parse_arguments` with dynamic prefix, macro call, etc.), any `Absent`
    /// variable may now have been bound dynamically, so widen `Absent` to `Unknown`.
    fn widen_absent_to_unknown(&mut self) {
        for state in self.bindings.values_mut() {
            if *state == BindingState::Absent {
                *state = BindingState::Unknown;
            }
        }
        self.unset_cache_vars.clear();
    }

    fn visible_bound_names(&self) -> Vec<&[u8]> {
        self.bindings
            .iter()
            .filter_map(|(k, v)| {
                matches!(v, BindingState::Definite | BindingState::Possible).then_some(k.as_slice())
            })
            .collect()
    }

    fn join_branches(branches: &[Self], max_vars: usize) -> Self {
        let Some(first) = branches.first() else {
            return Self::default();
        };
        if branches.len() == 1 {
            return first.clone();
        }

        let mut all_keys = BTreeSet::new();
        for branch in branches {
            for key in branch.bindings.keys() {
                if all_keys.len() >= max_vars {
                    break;
                }
                all_keys.insert(key.clone());
            }
        }

        let mut joined = first.clone();
        joined.bindings.clear();
        for key in all_keys {
            let mut acc: Option<BindingState> = None;
            for branch in branches {
                let st = branch
                    .bindings
                    .get(&key)
                    .copied()
                    .unwrap_or(BindingState::Unknown);
                acc = Some(match acc {
                    None => st,
                    Some(prev) => prev.join(st),
                });
            }
            if let Some(st) = acc {
                joined.bindings.insert(key, st);
            }
        }

        // Unset cache vars: only still unset if unset on ALL branches.
        joined.unset_cache_vars = first
            .unset_cache_vars
            .iter()
            .filter(|k| {
                branches
                    .iter()
                    .skip(1)
                    .all(|b| b.unset_cache_vars.contains(*k))
            })
            .cloned()
            .collect();

        // Multi-config vars: union (conservatively recognize guard vars bound on any branch).
        for branch in branches.iter().skip(1) {
            joined
                .multi_config_vars
                .extend(branch.multi_config_vars.iter().cloned());
            joined
                .defined_macros
                .extend(branch.defined_macros.iter().cloned());
        }

        // Glob origins: union across reaching branches (if any path reaches a target with an
        // unguarded glob without overwriting it, the def-use chain is still live; if ALL
        // branches overwrite the variable, none contain it so it is cleared!).
        let mut joined_globs: BTreeMap<BString, Vec<GlobOrigin>> = BTreeMap::new();
        for branch in branches {
            for (var, origins) in &branch.glob_origins {
                let slot = joined_globs.entry(var.clone()).or_default();
                for origin in origins {
                    if !slot.contains(origin) && slot.len() < 8 {
                        slot.push(origin.clone());
                    }
                }
            }
        }
        joined.glob_origins = joined_globs;

        // CMP0054: keep New only if New on all branches.
        if !branches.iter().all(|b| b.cmp0054 == Cmp0054State::New) {
            if branches.iter().all(|b| b.cmp0054 == Cmp0054State::Old) {
                joined.cmp0054 = Cmp0054State::Old;
            } else {
                joined.cmp0054 = Cmp0054State::Unknown;
            }
        }

        joined
    }
}

enum ControlFrame {
    If {
        pre_state: ScopeState,
        completed_branches: Vec<ScopeState>,
        has_else: bool,
        /// Negated guards from the opening `if` for `else()`.
        else_defined: Vec<BString>,
        else_single_config: bool,
    },
    Loop {
        pre_state: ScopeState,
    },
    Function {
        pre_state: ScopeState,
    },
    Macro {
        pre_state: ScopeState,
    },
    Block {
        pre_state: ScopeState,
        isolate_vars: bool,
        isolate_policies: bool,
        propagate_vars: Vec<BString>,
    },
}

pub(crate) struct Analyzer<'a> {
    primary_path: &'a Path,
    primary_src: &'a [u8],
    primary_lines: LineIndex,
    primary_directives: DirectiveMap,
    config: &'a LintConfig,
    limits: &'a LintLimits,
    reader: &'a dyn WorkspaceReader,
    workspace_root: Option<&'a Path>,
    state: ScopeState,
    mode: ExecutionMode,
    findings: Vec<Finding>,
    emitted_glob_sites: BTreeSet<usize>,
    dependencies: LintDependencies,
    active_includes: BTreeSet<PathBuf>,
    include_files_read: usize,
    include_bytes_read: usize,
}

impl<'a> Analyzer<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        primary_path: &'a Path,
        primary_src: &'a [u8],
        concrete: &ConcreteListFile,
        config: &'a LintConfig,
        limits: &'a LintLimits,
        reader: &'a dyn WorkspaceReader,
        workspace_root: Option<&'a Path>,
    ) -> Self {
        let mode = infer_initial_mode(primary_path, &concrete.list_file.commands);
        let primary_directives = DirectiveMap::parse(primary_src, concrete);
        let primary_lines = LineIndex::new(primary_src);
        Self {
            primary_path,
            primary_src,
            primary_lines,
            primary_directives,
            config,
            limits,
            reader,
            workspace_root,
            state: ScopeState::default(),
            mode,
            findings: Vec::new(),
            emitted_glob_sites: BTreeSet::new(),
            dependencies: LintDependencies::default(),
            active_includes: BTreeSet::new(),
            include_files_read: 0,
            include_bytes_read: 0,
        }
    }

    pub(crate) fn run(mut self, concrete: &ConcreteListFile) -> (Vec<Finding>, LintDependencies) {
        // Emit any unknown/malformed `# rcmake-lint:` directive diagnostics first.
        for issue in &self.primary_directives.issues {
            let (start, end) = self
                .primary_lines
                .span_coords(self.primary_src, issue.span.clone());
            self.findings.push(Finding {
                rule: INVALID_DIRECTIVE_RULE.id,
                severity: INVALID_DIRECTIVE_RULE.default_severity,
                source: "rcmake-lint",
                path: self.primary_path.to_path_buf(),
                primary_span: issue.span.clone(),
                line: start.line,
                column: start.column,
                end_line: end.line,
                end_column: end.column,
                message: issue.message.clone(),
                suggestion: issue.suggestion.clone(),
            });
        }

        let canonical_primary = normalize_lexical(self.primary_path);
        self.active_includes.insert(canonical_primary);
        self.walk_commands(
            self.primary_path,
            self.primary_src,
            &concrete.list_file.commands,
            true,
            0,
        );
        (self.findings, self.dependencies)
    }

    fn emit(
        &mut self,
        command_index: usize,
        rule: RuleId,
        span: Range<usize>,
        message: String,
        suggestion: Option<String>,
    ) {
        if !self.config.is_enabled(rule) {
            return;
        }
        if self.primary_directives.is_suppressed(command_index, rule) {
            return;
        }
        if self.findings.len() >= self.limits.max_findings {
            return;
        }
        let severity = self.config.effective_severity(rule);
        let (start, end) = self
            .primary_lines
            .span_coords(self.primary_src, span.clone());
        self.findings.push(Finding {
            rule: rule.as_str(),
            severity,
            source: "rcmake-lint",
            path: self.primary_path.to_path_buf(),
            primary_span: span,
            line: start.line,
            column: start.column,
            end_line: end.line,
            end_column: end.column,
            message,
            suggestion,
        });
    }

    fn walk_commands(
        &mut self,
        file_path: &Path,
        src: &[u8],
        commands: &[CommandInvocation],
        is_primary: bool,
        include_depth: usize,
    ) {
        let mut frames: Vec<ControlFrame> = Vec::new();
        let max_vars = self.limits.max_vars;

        for (cmd_idx, cmd) in commands.iter().enumerate() {
            let lower = cmd.lower.as_slice();

            // 1. Check variable references in expansion positions for `undefined-variable`
            //    (before updating scope with this command's own outputs!).
            if is_primary {
                self.check_undefined_variables(cmd_idx, cmd, src);
            }

            // 2. Check target source consumers for active unguarded `file(GLOB)` def-use chains.
            self.check_glob_consumers(cmd, src);

            // 3. Handle flow-control scope transitions & condition rules.
            match lower {
                b"if" => {
                    let cond = analyze_if_condition(&cmd.args, src, &self.state.multi_config_vars);
                    if is_primary {
                        self.emit_condition_rules(cmd_idx, &cond);
                    }
                    let pre_state = self.state.clone();
                    for name in &cond.defined_when_true {
                        self.state.set_definite(name.clone(), max_vars);
                    }
                    if cond.single_config_when_true {
                        self.state.single_config_guard_depth += 1;
                    }
                    if cond.guards_build_type_init {
                        self.state.build_type_guard_depth += 1;
                    }
                    frames.push(ControlFrame::If {
                        pre_state,
                        completed_branches: Vec::new(),
                        has_else: false,
                        else_defined: cond.defined_when_false,
                        else_single_config: cond.single_config_when_false,
                    });
                    continue;
                }
                b"elseif" => {
                    if let Some(ControlFrame::If {
                        pre_state,
                        completed_branches,
                        else_defined,
                        else_single_config,
                        ..
                    }) = frames.last_mut()
                    {
                        completed_branches.push(self.state.clone());
                        self.state = pre_state.clone();
                        // Negate prior branch guarantees that could become invalid on elseif.
                        else_defined.clear();
                        *else_single_config = false;
                    }
                    let cond = analyze_if_condition(&cmd.args, src, &self.state.multi_config_vars);
                    if is_primary {
                        self.emit_condition_rules(cmd_idx, &cond);
                    }
                    for name in &cond.defined_when_true {
                        self.state.set_definite(name.clone(), max_vars);
                    }
                    if cond.single_config_when_true {
                        self.state.single_config_guard_depth += 1;
                    }
                    if cond.guards_build_type_init {
                        self.state.build_type_guard_depth += 1;
                    }
                    continue;
                }
                b"else" => {
                    if let Some(ControlFrame::If {
                        pre_state,
                        completed_branches,
                        has_else,
                        else_defined,
                        else_single_config,
                    }) = frames.last_mut()
                    {
                        completed_branches.push(self.state.clone());
                        *has_else = true;
                        self.state = pre_state.clone();
                        for name in else_defined.iter() {
                            self.state.set_definite(name.clone(), max_vars);
                        }
                        if *else_single_config {
                            self.state.single_config_guard_depth += 1;
                        }
                    }
                    continue;
                }
                b"endif" => {
                    if let Some(ControlFrame::If {
                        pre_state,
                        mut completed_branches,
                        has_else,
                        ..
                    }) = frames.pop()
                    {
                        let base_sc = pre_state.single_config_guard_depth;
                        let base_bt = pre_state.build_type_guard_depth;
                        completed_branches.push(self.state.clone());
                        if !has_else {
                            completed_branches.push(pre_state);
                        }
                        self.state = ScopeState::join_branches(&completed_branches, max_vars);
                        self.state.single_config_guard_depth = base_sc;
                        self.state.build_type_guard_depth = base_bt;
                    }
                    continue;
                }
                b"foreach" => {
                    let pre_state = self.state.clone();
                    bind_foreach_vars(cmd, &mut self.state, max_vars);
                    frames.push(ControlFrame::Loop { pre_state });
                    continue;
                }
                b"while" => {
                    let pre_state = self.state.clone();
                    frames.push(ControlFrame::Loop { pre_state });
                    continue;
                }
                b"endforeach" | b"endwhile" => {
                    if let Some(ControlFrame::Loop { pre_state }) = frames.pop() {
                        let branches = [pre_state, self.state.clone()];
                        self.state = ScopeState::join_branches(&branches, max_vars);
                    }
                    continue;
                }
                b"function" => {
                    let pre_state = self.state.clone();
                    bind_callable_params(cmd, true, &mut self.state, max_vars);
                    frames.push(ControlFrame::Function { pre_state });
                    continue;
                }
                b"endfunction" => {
                    if let Some(ControlFrame::Function { pre_state }) = frames.pop() {
                        self.state = pre_state;
                    }
                    continue;
                }
                b"macro" => {
                    if let Some(name) = cmd.args.first()
                        && !name.text.is_empty()
                    {
                        self.state
                            .defined_macros
                            .insert(BString::from(name.text.as_slice().to_ascii_lowercase()));
                    }
                    let pre_state = self.state.clone();
                    bind_callable_params(cmd, false, &mut self.state, max_vars);
                    frames.push(ControlFrame::Macro { pre_state });
                    continue;
                }
                b"endmacro" => {
                    if let Some(ControlFrame::Macro { pre_state }) = frames.pop() {
                        self.state = pre_state;
                    }
                    continue;
                }
                b"block" => {
                    let (isolate_vars, isolate_policies, propagate_vars) = parse_block_options(cmd);
                    let pre_state = self.state.clone();
                    frames.push(ControlFrame::Block {
                        pre_state,
                        isolate_vars,
                        isolate_policies,
                        propagate_vars,
                    });
                    continue;
                }
                b"endblock" => {
                    if let Some(ControlFrame::Block {
                        mut pre_state,
                        isolate_vars,
                        isolate_policies,
                        propagate_vars,
                    }) = frames.pop()
                    {
                        if !isolate_vars {
                            pre_state.bindings = self.state.bindings.clone();
                            pre_state.unset_cache_vars = self.state.unset_cache_vars.clone();
                            pre_state.glob_origins = self.state.glob_origins.clone();
                            pre_state.multi_config_vars = self.state.multi_config_vars.clone();
                        } else {
                            for var in &propagate_vars {
                                if let Some(&st) = self.state.bindings.get(var) {
                                    pre_state.bindings.insert(var.clone(), st);
                                }
                                if let Some(origins) = self.state.glob_origins.get(var) {
                                    pre_state.glob_origins.insert(var.clone(), origins.clone());
                                } else {
                                    pre_state.glob_origins.remove(var);
                                }
                            }
                        }
                        if !isolate_policies {
                            pre_state.cmp0054 = self.state.cmp0054;
                            pre_state.effective_version = self.state.effective_version.clone();
                        }
                        self.state = pre_state;
                    }
                    continue;
                }
                _ => {}
            }

            // 4. Command-specific lint checks (only emitted for the primary file).
            if is_primary {
                self.check_command_rules(cmd_idx, cmd);
            }

            // 5. Transfer command effects into `self.state` in source order (including static includes).
            self.apply_command_effects(
                file_path,
                cmd_idx,
                cmd,
                src,
                is_primary,
                include_depth,
                &mut frames,
            );
        }
    }

    fn check_undefined_variables(&mut self, cmd_idx: usize, cmd: &CommandInvocation, src: &[u8]) {
        if !self.config.is_enabled(RuleId::UndefinedVariable) {
            return;
        }
        // Fast path: skip if no argument contains `$`.
        if !cmd
            .args
            .iter()
            .any(|a| a.kind != ArgKind::Bracket && a.text.as_slice().contains(&b'$'))
        {
            return;
        }

        let mut visible_cache: Option<Vec<BString>> = None;

        for arg in &cmd.args {
            if arg.kind == ArgKind::Bracket || !arg.text.as_slice().contains(&b'$') {
                continue;
            }
            let expansions = scan_arg_expansions(arg, src);
            for var_ref in expansions.refs {
                match var_ref.namespace {
                    VarNamespace::Env => continue,
                    VarNamespace::Cache => {
                        if self.state.unset_cache_vars.contains(&var_ref.name) {
                            let display = String::from_utf8_lossy(var_ref.name.as_slice());
                            self.emit(
                                cmd_idx,
                                RuleId::UndefinedVariable,
                                var_ref.span,
                                format!(
                                    "Cache variable '{display}' is referenced after being unset."
                                ),
                                None,
                            );
                        }
                        continue;
                    }
                    VarNamespace::Normal => {}
                }

                let name = var_ref.name.as_slice();
                match self.state.bindings.get(name).copied() {
                    Some(
                        BindingState::Definite | BindingState::Possible | BindingState::Unknown,
                    ) => {
                        continue;
                    }
                    Some(BindingState::Absent) => {
                        let display = String::from_utf8_lossy(name);
                        self.emit(
                            cmd_idx,
                            RuleId::UndefinedVariable,
                            var_ref.span,
                            format!("Variable '{display}' is referenced after being unset."),
                            Some(format!("Assign '{display}' before referencing it or guard with if(DEFINED {display}).")),
                        );
                        continue;
                    }
                    None => {}
                }

                if is_builtin_or_external_var(name) {
                    continue;
                }

                // Bare missing name in open scope is insufficient; emit ONLY when there is a
                // unique nearby typo match among visible bound variables!
                let candidates = visible_cache.get_or_insert_with(|| {
                    self.state
                        .visible_bound_names()
                        .into_iter()
                        .map(BString::from)
                        .collect()
                });
                if candidates.is_empty() {
                    continue;
                }
                if let Some((best, _)) = best_match_in(Namespace::Name, name, candidates.iter()) {
                    let display = String::from_utf8_lossy(name);
                    let best_str = String::from_utf8_lossy(best.as_slice());
                    self.emit(
                        cmd_idx,
                        RuleId::UndefinedVariable,
                        var_ref.span,
                        format!(
                            "Variable '{display}' is not defined in this scope; did you mean '{best_str}'?"
                        ),
                        Some(format!("Replace '{display}' with '{best_str}'.")),
                    );
                }
            }
        }
    }

    fn emit_condition_rules(&mut self, cmd_idx: usize, cond: &super::expr::ConditionAnalysis) {
        // Rule: single-config-condition
        if self.state.single_config_guard_depth == 0 && !cond.single_config_when_true {
            for span in &cond.build_type_comparisons {
                self.emit(
                    cmd_idx,
                    RuleId::SingleConfigCondition,
                    span.clone(),
                    "Condition inspects CMAKE_BUILD_TYPE directly, which is empty on multi-config generators.".into(),
                    Some("Use a $<CONFIG:...> generator expression for target settings, or guard with GENERATOR_IS_MULTI_CONFIG.".into()),
                );
            }
        }

        // Rule: fragile-if-expansion
        for fragile in &cond.fragile_expansions {
            let text = String::from_utf8_lossy(fragile.arg_text.as_slice());
            match fragile.role {
                FragileRole::ComparisonOrUnary => {
                    let (msg, suggestion) = match self.state.cmp0054 {
                        Cmp0054State::New => (
                            format!(
                                "Unquoted expanding operand '{text}' in condition can break arity if empty or a list, or auto-dereference its expanded value."
                            ),
                            Some(format!(
                                "Quote the operand as \"{text}\" (safe under CMP0054 NEW), or pass a bare variable name when comparing a single variable."
                            )),
                        ),
                        Cmp0054State::Old | Cmp0054State::Unknown => (
                            format!(
                                "Unquoted expanding operand '{text}' in condition can break arity if empty or a list; quoting alone still auto-dereferences unless CMP0054 is NEW."
                            ),
                            None,
                        ),
                    };
                    self.emit(
                        cmd_idx,
                        RuleId::FragileIfExpansion,
                        fragile.span.clone(),
                        msg,
                        suggestion,
                    );
                }
                FragileRole::StandaloneTruthTest => {
                    // Quoting `if(${VAR})` to `if("${VAR}")` can change intended dereferencing
                    // semantics under CMP0054 NEW, so explain the ambiguity and omit an automatic fix.
                    self.emit(
                        cmd_idx,
                        RuleId::FragileIfExpansion,
                        fragile.span.clone(),
                        format!(
                            "Unquoted expanding operand '{text}' in truthiness test is ambiguous: empty/list values alter arity, the expanded text may be re-dereferenced, and quoting changes semantics under CMP0054."
                        ),
                        None,
                    );
                }
            }
        }
    }

    fn check_glob_consumers(&mut self, cmd: &CommandInvocation, src: &[u8]) {
        if self.mode == ExecutionMode::Script {
            return;
        }
        let lower = cmd.lower.as_slice();
        let is_target_source_consumer = matches!(
            lower,
            b"add_executable" | b"add_library" | b"target_sources"
        ) || (lower == b"set_property"
            && cmd
                .args
                .first()
                .is_some_and(|a| a.text.as_slice() == b"TARGET")
            && cmd.args.iter().any(|a| a.text.as_slice() == b"SOURCES"));

        if !is_target_source_consumer {
            return;
        }

        // Skip target name (arg 0) for add_executable/add_library/target_sources.
        let start_arg = usize::from(matches!(
            lower,
            b"add_executable" | b"add_library" | b"target_sources"
        ));
        let mut triggered: Vec<GlobOrigin> = Vec::new();
        for arg in cmd.args.iter().skip(start_arg) {
            let exp = scan_arg_expansions(arg, src);
            for var_ref in exp.refs {
                if var_ref.namespace == VarNamespace::Normal
                    && let Some(origins) = self.state.glob_origins.get(&var_ref.name)
                {
                    triggered.extend(origins.iter().cloned());
                }
            }
        }

        for origin in triggered {
            if !origin.in_primary_file {
                continue;
            }
            if !self.emitted_glob_sites.insert(origin.command_index) {
                continue;
            }
            let glob_kind = if origin.is_recurse {
                "file(GLOB_RECURSE)"
            } else {
                "file(GLOB)"
            };
            let var_name = String::from_utf8_lossy(origin.var_name.as_slice());
            self.emit(
                origin.command_index,
                RuleId::GlobWithoutConfigureDepends,
                origin.span,
                format!(
                    "{glob_kind} variable '{var_name}' feeds a build target without CONFIGURE_DEPENDS."
                ),
                Some(format!(
                    "List source files explicitly, or add CONFIGURE_DEPENDS to {glob_kind}."
                )),
            );
        }
    }

    fn check_command_rules(&mut self, cmd_idx: usize, cmd: &CommandInvocation) {
        let lower = cmd.lower.as_slice();

        // Rule: obsolete-command (table of deprecated/removed commands)
        if let Some(info) = lookup_obsolete_command(lower) {
            let name_span = cmd.span.start..(cmd.span.start + cmd.name.len());
            let message = if let Some((rm_maj, rm_min)) = info.removed_in {
                format!(
                    "Command '{}' was deprecated in CMake {}.{} and removed in CMake {}.{}.",
                    info.name, info.deprecated_in.0, info.deprecated_in.1, rm_maj, rm_min
                )
            } else {
                format!(
                    "Command '{}' is deprecated since CMake {}.{}.",
                    info.name, info.deprecated_in.0, info.deprecated_in.1
                )
            };
            self.emit(
                cmd_idx,
                RuleId::ObsoleteCommand,
                name_span,
                message,
                Some(info.suggestion.to_owned()),
            );
        }

        // Rule: obsolete-command (`cmake_minimum_required` / `cmake_policy` version check)
        if matches!(lower, b"cmake_minimum_required" | b"cmake_policy")
            && let Some(range) = parse_version_arg(cmd)
        {
            let v3_5 = Version::from_tuple(3, 5, 0);
            let v3_10 = Version::from_tuple(3, 10, 0);
            let effective = range.effective_policy_version().clone();
            let span = range.span;
            if effective < v3_5 {
                self.emit(
                    cmd_idx,
                    RuleId::ObsoleteCommand,
                    span,
                    format!(
                        "Effective policy version {effective} is below 3.5; compatibility with CMake < 3.5 was removed in CMake 4.0."
                    ),
                    Some("Raise the VERSION (or policy max via min...max) to at least 3.5 (3.10+ recommended).".into()),
                );
            } else if effective < v3_10 {
                self.emit(
                    cmd_idx,
                    RuleId::ObsoleteCommand,
                    span,
                    format!(
                        "Effective policy version {effective} is below 3.10; compatibility with CMake < 3.10 is deprecated."
                    ),
                    Some("Raise the VERSION (or policy max via min...max) to at least 3.10.".into()),
                );
            } else if range.min < v3_5 {
                self.emit(
                    cmd_idx,
                    RuleId::ObsoleteCommand,
                    span,
                    format!(
                        "Minimum version {} is below 3.5, although effective policy max {effective} remains supported in CMake 4.x.",
                        range.min
                    ),
                    Some(format!(
                        "Consider raising the minimum version from {} to at least 3.5.",
                        range.min
                    )),
                );
            }
        }

        // Rule: old-policy (`cmake_policy(SET CMPnnnn OLD)`)
        if let Some(found) = check_old_policy_call(cmd) {
            let id = found.id;
            let span = found.span;
            let removed_old = found.removed_old;
            let introduced = found.introduced;
            let version_note = self
                .state
                .effective_version
                .as_ref()
                .map(|v| format!(" (active policy version {v})"))
                .unwrap_or_default();
            let message = if removed_old {
                format!(
                    "The OLD behavior for policy {id} (introduced in CMake {}.{}.{}) was removed in CMake 4.0{version_note}.",
                    introduced.0, introduced.1, introduced.2
                )
            } else {
                format!(
                    "Setting policy {id} (introduced in CMake {}.{}.{}) to OLD is deprecated{version_note}.",
                    introduced.0, introduced.1, introduced.2
                )
            };
            self.emit(
                cmd_idx,
                RuleId::OldPolicy,
                span,
                message,
                Some(format!(
                    "Migrate code to {id} NEW behavior and remove cmake_policy(SET {id} OLD)."
                )),
            );
        }

        // Rule: directory-options (only in proven project configuration scopes)
        if self.mode == ExecutionMode::Configure {
            let name_span = cmd.span.start..(cmd.span.start + cmd.name.len());
            match lower {
                b"include_directories" => {
                    self.emit(
                        cmd_idx,
                        RuleId::DirectoryOptions,
                        name_span,
                        "include_directories() applies directory-wide include paths to all targets in this directory and subdirectories.".into(),
                        Some("Use target_include_directories(<target> PRIVATE|PUBLIC|INTERFACE ...) instead.".into()),
                    );
                }
                b"link_libraries" => {
                    self.emit(
                        cmd_idx,
                        RuleId::DirectoryOptions,
                        name_span,
                        "link_libraries() links libraries directory-wide into all subsequently created targets.".into(),
                        Some("Use target_link_libraries(<target> PRIVATE|PUBLIC|INTERFACE ...) instead.".into()),
                    );
                }
                b"link_directories" => {
                    self.emit(
                        cmd_idx,
                        RuleId::DirectoryOptions,
                        name_span,
                        "link_directories() adds directory-wide linker search paths for all subsequent targets.".into(),
                        Some("Use target_link_directories(<target> PRIVATE|PUBLIC|INTERFACE ...) or full library targets instead.".into()),
                    );
                }
                b"add_definitions" => {
                    let cls = classify_add_definitions(cmd);
                    let suggestion = match (cls.has_definitions, cls.has_other_flags) {
                        (true, true) => {
                            "Split preprocessor definitions (-D/-U) into target_compile_definitions() and other compiler flags into target_compile_options()."
                        }
                        (false, true) => {
                            "Use target_compile_options(<target> PRIVATE|PUBLIC|INTERFACE ...) for non-definition compiler flags."
                        }
                        _ => {
                            "Use target_compile_definitions(<target> PRIVATE|PUBLIC|INTERFACE ...) instead."
                        }
                    };
                    self.emit(
                        cmd_idx,
                        RuleId::DirectoryOptions,
                        name_span,
                        "add_definitions() applies directory-wide definitions and flags to all targets in scope.".into(),
                        Some(suggestion.to_owned()),
                    );
                }
                _ => {}
            }
        }

        // Rule: unguarded-build-type
        if self.mode != ExecutionMode::Script
            && lower == b"set"
            && let Some(first) = cmd.args.first()
            && matches!(
                first.text.as_slice(),
                b"CMAKE_BUILD_TYPE" | b"CACHE{CMAKE_BUILD_TYPE}"
            )
            && self.state.build_type_guard_depth == 0
        {
            let is_cache =
                first.text.as_slice() == b"CACHE{CMAKE_BUILD_TYPE}" || has_cache_clause(cmd);
            let is_force = cmd
                .args
                .last()
                .is_some_and(|a| a.text.as_slice() == b"FORCE")
                || (first.text.as_slice() == b"CACHE{CMAKE_BUILD_TYPE}"
                    && cmd.args.iter().any(|a| a.text.as_slice() == b"FORCE"));
            if !is_cache {
                self.emit(
                    cmd_idx,
                    RuleId::UnguardedBuildType,
                    cmd.span.clone(),
                    "Unconditional normal assignment to CMAKE_BUILD_TYPE masks the cached build type.".into(),
                    Some("Guard default initialization with if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES) and use CACHE STRING.".into()),
                );
            } else if is_force {
                self.emit(
                    cmd_idx,
                    RuleId::UnguardedBuildType,
                    cmd.span.clone(),
                    "Unconditional set(CMAKE_BUILD_TYPE ... CACHE ... FORCE) overwrites the caller's build type.".into(),
                    Some("Remove FORCE so caller-supplied -DCMAKE_BUILD_TYPE is preserved, or guard with if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES).".into()),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_command_effects(
        &mut self,
        file_path: &Path,
        cmd_idx: usize,
        cmd: &CommandInvocation,
        src: &[u8],
        is_primary: bool,
        include_depth: usize,
        frames: &mut [ControlFrame],
    ) {
        let lower = cmd.lower.as_slice();
        let max_vars = self.limits.max_vars;

        match lower {
            b"cmake_minimum_required" | b"cmake_policy" => {
                if let Some(range) = parse_version_arg(cmd) {
                    let eff = range.effective_policy_version().clone();
                    if eff >= Version::from_tuple(3, 1, 0) {
                        self.state.cmp0054 = Cmp0054State::New;
                    }
                    self.state.effective_version = Some(eff);
                } else if lower == b"cmake_policy"
                    && cmd
                        .args
                        .first()
                        .is_some_and(|a| a.text.as_slice() == b"SET")
                    && cmd
                        .args
                        .get(1)
                        .is_some_and(|a| a.text.as_slice() == b"CMP0054")
                    && let Some(mode) = cmd.args.get(2)
                {
                    match mode.text.as_slice() {
                        b"NEW" => self.state.cmp0054 = Cmp0054State::New,
                        b"OLD" => self.state.cmp0054 = Cmp0054State::Old,
                        _ => {}
                    }
                } else if lower == b"cmake_policy"
                    && cmd
                        .args
                        .first()
                        .is_some_and(|a| a.text.as_slice() == b"GET")
                    && let Some(out) = cmd.args.get(2)
                {
                    bind_output_arg(out, &mut self.state, max_vars);
                }
            }
            b"project" => {
                self.mode = ExecutionMode::Configure;
                if let Some(parent) = file_path.parent() {
                    self.state.project_source_dir = Some(parent.to_owned());
                }
                if let Some(name_arg) = cmd.args.first()
                    && !name_arg.text.is_empty()
                    && !name_arg.text.as_slice().contains(&b'$')
                {
                    let proj = name_arg.text.as_slice();
                    for suffix in [
                        b"_SOURCE_DIR".as_slice(),
                        b"_BINARY_DIR",
                        b"_IS_TOP_LEVEL",
                        b"_VERSION",
                        b"_VERSION_MAJOR",
                        b"_VERSION_MINOR",
                        b"_VERSION_PATCH",
                        b"_VERSION_TWEAK",
                        b"_DESCRIPTION",
                        b"_HOMEPAGE_URL",
                    ] {
                        let mut full = Vec::with_capacity(proj.len() + suffix.len());
                        full.extend_from_slice(proj);
                        full.extend_from_slice(suffix);
                        self.state.set_definite(BString::from(full), max_vars);
                    }
                }
            }
            b"add_executable" | b"add_library" | b"add_subdirectory" | b"enable_language" => {
                self.mode = ExecutionMode::Configure;
            }
            b"set" => {
                let Some(var_arg) = cmd.args.first() else {
                    return;
                };
                let raw = var_arg.text.as_slice();
                if raw.starts_with(b"ENV{") {
                    return;
                }
                if raw.contains(&b'$') {
                    self.state.widen_absent_to_unknown();
                    return;
                }
                let is_parent_scope = cmd
                    .args
                    .last()
                    .is_some_and(|a| a.text.as_slice() == b"PARENT_SCOPE");
                let is_cache = raw.starts_with(b"CACHE{") || has_cache_clause(cmd);
                let name = if let Some(inner) = raw
                    .strip_prefix(b"CACHE{")
                    .and_then(|s| s.strip_suffix(b"}"))
                {
                    BString::from(inner)
                } else {
                    var_arg.text.clone()
                };

                // Propagate glob taint if RHS references an active glob variable (and this isn't
                // an empty unset-like `set(VAR)`).
                let value_end = if is_parent_scope {
                    cmd.args.len().saturating_sub(1)
                } else {
                    cmd.args.len()
                };
                let value_args = cmd.args.get(1..value_end).unwrap_or_default();
                let mut copied_globs = Vec::new();
                if !is_cache {
                    for val in value_args {
                        let exp = scan_arg_expansions(val, src);
                        for r in exp.refs {
                            if r.namespace == VarNamespace::Normal
                                && let Some(origins) = self.state.glob_origins.get(&r.name)
                            {
                                copied_globs.extend(origins.iter().cloned());
                            }
                        }
                    }
                }

                // Track CMAKE_MODULE_PATH updates for static include resolution.
                if name.as_slice() == b"CMAKE_MODULE_PATH" && !is_parent_scope {
                    update_module_paths(
                        &mut self.state.module_paths,
                        file_path,
                        self.workspace_root,
                        self.state.project_source_dir.as_deref(),
                        value_args,
                        false,
                    );
                }

                if is_parent_scope {
                    // Apply to the nearest enclosing function/block parent snapshot if any.
                    if let Some(parent_state) = nearest_parent_scope_state(frames) {
                        if value_args.is_empty() {
                            parent_state.set_absent(name, max_vars);
                        } else {
                            parent_state.set_definite(name.clone(), max_vars);
                            if !copied_globs.is_empty() {
                                parent_state.glob_origins.insert(name, copied_globs);
                            }
                        }
                    }
                    return;
                }

                // `set(VAR)` with no value arguments unsets `VAR` in CMake!
                if !is_cache && value_args.is_empty() {
                    self.state.set_absent(name, max_vars);
                } else {
                    self.state.set_definite(name.clone(), max_vars);
                    if !copied_globs.is_empty() {
                        self.state.glob_origins.insert(name, copied_globs);
                    }
                }
            }
            b"unset" => {
                let Some(var_arg) = cmd.args.first() else {
                    return;
                };
                let raw = var_arg.text.as_slice();
                if raw.starts_with(b"ENV{") || raw.contains(&b'$') {
                    return;
                }
                let is_cache = raw.starts_with(b"CACHE{")
                    || cmd
                        .args
                        .get(1)
                        .is_some_and(|a| a.text.as_slice() == b"CACHE");
                let is_parent_scope = cmd
                    .args
                    .get(1)
                    .is_some_and(|a| a.text.as_slice() == b"PARENT_SCOPE");
                let name = if let Some(inner) = raw
                    .strip_prefix(b"CACHE{")
                    .and_then(|s| s.strip_suffix(b"}"))
                {
                    BString::from(inner)
                } else {
                    var_arg.text.clone()
                };
                if is_parent_scope {
                    if let Some(parent_state) = nearest_parent_scope_state(frames) {
                        parent_state.set_absent(name, max_vars);
                    }
                } else {
                    if is_cache {
                        self.state.unset_cache_vars.insert(name.clone());
                    }
                    self.state.set_absent(name, max_vars);
                }
            }
            b"option" => {
                if let Some(arg) = cmd.args.first() {
                    bind_output_arg(arg, &mut self.state, max_vars);
                }
            }
            b"file" => {
                if let Some(glob) = parse_file_glob(cmd) {
                    self.state.set_definite(glob.output_var.clone(), max_vars);
                    if !glob.has_configure_depends {
                        self.state.glob_origins.insert(
                            glob.output_var.clone(),
                            vec![GlobOrigin {
                                command_index: cmd_idx,
                                span: cmd.span.clone(),
                                var_name: glob.output_var,
                                is_recurse: glob.is_recurse,
                                in_primary_file: is_primary,
                            }],
                        );
                    }
                } else {
                    bind_file_output(cmd, &mut self.state, max_vars);
                }
            }
            b"list" => {
                if let (Some(sub), Some(var_arg)) = (cmd.args.first(), cmd.args.get(1)) {
                    let sub_bytes = sub.text.as_slice();
                    if matches!(sub_bytes, b"APPEND" | b"PREPEND")
                        && var_arg.text.as_slice() == b"CMAKE_MODULE_PATH"
                    {
                        update_module_paths(
                            &mut self.state.module_paths,
                            file_path,
                            self.workspace_root,
                            self.state.project_source_dir.as_deref(),
                            cmd.args.get(2..).unwrap_or_default(),
                            true,
                        );
                    }
                    if matches!(sub_bytes, b"APPEND" | b"PREPEND" | b"INSERT")
                        && !var_arg.text.as_slice().contains(&b'$')
                    {
                        // Propagate glob taint if appending a globbed variable into another list.
                        let mut added_globs = Vec::new();
                        for arg in cmd.args.iter().skip(2) {
                            let exp = scan_arg_expansions(arg, src);
                            for r in exp.refs {
                                if r.namespace == VarNamespace::Normal
                                    && let Some(origins) = self.state.glob_origins.get(&r.name)
                                {
                                    added_globs.extend(origins.iter().cloned());
                                }
                            }
                        }
                        if self.state.bindings.len() < max_vars
                            || self.state.bindings.contains_key(&var_arg.text)
                        {
                            self.state
                                .bindings
                                .insert(var_arg.text.clone(), BindingState::Definite);
                        }
                        if !added_globs.is_empty() {
                            let entry = self
                                .state
                                .glob_origins
                                .entry(var_arg.text.clone())
                                .or_default();
                            for origin in added_globs {
                                if !entry.contains(&origin) && entry.len() < 8 {
                                    entry.push(origin);
                                }
                            }
                        }
                    } else if matches!(
                        sub_bytes,
                        b"LENGTH" | b"GET" | b"JOIN" | b"SUBLIST" | b"FIND"
                    ) && let Some(out) = cmd.args.last()
                    {
                        bind_output_arg(out, &mut self.state, max_vars);
                    } else if sub_bytes == b"TRANSFORM"
                        && let Some(pos) = cmd
                            .args
                            .iter()
                            .position(|a| a.text.as_slice() == b"OUTPUT_VARIABLE")
                        && let Some(out) = cmd.args.get(pos + 1)
                    {
                        bind_output_arg(out, &mut self.state, max_vars);
                    }
                }
            }
            b"get_property" => {
                if let Some(out) = cmd.args.first() {
                    bind_output_arg(out, &mut self.state, max_vars);
                    if cmd
                        .args
                        .get(1)
                        .is_some_and(|a| a.text.as_slice() == b"GLOBAL")
                        && cmd
                            .args
                            .iter()
                            .any(|a| a.text.as_slice() == b"GENERATOR_IS_MULTI_CONFIG")
                        && !out.text.as_slice().contains(&b'$')
                    {
                        self.state.multi_config_vars.insert(out.text.clone());
                    }
                }
            }
            b"get_cmake_property" => {
                if let Some(out) = cmd.args.first() {
                    bind_output_arg(out, &mut self.state, max_vars);
                    if cmd
                        .args
                        .get(1)
                        .is_some_and(|a| a.text.as_slice() == b"GENERATOR_IS_MULTI_CONFIG")
                        && !out.text.as_slice().contains(&b'$')
                    {
                        self.state.multi_config_vars.insert(out.text.clone());
                    }
                }
            }
            b"get_target_property"
            | b"get_directory_property"
            | b"get_source_file_property"
            | b"get_test_property"
            | b"get_filename_component"
            | b"separate_arguments"
            | b"find_library"
            | b"find_path"
            | b"find_file"
            | b"find_program"
            | b"site_name"
            | b"try_compile" => {
                if let Some(out) = cmd.args.first() {
                    bind_output_arg(out, &mut self.state, max_vars);
                }
            }
            b"try_run" => {
                if let Some(a) = cmd.args.first() {
                    bind_output_arg(a, &mut self.state, max_vars);
                }
                if let Some(b) = cmd.args.get(1) {
                    bind_output_arg(b, &mut self.state, max_vars);
                }
            }
            b"math" => {
                if cmd
                    .args
                    .first()
                    .is_some_and(|a| a.text.as_slice() == b"EXPR")
                    && let Some(out) = cmd.args.get(1)
                {
                    bind_output_arg(out, &mut self.state, max_vars);
                }
            }
            b"string" => {
                bind_string_output(cmd, &mut self.state, max_vars);
            }
            b"cmake_path" => {
                bind_cmake_path_output(cmd, &mut self.state, max_vars);
            }
            b"cmake_host_system_information" => {
                if let Some(pos) = cmd.args.iter().position(|a| a.text.as_slice() == b"RESULT")
                    && let Some(out) = cmd.args.get(pos + 1)
                {
                    bind_output_arg(out, &mut self.state, max_vars);
                }
            }
            b"execute_process" => {
                for kw in [
                    b"RESULT_VARIABLE".as_slice(),
                    b"RESULTS_VARIABLE",
                    b"OUTPUT_VARIABLE",
                    b"ERROR_VARIABLE",
                ] {
                    if let Some(pos) = cmd.args.iter().position(|a| a.text.as_slice() == kw)
                        && let Some(out) = cmd.args.get(pos + 1)
                    {
                        bind_output_arg(out, &mut self.state, max_vars);
                    }
                }
            }
            b"find_package" => {
                if let Some(pkg) = cmd.args.first()
                    && !pkg.text.is_empty()
                    && !pkg.text.as_slice().contains(&b'$')
                {
                    let name = pkg.text.as_slice();
                    let upper = name.to_ascii_uppercase();
                    for prefix in [name, upper.as_slice()] {
                        for suffix in [
                            b"_FOUND".as_slice(),
                            b"_DIR",
                            b"_VERSION",
                            b"_INCLUDE_DIRS",
                            b"_INCLUDE_DIR",
                            b"_LIBRARIES",
                            b"_LIBRARY",
                            b"_DEFINITIONS",
                            b"_ROOT",
                        ] {
                            let mut full = Vec::with_capacity(prefix.len() + suffix.len());
                            full.extend_from_slice(prefix);
                            full.extend_from_slice(suffix);
                            self.state.set_definite(BString::from(full), max_vars);
                        }
                    }
                }
                self.state.widen_absent_to_unknown();
            }
            b"cmake_parse_arguments" => {
                bind_parse_arguments(cmd, &mut self.state, max_vars);
            }
            b"include" => {
                self.handle_include(file_path, cmd, include_depth);
            }
            _ => {
                // Common Check* module functions place their output variable last.
                if (lower.starts_with(b"check_") || lower.starts_with(b"cmake_check_"))
                    && let Some(last) = cmd.args.last()
                {
                    bind_output_arg(last, &mut self.state, max_vars);
                } else if self.state.defined_macros.contains(lower) {
                    // User-defined macro executes in caller scope.
                    self.state.widen_absent_to_unknown();
                }
            }
        }
    }

    fn handle_include(&mut self, file_path: &Path, cmd: &CommandInvocation, include_depth: usize) {
        let max_vars = self.limits.max_vars;
        // Bind RESULT_VARIABLE if requested.
        if let Some(pos) = cmd
            .args
            .iter()
            .position(|a| a.text.as_slice() == b"RESULT_VARIABLE")
            && let Some(out) = cmd.args.get(pos + 1)
        {
            bind_output_arg(out, &mut self.state, max_vars);
        }

        let Some(target_arg) = cmd.args.first() else {
            return;
        };
        let is_optional = cmd
            .args
            .iter()
            .skip(1)
            .any(|a| a.text.as_slice() == b"OPTIONAL");
        let no_policy_scope = cmd
            .args
            .iter()
            .skip(1)
            .any(|a| a.text.as_slice() == b"NO_POLICY_SCOPE");

        // Resolve statically known include path(s).
        let Some(candidates) = self.resolve_include_candidates(file_path, target_arg) else {
            // Dynamic include path: mark unresolved effects as unknown.
            self.state.widen_absent_to_unknown();
            return;
        };

        let mut chosen: Option<PathBuf> = None;
        for cand in candidates {
            let norm = normalize_lexical(&cand);
            // Record every probed candidate in `dependencies.included_files` so creating a
            // shadowing include file later also triggers LSP refresh!
            if self.dependencies.included_files.len() < self.limits.max_include_files * 4 {
                self.dependencies.included_files.insert(norm.clone());
            }
            if self.reader.file_exists(&norm) {
                chosen = Some(norm);
                break;
            }
        }

        let Some(inc_path) = chosen else {
            // Unresolved project/module include: widen Absent -> Unknown.
            self.state.widen_absent_to_unknown();
            return;
        };

        // Bundled CMake standard modules: do not parse whole stdlib on every keystroke;
        // treat as external/known effects instead.
        if self
            .reader
            .bundled_modules_dir()
            .is_some_and(|dir| inc_path.starts_with(dir))
        {
            self.state.widen_absent_to_unknown();
            return;
        }

        // Enforce cycle, depth, file count, and total byte limits.
        if include_depth >= self.limits.max_include_depth
            || self.include_files_read >= self.limits.max_include_files
            || self.include_bytes_read >= self.limits.max_include_bytes
            || self.active_includes.contains(&inc_path)
        {
            self.state.widen_absent_to_unknown();
            return;
        }

        let remaining_bytes = self
            .limits
            .max_include_bytes
            .saturating_sub(self.include_bytes_read);
        let Some(inc_bytes) = self.reader.read_file(&inc_path, remaining_bytes as u64) else {
            self.state.widen_absent_to_unknown();
            return;
        };

        self.include_files_read += 1;
        self.include_bytes_read = self.include_bytes_read.saturating_add(inc_bytes.len());

        let mut sources = SourceMap::default();
        let Some(source_id) = sources.add("".into(), SourceKind::File, Arc::from(b"".as_slice()))
        else {
            self.state.widen_absent_to_unknown();
            return;
        };
        let parse_limits = Limits {
            max_file_read: remaining_bytes as u64,
            ..Limits::default()
        };
        let Ok(parsed) = parse_concrete_with_budget(
            &inc_bytes,
            source_id,
            SourceKind::File,
            &parse_limits,
            100_000,
        ) else {
            self.state.widen_absent_to_unknown();
            return;
        };

        let saved_state_for_optional = is_optional.then(|| self.state.clone());
        let saved_cmp0054 = self.state.cmp0054;
        let saved_version = self.state.effective_version.clone();

        self.active_includes.insert(inc_path.clone());
        self.walk_commands(
            &inc_path,
            &inc_bytes,
            &parsed.list_file.commands,
            false,
            include_depth + 1,
        );
        self.active_includes.remove(&inc_path);

        if !no_policy_scope {
            self.state.cmp0054 = saved_cmp0054;
            self.state.effective_version = saved_version;
        }

        if let Some(before) = saved_state_for_optional {
            let branches = [before, self.state.clone()];
            self.state = ScopeState::join_branches(&branches, max_vars);
        }
    }

    fn resolve_include_candidates(
        &self,
        current_file: &Path,
        arg: &rcmake_syntax::Argument,
    ) -> Option<Vec<PathBuf>> {
        let name = core::str::from_utf8(arg.text.as_slice()).ok()?;
        let literal = resolve_literal_path(
            name,
            arg.kind,
            current_file,
            self.workspace_root,
            self.state.project_source_dir.as_deref(),
        )?;
        let mut out = Vec::new();
        if literal.is_absolute() {
            out.push(literal);
            return Some(out);
        }

        // Direct path relative to current listfile's directory (or source dir).
        let current_dir = current_file.parent().unwrap_or(current_file);
        if name.ends_with(".cmake") || name.contains('/') {
            out.push(current_dir.join(&literal));
            if let Some(root) = self.workspace_root
                && root != current_dir
            {
                out.push(root.join(&literal));
            }
        } else {
            // Module name: check CMAKE_MODULE_PATH, then bundled modules, then direct file.
            let mut module_file = literal.as_os_str().to_owned();
            module_file.push(".cmake");
            for dir in &self.state.module_paths {
                out.push(dir.join(&module_file));
            }
            if let Some(bundled) = self.reader.bundled_modules_dir() {
                out.push(bundled.join(&module_file));
            }
            out.push(current_dir.join(&literal));
        }
        Some(out)
    }
}

fn infer_initial_mode(path: &Path, commands: &[CommandInvocation]) -> ExecutionMode {
    if path
        .file_name()
        .is_some_and(|name| name == "CMakeLists.txt")
    {
        return ExecutionMode::Configure;
    }
    if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n == "cmake_install.cmake" || n == "CTestTestfile.cmake" || n.ends_with(".ctest")
    }) {
        return ExecutionMode::Script;
    }
    let has_configure_cmds = commands.iter().any(|cmd| {
        matches!(
            cmd.lower.as_slice(),
            b"project"
                | b"add_executable"
                | b"add_library"
                | b"add_subdirectory"
                | b"enable_language"
                | b"target_sources"
                | b"target_link_libraries"
                | b"target_include_directories"
        )
    });
    if has_configure_cmds {
        ExecutionMode::Configure
    } else {
        ExecutionMode::Unknown
    }
}

fn nearest_parent_scope_state(frames: &mut [ControlFrame]) -> Option<&mut ScopeState> {
    for frame in frames.iter_mut().rev() {
        match frame {
            ControlFrame::Function { pre_state } => return Some(pre_state),
            ControlFrame::Block {
                pre_state,
                isolate_vars: true,
                ..
            } => return Some(pre_state),
            _ => {}
        }
    }
    None
}

fn has_cache_clause(cmd: &CommandInvocation) -> bool {
    let len = cmd.args.len();
    let force = usize::from(
        cmd.args
            .last()
            .is_some_and(|a| a.text.as_slice() == b"FORCE"),
    );
    len >= 5 + force
        && cmd
            .args
            .get(len - 3 - force)
            .is_some_and(|a| a.text.as_slice() == b"CACHE")
}

fn bind_output_arg(arg: &rcmake_syntax::Argument, state: &mut ScopeState, max_vars: usize) {
    if arg.text.is_empty() {
        return;
    }
    if arg.text.as_slice().contains(&b'$') {
        state.widen_absent_to_unknown();
    } else {
        state.set_definite(arg.text.clone(), max_vars);
    }
}

fn bind_foreach_vars(cmd: &CommandInvocation, state: &mut ScopeState, max_vars: usize) {
    let Some(first) = cmd.args.first() else {
        return;
    };
    bind_output_arg(first, state, max_vars);
    // Handle `foreach(v1 v2 ... IN ZIP_LISTS ...)`.
    if let Some(in_pos) = cmd.args.iter().position(|a| a.text.as_slice() == b"IN")
        && cmd
            .args
            .get(in_pos + 1)
            .is_some_and(|a| a.text.as_slice() == b"ZIP_LISTS")
    {
        for var in cmd.args.iter().take(in_pos) {
            bind_output_arg(var, state, max_vars);
        }
    }
}

fn bind_callable_params(
    cmd: &CommandInvocation,
    is_function: bool,
    state: &mut ScopeState,
    max_vars: usize,
) {
    for param in cmd.args.iter().skip(1) {
        if !param.text.is_empty() && !param.text.as_slice().contains(&b'$') {
            state.set_definite(param.text.clone(), max_vars);
        }
    }
    for builtin in [b"ARGC".as_slice(), b"ARGV", b"ARGN"] {
        state.set_definite(BString::from(builtin), max_vars);
    }
    if is_function {
        for builtin in [
            b"CMAKE_CURRENT_FUNCTION".as_slice(),
            b"CMAKE_CURRENT_FUNCTION_LIST_DIR",
            b"CMAKE_CURRENT_FUNCTION_LIST_FILE",
            b"CMAKE_CURRENT_FUNCTION_LIST_LINE",
        ] {
            state.set_definite(BString::from(builtin), max_vars);
        }
    }
}

fn parse_block_options(cmd: &CommandInvocation) -> (bool, bool, Vec<BString>) {
    let mut saw_scope_for = false;
    let mut isolate_vars = false;
    let mut isolate_policies = false;
    let mut propagate = Vec::new();
    let mut in_scope_for = false;
    let mut in_propagate = false;

    for arg in &cmd.args {
        match arg.text.as_slice() {
            b"SCOPE_FOR" => {
                saw_scope_for = true;
                in_scope_for = true;
                in_propagate = false;
            }
            b"PROPAGATE" => {
                in_scope_for = false;
                in_propagate = true;
            }
            b"VARIABLES" if in_scope_for => isolate_vars = true,
            b"POLICIES" if in_scope_for => isolate_policies = true,
            b"DIAGNOSTICS" if in_scope_for => {}
            other if in_propagate && !other.is_empty() && !other.contains(&b'$') => {
                propagate.push(BString::from(other));
            }
            _ => {}
        }
    }

    if !saw_scope_for {
        isolate_vars = true;
        isolate_policies = true;
    }
    (isolate_vars, isolate_policies, propagate)
}

fn bind_file_output(cmd: &CommandInvocation, state: &mut ScopeState, max_vars: usize) {
    let Some(sub) = cmd.args.first().map(|a| a.text.as_slice()) else {
        return;
    };
    let out_index = match sub {
        b"READ" | b"STRINGS" | b"MD5" | b"SHA1" | b"SHA224" | b"SHA256" | b"SHA384" | b"SHA512"
        | b"SHA3_224" | b"SHA3_256" | b"SHA3_384" | b"SHA3_512" | b"SIZE" | b"READ_SYMLINK"
        | b"REAL_PATH" | b"TIMESTAMP" => Some(2usize),
        b"RELATIVE_PATH" | b"TO_CMAKE_PATH" | b"TO_NATIVE_PATH" => Some(1usize),
        _ => None,
    };
    if let Some(idx) = out_index
        && let Some(arg) = cmd.args.get(idx)
    {
        bind_output_arg(arg, state, max_vars);
    }
    if sub == b"GET_RUNTIME_DEPENDENCIES" {
        for kw in [
            b"RESOLVED_DEPENDENCIES_VAR".as_slice(),
            b"UNRESOLVED_DEPENDENCIES_VAR",
            b"CONFLICTING_DEPENDENCIES_PREFIX",
        ] {
            if let Some(pos) = cmd.args.iter().position(|a| a.text.as_slice() == kw)
                && let Some(out) = cmd.args.get(pos + 1)
            {
                bind_output_arg(out, state, max_vars);
            }
        }
    }
}

fn bind_string_output(cmd: &CommandInvocation, state: &mut ScopeState, max_vars: usize) {
    let Some(sub) = cmd.args.first().map(|a| a.text.as_slice()) else {
        return;
    };
    let out_idx = match sub {
        b"APPEND" | b"PREPEND" | b"CONCAT" | b"TIMESTAMP" | b"UUID" | b"JSON" => Some(1usize),
        b"TOLOWER" | b"TOUPPER" | b"LENGTH" | b"STRIP" | b"GENEX_STRIP" | b"MAKE_C_IDENTIFIER"
        | b"HEX" | b"CONFIGURE" | b"JOIN" | b"MD5" | b"SHA1" | b"SHA224" | b"SHA256"
        | b"SHA384" | b"SHA512" | b"SHA3_224" | b"SHA3_256" | b"SHA3_384" | b"SHA3_512" => {
            Some(2usize)
        }
        b"FIND" | b"REPEAT" | b"REPLACE" => Some(3usize),
        b"SUBSTRING" | b"COMPARE" => Some(4usize),
        b"REGEX" => match cmd.args.get(1).map(|a| a.text.as_slice()) {
            Some(b"MATCH" | b"MATCHALL") => Some(3usize),
            Some(b"REPLACE") => Some(4usize),
            _ => None,
        },
        b"RANDOM" => cmd.args.len().checked_sub(1),
        _ => None,
    };
    if let Some(idx) = out_idx
        && let Some(arg) = cmd.args.get(idx)
    {
        bind_output_arg(arg, state, max_vars);
    }
}

fn bind_cmake_path_output(cmd: &CommandInvocation, state: &mut ScopeState, max_vars: usize) {
    let Some(sub) = cmd.args.first().map(|a| a.text.as_slice()) else {
        return;
    };
    if let Some(pos) = cmd
        .args
        .iter()
        .position(|a| a.text.as_slice() == b"OUTPUT_VARIABLE")
        && let Some(out) = cmd.args.get(pos + 1)
    {
        bind_output_arg(out, state, max_vars);
        return;
    }
    match sub {
        b"SET" | b"APPEND" | b"APPEND_STRING" | b"REMOVE_FILENAME" | b"REPLACE_FILENAME"
        | b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION" | b"NORMAL_PATH" => {
            if let Some(arg) = cmd.args.get(1) {
                bind_output_arg(arg, state, max_vars);
            }
        }
        b"GET" | b"COMPARE" | b"HASH" | b"RELATIVE_PATH" | b"ABSOLUTE_PATH" | b"NATIVE_PATH"
        | b"CONVERT" => {
            if let Some(arg) = cmd.args.last() {
                bind_output_arg(arg, state, max_vars);
            }
        }
        s if s.starts_with(b"IS_") || s.starts_with(b"HAS_") => {
            if let Some(arg) = cmd.args.last() {
                bind_output_arg(arg, state, max_vars);
            }
        }
        _ => {}
    }
}

fn bind_parse_arguments(cmd: &CommandInvocation, state: &mut ScopeState, max_vars: usize) {
    let offset = usize::from(
        cmd.args
            .first()
            .is_some_and(|a| a.text.as_slice() == b"PARSE_ARGV"),
    ) * 2;
    let Some(prefix_arg) = cmd.args.get(offset) else {
        state.widen_absent_to_unknown();
        return;
    };
    if prefix_arg.text.is_empty() || prefix_arg.text.as_slice().contains(&b'$') {
        state.widen_absent_to_unknown();
        return;
    }
    let prefix = prefix_arg.text.as_slice();
    for suffix in [b"UNPARSED_ARGUMENTS".as_slice(), b"KEYWORDS_MISSING_VALUES"] {
        let mut full = Vec::with_capacity(prefix.len() + 1 + suffix.len());
        full.extend_from_slice(prefix);
        full.push(b'_');
        full.extend_from_slice(suffix);
        state.set_definite(BString::from(full), max_vars);
    }
    for group_idx in [offset + 1, offset + 2, offset + 3] {
        let Some(group) = cmd.args.get(group_idx) else {
            continue;
        };
        if group.text.as_slice().contains(&b'$') {
            state.widen_absent_to_unknown();
            continue;
        }
        for key in group.text.as_slice().split(|&b| b == b';') {
            if !key.is_empty() {
                let mut full = Vec::with_capacity(prefix.len() + 1 + key.len());
                full.extend_from_slice(prefix);
                full.push(b'_');
                full.extend_from_slice(key);
                state.set_definite(BString::from(full), max_vars);
            }
        }
    }
}

fn update_module_paths(
    paths: &mut Vec<PathBuf>,
    current_file: &Path,
    workspace_root: Option<&Path>,
    project_source_dir: Option<&Path>,
    args: &[rcmake_syntax::Argument],
    append: bool,
) {
    if !append {
        paths.clear();
    }
    for arg in args {
        let Ok(text) = core::str::from_utf8(arg.text.as_slice()) else {
            continue;
        };
        for part in text.split(';').filter(|p| !p.is_empty()) {
            if paths.len() >= 32 {
                return;
            }
            if let Some(resolved) = resolve_literal_path(
                part,
                arg.kind,
                current_file,
                workspace_root,
                project_source_dir,
            ) {
                let full = if resolved.is_absolute() {
                    resolved
                } else {
                    current_file.parent().unwrap_or(current_file).join(resolved)
                };
                paths.push(normalize_lexical(&full));
            }
        }
    }
}

fn resolve_literal_path(
    name: &str,
    kind: ArgKind,
    current_file: &Path,
    workspace_root: Option<&Path>,
    project_source_dir: Option<&Path>,
) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['\0', '\\', ';']) {
        return None;
    }
    if kind == ArgKind::Bracket || !name.contains('$') {
        return Some(PathBuf::from(name));
    }
    let current_dir = current_file.parent();
    for (var, base) in [
        ("${CMAKE_CURRENT_LIST_DIR}", current_dir),
        ("${CMAKE_CURRENT_SOURCE_DIR}", current_dir),
        ("${CMAKE_SOURCE_DIR}", workspace_root),
        (
            "${PROJECT_SOURCE_DIR}",
            project_source_dir.or(workspace_root),
        ),
    ] {
        if let Some(tail) = name.strip_prefix(var)
            && (tail.is_empty() || tail.starts_with('/'))
            && !tail.contains('$')
        {
            return Some(base?.join(tail.trim_start_matches('/')));
        }
    }
    None
}
