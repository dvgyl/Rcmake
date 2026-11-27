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
use super::{Finding, LintDependencies, LintLimits, LintResult, WorkspaceReader};

const MAX_PROPAGATE_VARS: usize = 256;
const MAX_ELSE_DEFINED: usize = 64;
const MAX_GLOB_ORIGINS_PER_VAR: usize = 8;
const MAX_MODULE_PATHS: usize = 32;

#[derive(Clone, Copy, Debug)]
struct ScopeBudget {
    max_vars: usize,
    max_entries: usize,
    max_bytes: usize,
}

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Cmp0054State {
    #[default]
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
#[derive(Clone, Debug, Default)]
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
    /// Tracked retained entries across all collections in this scope.
    entries: usize,
    /// Tracked retained bytes across all collections in this scope.
    bytes: usize,
}

impl ScopeState {
    fn usage(&self) -> (usize, usize) {
        (self.entries, self.bytes)
    }

    fn shallow_scalars(&self) -> Self {
        Self {
            project_source_dir: self.project_source_dir.clone(),
            cmp0054: self.cmp0054,
            effective_version: self.effective_version.clone(),
            single_config_guard_depth: self.single_config_guard_depth,
            build_type_guard_depth: self.build_type_guard_depth,
            ..Self::default()
        }
    }

    fn can_fit(&self, added_entries: usize, added_bytes: usize, budget: ScopeBudget) -> bool {
        self.entries.saturating_add(added_entries) <= budget.max_entries
            && self.bytes.saturating_add(added_bytes) <= budget.max_bytes
    }

    fn insert_binding(
        &mut self,
        name: BString,
        state: BindingState,
        budget: ScopeBudget,
        truncated: &mut bool,
    ) {
        if let Some(existing) = self.bindings.get_mut(&name) {
            *existing = state;
            return;
        }
        let added_bytes = name.len();
        if self.bindings.len() < budget.max_vars && self.can_fit(1, added_bytes, budget) {
            self.bindings.insert(name, state);
            self.entries = self.entries.saturating_add(1);
            self.bytes = self.bytes.saturating_add(added_bytes);
        } else {
            *truncated = true;
        }
    }

    fn set_var(
        &mut self,
        name: BString,
        state: BindingState,
        budget: ScopeBudget,
        truncated: &mut bool,
    ) {
        // Overwriting or unsetting a variable kills any prior unguarded glob origin on it.
        self.remove_glob_origins(&name);
        self.insert_binding(name, state, budget, truncated);
    }

    fn set_definite(
        &mut self,
        name: impl Into<BString>,
        budget: ScopeBudget,
        truncated: &mut bool,
    ) {
        let name = name.into();
        self.remove_unset_cache(&name);
        self.set_var(name, BindingState::Definite, budget, truncated);
    }

    fn set_absent(&mut self, name: impl Into<BString>, budget: ScopeBudget, truncated: &mut bool) {
        self.set_var(name.into(), BindingState::Absent, budget, truncated);
    }

    fn insert_unset_cache(&mut self, name: BString, budget: ScopeBudget, truncated: &mut bool) {
        if self.unset_cache_vars.contains(&name) {
            return;
        }
        let added_bytes = name.len();
        if self.can_fit(1, added_bytes, budget) {
            self.unset_cache_vars.insert(name);
            self.entries = self.entries.saturating_add(1);
            self.bytes = self.bytes.saturating_add(added_bytes);
        } else {
            *truncated = true;
        }
    }

    fn remove_unset_cache(&mut self, name: &BString) {
        if self.unset_cache_vars.remove(name) {
            self.entries = self.entries.saturating_sub(1);
            self.bytes = self.bytes.saturating_sub(name.len());
        }
    }

    fn clear_unset_cache(&mut self) {
        let count = self.unset_cache_vars.len();
        let removed_bytes: usize = self.unset_cache_vars.iter().map(|k| k.len()).sum();
        self.unset_cache_vars.clear();
        self.entries = self.entries.saturating_sub(count);
        self.bytes = self.bytes.saturating_sub(removed_bytes);
    }

    fn insert_multi_config(&mut self, name: BString, budget: ScopeBudget, truncated: &mut bool) {
        if self.multi_config_vars.contains(&name) {
            return;
        }
        let added_bytes = name.len();
        if self.can_fit(1, added_bytes, budget) {
            self.multi_config_vars.insert(name);
            self.entries = self.entries.saturating_add(1);
            self.bytes = self.bytes.saturating_add(added_bytes);
        } else {
            *truncated = true;
        }
    }

    fn insert_defined_macro(&mut self, name: BString, budget: ScopeBudget, truncated: &mut bool) {
        if self.defined_macros.contains(&name) {
            return;
        }
        let added_bytes = name.len();
        if self.can_fit(1, added_bytes, budget) {
            self.defined_macros.insert(name);
            self.entries = self.entries.saturating_add(1);
            self.bytes = self.bytes.saturating_add(added_bytes);
        } else {
            *truncated = true;
        }
    }

    fn remove_glob_origins(&mut self, name: &BString) {
        if let Some(removed) = self.glob_origins.remove(name) {
            let count = removed.len();
            let origin_bytes: usize = removed.iter().map(|o| o.var_name.len()).sum();
            self.entries = self.entries.saturating_sub(count);
            self.bytes = self
                .bytes
                .saturating_sub(name.len().saturating_add(origin_bytes));
        }
    }

    fn clear_glob_origins(&mut self) {
        let mut count = 0usize;
        let mut removed_bytes = 0usize;
        for (k, origins) in &self.glob_origins {
            count = count.saturating_add(origins.len());
            removed_bytes = removed_bytes.saturating_add(k.len());
            for origin in origins {
                removed_bytes = removed_bytes.saturating_add(origin.var_name.len());
            }
        }
        self.glob_origins.clear();
        self.entries = self.entries.saturating_sub(count);
        self.bytes = self.bytes.saturating_sub(removed_bytes);
    }

    fn add_glob_origin(
        &mut self,
        var: BString,
        origin: GlobOrigin,
        budget: ScopeBudget,
        truncated: &mut bool,
    ) {
        if let Some(slot) = self.glob_origins.get(&var) {
            if slot.contains(&origin) || slot.len() >= MAX_GLOB_ORIGINS_PER_VAR {
                return;
            }
            let added_bytes = origin.var_name.len();
            if self.can_fit(1, added_bytes, budget) {
                if let Some(slot_mut) = self.glob_origins.get_mut(&var) {
                    slot_mut.push(origin);
                    self.entries = self.entries.saturating_add(1);
                    self.bytes = self.bytes.saturating_add(added_bytes);
                }
            } else {
                *truncated = true;
            }
        } else {
            let added_bytes = var.len().saturating_add(origin.var_name.len());
            if self.can_fit(1, added_bytes, budget) {
                self.glob_origins.insert(var, vec![origin]);
                self.entries = self.entries.saturating_add(1);
                self.bytes = self.bytes.saturating_add(added_bytes);
            } else {
                *truncated = true;
            }
        }
    }

