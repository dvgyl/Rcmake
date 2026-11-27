//! Static linter for CMake listfiles with 8 stable rules, bounded workspace includes,
//! comment directives, terminal rendering, and SARIF 2.1.0 output.

#![forbid(unsafe_code)]

mod directives;
mod expr;
mod rules;
mod scope;

use core::ops::Range;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::Limits;
use rcmake_diag::sarif::{SarifDocument, SarifRegion, SarifResult, SarifRule, build_document};
use rcmake_diag::source::{SourceKind, SourceMap};
use rcmake_host::{FileKind, Follow, Host, ReadRange};
use rcmake_json::Value;
use rcmake_syntax::concrete::{ConcreteListFile, parse_concrete_with_budget};

use crate::config::LintConfig;
use crate::discovery::normalize_lexical;
pub use crate::rules_meta::{ALL_RULES, INVALID_DIRECTIVE_RULE, RuleId, RuleInfo, Severity};

/// One static lint finding emitted for a CMake listfile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Stable public rule identifier (for example `"undefined-variable"`).
    pub rule: &'static str,
    /// Effective severity after applying `.rcmake.toml` overrides.
    pub severity: Severity,
    /// Tool source label (`"rcmake-lint"`).
    pub source: &'static str,
    /// File path associated with this finding.
    pub path: PathBuf,
    /// Primary raw byte span within the file.
    pub primary_span: Range<usize>,
    /// 1-based starting line number (`SourceKind::File` line rules).
    pub line: u32,
    /// 1-based starting column (`unicodeCodePoints` on UTF-8, raw bytes otherwise).
    pub column: u32,
    /// 1-based ending line number.
    pub end_line: u32,
    /// 1-based ending column.
    pub end_column: u32,
    /// Human-readable explanation of the finding.
    pub message: String,
    /// Optional actionable suggestion.
    pub suggestion: Option<String>,
}

/// Resource budgets enforced during static linting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LintLimits {
    /// Maximum primary input size in bytes.
    pub max_input_bytes: usize,
    /// Maximum lossless tokens in the primary file.
    pub max_tokens: usize,
    /// Maximum tracked variables per scope.
    pub max_vars: usize,
    /// Maximum tracked flow-control nesting depth (`if`/`foreach`/`while`/`function`/`macro`/`block`).
    pub max_flow_depth: usize,
    /// Maximum cumulative retained scope entries across active state and stacked control frames.
    pub max_total_scope_entries: usize,
    /// Maximum cumulative retained scope bytes across active state and stacked control frames.
    pub max_total_scope_bytes: usize,
    /// Maximum static include recursion depth.
    pub max_include_depth: usize,
    /// Maximum statically included files read per linted file.
    pub max_include_files: usize,
    /// Maximum cumulative bytes read across static includes per linted file.
    pub max_include_bytes: usize,
    /// Maximum findings emitted per file.
    pub max_findings: usize,
}

impl Default for LintLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_tokens: 500_000,
            max_vars: 8192,
            max_flow_depth: 128,
            max_total_scope_entries: 65_536,
            max_total_scope_bytes: 8 * 1024 * 1024,
            max_include_depth: 16,
            max_include_files: 64,
            max_include_bytes: 2 * 1024 * 1024,
            max_findings: 4096,
        }
    }
}

/// Files and configuration directories observed while linting one document.
///
/// LSP uses these sets to republish open callers when an included buffer/file
/// or an ancestor `.rcmake.toml` is created, edited, or deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LintDependencies {
    /// Included paths probed or loaded during static analysis.
    ///
    /// Successfully consumed include files are recorded independently of the failed-probe cap
    /// so missed probes cannot evict an actual dependency.
    pub included_files: BTreeSet<PathBuf>,
    /// Workspace include files that existed and were consumed (excluding bundled stdlib).
    ///
    /// LSP registers file watchers for non-`.cmake` entries in this set.
    pub consumed_files: BTreeSet<PathBuf>,
    /// `true` if dependency tracking hit a probe/include budget or saw a dynamic `include()`.
    ///
    /// When `true`, LSP conservatively refreshes the document on workspace file events.
    pub incomplete: bool,
    /// Every ancestor directory checked for `.rcmake.toml`.
    pub config_dirs: Vec<PathBuf>,
    /// Active `.rcmake.toml` path, if one was loaded.
    pub config_file: Option<PathBuf>,
}

