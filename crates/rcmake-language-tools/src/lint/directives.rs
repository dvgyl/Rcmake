//! Parser and command-attachment logic for `# rcmake-lint: disable=<rule>` directives.
//!
//! Only real [`TokenKind::LineComment`] tokens are recognized; directive-like text
//! inside arguments or bracket comments is ignored. Each directive has a strict
//! one-command extent:
//! - Inside a command's `(...)`: applies to that containing command.
//! - Trailing on the same physical line after a command's `)`: applies to that command.
//! - Standalone before a command: applies to the immediately following command.

#![forbid(unsafe_code)]

use core::ops::Range;
use std::collections::{BTreeMap, BTreeSet};

use rcmake_syntax::concrete::{ConcreteListFile, TokenKind};

use crate::rules_meta::RuleId;

/// An unknown rule ID or malformed `# rcmake-lint:` directive found in a line comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectiveIssue {
    pub(crate) span: Range<usize>,
    pub(crate) message: String,
    pub(crate) suggestion: Option<String>,
}

/// Parsed per-command rule suppressions and directive validation issues for one listfile.
#[derive(Clone, Debug, Default)]
pub(crate) struct DirectiveMap {
    /// Map from `command_index` to set of suppressed [`RuleId`]s.
    suppressed: BTreeMap<usize, BTreeSet<RuleId>>,
    /// Malformed or unknown directives to emit as findings (bounded by `max_issues`).
    pub(crate) issues: Vec<DirectiveIssue>,
    /// `true` if additional invalid directive issues were dropped at `max_issues`.
    pub(crate) issues_truncated: bool,
}

impl DirectiveMap {
    /// Parse all `# rcmake-lint:` line-comment directives in `concrete`, retaining at most
    /// `max_issues` invalid-directive diagnostics.
    pub(crate) fn parse(src: &[u8], concrete: &ConcreteListFile, max_issues: usize) -> Self {
        let mut map = Self::default();
        let tokens = &concrete.tokens;
        let commands = &concrete.commands;

        // Track which command each token is inside, trailing after, or preceding.
        let mut cmd_idx = 0usize;
        let mut last_finished_cmd: Option<(usize, u32)> = None; // (command_index, close_line)
        let mut pending_for_next: Vec<RuleId> = Vec::new();

        for (tok_idx, tok) in tokens.iter().enumerate() {
            while let Some(cmd) = commands.get(cmd_idx) {
                if tok_idx > cmd.close_paren_token {
                    let close_line = tokens.get(cmd.close_paren_token).map_or(0, |t| t.end_line);
                    last_finished_cmd = Some((cmd.command_index, close_line));
                    cmd_idx += 1;
                } else {
                    break;
                }
            }

            if let Some(cmd) = commands.get(cmd_idx)
                && tok_idx == cmd.name_token
                && !pending_for_next.is_empty()
            {
                let entry = map.suppressed.entry(cmd.command_index).or_default();
                entry.extend(pending_for_next.drain(..));
            }

            if tok.kind != TokenKind::LineComment {
                continue;
            }

            let raw = tok.slice(src);
            let Some(after_hash) = raw.strip_prefix(b"#") else {
                continue;
            };
            let lead_ws = after_hash
                .iter()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count();
            let trimmed = after_hash.get(lead_ws..).unwrap_or_default();
            let Some(after_prefix) = trimmed.strip_prefix(b"rcmake-lint:") else {
                continue;
            };
            let prefix_end_offset = tok.span.start + 1 + lead_ws + b"rcmake-lint:".len();
            let ws_after_colon = after_prefix
                .iter()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count();
            let body = after_prefix.get(ws_after_colon..).unwrap_or_default();
            let body_offset = prefix_end_offset + ws_after_colon;

            let Some(after_disable) = body.strip_prefix(b"disable=") else {
                if map.issues.len() < max_issues {
                    map.issues.push(DirectiveIssue {
                        span: body_offset..tok.span.end,
                        message: "Malformed rcmake-lint directive; expected 'disable=<rule>'."
                            .into(),
                        suggestion: Some(
                            "Use '# rcmake-lint: disable=<rule-id>' with a known rule identifier."
                                .into(),
                        ),
                    });
                } else {
                    map.issues_truncated = true;
                }
                continue;
            };

            let rules_offset = body_offset + b"disable=".len();
            // Stop at whitespace or trailing comment separator so `# rcmake-lint: disable=old-policy -- reason` works.
            let rules_len = after_disable
                .iter()
                .position(|&b| b == b' ' || b == b'\t' || b == b'\r' || b == b'#')
                .unwrap_or(after_disable.len());
            let rules_slice = after_disable.get(..rules_len).unwrap_or_default();
            if rules_slice.is_empty() {
                if map.issues.len() < max_issues {
                    map.issues.push(DirectiveIssue {
                        span: body_offset..rules_offset,
                        message: "Empty rule list in '# rcmake-lint: disable=' directive.".into(),
                        suggestion: Some("Specify at least one valid lint rule ID.".into()),
                    });
                } else {
                    map.issues_truncated = true;
                }
                continue;
            }

            let mut parsed_rules = Vec::new();
            let mut rel = 0usize;
            for part in rules_slice.split(|&b| b == b',') {
                let part_start = rules_offset + rel;
                let part_end = part_start + part.len();
                rel += part.len() + 1;
                let name = core::str::from_utf8(part).unwrap_or("");
                if let Some(rule) = RuleId::parse(name) {
                    parsed_rules.push(rule);
                } else if map.issues.len() < max_issues {
                    let display = String::from_utf8_lossy(part);
                    let valid = crate::rules_meta::ALL_RULES
                        .iter()
                        .map(|r| r.as_str())
                        .collect::<Vec<_>>();
                    let suggestion = rcmake_diag::suggest::best_match_in(
                        rcmake_diag::suggest::Namespace::Query,
                        part,
                        &valid,
                    )
                    .map(|(best, _)| format!("Did you mean '{}'?", String::from_utf8_lossy(&best)));
                    map.issues.push(DirectiveIssue {
                        span: part_start..part_end.max(part_start + 1).min(tok.span.end),
                        message: format!("Unknown lint rule '{display}' in rcmake-lint directive."),
                        suggestion,
                    });
                } else {
                    map.issues_truncated = true;
                }
            }

            if parsed_rules.is_empty() {
                continue;
            }

            // Determine one-command target:
            // 1. Inside an active command's `(...)` -> attaches to that command.
            // 2. Trailing on the same physical line after a command's `)` -> attaches to that command.
            // 3. Standalone comment -> attaches to the next command.
            if let Some(cmd) = commands.get(cmd_idx)
                && tok_idx > cmd.name_token
                && tok_idx < cmd.close_paren_token
            {
                map.suppressed
                    .entry(cmd.command_index)
                    .or_default()
                    .extend(parsed_rules);
            } else if let Some((prev_cmd, prev_close_line)) = last_finished_cmd
                && tok.start_line == prev_close_line
            {
                map.suppressed
                    .entry(prev_cmd)
                    .or_default()
                    .extend(parsed_rules);
            } else {
                pending_for_next.extend(parsed_rules);
            }
        }

        map
    }

    /// Return `true` if `rule` is suppressed on `command_index`.
    #[must_use]
    pub(crate) fn is_suppressed(&self, command_index: usize, rule: RuleId) -> bool {
        self.suppressed
            .get(&command_index)
            .is_some_and(|rules| rules.contains(&rule))
    }
}
