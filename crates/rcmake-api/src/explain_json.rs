use core::fmt::Write as _;

use rcmake_base::BString;
use rcmake_diag::{Backtrace, FrameKind};
use rcmake_eval::ValuePreview;
use rcmake_json::{Value, write_compact};
use rcmake_model::BT;

use crate::explain_render::{chain_kind, prop_op, usage_scope, var_op};
use crate::{
    ExplainConfig, ExplainReport, ExplainTarget, ExplainVariable, PreviousValue, SourceLocation,
};

/// Schema identifier for machine-readable explanations.
pub const EXPLAIN_SCHEMA: &str = "rcmake-explain-v1";

/// Render a report as compact JSON in the [EXPLAIN_SCHEMA] schema.
#[must_use]
pub fn render_explain_json(report: &ExplainReport, config: Option<&[u8]>) -> BString {
    write_compact(&explain_json(report, config))
}

/// Convert a report to a JSON value for command-line and editor clients.
///
/// The top-level object has `schema`, `kind`, `name`, and `details` fields.
/// Configuration filtering affects only target configuration records. A byte
/// string with invalid UTF-8 is an object with a lowercase `bytes_hex` string.
/// A JSON null records an unavailable optional field.
#[must_use]
pub fn explain_json(report: &ExplainReport, config: Option<&[u8]>) -> Value {
    let (kind, name, details) = match report {
        ExplainReport::Target(target) => ("target", &target.name, target_json(target, config)),
        ExplainReport::Variable(variable) => ("variable", &variable.name, variable_json(variable)),
        ExplainReport::Ambiguous { name } => ("ambiguous", name, object([])),
        ExplainReport::NotFound { name, suggestions } => (
            "not_found",
            name,
            object([(
                "suggestions",
                array(suggestions.iter().map(|name| bytes(name))),
            )]),
        ),
    };
    object([
        ("schema", Value::string(EXPLAIN_SCHEMA)),
        ("kind", Value::string(kind)),
        ("name", bytes(name)),
        ("details", details),
    ])
}

fn target_json(target: &ExplainTarget, config: Option<&[u8]>) -> Value {
    let configs = target
        .configurations
        .iter()
        .filter(|entry| config.is_none_or(|name| name.eq_ignore_ascii_case(&entry.name)))
        .map(config_json);
    let properties = target.properties.iter().map(|property| {
        object([
            ("target", bytes(&property.target)),
            ("name", bytes(&property.name)),
            (
                "writes",
                array(property.writes.iter().map(|event| {
                    object([
                        ("sequence", Value::UInt(event.seq)),
                        ("operation", Value::string(prop_op(event.op))),
                        ("origin", trace(&event.origin)),
                    ])
                })),
            ),
        ])
    });
    object([
        ("target", bytes(&target.target)),
        ("target_kind", Value::string(target.kind.as_str())),
        ("definition", trace(&target.definition)),
        (
            "alias_definition",
            optional(target.alias_definition.as_ref().map(location)),
        ),
        ("sources", array(target.sources.iter().map(tagged))),
        ("configurations", array(configs)),
        ("properties", array(properties)),
        (
            "property_history_incomplete",
            optional(target.property_history_incomplete.map(Value::Bool)),
        ),
        ("provisional", Value::Bool(target.provisional)),
    ])
}

fn config_json(config: &ExplainConfig) -> Value {
    let entries = config.entries.iter().map(|entry| {
        object([
            ("kind", Value::string(chain_kind(entry.kind))),
            ("value", bytes(&entry.value)),
            ("origin", trace(&entry.origin)),
            (
                "chain",
                optional(entry.chain.as_ref().map(|hops| {
                    array(hops.iter().map(|hop| {
                        object([
                            (
                                "target",
                                optional(hop.target.as_ref().map(|name| bytes(name))),
                            ),
                            ("target_id", Value::UInt(hop.id.index() as u64)),
                            ("scope", Value::string(usage_scope(hop.via))),
                            ("origin", trace(&hop.origin)),
                        ])
                    }))
                })),
            ),
        ])
    });
    object([
        ("name", bytes(&config.name)),
        ("sources", array(config.sources.iter().map(tagged))),
        ("entries", array(entries)),
    ])
}

fn variable_json(variable: &ExplainVariable) -> Value {
    let cache = variable.cache.as_ref().map(|cache| {
        object([
            ("value", bytes(&cache.value)),
            ("type", Value::string(cache.kind.as_str())),
            ("help", bytes(&cache.help)),
            ("force", optional(cache.force.map(Value::Bool))),
        ])
    });
    let writes = variable.writes.iter().map(|event| {
        let previous = match &event.previous {
            PreviousValue::Unknown => object([("state", Value::string("unknown"))]),
            PreviousValue::Unset => object([("state", Value::string("unset"))]),
            PreviousValue::Value(value) => object([
                ("state", Value::string("value")),
                ("preview", preview(value)),
            ]),
        };
        object([
            ("sequence", Value::UInt(event.sequence)),
            ("operation", Value::string(var_op(event.operation))),
            (
                "command",
                optional(event.command.as_ref().map(|name| bytes(name))),
            ),
            (
                "directory_id",
                optional(event.directory.map(|id| Value::UInt(id.index() as u64))),
            ),
            ("scope_depth", Value::UInt(u64::from(event.scope_depth))),
            ("previous", previous),
            ("value", optional(event.value.as_ref().map(preview))),
            ("origin", trace(&event.origin)),
        ])
    });
    object([
        (
            "value",
            optional(variable.value.as_ref().map(|value| bytes(value))),
        ),
        ("cache", optional(cache)),
        ("writes", array(writes)),
        (
            "history_incomplete",
            optional(variable.history_incomplete.map(Value::Bool)),
        ),
        ("provisional", Value::Bool(variable.provisional)),
    ])
}

fn tagged(entry: &BT<BString>) -> Value {
    object([
        ("value", bytes(&entry.value)),
        ("origin", trace(&entry.backtrace)),
    ])
}

fn trace(trace: &Backtrace) -> Value {
    array(trace.frames().map(|frame| {
        object([
            ("path", bytes(&frame.file)),
            ("line", Value::UInt(u64::from(frame.line))),
            ("command", optional(frame.name.as_deref().map(bytes))),
            (
                "kind",
                Value::string(match frame.kind {
                    FrameKind::Command => "command",
                    FrameKind::Deferred => "deferred",
                    FrameKind::ModuleInclude => "module_include",
                }),
            ),
        ])
    }))
}

fn location(location: &SourceLocation) -> Value {
    object([
        ("path", bytes(&location.path)),
        ("line", Value::UInt(u64::from(location.line))),
        (
            "byte_offset",
            optional(
                location
                    .byte_offset
                    .map(|offset| Value::UInt(u64::from(offset))),
            ),
        ),
    ])
}

fn preview(value: &ValuePreview) -> Value {
    object([
        ("prefix", bytes(&value.prefix)),
        ("byte_length", Value::UInt(value.len as u64)),
        ("hash", Value::string(format!("{:016x}", value.hash))),
    ])
}

fn bytes(value: &[u8]) -> Value {
    if let Ok(utf8) = core::str::from_utf8(value) {
        return Value::string(utf8);
    }
    let mut hex = String::new();
    for byte in value {
        let _ = write!(hex, "{byte:02x}");
    }
    object([("bytes_hex", Value::string(hex))])
}

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn array(values: impl IntoIterator<Item = Value>) -> Value {
    Value::Array(values.into_iter().collect())
}

fn optional(value: Option<Value>) -> Value {
    value.unwrap_or(Value::Null)
}