    fn clear_module_paths(&mut self) {
        let count = self.module_paths.len();
        let removed_bytes: usize = self.module_paths.iter().map(|p| p.as_os_str().len()).sum();
        self.module_paths.clear();
        self.entries = self.entries.saturating_sub(count);
        self.bytes = self.bytes.saturating_sub(removed_bytes);
    }

    fn push_module_path(
        &mut self,
        path: PathBuf,
        budget: ScopeBudget,
        truncated: &mut bool,
    ) -> bool {
        if self.module_paths.len() >= MAX_MODULE_PATHS {
            return false;
        }
        let added_bytes = path.as_os_str().len();
        if self.can_fit(1, added_bytes, budget) {
            self.module_paths.push(path);
            self.entries = self.entries.saturating_add(1);
            self.bytes = self.bytes.saturating_add(added_bytes);
            true
        } else {
            *truncated = true;
            false
        }
    }

    /// Conservatively invalidate scope facts after an unresolved dynamic effect
    /// (`set(${X} ...)`, unresolved `include()`, macro invocation, etc.).
    ///
    /// Any `Absent` variable may now have been bound dynamically, and any active
    /// `file(GLOB)` variable may have been overwritten, so widen `Absent -> Unknown`
    /// and clear tracked unset-cache and glob-origin facts.
    fn widen_unknown_scope_effect(&mut self) {
        for state in self.bindings.values_mut() {
            if *state == BindingState::Absent {
                *state = BindingState::Unknown;
            }
        }
        self.clear_unset_cache();
        self.clear_glob_origins();
    }

    /// Conservatively invalidate only bindings/globs under `prefix_` (for
    /// `cmake_parse_arguments` with a known prefix and dynamic keyword list).
    fn widen_prefix_scope_effect(&mut self, prefix_with_underscore: &[u8]) {
        for (k, state) in &mut self.bindings {
            if k.as_slice().starts_with(prefix_with_underscore) && *state == BindingState::Absent {
                *state = BindingState::Unknown;
            }
        }
        let drop_cache: Vec<BString> = self
            .unset_cache_vars
            .iter()
            .filter(|k| k.as_slice().starts_with(prefix_with_underscore))
            .cloned()
            .collect();
        for k in drop_cache {
            self.remove_unset_cache(&k);
        }
        let drop_globs: Vec<BString> = self
            .glob_origins
            .keys()
            .filter(|k| k.as_slice().starts_with(prefix_with_underscore))
            .cloned()
            .collect();
        for k in drop_globs {
            self.remove_glob_origins(&k);
        }
    }

    fn visible_bound_names(&self) -> Vec<&[u8]> {
        self.bindings
            .iter()
            .filter_map(|(k, v)| {
                matches!(v, BindingState::Definite | BindingState::Possible).then_some(k.as_slice())
            })
            .collect()
    }

    /// Incrementally join two reaching branch states under `budget`.
    fn join_two(left: Self, right: Self, budget: ScopeBudget, truncated: &mut bool) -> Self {
        let mut joined = Self {
            project_source_dir: left.project_source_dir,
            cmp0054: if left.cmp0054 == right.cmp0054 {
                left.cmp0054
            } else {
                Cmp0054State::Unknown
            },
            effective_version: left.effective_version,
            single_config_guard_depth: left.single_config_guard_depth,
            build_type_guard_depth: left.build_type_guard_depth,
            ..Self::default()
        };

        for path in left.module_paths {
            joined.push_module_path(path, budget, truncated);
        }

        for (key, st_left) in left.bindings {
            let st_right = right
                .bindings
                .get(&key)
                .copied()
                .unwrap_or(BindingState::Unknown);
            joined.insert_binding(key, st_left.join(st_right), budget, truncated);
        }
        for (key, st_right) in right.bindings {
            if !joined.bindings.contains_key(&key) {
                joined.insert_binding(key, BindingState::Unknown.join(st_right), budget, truncated);
            }
        }

        // Unset cache vars stay unset only when unset on both branches.
        for key in left.unset_cache_vars {
            if right.unset_cache_vars.contains(&key) {
                joined.insert_unset_cache(key, budget, truncated);
            }
        }

        for key in left
            .multi_config_vars
            .into_iter()
            .chain(right.multi_config_vars)
        {
            joined.insert_multi_config(key, budget, truncated);
        }

        for key in left.defined_macros.into_iter().chain(right.defined_macros) {
            joined.insert_defined_macro(key, budget, truncated);
        }

        // Glob origins: union across reaching branches so any un-overwritten glob path stays live,
        // while overwriting on all branches clears it.
        for (var, origins) in left.glob_origins.into_iter().chain(right.glob_origins) {
            for origin in origins {
                joined.add_glob_origin(var.clone(), origin, budget, truncated);
            }
        }

        joined
    }
}

enum ControlFrame {
    If {
        pre_state: ScopeState,
        joined_branches: Option<Box<ScopeState>>,
        has_else: bool,
        degraded: bool,
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

impl ControlFrame {
    fn usage(&self) -> (usize, usize) {
        match self {
            Self::If {
                pre_state,
                joined_branches,
                else_defined,
                ..
            } => {
                let (pe, pb) = pre_state.usage();
                let (je, jb) = joined_branches.as_deref().map_or((0, 0), ScopeState::usage);
                let ed_bytes: usize = else_defined.iter().map(|s| s.len()).sum();
                (
                    pe.saturating_add(je).saturating_add(else_defined.len()),
                    pb.saturating_add(jb).saturating_add(ed_bytes),
                )
            }
            Self::Loop { pre_state } | Self::Function { pre_state } | Self::Macro { pre_state } => {
                pre_state.usage()
            }
            Self::Block {
                pre_state,
                propagate_vars,
                ..
            } => {
                let (pe, pb) = pre_state.usage();
                let prop_bytes: usize = propagate_vars.iter().map(|s| s.len()).sum();
                (
                    pe.saturating_add(propagate_vars.len()),
                    pb.saturating_add(prop_bytes),
                )
            }
        }
    }
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
    findings_truncated: bool,
    scope_truncated: bool,
    emitted_glob_sites: BTreeSet<usize>,
    dependencies: LintDependencies,
    probed_misses: usize,
    active_includes: BTreeSet<PathBuf>,
    include_files_read: usize,
    include_bytes_read: usize,
    active_flow_depth: usize,
    frame_entries: usize,
    frame_bytes: usize,
    scope_ops: usize,
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
        let primary_directives = DirectiveMap::parse(primary_src, concrete, limits.max_findings);
        let primary_lines = LineIndex::new(primary_src);
        let findings_truncated = primary_directives.issues_truncated;
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
            findings_truncated,
            scope_truncated: false,
            emitted_glob_sites: BTreeSet::new(),
            dependencies: LintDependencies::default(),
            probed_misses: 0,
            active_includes: BTreeSet::new(),
            include_files_read: 0,
            include_bytes_read: 0,
            active_flow_depth: 0,
            frame_entries: 0,
            frame_bytes: 0,
            scope_ops: 0,
        }
    }

