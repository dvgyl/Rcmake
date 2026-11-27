//! Lossless formatter, static linter, and `.rcmake.toml` configuration loader for CMake listfiles.

#![forbid(unsafe_code)]

pub mod config;
pub mod discovery;
pub mod fmt;
pub mod line_index;
pub mod lint;
pub mod rules_meta;

pub use config::{
    CommandCase, Config, ConfigCache, ConfigError, EndOfLine, FmtConfig, LintConfig,
    ResolvedConfig, parse_config, resolve_config,
};
pub use fmt::{
    FormatError, FormatLimits, FormatResult, FormatStyle, TextEdit, VerifyError, apply_edits,
    format_bytes, format_concrete, unified_diff, verify_formatted,
};
pub use line_index::{LineCol, LineIndex, display_width};
pub use lint::{
    EmptyWorkspaceReader, Finding, HostWorkspaceReader, LintDependencies, LintLimits, LintResult,
    WorkspaceReader, lint_bytes, lint_concrete, render_terminal, sort_and_dedup_findings,
    to_sarif_document,
};
pub use rules_meta::{ALL_RULES, INVALID_DIRECTIVE_RULE, RuleId, RuleInfo, Severity};
