use core::fmt::Write as _;

use rcmake_base::BString;
use rcmake_diag::{Backtrace, FrameKind};
use rcmake_eval::ValuePreview;
use rcmake_json::{Value, write_compact};
use rcmake_model::BT;

use crate::explain_entries::link_role;
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
/// # Schema
///
/// The top-level object contains `schema`, `kind`, `name`, and `details`.
/// The value of `kind` selects the fields in `details`.
///
/// - `target` contains `target`, `target_kind`, `definition`,
///   `alias_definition`, `sources`, `filter`, `configuration_selection`,
///   `configurations`, `properties`, `property_history_incomplete`, and
///   `provisional`. Configuration selection contains `requested`, `available`,
///   and `status`. Status is `all`, `no_resolved_configurations`, `matched`, or
///   `unavailable`. An empty configuration name is the default configuration.
///   Each selected configuration has `name`, `sources`, `compile_groups`,
///   and `entries`. Entry positions index the compile groups and preserve
///   the order within each group. A chain is an ordered array of link hops.
///   An empty chain is direct. A null chain means provenance is unavailable.
///   A property record lists the configurations that use the contributor.
///   Its writes apply across configurations. Each write includes an old and
///   new value preview when a value exists. The property record also has
///   `dropped_writes`, which counts discarded events for that property.
/// - `variable` contains `value`, `location`, `binding`, `directory`, `cache`,
///   `writes`, `history`, `session_history_truncated`, and `provisional`.
///   Cache entries include their type, help, advanced flag and optional strings.
///   History is `unavailable`, `excluded`, `complete`, `truncated`, or
///   `unknown`. Complete history can have zero writes. A write's `previous`
///   field has a `state` of `unknown`, `unset`, or `value`. The value state
///   includes `preview`. A preview contains `prefix`, `byte_length`,
///   `truncated`, and a fixed-width lowercase hexadecimal 64-bit FNV-1a
///   hash of the full value. A null write value indicates removal.
/// - `ambiguous` contains `kinds`, with values `target` and `variable`.
/// - `not_found` contains ordered `suggestions`. Each has `name` and `kind`.
/// - `invalid_directory` contains the invalid `directory_id`.
///
/// Event `sequence` and variable `scope_id` values are unsigned 64-bit
/// integers encoded as decimal
/// strings. Parse them as `u64` or JavaScript `BigInt`. Source IDs, directory
/// IDs, line numbers, and offsets are JSON numbers. Lines are one-based with
/// zero for an unknown line. Byte ranges use zero-based, half-open offsets.
/// Trace arrays list the innermost frame first. Each frame has `path`, `line`,
/// `command`, and `kind`.
///
/// Byte strings are JSON strings when valid UTF-8. Other byte strings are
/// objects with a lowercase `bytes_hex` field. The encoder preserves the
/// input bytes. A Host path conversion can be lossy outside Unix.
/// Null represents an unavailable optional field unless stated above. Empty
/// arrays represent zero retained records.
///
/// Readers should accept additional fields. Changes to existing field types
/// or meanings require a new schema identifier.
#[must_use]
pub fn explain_json(report: &ExplainReport, config: Option<&[u8]>) -> Value {
    let (kind, name, details) = match report {
        ExplainReport::Target(target) => ("target", &target.name, target_json(target, config)),
        ExplainReport::Variable(variable) => ("variable", &variable.name, variable_json(variable)),
        ExplainReport::Ambiguous { name, kinds } => (
            "ambiguous",
            name,
            object([(
                "kinds",
                array(kinds.iter().map(|kind| Value::string(kind.as_str()))),
            )]),
        ),
        ExplainReport::NotFound { name, suggestions } => (
            "not_found",
            name,
            object([(
                "suggestions",
                array(suggestions.iter().map(|item| {
                    object([
                        ("name", bytes(&item.name)),
                        ("kind", Value::string(item.kind.as_str())),
                    ])
                })),
            )]),
        ),
        ExplainReport::InvalidDirectory { name, directory } => (
            "invalid_directory",
            name,
            object([("directory_id", Value::UInt(directory.index() as u64))]),
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
    let configs = target.configurations_for(config).map(config_json);
    let status = match config {
        Some(_) if target.configurations_for(config).next().is_none() => "unavailable",
        Some(_) => "matched",
        None if target.configurations.is_empty() => "no_resolved_configurations",
        None => "all",
    };
    let selection = object([
        ("requested", optional(config.map(bytes))),
        (
            "available",
            array(
                target
                    .configurations
                    .iter()
                    .map(|config| bytes(&config.name)),
            ),
        ),
        ("status", Value::string(status)),
    ]);
    let properties = target.properties_for(config).map(|property| {
        object([
            ("target", bytes(&property.target)),
            ("name", bytes(&property.name)),
            (
                "configurations",
                array(property.configurations.iter().map(|name| bytes(name))),
            ),
            (
                "dropped_writes",
                Value::UInt(u64::from(property.dropped_writes)),
            ),
            (
                "writes",
                array(property.writes.iter().map(|event| {
                    object([
                        ("sequence", Value::string(event.seq.to_string())),
                        ("operation", Value::string(prop_op(event.op))),
                        (
                            "previous",
                            optional(event.old_value.as_ref().map(|value| {
                                preview_fields(value.prefix(), value.len(), value.hash())
                            })),
                        ),
                        (
                            "value",
                            optional(event.new_value.as_ref().map(|value| {
                                preview_fields(value.prefix(), value.len(), value.hash())
                            })),
                        ),
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
        (
            "filter",
            optional(target.filter.as_ref().map(|value| bytes(value))),
        ),
        (
            "filter_kind",
            optional(
                target
                    .typed_filter
                    .as_ref()
                    .map(|filter| Value::string(filter.kind())),
            ),
        ),
        (
            "dependency",
            optional(
                target
                    .dependency
                    .as_deref()
                    .map(|report| dependency_json(report, config)),
            ),
        ),
        ("configuration_selection", selection),
        ("configurations", array(configs)),
        ("properties", array(properties)),
        (
            "property_history_incomplete",
            optional(target.property_history_incomplete.map(Value::Bool)),
        ),
        ("provisional", Value::Bool(target.provisional)),
    ])
}

fn dependency_json(report: &crate::DependencyReport, config: Option<&[u8]>) -> Value {
    let paths = report.paths_for(config).map(|path| {
        object([
            ("config", bytes(&path.config)),
            ("link_only", Value::Bool(path.link_only)),
            ("order_only", Value::Bool(path.order_only)),
            (
                "hops",
                array(path.hops.iter().map(|hop| {
                    let (edge, scope, link_only) = match hop.edge {
                        crate::DependencyEdge::Link { scope, link_only } => {
                            ("link", Some(scope_name(scope)), link_only)
                        }
                        crate::DependencyEdge::OrderOnly => ("order_only", None, false),
                    };
                    object([
                        ("from", bytes(&hop.from)),
                        ("to", bytes(&hop.to)),
                        ("edge", Value::string(edge)),
                        ("scope", optional(scope.map(Value::string))),
                        ("link_only", Value::Bool(link_only)),
                        ("origin", trace(&hop.origin)),
                    ])
                })),
            ),
        ])
    });
    object([
        ("target", bytes(&report.target)),
        (
            "resolved",
            optional(report.resolved.as_ref().map(|name| bytes(name))),
        ),
        ("paths", array(paths)),
        ("truncated", Value::Bool(report.truncated)),
        ("cycles", Value::UInt(u64::from(report.cycles))),
        (
            "order_only_available",
            Value::Bool(report.order_only_available),
        ),
        ("complete", Value::Bool(report.complete())),
        (
            "errors",
            array(report.errors.iter().map(|error| bytes(error))),
        ),
        (
            "suggestions",
            array(report.suggestions.iter().map(|name| bytes(name))),
        ),
    ])
}

fn scope_name(scope: rcmake_model::UsageScope) -> &'static str {
    match scope {
        rcmake_model::UsageScope::Private => "PRIVATE",
        rcmake_model::UsageScope::Public => "PUBLIC",
        rcmake_model::UsageScope::Interface => "INTERFACE",
    }
}

fn config_json(config: &ExplainConfig) -> Value {
    let entries = config.entries.iter().map(|entry| {
        object([
            ("kind", Value::string(chain_kind(entry.kind))),
            ("value", bytes(&entry.value)),
            (
                "spelling",
                optional(entry.spelling.as_ref().map(|value| bytes(value))),
            ),
            (
                "system_include",
                optional(entry.system_include.map(Value::Bool)),
            ),
            ("build_entry", Value::Bool(entry.build_entry)),
            (
                "compile_positions",
                array(entry.compile_positions.iter().map(|position| {
                    object([
                        ("group", Value::UInt(position.group as u64)),
                        ("ordinal", Value::UInt(position.ordinal as u64)),
                    ])
                })),
            ),
            (
                "link_role",
                optional(entry.link_role.map(|role| Value::string(link_role(role)))),
            ),
            (
                "link_targets",
                array(
                    entry
                        .link_targets
                        .iter()
                        .map(|id| Value::UInt(id.index() as u64)),
                ),
            ),
            (
                "input",
                optional(entry.input.as_ref().map(|input| {
                    object([
                        ("name", bytes(&input.name)),
                        ("location", optional(input.location.as_ref().map(location))),
                    ])
                })),
            ),
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
        (
            "compile_groups",
            array(config.compile_groups.iter().map(|group| {
                object([
                    ("language", bytes(&group.language)),
                    (
                        "sources",
                        array(group.sources.iter().map(|source| bytes(source))),
                    ),
                ])
            })),
        ),
        ("entries", array(entries)),
    ])
}

fn variable_json(variable: &ExplainVariable) -> Value {
    let cache = variable.cache.as_ref().map(|cache| {
        object([
            ("value", bytes(&cache.value)),
            ("type", Value::string(cache.kind.as_str())),
            ("help", bytes(&cache.help)),
            ("advanced", Value::Bool(cache.advanced)),
            (
                "strings",
                optional(
                    cache
                        .strings
                        .as_ref()
                        .map(|values| array(values.iter().map(|value| bytes(value)))),
                ),
            ),
            ("force", optional(cache.force.map(Value::Bool))),
            ("location", optional(cache.location.as_ref().map(location))),
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
            ("sequence", Value::string(event.sequence.to_string())),
            ("operation", Value::string(var_op(event.operation))),
            (
                "command",
                optional(event.command.as_ref().map(|name| bytes(name))),
            ),
            (
                "directory_id",
                optional(event.directory.map(|id| Value::UInt(id.index() as u64))),
            ),
            (
                "directory",
                optional(event.directory_path.as_ref().map(|path| bytes(path))),
            ),
            (
                "binding_directory_id",
                optional(
                    event
                        .binding_directory
                        .map(|id| Value::UInt(id.index() as u64)),
                ),
            ),
            ("in_queried_scope", Value::Bool(event.in_queried_scope)),
            ("scope_depth", Value::UInt(u64::from(event.scope_depth))),
            ("scope_id", Value::string(event.scope_id.to_string())),
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
        (
            "location",
            optional(variable.location.as_ref().map(location)),
        ),
        (
            "binding",
            match (variable.normal_binding, &variable.cache) {
                (true, _) => Value::string("normal"),
                (false, Some(_)) => Value::string("cache"),
                (false, None) => Value::Null,
            },
        ),
        ("directory", bytes(&variable.directory)),
        ("cache", optional(cache)),
        ("writes", array(writes)),
        ("history", Value::string(variable.history.as_str())),
        (
            "session_history_truncated",
            optional(variable.session_history_truncated.map(Value::Bool)),
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
            "source_id",
            optional(
                location
                    .source_id
                    .map(|id| Value::UInt(u64::from(id.get()))),
            ),
        ),
        (
            "range",
            optional(location.range.map(|range| {
                object([
                    ("start", Value::UInt(u64::from(range.start))),
                    ("end", Value::UInt(u64::from(range.end))),
                ])
            })),
        ),
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
    preview_fields(&value.prefix, value.len, value.hash)
}

fn preview_fields(prefix: &[u8], length: usize, hash: u64) -> Value {
    object([
        ("prefix", bytes(prefix)),
        ("byte_length", Value::UInt(length as u64)),
        ("hash", Value::string(format!("{hash:016x}"))),
        ("truncated", Value::Bool(length > prefix.len())),
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