    fn state_budget(&self) -> ScopeBudget {
        ScopeBudget {
            max_vars: self.limits.max_vars,
            max_entries: self
                .limits
                .max_total_scope_entries
                .saturating_sub(self.frame_entries),
            max_bytes: self
                .limits
                .max_total_scope_bytes
                .saturating_sub(self.frame_bytes),
        }
    }

    fn max_scope_ops(&self) -> usize {
        self.limits
            .max_total_scope_entries
            .saturating_mul(16)
            .max(65_536)
    }

    fn can_retain_clone(
        &self,
        state: &ScopeState,
        extra_entries: usize,
        extra_bytes: usize,
    ) -> bool {
        let (se, sb) = state.usage();
        let (ce, cb) = self.state.usage();
        let next_entries = self
            .frame_entries
            .saturating_add(ce)
            .saturating_add(se)
            .saturating_add(extra_entries);
        let next_bytes = self
            .frame_bytes
            .saturating_add(cb)
            .saturating_add(sb)
            .saturating_add(extra_bytes);
        let next_ops = self.scope_ops.saturating_add(se.max(1));
        next_entries <= self.limits.max_total_scope_entries
            && next_bytes <= self.limits.max_total_scope_bytes
            && next_ops <= self.max_scope_ops()
    }

    fn push_frame(&mut self, frames: &mut Vec<ControlFrame>, frame: ControlFrame) {
        let (fe, fb) = frame.usage();
        self.frame_entries = self.frame_entries.saturating_add(fe);
        self.frame_bytes = self.frame_bytes.saturating_add(fb);
        self.active_flow_depth = self.active_flow_depth.saturating_add(1);
        frames.push(frame);
    }

    fn pop_frame(&mut self, frames: &mut Vec<ControlFrame>) -> Option<ControlFrame> {
        let frame = frames.pop()?;
        let (fe, fb) = frame.usage();
        self.frame_entries = self.frame_entries.saturating_sub(fe);
        self.frame_bytes = self.frame_bytes.saturating_sub(fb);
        self.active_flow_depth = self.active_flow_depth.saturating_sub(1);
        Some(frame)
    }