/// Result of linting one CMake listfile.
#[derive(Clone, Debug, Default)]
pub struct LintResult {
    /// Sorted, deduplicated findings for this file.
    pub findings: Vec<Finding>,
    /// Reachable include and configuration dependencies observed during analysis.
    pub dependencies: LintDependencies,
    /// `true` if one or more findings (including invalid directives) were dropped at `max_findings`.
    pub findings_truncated: bool,
    /// `true` if static scope tracking degraded after hitting flow-depth, entry, or byte budgets.
    pub scope_truncated: bool,
}

/// Bounded read-only workspace reader shared by CLI and LSP lint.
///
/// Implementations prefer all open in-memory editor buffers over on-disk files.
pub trait WorkspaceReader {
    /// Read up to `limit` raw bytes from `path` (returning `None` if absent, unreadable,
    /// outside readable roots, or larger than `limit`).
    fn read_file(&self, path: &Path, limit: u64) -> Option<Vec<u8>>;

    /// Return `true` if `path` exists as a readable regular file (in memory or on disk).
    fn file_exists(&self, path: &Path) -> bool;

    /// Optional bundled CMake `Modules/` directory (recognized without parsing stdlib).
    fn bundled_modules_dir(&self) -> Option<&Path> {
        None
    }
}

/// Empty workspace reader that performs no external include reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyWorkspaceReader;

impl WorkspaceReader for EmptyWorkspaceReader {
    fn read_file(&self, _path: &Path, _limit: u64) -> Option<Vec<u8>> {
        None
    }

    fn file_exists(&self, _path: &Path) -> bool {
        false
    }
}

/// [`WorkspaceReader`] backed by [`Host`] with an optional in-memory overlay of ALL
/// open LSP buffers and workspace root containment bounds.
pub struct HostWorkspaceReader<'a> {
    host: &'a dyn Host,
    readable_roots: Vec<PathBuf>,
    bundled_modules: Option<PathBuf>,
    open_buffers: BTreeMap<PathBuf, Arc<[u8]>>,
}

impl<'a> HostWorkspaceReader<'a> {
    /// Create a workspace reader over `host` bounded to `readable_roots`.
    ///
    /// If `readable_roots` is empty, reads are allowed only within `bundled_modules`
    /// or open buffers (pass the project root directory to enable project includes).
    #[must_use]
    pub fn new(
        host: &'a dyn Host,
        readable_roots: Vec<PathBuf>,
        bundled_modules: Option<PathBuf>,
    ) -> Self {
        let canonical_roots = readable_roots
            .into_iter()
            .map(|r| {
                host.canonicalize(&r)
                    .unwrap_or_else(|_| normalize_lexical(&r))
            })
            .collect();
        let bundled = bundled_modules.map(|b| {
            host.canonicalize(&b)
                .unwrap_or_else(|_| normalize_lexical(&b))
        });
        Self {
            host,
            readable_roots: canonical_roots,
            bundled_modules: bundled,
            open_buffers: BTreeMap::new(),
        }
    }

    /// Register an open in-memory buffer that takes precedence over disk for `path`.
    pub fn insert_open_buffer(&mut self, path: PathBuf, bytes: Arc<[u8]>) {
        let norm = normalize_lexical(&path);
        self.open_buffers.insert(norm, bytes);
    }

    fn is_within_roots(&self, canonical: &Path) -> bool {
        self.readable_roots.iter().any(|r| canonical.starts_with(r))
            || self
                .bundled_modules
                .as_deref()
                .is_some_and(|b| canonical.starts_with(b))
    }
}

impl WorkspaceReader for HostWorkspaceReader<'_> {
    fn read_file(&self, path: &Path, limit: u64) -> Option<Vec<u8>> {
        let norm = normalize_lexical(path);
        if let Some(buf) = self.open_buffers.get(&norm) {
            return (buf.len() as u64 <= limit).then(|| buf.as_ref().to_vec());
        }
        let canonical = self.host.canonicalize(&norm).ok()?;
        if let Some(buf) = self.open_buffers.get(&canonical) {
            return (buf.len() as u64 <= limit).then(|| buf.as_ref().to_vec());
        }
        if !self.is_within_roots(&canonical) {
            return None;
        }
        let meta = self.host.metadata(&canonical, Follow::Yes).ok()?;
        if meta.kind != FileKind::File || meta.len > limit {
            return None;
        }
        self.host.read(&canonical, ReadRange::All { limit }).ok()
    }

    fn file_exists(&self, path: &Path) -> bool {
        let norm = normalize_lexical(path);
        if self.open_buffers.contains_key(&norm) {
            return true;
        }
        let Ok(canonical) = self.host.canonicalize(&norm) else {
            return false;
        };
        if self.open_buffers.contains_key(&canonical) {
            return true;
        }
        if !self.is_within_roots(&canonical) {
            return false;
        }
        self.host
            .metadata(&canonical, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    }

    fn bundled_modules_dir(&self) -> Option<&Path> {
        self.bundled_modules.as_deref()
    }
}

