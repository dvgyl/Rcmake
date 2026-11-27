//! Explicit-rule, span-aware SARIF 2.1.0 document builder for native `rcmake` tools.
//!
//! CMake compatibility diagnostics continue to use [`crate::DiagEngine::sarif_json`] unchanged.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_json::Value;

/// Official SARIF 2.1.0 JSON schema URI.
pub const SARIF_SCHEMA_URI: &str =
    "https://docs.oasis-open.org/sarif/sarif/v2.1.0/errata01/os/schemas/sarif-schema-2.1.0.json";

/// SARIF format version string (`2.1.0`).
pub const SARIF_VERSION: &str = "2.1.0";

/// SARIF severity level for a rule or result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SarifLevel {
    /// Fatal or build-blocking error (`"error"`).
    Error,
    /// Actionable warning (`"warning"`).
    Warning,
    /// Informational or advisory note (`"note"`).
    Note,
}

impl SarifLevel {
    /// Wire-format SARIF level string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
        }
    }
}

/// Rule descriptor placed under `runs[0].tool.driver.rules`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SarifRule<'a> {
    /// Stable rule identifier (for example `undefined-variable`).
    pub id: &'a str,
    /// Short human-readable summary of the rule.
    pub short_description: &'a str,
    /// Default reporting level for the rule.
    pub default_level: SarifLevel,
}

/// 1-based source region attached to a SARIF physical location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SarifRegion {
    /// 1-based starting line number (`0` omits the region).
    pub start_line: u32,
    /// Optional 1-based starting column number.
    pub start_column: Option<u32>,
    /// Optional 1-based ending line number.
    pub end_line: Option<u32>,
    /// Optional 1-based ending column number.
    pub end_column: Option<u32>,
}

/// Single diagnostic entry in `runs[0].results`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SarifResult<'a> {
    /// Explicit rule identifier matching a [`SarifRule`].
    pub rule_id: &'a str,
    /// Effective severity level for this finding.
    pub level: SarifLevel,
    /// Human-readable message text.
    pub message: &'a str,
    /// Artifact URI or slash-normalized path bytes.
    pub uri: &'a [u8],
    /// Optional source region within `uri`.
    pub region: Option<SarifRegion>,
}

/// Complete single-run SARIF 2.1.0 document descriptor.
#[derive(Clone, Copy, Debug)]
pub struct SarifDocument<'a> {
    /// Tool driver name (for example `rcmake`).
    pub tool_name: &'a str,
    /// Tool driver version string.
    pub tool_version: &'a str,
    /// Optional SARIF `columnKind` (for example `"utf16CodeUnits"` or `"unicodeCodePoints"`).
    pub column_kind: Option<&'a str>,
    /// Rule metadata catalog.
    pub rules: &'a [SarifRule<'a>],
    /// Ordered findings to emit (may be empty).
    pub results: &'a [SarifResult<'a>],
}

/// Build a SARIF 2.1.0 [`Value`] from an explicit rule catalog and result slice.
#[must_use]
pub fn build_document(doc: &SarifDocument<'_>) -> Value {
    let mut driver = BTreeMap::new();
    driver.insert(BString::from("name"), Value::string(doc.tool_name));
    driver.insert(BString::from("version"), Value::string(doc.tool_version));
    if !doc.rules.is_empty() {
        let rules = doc
            .rules
            .iter()
            .map(|rule| {
                Value::object_from([
                    (
                        "defaultConfiguration",
                        Value::object_from([("level", Value::string(rule.default_level.as_str()))]),
                    ),
                    ("id", Value::string(rule.id)),
                    (
                        "shortDescription",
                        Value::object_from([("text", Value::string(rule.short_description))]),
                    ),
                ])
            })
            .collect();
        driver.insert(BString::from("rules"), Value::Array(rules));
    }

    let results = doc.results.iter().map(build_result).collect();

    let mut run = BTreeMap::new();
    if let Some(column_kind) = doc.column_kind {
        run.insert(BString::from("columnKind"), Value::string(column_kind));
    }
    run.insert(BString::from("results"), Value::Array(results));
    run.insert(
        BString::from("tool"),
        Value::object_from([("driver", Value::Object(driver))]),
    );

    Value::object_from([
        ("$schema", Value::string(SARIF_SCHEMA_URI)),
        ("runs", Value::array_from([Value::Object(run)])),
        ("version", Value::string(SARIF_VERSION)),
    ])
}

fn build_result(result: &SarifResult<'_>) -> Value {
    let mut physical = BTreeMap::from([(
        BString::from("artifactLocation"),
        Value::object_from([("uri", Value::string(result.uri))]),
    )]);
    if let Some(region) = result.region.filter(|region| region.start_line > 0) {
        let mut reg = BTreeMap::from([(
            BString::from("startLine"),
            Value::UInt(u64::from(region.start_line)),
        )]);
        if let Some(col) = region.start_column.filter(|&col| col > 0) {
            reg.insert(BString::from("startColumn"), Value::UInt(u64::from(col)));
        }
        if let Some(line) = region.end_line.filter(|&line| line > 0) {
            reg.insert(BString::from("endLine"), Value::UInt(u64::from(line)));
        }
        if let Some(col) = region.end_column.filter(|&col| col > 0) {
            reg.insert(BString::from("endColumn"), Value::UInt(u64::from(col)));
        }
        physical.insert(BString::from("region"), Value::Object(reg));
    }

    let location = Value::object_from([("physicalLocation", Value::Object(physical))]);
    Value::object_from([
        ("level", Value::string(result.level.as_str())),
        ("locations", Value::array_from([location])),
        (
            "message",
            Value::object_from([("text", Value::string(result.message))]),
        ),
        ("ruleId", Value::string(result.rule_id)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_explicit_rule_sarif_with_empty_and_non_empty_results() {
        let rules = [SarifRule {
            id: "old-policy",
            short_description: "Avoid setting policies to OLD.",
            default_level: SarifLevel::Warning,
        }];
        let empty = build_document(&SarifDocument {
            tool_name: "rcmake",
            tool_version: "0.1.0",
            column_kind: Some("unicodeCodePoints"),
            rules: &rules,
            results: &[],
        });
        let empty_json = rcmake_json::write_compact(&empty).to_string();
        assert!(empty_json.contains(r#""version":"2.1.0""#));
        assert!(empty_json.contains(r#""columnKind":"unicodeCodePoints""#));
        assert!(empty_json.contains(r#""results":[]"#));
        assert!(empty_json.contains(r#""id":"old-policy""#));

        let findings = [SarifResult {
            rule_id: "old-policy",
            level: SarifLevel::Error,
            message: "Policy CMP0054 is set to OLD.",
            uri: b"CMakeLists.txt",
            region: Some(SarifRegion {
                start_line: 3,
                start_column: Some(18),
                end_line: Some(3),
                end_column: Some(25),
            }),
        }];
        let populated = build_document(&SarifDocument {
            tool_name: "rcmake",
            tool_version: "0.1.0",
            column_kind: Some("unicodeCodePoints"),
            rules: &rules,
            results: &findings,
        });
        let json = rcmake_json::write_compact(&populated).to_string();
        assert!(json.contains(r#""ruleId":"old-policy""#));
        assert!(json.contains(r#""level":"error""#));
        assert!(json.contains(r#""startLine":3"#));
        assert!(json.contains(r#""startColumn":18"#));
        assert!(json.contains(r#""endLine":3"#));
        assert!(json.contains(r#""endColumn":25"#));
    }
}