    pub(crate) fn run(mut self, concrete: &ConcreteListFile) -> LintResult {
        for issue in &self.primary_directives.issues {
            if self.findings.len() >= self.limits.max_findings {
                self.findings_truncated = true;
                break;
            }
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
        LintResult {
            findings: self.findings,
            dependencies: self.dependencies,
            findings_truncated: self.findings_truncated,
            scope_truncated: self.scope_truncated,
        }
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
            self.findings_truncated = true;
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
        let mut untracked_depth = 0usize;

        for (cmd_idx, cmd) in commands.iter().enumerate() {
            let lower = cmd.lower.as_slice();

            // Check variable reads and glob consumers before applying this command's outputs.
            if is_primary {
                self.check_undefined_variables(cmd_idx, cmd, src);
            }
            self.check_glob_consumers(cmd, src);

            if untracked_depth > 0 {
                match lower {
                    b"if" => {
                        let cond =
                            analyze_if_condition(&cmd.args, src, &self.state.multi_config_vars);
                        if is_primary {
                            self.emit_condition_rules(cmd_idx, &cond);
                        }
                        untracked_depth = untracked_depth.saturating_add(1);
                        continue;
                    }
                    b"elseif" => {
                        let cond =
                            analyze_if_condition(&cmd.args, src, &self.state.multi_config_vars);
                        if is_primary {
                            self.emit_condition_rules(cmd_idx, &cond);
                        }
                        self.state.widen_unknown_scope_effect();
                        continue;
                    }
                    b"else" => {
                        self.state.widen_unknown_scope_effect();
                        continue;
                    }
                    b"foreach" | b"while" | b"function" | b"macro" | b"block" => {
                        untracked_depth = untracked_depth.saturating_add(1);
                        continue;
                    }
                    b"endif" | b"endforeach" | b"endwhile" | b"endfunction" | b"endmacro"
                    | b"endblock" => {
                        untracked_depth -= 1;
                        if untracked_depth == 0 {
                            self.state.widen_unknown_scope_effect();
                        }
                        continue;
                    }
                    _ => {}
                }
            } else {
                match lower {
                    b"if" => {
                        let cond =
                            analyze_if_condition(&cmd.args, src, &self.state.multi_config_vars);
                        if is_primary {
                            self.emit_condition_rules(cmd_idx, &cond);
                        }
                        let mut else_defined = cond.defined_when_false;
                        else_defined.truncate(MAX_ELSE_DEFINED);
                        let extra_entries = else_defined.len();
                        let extra_bytes: usize = else_defined.iter().map(|s| s.len()).sum();
                        if self.active_flow_depth < self.limits.max_flow_depth
                            && self.can_retain_clone(&self.state, extra_entries, extra_bytes)
                        {
                            self.scope_ops =
                                self.scope_ops.saturating_add(self.state.entries.max(1));
                            let pre_state = self.state.clone();
                            self.push_frame(
                                &mut frames,
                                ControlFrame::If {
                                    pre_state,
                                    joined_branches: None,
                                    has_else: false,
                                    degraded: false,
                                    else_defined,
                                    else_single_config: cond.single_config_when_false,
                                },
                            );
                            let budget = self.state_budget();
                            for name in &cond.defined_when_true {
                                self.state.set_definite(
                                    name.clone(),
                                    budget,
                                    &mut self.scope_truncated,
                                );
                            }
                            if cond.single_config_when_true {
                                self.state.single_config_guard_depth += 1;
                            }
                            if cond.guards_build_type_init {
                                self.state.build_type_guard_depth += 1;
                            }
                        } else {
                            self.scope_truncated = true;
                            self.state.widen_unknown_scope_effect();
                            untracked_depth = 1;
                        }
                        continue;
                    }
                    b"elseif" => {
                        self.transition_if_branch(&mut frames, false);
                        let cond =
                            analyze_if_condition(&cmd.args, src, &self.state.multi_config_vars);
                        if is_primary {
                            self.emit_condition_rules(cmd_idx, &cond);
                        }
                        let budget = self.state_budget();
                        for name in &cond.defined_when_true {
                            self.state.set_definite(
                                name.clone(),
                                budget,
                                &mut self.scope_truncated,
                            );
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
                        self.transition_if_branch(&mut frames, true);
                        continue;
                    }
                    b"endif" => {
                        if let Some(ControlFrame::If {
                            mut pre_state,
                            joined_branches,
                            has_else,
                            degraded,
                            ..
                        }) = self.pop_frame(&mut frames)
                        {
                            let base_sc = pre_state.single_config_guard_depth;
                            let base_bt = pre_state.build_type_guard_depth;
                            if degraded {
                                pre_state.widen_unknown_scope_effect();
                                self.state = pre_state;
                            } else {
                                let budget = self.state_budget();
                                let current = std::mem::take(&mut self.state);
                                let mut acc = match joined_branches {
                                    None => current,
                                    Some(prev) => {
                                        let prev = *prev;
                                        self.scope_ops = self.scope_ops.saturating_add(
                                            prev.entries.max(current.entries).max(1),
                                        );
                                        ScopeState::join_two(
                                            prev,
                                            current,
                                            budget,
                                            &mut self.scope_truncated,
                                        )
                                    }
                                };
                                if !has_else {
                                    self.scope_ops = self
                                        .scope_ops
                                        .saturating_add(acc.entries.max(pre_state.entries).max(1));
                                    acc = ScopeState::join_two(
                                        acc,
                                        pre_state,
                                        budget,
                                        &mut self.scope_truncated,
                                    );
                                }
                                self.state = acc;
                            }
                            self.state.single_config_guard_depth = base_sc;
                            self.state.build_type_guard_depth = base_bt;
                        }
                        continue;
                    }
                    b"foreach" => {
                        if self.active_flow_depth < self.limits.max_flow_depth
                            && self.can_retain_clone(&self.state, 0, 0)
                        {
                            self.scope_ops =
                                self.scope_ops.saturating_add(self.state.entries.max(1));
                            let pre_state = self.state.clone();
                            self.push_frame(&mut frames, ControlFrame::Loop { pre_state });
                            let budget = self.state_budget();
                            bind_foreach_vars(
                                cmd,
                                &mut self.state,
                                budget,
                                &mut self.scope_truncated,
                            );
                        } else {
                            self.scope_truncated = true;
                            self.state.widen_unknown_scope_effect();
                            untracked_depth = 1;
                        }
                        continue;
                    }
                    b"while" => {
                        if self.active_flow_depth < self.limits.max_flow_depth
                            && self.can_retain_clone(&self.state, 0, 0)
                        {
                            self.scope_ops =
                                self.scope_ops.saturating_add(self.state.entries.max(1));
                            let pre_state = self.state.clone();
                            self.push_frame(&mut frames, ControlFrame::Loop { pre_state });
                        } else {
                            self.scope_truncated = true;
                            self.state.widen_unknown_scope_effect();
                            untracked_depth = 1;
                        }
                        continue;
                    }
                    b"endforeach" | b"endwhile" => {
                        if let Some(ControlFrame::Loop { pre_state }) = self.pop_frame(&mut frames)
                        {
                            let budget = self.state_budget();
                            let current = std::mem::take(&mut self.state);
                            self.scope_ops = self
                                .scope_ops
                                .saturating_add(pre_state.entries.max(current.entries).max(1));
                            self.state = ScopeState::join_two(
                                pre_state,
                                current,
                                budget,
                                &mut self.scope_truncated,
                            );
                        }
                        continue;
                    }
                    b"function" => {
                        if self.active_flow_depth < self.limits.max_flow_depth
                            && self.can_retain_clone(&self.state, 0, 0)
                        {
                            self.scope_ops =
                                self.scope_ops.saturating_add(self.state.entries.max(1));
                            let pre_state = self.state.clone();
                            self.push_frame(&mut frames, ControlFrame::Function { pre_state });
                            let budget = self.state_budget();
                            bind_callable_params(
                                cmd,
                                true,
                                &mut self.state,
                                budget,
                                &mut self.scope_truncated,
                            );
                        } else {
                            self.scope_truncated = true;
                            self.state.widen_unknown_scope_effect();
                            untracked_depth = 1;
                        }
                        continue;
                    }
                    b"endfunction" => {
                        if let Some(ControlFrame::Function { pre_state }) =
                            self.pop_frame(&mut frames)
                        {
                            self.state = pre_state;
                        }
                        continue;
                    }
                    b"macro" => {
                        if let Some(name) = cmd.args.first()
                            && !name.text.is_empty()
                        {
                            let budget = self.state_budget();
                            self.state.insert_defined_macro(
                                BString::from(name.text.as_slice().to_ascii_lowercase()),
                                budget,
                                &mut self.scope_truncated,
                            );
                        }
                        if self.active_flow_depth < self.limits.max_flow_depth
                            && self.can_retain_clone(&self.state, 0, 0)
                        {
                            self.scope_ops =
                                self.scope_ops.saturating_add(self.state.entries.max(1));
                            let pre_state = self.state.clone();
                            self.push_frame(&mut frames, ControlFrame::Macro { pre_state });
                            let budget = self.state_budget();
                            bind_callable_params(
                                cmd,
                                false,
                                &mut self.state,
                                budget,
                                &mut self.scope_truncated,
                            );
                        } else {
                            self.scope_truncated = true;
                            self.state.widen_unknown_scope_effect();
                            untracked_depth = 1;
                        }
                        continue;
                    }
                    b"endmacro" => {
                        if let Some(ControlFrame::Macro { pre_state }) = self.pop_frame(&mut frames)
                        {
                            self.state = pre_state;
                        }
                        continue;
                    }
                    b"block" => {
                        let (isolate_vars, isolate_policies, mut propagate_vars) =
                            parse_block_options(cmd);
                        if propagate_vars.len() > MAX_PROPAGATE_VARS {
                            propagate_vars.truncate(MAX_PROPAGATE_VARS);
                            self.scope_truncated = true;
                        }
                        let extra_entries = propagate_vars.len();
                        let extra_bytes: usize = propagate_vars.iter().map(|s| s.len()).sum();
                        if self.active_flow_depth < self.limits.max_flow_depth
                            && self.can_retain_clone(&self.state, extra_entries, extra_bytes)
                        {
                            self.scope_ops =
                                self.scope_ops.saturating_add(self.state.entries.max(1));
                            let pre_state = self.state.clone();
                            self.push_frame(
                                &mut frames,
                                ControlFrame::Block {
                                    pre_state,
                                    isolate_vars,
                                    isolate_policies,
                                    propagate_vars,
                                },
                            );
                        } else {
                            self.scope_truncated = true;
                            self.state.widen_unknown_scope_effect();
                            untracked_depth = 1;
                        }
                        continue;
                    }
                    b"endblock" => {
                        if let Some(ControlFrame::Block {
                            mut pre_state,
                            isolate_vars,
                            isolate_policies,
                            propagate_vars,
                        }) = self.pop_frame(&mut frames)
                        {
                            let inner = std::mem::take(&mut self.state);
                            if !isolate_vars {
                                let saved_cmp = pre_state.cmp0054;
                                let saved_ver = pre_state.effective_version;
                                pre_state = inner;
                                if isolate_policies {
                                    pre_state.cmp0054 = saved_cmp;
                                    pre_state.effective_version = saved_ver;
                                }
                            } else {
                                let budget = self.state_budget();
                                for var in &propagate_vars {
                                    if let Some(&st) = inner.bindings.get(var) {
                                        pre_state.insert_binding(
                                            var.clone(),
                                            st,
                                            budget,
                                            &mut self.scope_truncated,
                                        );
                                    }
                                    pre_state.remove_glob_origins(var);
                                    if let Some(origins) = inner.glob_origins.get(var) {
                                        for origin in origins {
                                            pre_state.add_glob_origin(
                                                var.clone(),
                                                origin.clone(),
                                                budget,
                                                &mut self.scope_truncated,
                                            );
                                        }
                                    }
                                }
                                if !isolate_policies {
                                    pre_state.cmp0054 = inner.cmp0054;
                                    pre_state.effective_version = inner.effective_version;
                                }
                            }
                            self.state = pre_state;
                        }
                        continue;
                    }
                    _ => {}
                }
            }

            // Emit command rules only for the primary file; still transfer scope effects for includes.
            if is_primary {
                self.check_command_rules(cmd_idx, cmd);
            }
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

        // Release any unclosed frames so caller accounting remains exact after included files.
        while self.pop_frame(&mut frames).is_some() {}
    }

    fn transition_if_branch(&mut self, frames: &mut [ControlFrame], is_else: bool) {
        let Some(frame) = frames.last_mut() else {
            return;
        };
        let old_usage = frame.usage();
        let ControlFrame::If {
            pre_state,
            joined_branches,
            has_else,
            degraded,
            else_defined,
            else_single_config,
        } = frame
        else {
            return;
        };

        self.frame_entries = self.frame_entries.saturating_sub(old_usage.0);
        self.frame_bytes = self.frame_bytes.saturating_sub(old_usage.1);

        if is_else {
            *has_else = true;
        }

        let finished = std::mem::take(&mut self.state);
        if !*degraded {
            let other_entries = self.frame_entries.saturating_add(pre_state.entries);
            let other_bytes = self.frame_bytes.saturating_add(pre_state.bytes);
            let join_budget = ScopeBudget {
                max_vars: self.limits.max_vars,
                max_entries: self
                    .limits
                    .max_total_scope_entries
                    .saturating_sub(other_entries),
                max_bytes: self
                    .limits
                    .max_total_scope_bytes
                    .saturating_sub(other_bytes),
            };
            let next_joined = match joined_branches.take() {
                None => finished,
                Some(acc) => {
                    let acc = *acc;
                    self.scope_ops = self
                        .scope_ops
                        .saturating_add(acc.entries.max(finished.entries).max(1));
                    ScopeState::join_two(acc, finished, join_budget, &mut self.scope_truncated)
                }
            };
            *joined_branches = Some(Box::new(next_joined));
        }

        let after_join_entries = self
            .frame_entries
            .saturating_add(pre_state.entries)
            .saturating_add(joined_branches.as_deref().map_or(0, |j| j.entries));
        let after_join_bytes = self
            .frame_bytes
            .saturating_add(pre_state.bytes)
            .saturating_add(joined_branches.as_deref().map_or(0, |j| j.bytes));

        let next_entries = after_join_entries.saturating_add(pre_state.entries);
        let next_bytes = after_join_bytes.saturating_add(pre_state.bytes);
        let next_ops = self.scope_ops.saturating_add(pre_state.entries.max(1));

        if !*degraded
            && next_entries <= self.limits.max_total_scope_entries
            && next_bytes <= self.limits.max_total_scope_bytes
            && next_ops <= self.max_scope_ops()
        {
            self.scope_ops = next_ops;
            self.state = pre_state.clone();
        } else {
            self.scope_truncated = true;
            *degraded = true;
            *joined_branches = None;
            self.state = pre_state.shallow_scalars();
        }

        let apply_else_defined = if is_else {
            else_defined.clone()
        } else {
            else_defined.clear();
            *else_single_config = false;
            Vec::new()
        };
        let apply_else_single_config = is_else && *else_single_config;

        let new_usage = frame.usage();
        self.frame_entries = self.frame_entries.saturating_add(new_usage.0);
        self.frame_bytes = self.frame_bytes.saturating_add(new_usage.1);

        if is_else {
            let budget = self.state_budget();
            for name in apply_else_defined {
                self.state
                    .set_definite(name, budget, &mut self.scope_truncated);
            }
            if apply_else_single_config {
                self.state.single_config_guard_depth += 1;
            }
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
            if self.findings.len() >= self.limits.max_findings {
                self.findings_truncated = true;
                break;
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

    fn mutate_nearest_parent_scope(
        &mut self,
        frames: &mut [ControlFrame],
        mutate: impl FnOnce(&mut ScopeState, ScopeBudget, &mut bool),
    ) {
        let Some(idx) = frames.iter().rposition(|frame| {
            matches!(
                frame,
                ControlFrame::Function { .. }
                    | ControlFrame::Block {
                        isolate_vars: true,
                        ..
                    }
            )
        }) else {
            return;
        };
        let Some(frame) = frames.get_mut(idx) else {
            return;
        };
        let old_usage = frame.usage();
        self.frame_entries = self.frame_entries.saturating_sub(old_usage.0);
        self.frame_bytes = self.frame_bytes.saturating_sub(old_usage.1);
        let (cur_e, cur_b) = self.state.usage();
        let budget = ScopeBudget {
            max_vars: self.limits.max_vars,
            max_entries: self
                .limits
                .max_total_scope_entries
                .saturating_sub(self.frame_entries)
                .saturating_sub(cur_e),
            max_bytes: self
                .limits
                .max_total_scope_bytes
                .saturating_sub(self.frame_bytes)
                .saturating_sub(cur_b),
        };
        match frame {
            ControlFrame::Function { pre_state } | ControlFrame::Block { pre_state, .. } => {
                mutate(pre_state, budget, &mut self.scope_truncated);
            }
            _ => {}
        }
        let new_usage = frame.usage();
        self.frame_entries = self.frame_entries.saturating_add(new_usage.0);
        self.frame_bytes = self.frame_bytes.saturating_add(new_usage.1);
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
        let budget = self.state_budget();

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
                    bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
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
                        self.state.set_definite(
                            BString::from(full),
                            budget,
                            &mut self.scope_truncated,
                        );
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
                    self.state.widen_unknown_scope_effect();
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

                // Propagate glob taint if RHS references an active glob variable.
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

                if name.as_slice() == b"CMAKE_MODULE_PATH" && !is_parent_scope {
                    let overflowed = update_module_paths(
                        &mut self.state,
                        file_path,
                        self.workspace_root,
                        value_args,
                        false,
                        budget,
                        &mut self.scope_truncated,
                    );
                    if overflowed {
                        self.dependencies.incomplete = true;
                    }
                }

                if is_parent_scope {
                    let is_unset = value_args.is_empty();
                    self.mutate_nearest_parent_scope(frames, |parent_state, p_budget, trunc| {
                        if is_unset {
                            parent_state.set_absent(name, p_budget, trunc);
                        } else {
                            parent_state.set_definite(name.clone(), p_budget, trunc);
                            for origin in copied_globs {
                                parent_state.add_glob_origin(name.clone(), origin, p_budget, trunc);
                            }
                        }
                    });
                    return;
                }

                // `set(VAR)` with no value arguments unsets `VAR` in CMake.
                if !is_cache && value_args.is_empty() {
                    self.state
                        .set_absent(name, budget, &mut self.scope_truncated);
                } else {
                    self.state
                        .set_definite(name.clone(), budget, &mut self.scope_truncated);
                    for origin in copied_globs {
                        self.state.add_glob_origin(
                            name.clone(),
                            origin,
                            budget,
                            &mut self.scope_truncated,
                        );
                    }
                }
            }
            b"unset" => {
                let Some(var_arg) = cmd.args.first() else {
                    return;
                };
                let raw = var_arg.text.as_slice();
                if raw.starts_with(b"ENV{") {
                    return;
                }
                if raw.contains(&b'$') {
                    // Dynamic unset can clear any glob variable's value.
                    self.state.clear_glob_origins();
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
                    self.mutate_nearest_parent_scope(frames, |parent_state, p_budget, trunc| {
                        parent_state.set_absent(name, p_budget, trunc);
                    });
                } else {
                    if is_cache {
                        self.state.insert_unset_cache(
                            name.clone(),
                            budget,
                            &mut self.scope_truncated,
                        );
                    }
                    self.state
                        .set_absent(name, budget, &mut self.scope_truncated);
                }
            }
            b"option" => {
                if let Some(arg) = cmd.args.first() {
                    bind_output_arg(arg, &mut self.state, budget, &mut self.scope_truncated);
                }
            }
            b"file" => {
                if let Some(glob) = parse_file_glob(cmd) {
                    self.state.set_definite(
                        glob.output_var.clone(),
                        budget,
                        &mut self.scope_truncated,
                    );
                    if !glob.has_configure_depends {
                        self.state.add_glob_origin(
                            glob.output_var.clone(),
                            GlobOrigin {
                                command_index: cmd_idx,
                                span: cmd.span.clone(),
                                var_name: glob.output_var,
                                is_recurse: glob.is_recurse,
                                in_primary_file: is_primary,
                            },
                            budget,
                            &mut self.scope_truncated,
                        );
                    }
                } else {
                    bind_file_output(cmd, &mut self.state, budget, &mut self.scope_truncated);
                }
            }
            b"list" => {
                if let (Some(sub), Some(var_arg)) = (cmd.args.first(), cmd.args.get(1)) {
                    let sub_bytes = sub.text.as_slice();
                    if matches!(sub_bytes, b"APPEND" | b"PREPEND")
                        && var_arg.text.as_slice() == b"CMAKE_MODULE_PATH"
                    {
                        let overflowed = update_module_paths(
                            &mut self.state,
                            file_path,
                            self.workspace_root,
                            cmd.args.get(2..).unwrap_or_default(),
                            true,
                            budget,
                            &mut self.scope_truncated,
                        );
                        if overflowed {
                            self.dependencies.incomplete = true;
                        }
                    }
                    if matches!(sub_bytes, b"APPEND" | b"PREPEND" | b"INSERT")
                        && !var_arg.text.as_slice().contains(&b'$')
                    {
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
                        self.state.insert_binding(
                            var_arg.text.clone(),
                            BindingState::Definite,
                            budget,
                            &mut self.scope_truncated,
                        );
                        for origin in added_globs {
                            self.state.add_glob_origin(
                                var_arg.text.clone(),
                                origin,
                                budget,
                                &mut self.scope_truncated,
                            );
                        }
                    } else if matches!(
                        sub_bytes,
                        b"LENGTH" | b"GET" | b"JOIN" | b"SUBLIST" | b"FIND"
                    ) && let Some(out) = cmd.args.last()
                    {
                        bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
                    } else if sub_bytes == b"TRANSFORM"
                        && let Some(pos) = cmd
                            .args
                            .iter()
                            .position(|a| a.text.as_slice() == b"OUTPUT_VARIABLE")
                        && let Some(out) = cmd.args.get(pos + 1)
                    {
                        bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
                    }
                }
            }
            b"get_property" => {
                if let Some(out) = cmd.args.first() {
                    bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
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
                        self.state.insert_multi_config(
                            out.text.clone(),
                            budget,
                            &mut self.scope_truncated,
                        );
                    }
                }
            }
            b"get_cmake_property" => {
                if let Some(out) = cmd.args.first() {
                    bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
                    if cmd
                        .args
                        .get(1)
                        .is_some_and(|a| a.text.as_slice() == b"GENERATOR_IS_MULTI_CONFIG")
                        && !out.text.as_slice().contains(&b'$')
                    {
                        self.state.insert_multi_config(
                            out.text.clone(),
                            budget,
                            &mut self.scope_truncated,
                        );
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
                    bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
                }
            }
            b"try_run" => {
                if let Some(a) = cmd.args.first() {
                    bind_output_arg(a, &mut self.state, budget, &mut self.scope_truncated);
                }
                if let Some(b) = cmd.args.get(1) {
                    bind_output_arg(b, &mut self.state, budget, &mut self.scope_truncated);
                }
            }
            b"math" => {
                if cmd
                    .args
                    .first()
                    .is_some_and(|a| a.text.as_slice() == b"EXPR")
                    && let Some(out) = cmd.args.get(1)
                {
                    bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
                }
            }
            b"string" => {
                bind_string_output(cmd, &mut self.state, budget, &mut self.scope_truncated);
            }
            b"cmake_path" => {
                bind_cmake_path_output(cmd, &mut self.state, budget, &mut self.scope_truncated);
            }
            b"cmake_host_system_information" => {
                if let Some(pos) = cmd.args.iter().position(|a| a.text.as_slice() == b"RESULT")
                    && let Some(out) = cmd.args.get(pos + 1)
                {
                    bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
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
                        bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
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
                            self.state.set_definite(
                                BString::from(full),
                                budget,
                                &mut self.scope_truncated,
                            );
                        }
                    }
                }
                self.state.widen_unknown_scope_effect();
            }
            b"cmake_parse_arguments" => {
                bind_parse_arguments(cmd, &mut self.state, budget, &mut self.scope_truncated);
            }
            b"include" => {
                self.handle_include(file_path, cmd, include_depth);
            }
            _ => {
                // Common Check* module functions place their output variable last.
                if (lower.starts_with(b"check_") || lower.starts_with(b"cmake_check_"))
                    && let Some(last) = cmd.args.last()
                {
                    bind_output_arg(last, &mut self.state, budget, &mut self.scope_truncated);
                } else if self.state.defined_macros.contains(lower) {
                    // User-defined macro executes in caller scope.
                    self.state.widen_unknown_scope_effect();
                }
            }
        }
    }

    fn handle_include(&mut self, file_path: &Path, cmd: &CommandInvocation, include_depth: usize) {
        let budget = self.state_budget();
        if let Some(pos) = cmd
            .args
            .iter()
            .position(|a| a.text.as_slice() == b"RESULT_VARIABLE")
            && let Some(out) = cmd.args.get(pos + 1)
        {
            bind_output_arg(out, &mut self.state, budget, &mut self.scope_truncated);
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

        let Some(candidates) = self.resolve_include_candidates(file_path, target_arg) else {
            // Dynamic include path cannot be tracked statically; mark dependencies incomplete
            // and invalidate scope facts (including active glob origins).
            self.dependencies.incomplete = true;
            self.state.widen_unknown_scope_effect();
            return;
        };

        let max_probe_misses = self.limits.max_include_files.saturating_mul(4);
        let max_consumed = self.limits.max_include_files.max(1).saturating_mul(4);
        let mut chosen: Option<PathBuf> = None;
        for cand in candidates {
            let norm = normalize_lexical(&cand);
            if self.reader.file_exists(&norm) {
                let is_bundled = self
                    .reader
                    .bundled_modules_dir()
                    .is_some_and(|dir| norm.starts_with(dir));
                // Record consumed files independently of missed-probe counts so failed probes
                // can never evict an actually read dependency.
                if is_bundled {
                    self.dependencies.included_files.insert(norm.clone());
                } else if self.dependencies.consumed_files.len() < max_consumed
                    || self.dependencies.consumed_files.contains(&norm)
                {
                    self.dependencies.consumed_files.insert(norm.clone());
                    self.dependencies.included_files.insert(norm.clone());
                } else {
                    self.dependencies.incomplete = true;
                }
                chosen = Some(norm);
                break;
            }
            if self.probed_misses < max_probe_misses {
                if self.dependencies.included_files.insert(norm) {
                    self.probed_misses += 1;
                }
            } else {
                self.dependencies.incomplete = true;
            }
        }

        let Some(inc_path) = chosen else {
            self.state.widen_unknown_scope_effect();
            return;
        };

        // Bundled CMake standard modules: avoid parsing stdlib on every keystroke;
        // invalidate scope conservatively instead.
        if self
            .reader
            .bundled_modules_dir()
            .is_some_and(|dir| inc_path.starts_with(dir))
        {
            self.state.widen_unknown_scope_effect();
            return;
        }

        if include_depth >= self.limits.max_include_depth
            || self.include_files_read >= self.limits.max_include_files
            || self.include_bytes_read >= self.limits.max_include_bytes
            || self.active_includes.contains(&inc_path)
        {
            self.dependencies.incomplete = true;
            self.state.widen_unknown_scope_effect();
            return;
        }

        let remaining_bytes = self
            .limits
            .max_include_bytes
            .saturating_sub(self.include_bytes_read);
        let Some(inc_bytes) = self.reader.read_file(&inc_path, remaining_bytes as u64) else {
            self.dependencies.incomplete = true;
            self.state.widen_unknown_scope_effect();
            return;
        };

        self.include_files_read += 1;
        self.include_bytes_read = self.include_bytes_read.saturating_add(inc_bytes.len());

        let mut sources = SourceMap::default();
        let Some(source_id) = sources.add("".into(), SourceKind::File, Arc::from(b"".as_slice()))
        else {
            self.dependencies.incomplete = true;
            self.state.widen_unknown_scope_effect();
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
            self.dependencies.incomplete = true;
            self.state.widen_unknown_scope_effect();
            return;
        };

        let mut optional_degraded = false;
        let saved_state_for_optional = if is_optional {
            if self.can_retain_clone(&self.state, 0, 0) {
                self.scope_ops = self.scope_ops.saturating_add(self.state.entries.max(1));
                let saved = self.state.clone();
                let (se, sb) = saved.usage();
                self.frame_entries = self.frame_entries.saturating_add(se);
                self.frame_bytes = self.frame_bytes.saturating_add(sb);
                Some(saved)
            } else {
                self.scope_truncated = true;
                optional_degraded = true;
                None
            }
        } else {
            None
        };
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
            let (be, bb) = before.usage();
            self.frame_entries = self.frame_entries.saturating_sub(be);
            self.frame_bytes = self.frame_bytes.saturating_sub(bb);
            let join_budget = self.state_budget();
            let current = std::mem::take(&mut self.state);
            self.scope_ops = self
                .scope_ops
                .saturating_add(before.entries.max(current.entries).max(1));
            self.state =
                ScopeState::join_two(before, current, join_budget, &mut self.scope_truncated);
        } else if optional_degraded {
            self.state.widen_unknown_scope_effect();
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

fn bind_output_arg(
    arg: &rcmake_syntax::Argument,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
    if arg.text.is_empty() {
        return;
    }
    if arg.text.as_slice().contains(&b'$') {
        state.widen_unknown_scope_effect();
    } else {
        state.set_definite(arg.text.clone(), budget, truncated);
    }
}

fn bind_foreach_vars(
    cmd: &CommandInvocation,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
    let Some(first) = cmd.args.first() else {
        return;
    };
    bind_output_arg(first, state, budget, truncated);
    // Handle `foreach(v1 v2 ... IN ZIP_LISTS ...)`.
    if let Some(in_pos) = cmd.args.iter().position(|a| a.text.as_slice() == b"IN")
        && cmd
            .args
            .get(in_pos + 1)
            .is_some_and(|a| a.text.as_slice() == b"ZIP_LISTS")
    {
        for var in cmd.args.iter().take(in_pos) {
            bind_output_arg(var, state, budget, truncated);
        }
    }
}

fn bind_callable_params(
    cmd: &CommandInvocation,
    is_function: bool,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
    for param in cmd.args.iter().skip(1) {
        if !param.text.is_empty() && !param.text.as_slice().contains(&b'$') {
            state.set_definite(param.text.clone(), budget, truncated);
        }
    }
    for builtin in [b"ARGC".as_slice(), b"ARGV", b"ARGN"] {
        state.set_definite(BString::from(builtin), budget, truncated);
    }
    if is_function {
        for builtin in [
            b"CMAKE_CURRENT_FUNCTION".as_slice(),
            b"CMAKE_CURRENT_FUNCTION_LIST_DIR",
            b"CMAKE_CURRENT_FUNCTION_LIST_FILE",
            b"CMAKE_CURRENT_FUNCTION_LIST_LINE",
        ] {
            state.set_definite(BString::from(builtin), budget, truncated);
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

fn bind_file_output(
    cmd: &CommandInvocation,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
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
        bind_output_arg(arg, state, budget, truncated);
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
                bind_output_arg(out, state, budget, truncated);
            }
        }
    }
}

fn bind_string_output(
    cmd: &CommandInvocation,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
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
        bind_output_arg(arg, state, budget, truncated);
    }
}

fn bind_cmake_path_output(
    cmd: &CommandInvocation,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
    let Some(sub) = cmd.args.first().map(|a| a.text.as_slice()) else {
        return;
    };
    if let Some(pos) = cmd
        .args
        .iter()
        .position(|a| a.text.as_slice() == b"OUTPUT_VARIABLE")
        && let Some(out) = cmd.args.get(pos + 1)
    {
        bind_output_arg(out, state, budget, truncated);
        return;
    }
    match sub {
        b"SET" | b"APPEND" | b"APPEND_STRING" | b"REMOVE_FILENAME" | b"REPLACE_FILENAME"
        | b"REMOVE_EXTENSION" | b"REPLACE_EXTENSION" | b"NORMAL_PATH" => {
            if let Some(arg) = cmd.args.get(1) {
                bind_output_arg(arg, state, budget, truncated);
            }
        }
        b"GET" | b"COMPARE" | b"HASH" | b"RELATIVE_PATH" | b"ABSOLUTE_PATH" | b"NATIVE_PATH"
        | b"CONVERT" => {
            if let Some(arg) = cmd.args.last() {
                bind_output_arg(arg, state, budget, truncated);
            }
        }
        s if s.starts_with(b"IS_") || s.starts_with(b"HAS_") => {
            if let Some(arg) = cmd.args.last() {
                bind_output_arg(arg, state, budget, truncated);
            }
        }
        _ => {}
    }
}

fn bind_parse_arguments(
    cmd: &CommandInvocation,
    state: &mut ScopeState,
    budget: ScopeBudget,
    truncated: &mut bool,
) {
    let offset = usize::from(
        cmd.args
            .first()
            .is_some_and(|a| a.text.as_slice() == b"PARSE_ARGV"),
    ) * 2;
    let Some(prefix_arg) = cmd.args.get(offset) else {
        state.widen_unknown_scope_effect();
        return;
    };
    if prefix_arg.text.is_empty() || prefix_arg.text.as_slice().contains(&b'$') {
        state.widen_unknown_scope_effect();
        return;
    }
    let prefix = prefix_arg.text.as_slice();
    let mut prefix_under = Vec::with_capacity(prefix.len() + 1);
    prefix_under.extend_from_slice(prefix);
    prefix_under.push(b'_');

    for suffix in [b"UNPARSED_ARGUMENTS".as_slice(), b"KEYWORDS_MISSING_VALUES"] {
        let mut full = Vec::with_capacity(prefix_under.len() + suffix.len());
        full.extend_from_slice(&prefix_under);
        full.extend_from_slice(suffix);
        state.set_definite(BString::from(full), budget, truncated);
    }
    for group_idx in [offset + 1, offset + 2, offset + 3] {
        let Some(group) = cmd.args.get(group_idx) else {
            continue;
        };
        if group.text.as_slice().contains(&b'$') {
            state.widen_prefix_scope_effect(&prefix_under);
            continue;
        }
        for key in group.text.as_slice().split(|&b| b == b';') {
            if !key.is_empty() {
                let mut full = Vec::with_capacity(prefix_under.len() + key.len());
                full.extend_from_slice(&prefix_under);
                full.extend_from_slice(key);
                state.set_definite(BString::from(full), budget, truncated);
            }
        }
    }
}

fn update_module_paths(
    state: &mut ScopeState,
    current_file: &Path,
    workspace_root: Option<&Path>,
    args: &[rcmake_syntax::Argument],
    append: bool,
    budget: ScopeBudget,
    truncated: &mut bool,
) -> bool {
    let project_source_dir = state.project_source_dir.clone();
    if !append {
        state.clear_module_paths();
    }
    let mut overflowed = false;
    for arg in args {
        let Ok(text) = core::str::from_utf8(arg.text.as_slice()) else {
            continue;
        };
        for part in text.split(';').filter(|p| !p.is_empty()) {
            if state.module_paths.len() >= MAX_MODULE_PATHS {
                overflowed = true;
                return overflowed;
            }
            if let Some(resolved) = resolve_literal_path(
                part,
                arg.kind,
                current_file,
                workspace_root,
                project_source_dir.as_deref(),
            ) {
                let full = if resolved.is_absolute() {
                    resolved
                } else {
                    current_file.parent().unwrap_or(current_file).join(resolved)
                };
                if !state.push_module_path(normalize_lexical(&full), budget, truncated) {
                    overflowed = true;
                    return overflowed;
                }
            }
        }
    }
    overflowed
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

#[cfg(test)]
mod tests {
    use super::*;

    fn recompute_usage(state: &ScopeState) -> (usize, usize) {
        let mut entries = 0usize;
        let mut bytes = 0usize;
        for k in state.bindings.keys() {
            entries += 1;
            bytes += k.len();
        }
        for k in &state.unset_cache_vars {
            entries += 1;
            bytes += k.len();
        }
        for k in &state.multi_config_vars {
            entries += 1;
            bytes += k.len();
        }
        for k in &state.defined_macros {
            entries += 1;
            bytes += k.len();
        }
        for (k, origins) in &state.glob_origins {
            bytes += k.len();
            for o in origins {
                entries += 1;
                bytes += o.var_name.len();
            }
        }
        for p in &state.module_paths {
            entries += 1;
            bytes += p.as_os_str().len();
        }
        (entries, bytes)
    }

    #[test]
    fn scope_state_entry_and_byte_accounting_matches_recomputed_usage() {
        let budget = ScopeBudget {
            max_vars: 128,
            max_entries: 128,
            max_bytes: 8192,
        };
        let mut truncated = false;
        let mut left = ScopeState::default();
        left.set_definite("ALPHA", budget, &mut truncated);
        left.insert_unset_cache(BString::from("CACHE_X"), budget, &mut truncated);
        left.insert_multi_config(BString::from("IS_MULTI"), budget, &mut truncated);
        left.insert_defined_macro(BString::from("my_mac"), budget, &mut truncated);
        left.push_module_path(PathBuf::from("/work/cmake"), budget, &mut truncated);
        left.add_glob_origin(
            BString::from("SRCS"),
            GlobOrigin {
                command_index: 1,
                span: 0..10,
                var_name: BString::from("SRCS"),
                is_recurse: false,
                in_primary_file: true,
            },
            budget,
            &mut truncated,
        );
        assert_eq!(left.usage(), recompute_usage(&left));

        let mut right = left.clone();
        right.set_absent("ALPHA", budget, &mut truncated);
        right.remove_glob_origins(&BString::from("SRCS"));
        right.set_definite("BETA", budget, &mut truncated);
        assert_eq!(right.usage(), recompute_usage(&right));

        let mut joined = ScopeState::join_two(left, right, budget, &mut truncated);
        assert_eq!(joined.usage(), recompute_usage(&joined));

        joined.widen_unknown_scope_effect();
        assert_eq!(joined.usage(), recompute_usage(&joined));
        assert!(joined.glob_origins.is_empty());
        assert!(joined.unset_cache_vars.is_empty());
        assert!(!truncated);
    }
}