/// Strictly parse raw file bytes (`SourceKind::File`) and run all enabled lint rules.
pub fn lint_bytes(
    path: &Path,
    src: &[u8],
    config: &LintConfig,
    limits: &LintLimits,
    reader: &dyn WorkspaceReader,
    workspace_root: Option<&Path>,
) -> Result<LintResult, rcmake_syntax::ParseError> {
    if src.len() > limits.max_input_bytes {
        return Err(rcmake_syntax::ParseError {
            line: 1,
            column: 0,
            command: None,
            message: "Listfile exceeds the lint input size limit.".into(),
        });
    }
    let mut sources = SourceMap::default();
    let Some(source_id) = sources.add("".into(), SourceKind::File, Arc::from(b"".as_slice()))
    else {
        return Ok(LintResult::default());
    };
    let parse_limits = Limits {
        max_file_read: limits.max_input_bytes as u64,
        ..Limits::default()
    };
    let concrete = parse_concrete_with_budget(
        src,
        source_id,
        SourceKind::File,
        &parse_limits,
        limits.max_tokens,
    )
    .map_err(|(err, _)| err)?;
    Ok(lint_concrete(
        path,
        src,
        &concrete,
        config,
        limits,
        reader,
        workspace_root,
    ))
}

/// Run all enabled lint rules on an already strictly-parsed [`ConcreteListFile`].
#[must_use]
pub fn lint_concrete(
    path: &Path,
    src: &[u8],
    concrete: &ConcreteListFile,
    config: &LintConfig,
    limits: &LintLimits,
    reader: &dyn WorkspaceReader,
    workspace_root: Option<&Path>,
) -> LintResult {
    let analyzer =
        scope::Analyzer::new(path, src, concrete, config, limits, reader, workspace_root);
    let mut result = analyzer.run(concrete);
    sort_and_dedup_findings(&mut result.findings);
    result
}

/// Sort findings deterministically by `(path, span.start, span.end, rule, message)` and
/// remove exact duplicates.
pub fn sort_and_dedup_findings(findings: &mut Vec<Finding>) {
    findings.sort_by(|a, b| {
        (
            &a.path,
            a.primary_span.start,
            a.primary_span.end,
            a.rule,
            &a.message,
        )
            .cmp(&(
                &b.path,
                b.primary_span.start,
                b.primary_span.end,
                b.rule,
                &b.message,
            ))
    });
    findings.dedup_by(|a, b| {
        a.path == b.path
            && a.primary_span == b.primary_span
            && a.rule == b.rule
            && a.message == b.message
    });
}

/// Build a SARIF 2.1.0 JSON [`Value`] for `findings` using [`rcmake_diag::sarif::build_document`].
///
/// Always emits a valid SARIF document, including when `findings` is empty, and preserves
/// distinct rule IDs and `unicodeCodePoints` column encoding.
#[must_use]
pub fn to_sarif_document(findings: &[Finding]) -> Value {
    let mut rules: Vec<SarifRule<'static>> = ALL_RULES.iter().map(|r| r.to_sarif()).collect();
    if findings.iter().any(|f| f.rule == INVALID_DIRECTIVE_RULE.id) {
        rules.push(SarifRule {
            id: INVALID_DIRECTIVE_RULE.id,
            short_description: INVALID_DIRECTIVE_RULE.short_description,
            default_level: INVALID_DIRECTIVE_RULE.default_severity.to_sarif(),
        });
    }

    let uris: Vec<Vec<u8>> = findings
        .iter()
        .map(|f| rcmake_host::path::from_native_cmake(&f.path).into())
        .collect();
    let messages: Vec<String> = findings
        .iter()
        .map(|f| match &f.suggestion {
            Some(sug) => format!("{} {sug}", f.message),
            None => f.message.clone(),
        })
        .collect();

    let results: Vec<SarifResult<'_>> = findings
        .iter()
        .zip(uris.iter())
        .zip(messages.iter())
        .map(|((finding, uri), msg)| SarifResult {
            rule_id: finding.rule,
            level: finding.severity.to_sarif(),
            message: msg.as_str(),
            uri: uri.as_slice(),
            region: Some(SarifRegion {
                start_line: finding.line,
                start_column: Some(finding.column),
                end_line: Some(finding.end_line),
                end_column: Some(finding.end_column),
            }),
        })
        .collect();

    build_document(&SarifDocument {
        tool_name: "rcmake",
        tool_version: env!("CARGO_PKG_VERSION"),
        column_kind: Some("unicodeCodePoints"),
        rules: &rules,
        results: &results,
    })
}

/// Render sorted findings in human-readable terminal format (`path:line:col: severity[rule]: message`).
#[must_use]
pub fn render_terminal(findings: &[Finding], base_dir: Option<&Path>) -> String {
    let mut out = String::new();
    for f in findings {
        let display_path = base_dir
            .and_then(|base| f.path.strip_prefix(base).ok())
            .unwrap_or(&f.path);
        out.push_str(&format!(
            "{}:{}:{}: {}[{}]: {}\n",
            display_path.display(),
            f.line,
            f.column,
            f.severity.as_str(),
            f.rule,
            f.message
        ));
        if let Some(suggestion) = &f.suggestion {
            out.push_str(&format!("  help: {suggestion}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    fn lint_single(path: &str, src: &str) -> Vec<Finding> {
        lint_bytes(
            Path::new(path),
            src.as_bytes(),
            &LintConfig::default(),
            &LintLimits::default(),
            &EmptyWorkspaceReader,
            Some(Path::new("/work")),
        )
        .expect("valid cmake")
        .findings
    }

    #[test]
    fn undefined_variable_reports_unset_and_unique_typos_but_allows_open_externals_and_defined_guards()
     {
        let src = concat!(
            "set(MY_SOURCES main.cpp)\n",
            "add_executable(app ${MY_SORUCES})\n",
            "set(TEMP 1)\n",
            "unset(TEMP)\n",
            "message(STATUS \"${TEMP}\")\n",
            "if(DEFINED TEMP)\n",
            "  message(STATUS \"${TEMP}\")\n",
            "endif()\n",
            "message(STATUS \"${EXTERNAL_USER_OPT} $ENV{HOME} $CACHE{FOO} ${CMAKE_SOURCE_DIR}\")\n",
            "message([=[${MY_SORUCES}]=])\n",
            "message(\"\\${MY_SORUCES}\")\n",
        );
        let findings = lint_single("/work/CMakeLists.txt", src);
        let undef: Vec<_> = findings
            .iter()
            .filter(|f| f.rule == "undefined-variable")
            .collect();
        assert_eq!(undef.len(), 2, "got {undef:?}");
        assert!(undef[0].message.contains("MY_SORUCES"));
        assert!(undef[0].message.contains("MY_SOURCES"));
        assert!(undef[1].message.contains("TEMP"));
        assert!(undef[1].message.contains("unset"));
    }

    #[test]
    fn glob_without_configure_depends_requires_live_def_use_chain_to_target() {
        let src = concat!(
            "file(GLOB SRCS src/*.cpp)\n",
            "add_executable(app ${SRCS})\n",
            "file(GLOB OVERWRITTEN src/*.cpp)\n",
            "set(OVERWRITTEN main.cpp)\n",
            "add_executable(app2 ${OVERWRITTEN})\n",
            "file(GLOB ASSETS assets/*.png)\n",
            "message(STATUS \"${ASSETS}\")\n",
            "file(GLOB OK_SRCS CONFIGURE_DEPENDS src/*.cpp)\n",
            "add_executable(app3 ${OK_SRCS})\n",
        );
        let findings = lint_single("/work/CMakeLists.txt", src);
        let globs: Vec<_> = findings
            .iter()
            .filter(|f| f.rule == "glob-without-configure-depends")
            .collect();
        assert_eq!(globs.len(), 1, "got {globs:?}");
        assert!(globs[0].message.contains("'SRCS'"));
        assert_eq!(globs[0].line, 1);
    }

    #[test]
    fn single_config_condition_and_unguarded_build_type_respect_guards() {
        let src = concat!(
            "if(CMAKE_BUILD_TYPE STREQUAL \"Debug\")\n",
            "  message(STATUS dbg)\n",
            "endif()\n",
            "get_property(is_multi GLOBAL PROPERTY GENERATOR_IS_MULTI_CONFIG)\n",
            "if(NOT is_multi)\n",
            "  if(CMAKE_BUILD_TYPE STREQUAL \"Debug\")\n",
            "    message(STATUS guarded)\n",
            "  endif()\n",
            "endif()\n",
            "set(CMAKE_BUILD_TYPE Release)\n",
            "set(CMAKE_BUILD_TYPE Release CACHE STRING \"ok\")\n",
            "set(CMAKE_BUILD_TYPE Release CACHE STRING \"bad\" FORCE)\n",
            "if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)\n",
            "  set(CMAKE_BUILD_TYPE Release CACHE STRING \"guarded\" FORCE)\n",
            "endif()\n",
        );
        let findings = lint_single("/work/CMakeLists.txt", src);
        let sc: Vec<_> = findings
            .iter()
            .filter(|f| f.rule == "single-config-condition")
            .collect();
        assert_eq!(sc.len(), 1, "got {sc:?}");
        assert_eq!(sc[0].line, 1);

        let bt: Vec<_> = findings
            .iter()
            .filter(|f| f.rule == "unguarded-build-type")
            .collect();
        assert_eq!(bt.len(), 2, "got {bt:?}");
        assert_eq!(bt[0].line, 10);
        assert_eq!(bt[1].line, 12);
    }

    #[test]
    fn directory_options_obsolete_command_old_policy_and_fragile_if_rules() {
        let src = concat!(
            "cmake_minimum_required(VERSION 3.2)\n",
            "cmake_minimum_required(VERSION 3.0...3.28)\n",
            "project(Demo)\n",
            "cmake_policy(SET CMP0054 OLD)\n",
            "cmake_policy(SET CMP0077 OLD)\n",
            "exec_program(ls)\n",
            "qt_wrap_cpp(OUT src.h)\n",
            "include_directories(inc)\n",
            "add_definitions(-DFOO -Wall)\n",
            "if(${FLAG} STREQUAL \"yes\")\n",
            "endif()\n",
            "if(${BARE_EXP})\n",
            "endif()\n",
            "if(DEFINED ${OK_DEF} AND BARE_VAR)\n",
            "endif()\n",
        );
        let findings = lint_single("/work/CMakeLists.txt", src);
        let by_rule =
            |rule: &str| -> Vec<&Finding> { findings.iter().filter(|f| f.rule == rule).collect() };

        let obs = by_rule("obsolete-command");
        assert_eq!(obs.len(), 4, "got {obs:?}");
        assert!(obs[0].message.contains("removed in CMake 4.0"));
        assert!(obs[1].message.contains("effective policy max 3.28"));
        assert!(obs[2].message.contains("removed in CMake 4.0"));
        assert!(obs[3].message.contains("deprecated"));

        let pol = by_rule("old-policy");
        assert_eq!(pol.len(), 2, "got {pol:?}");
        assert!(pol[0].message.contains("CMP0054") && pol[0].message.contains("removed"));
        assert!(pol[1].message.contains("CMP0077") && pol[1].message.contains("deprecated"));

        let dir_opts = by_rule("directory-options");
        assert_eq!(dir_opts.len(), 2, "got {dir_opts:?}");
        assert_eq!(dir_opts[0].severity, Severity::Note);
        assert!(
            dir_opts[1]
                .suggestion
                .as_deref()
                .is_some_and(|s| s.contains("target_compile_definitions")
                    && s.contains("target_compile_options"))
        );

        let fragile = by_rule("fragile-if-expansion");
        assert_eq!(fragile.len(), 2, "got {fragile:?}");
        assert!(fragile[0].message.contains("CMP0054"));
        assert!(fragile[1].suggestion.is_none());
    }

    #[test]
    fn comment_directives_enforce_one_command_extent_and_report_unknown_ids() {
        let src = concat!(
            "# rcmake-lint: disable=old-policy\n",
            "cmake_policy(SET CMP0077 OLD)\n",
            "cmake_policy(SET CMP0077 OLD)\n",
            "cmake_policy(SET CMP0077 OLD) # rcmake-lint: disable=old-policy\n",
            "cmake_policy(\n",
            "  # rcmake-lint: disable=old-policy\n",
            "  SET CMP0077 OLD\n",
            ")\n",
            "#[=[ rcmake-lint: disable=old-policy ]=]\n",
            "cmake_policy(SET CMP0077 OLD)\n",
            "# rcmake-lint: disable=no-such-rule\n",
            "set(X 1)\n",
        );
        let findings = lint_single("/work/CMakeLists.txt", src);
        let old_pol: Vec<_> = findings.iter().filter(|f| f.rule == "old-policy").collect();
        // Only lines 3 and 10 should trigger! (Lines 2, 4, 7 suppressed; bracket comment ignored.)
        assert_eq!(old_pol.len(), 2, "got {old_pol:?}");
        assert_eq!(old_pol[0].line, 3);
        assert_eq!(old_pol[1].line, 10);

        let invalid: Vec<_> = findings
            .iter()
            .filter(|f| f.rule == INVALID_DIRECTIVE_RULE.id)
            .collect();
        assert_eq!(invalid.len(), 1);
        assert!(invalid[0].message.contains("no-such-rule"));
    }

    #[test]
    fn source_order_include_uses_open_buffers_over_disk() {
        let host = PureHost::default();
        host.seed_file(Path::new("/work/helpers.cmake"), b"set(OLD_HELPER 1)\n")
            .unwrap();
        // On disk, helpers.cmake does NOT define MY_HELPER_VAR, so MY_HELPER_VAR looks like a
        // typo of MY_HELPER_VA (edit distance 1).
        let caller =
            "include(helpers.cmake)\nset(MY_HELPER_VA 1)\nmessage(STATUS \"${MY_HELPER_VAR}\")\n";
        let disk_reader = HostWorkspaceReader::new(&host, vec![PathBuf::from("/work")], None);
        let before = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            caller.as_bytes(),
            &LintConfig::default(),
            &LintLimits::default(),
            &disk_reader,
            Some(Path::new("/work")),
        )
        .unwrap();
        assert_eq!(before.findings.len(), 1);
        assert!(
            before
                .dependencies
                .included_files
                .contains(Path::new("/work/helpers.cmake"))
        );

        // Now supply an unsaved open buffer for helpers.cmake that defines MY_HELPER_VAR!
        let mut live_reader = HostWorkspaceReader::new(&host, vec![PathBuf::from("/work")], None);
        live_reader.insert_open_buffer(
            PathBuf::from("/work/helpers.cmake"),
            Arc::from(b"set(MY_HELPER_VAR 42)\n".as_slice()),
        );
        let after = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            caller.as_bytes(),
            &LintConfig::default(),
            &LintLimits::default(),
            &live_reader,
            Some(Path::new("/work")),
        )
        .unwrap();
        assert!(after.findings.is_empty(), "got {:?}", after.findings);
    }

    #[test]
    fn sarif_output_works_for_empty_and_populated_findings() {
        let empty = to_sarif_document(&[]);
        let empty_json = rcmake_json::write_compact(&empty).to_string();
        assert!(empty_json.contains(r#""version":"2.1.0""#));
        assert!(empty_json.contains(r#""columnKind":"unicodeCodePoints""#));
        assert!(empty_json.contains(r#""results":[]"#));
        assert!(empty_json.contains(r#""id":"undefined-variable""#));

        let findings = lint_single("/work/CMakeLists.txt", "cmake_policy(SET CMP0077 OLD)\n");
        let doc = to_sarif_document(&findings);
        let json = rcmake_json::write_compact(&doc).to_string();
        assert!(json.contains(r#""ruleId":"old-policy""#));
        assert!(json.contains(r#""uri":"/work/CMakeLists.txt""#));
    }

    #[test]
    fn deeply_nested_if_and_many_elseif_branches_stay_strictly_bounded() {
        let mut nested = String::new();
        for i in 0..300 {
            nested.push_str(&format!("set(VAR_{i} {i})\n"));
        }
        for _ in 0..400 {
            nested.push_str("if(ON)\n");
        }
        nested.push_str("message(STATUS \"${VAR_0}\")\n");
        for _ in 0..400 {
            nested.push_str("endif()\n");
        }
        let limits = LintLimits {
            max_flow_depth: 64,
            max_total_scope_entries: 4096,
            max_total_scope_bytes: 256 * 1024,
            ..LintLimits::default()
        };
        let res = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            nested.as_bytes(),
            &LintConfig::default(),
            &limits,
            &EmptyWorkspaceReader,
            Some(Path::new("/work")),
        )
        .unwrap();
        assert!(res.scope_truncated, "expected nested scope truncation");

        // Many elseif branches with many bindings join incrementally without storing every branch.
        let mut chain = String::new();
        for i in 0..200 {
            chain.push_str(&format!("set(B_{i} {i})\n"));
        }
        chain.push_str("if(C0)\n  set(OUT 1)\n");
        for i in 1..200 {
            chain.push_str(&format!("elseif(C{i})\n  set(OUT {i})\n"));
        }
        chain.push_str("else()\n  set(OUT 0)\nendif()\nmessage(STATUS \"${OUT}\")\n");
        let chain_res = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            chain.as_bytes(),
            &LintConfig::default(),
            &limits,
            &EmptyWorkspaceReader,
            Some(Path::new("/work")),
        )
        .unwrap();
        assert!(
            chain_res.findings.is_empty(),
            "got {:?}",
            chain_res.findings
        );
    }

    #[test]
    fn invalid_directives_are_charged_against_max_findings_and_report_truncation() {
        let mut src = String::new();
        for i in 0..50 {
            src.push_str(&format!("# rcmake-lint: disable=unknown-rule-{i}\n"));
        }
        src.push_str("cmake_policy(SET CMP0077 OLD)\n");
        let limits = LintLimits {
            max_findings: 5,
            ..LintLimits::default()
        };
        let res = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            src.as_bytes(),
            &LintConfig::default(),
            &limits,
            &EmptyWorkspaceReader,
            Some(Path::new("/work")),
        )
        .unwrap();
        assert_eq!(res.findings.len(), 5);
        assert!(res.findings_truncated);
    }

    #[test]
    fn unresolved_include_invalidates_glob_origins_to_prevent_false_positives() {
        let src = concat!(
            "file(GLOB SRCS src/*.cpp)\n",
            "include(missing_override.cmake)\n",
            "add_executable(app ${SRCS})\n",
            "file(GLOB DYN_SRCS src/*.cpp)\n",
            "include(\"${DYNAMIC_FILE}\")\n",
            "add_executable(app2 ${DYN_SRCS})\n",
            "file(GLOB SET_DYN src/*.cpp)\n",
            "set(${TARGET_VAR} main.cpp)\n",
            "add_executable(app3 ${SET_DYN})\n",
        );
        let res = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            src.as_bytes(),
            &LintConfig::default(),
            &LintLimits::default(),
            &EmptyWorkspaceReader,
            Some(Path::new("/work")),
        )
        .unwrap();
        let globs: Vec<_> = res
            .findings
            .iter()
            .filter(|f| f.rule == "glob-without-configure-depends")
            .collect();
        assert!(
            globs.is_empty(),
            "expected no false glob finding: {globs:?}"
        );
        assert!(res.dependencies.incomplete);
    }

    #[test]
    fn probe_cap_marks_dependencies_incomplete_without_dropping_consumed_includes() {
        let host = PureHost::default();
        host.seed_file(Path::new("/work/real_helper.inc"), b"set(REAL_VAR 1)\n")
            .unwrap();
        let mut src = String::new();
        for i in 0..20 {
            src.push_str(&format!("include(missing_{i}.cmake OPTIONAL)\n"));
        }
        src.push_str("include(real_helper.inc)\n");
        src.push_str("set(REAL_VA 1)\nmessage(STATUS \"${REAL_VAR}\")\n");

        let reader = HostWorkspaceReader::new(&host, vec![PathBuf::from("/work")], None);
        let limits = LintLimits {
            max_include_files: 2, // probe miss cap = 8
            ..LintLimits::default()
        };
        let res = lint_bytes(
            Path::new("/work/CMakeLists.txt"),
            src.as_bytes(),
            &LintConfig::default(),
            &limits,
            &reader,
            Some(Path::new("/work")),
        )
        .unwrap();
        assert!(res.findings.is_empty(), "got {:?}", res.findings);
        assert!(res.dependencies.incomplete);
        assert!(
            res.dependencies
                .consumed_files
                .contains(Path::new("/work/real_helper.inc")),
            "consumed_files={:?}",
            res.dependencies.consumed_files
        );
        assert!(
            res.dependencies
                .included_files
                .contains(Path::new("/work/real_helper.inc")),
            "included_files={:?}",
            res.dependencies.included_files
        );
    }
}
