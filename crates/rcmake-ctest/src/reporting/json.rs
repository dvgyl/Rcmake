use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, list};
use rcmake_host::{Host, Stream, path};
use rcmake_json::Value;

use super::{CtestOptions, Plan, PlannedTest, resolved_command};
use crate::cli;

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BacktraceNode {
    file: u64,
    line: Option<u64>,
    command: Option<u64>,
    parent: Option<u64>,
}

#[derive(Default)]
struct BacktraceGraph {
    files: Vec<Value>,
    commands: Vec<Value>,
    nodes: Vec<Value>,
    file_ids: BTreeMap<BString, u64>,
    command_ids: BTreeMap<BString, u64>,
    node_ids: BTreeMap<BacktraceNode, u64>,
}

impl BacktraceGraph {
    /// Add the backtrace of one test and return the index of its innermost node.
    ///
    /// Files and commands are shared by all tests. Nodes are not: CTest builds a
    /// new node chain for each test, so each test has its own root node.
    fn push(&mut self, triples: &[u8]) -> Option<Value> {
        let parts = rcmake_base::list::split(triples, rcmake_base::list::Empty::Keep);
        if !parts.len().is_multiple_of(3) {
            return None;
        }
        self.node_ids.clear();
        for frame in parts.chunks(3) {
            if let Some(file) = frame.first().filter(|file| !file.is_empty()) {
                self.file_ids.entry(file.clone()).or_insert_with(|| {
                    let index = self.files.len() as u64;
                    self.files.push(Value::string(file.clone()));
                    index
                });
            }
        }
        let mut parent = None;
        for frame in parts.chunks(3).rev() {
            let Some(file) = frame.first().filter(|file| !file.is_empty()) else {
                continue;
            };
            let file_id = *self.file_ids.get(file)?;
            let line = frame
                .get(1)
                .and_then(|line| json_integer(line))
                .filter(|line| *line > 0)
                .map(|line| line as u64);
            let command = frame
                .get(2)
                .filter(|command| !command.is_empty())
                .map(|command| {
                    *self.command_ids.entry(command.clone()).or_insert_with(|| {
                        let index = self.commands.len() as u64;
                        self.commands.push(Value::string(command.clone()));
                        index
                    })
                });
            let id = *self
                .node_ids
                .entry(BacktraceNode {
                    file: file_id,
                    line,
                    command,
                    parent,
                })
                .or_insert_with(|| {
                    let mut node = BTreeMap::from([("file".into(), Value::UInt(file_id))]);
                    for (field, value) in [("line", line), ("command", command), ("parent", parent)]
                    {
                        if let Some(value) = value {
                            node.insert(field.into(), Value::UInt(value));
                        }
                    }
                    let index = self.nodes.len() as u64;
                    self.nodes.push(Value::Object(node));
                    index
                });
            parent = Some(id);
        }
        parent.map(Value::UInt)
    }
}

pub(super) fn json_integer(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.trim().parse().ok()
}

fn json_property(key: &[u8], value: &BString) -> Value {
    use rcmake_base::{list, truth};
    match key {
        b"DISABLED" | b"PROCESSOR_AFFINITY" | b"RUN_SERIAL" | b"WILL_FAIL" => {
            Value::Bool(truth::is_on(value))
        }
        b"PROCESSORS" | b"SKIP_RETURN_CODE" => Value::Int(json_integer(value).unwrap_or(-1)),
        b"TIMEOUT" | b"COST" | b"TIMEOUT_SIGNAL_GRACE_PERIOD" => Value::Real(
            cli::number_prefix(value)
                .filter(|value| value.is_finite())
                .unwrap_or_default(),
        ),
        b"FIXTURES_SETUP" | b"FIXTURES_CLEANUP" | b"FIXTURES_REQUIRED" | b"LABELS"
        | b"RESOURCE_LOCK" => {
            let values: BTreeSet<_> = list::split(value, list::Empty::Drop).into_iter().collect();
            Value::Array(values.into_iter().map(Value::string).collect())
        }
        b"ATTACHED_FILES"
        | b"ATTACHED_FILES_ON_FAIL"
        | b"DEPENDS"
        | b"ENVIRONMENT"
        | b"ENVIRONMENT_MODIFICATION"
        | b"FAIL_REGULAR_EXPRESSION"
        | b"PASS_REGULAR_EXPRESSION"
        | b"REQUIRED_FILES"
        | b"SKIP_REGULAR_EXPRESSION" => Value::Array(
            list::split(value, list::Empty::Drop)
                .into_iter()
                .map(Value::string)
                .collect(),
        ),
        b"MEASUREMENT" => {
            let text = value.to_string();
            let measurements = if text.is_empty() {
                Vec::new()
            } else {
                let (name, value) = text.split_once('=').unwrap_or((&text, "1"));
                vec![object([
                    ("measurement", Value::string(name)),
                    ("value", Value::string(value)),
                ])]
            };
            Value::Array(measurements)
        }
        b"RESOURCE_GROUPS" => resource_groups(value),
        _ => Value::string(value.clone()),
    }
}

pub(crate) fn show_json(host: &dyn Host, plan: &Plan, options: &CtestOptions, raw: bool) {
    let mut graph = BacktraceGraph::default();
    let tests = plan
        .tests
        .iter()
        .filter(|test| test.command.program != "NOT_AVAILABLE")
        .map(|test| {
            let mut fields = BTreeMap::from([("name".into(), Value::string(test.name.clone()))]);
            if let Some(command) = resolved_command(host, test, options) {
                fields.insert(
                    "command".into(),
                    Value::Array(
                        std::iter::once(command.program)
                            .chain(command.args)
                            .map(Value::string)
                            .collect(),
                    ),
                );
            }
            if let Some(config) = &options.config {
                fields.insert("config".into(), Value::string(config.clone()));
            }
            if let Some(frame) = graph.push(test.property(b"_BACKTRACE_TRIPLES")) {
                fields.insert("backtrace".into(), frame);
            }
            fields.insert(
                "properties".into(),
                Value::Array(json_properties(test, raw)),
            );
            Value::Object(fields)
        })
        .collect();
    let data = object([
        ("kind", Value::string("ctestInfo")),
        (
            "version",
            object([("major", Value::UInt(1)), ("minor", Value::UInt(0))]),
        ),
        (
            "backtraceGraph",
            object([
                ("commands", Value::Array(graph.commands)),
                ("files", Value::Array(graph.files)),
                ("nodes", Value::Array(graph.nodes)),
            ]),
        ),
        ("tests", Value::Array(tests)),
    ]);
    host.write_out(Stream::Stdout, &rcmake_json::write_styled(&data, "  "));
}

const JSON_PROPERTIES: &[&[u8]] = &[
    b"ATTACHED_FILES_ON_FAIL",
    b"ATTACHED_FILES",
    b"COST",
    b"DEPENDS",
    b"DISABLED",
    b"ENVIRONMENT",
    b"ENVIRONMENT_MODIFICATION",
    b"FAIL_REGULAR_EXPRESSION",
    b"SKIP_REGULAR_EXPRESSION",
    b"FIXTURES_CLEANUP",
    b"FIXTURES_REQUIRED",
    b"FIXTURES_SETUP",
    b"GENERATED_RESOURCE_SPEC_FILE",
    b"LABELS",
    b"MEASUREMENT",
    b"PASS_REGULAR_EXPRESSION",
    b"RESOURCE_GROUPS",
    b"PROCESSOR_AFFINITY",
    b"PROCESSORS",
    b"REQUIRED_FILES",
    b"RESOURCE_LOCK",
    b"RUN_SERIAL",
    b"SKIP_RETURN_CODE",
    b"TIMEOUT",
    b"TIMEOUT_SIGNAL_NAME",
    b"TIMEOUT_SIGNAL_GRACE_PERIOD",
    b"WILL_FAIL",
    b"WORKING_DIRECTORY",
];

fn json_properties(test: &PlannedTest, raw: bool) -> Vec<Value> {
    let working_directory = path::from_native(&test.directory);
    let properties = JSON_PROPERTIES.iter().filter_map(|&key| {
        let source = test
            .properties
            .get(key)
            .or_else(|| (key == b"WORKING_DIRECTORY").then_some(&working_directory))?;
        let typed = json_property(key, source);
        let present = match (&typed, key) {
            (Value::Bool(on), _) => *on,
            (Value::Real(cost), b"COST") => *cost != 0.0,
            (Value::Int(count), b"PROCESSORS") => *count > 1,
            (Value::Int(code), b"SKIP_RETURN_CODE") => (0..=255).contains(code),
            (Value::Array(items), _) => !items.is_empty(),
            (Value::String(_), _) => !source.is_empty(),
            _ => true,
        };
        present.then(|| {
            object([
                ("name", Value::string(BString::from(key))),
                (
                    "value",
                    if raw {
                        Value::string(source.clone())
                    } else {
                        typed
                    },
                ),
            ])
        })
    });

    properties
        .chain(
            test.properties
                .iter()
                .filter(|(key, _)| {
                    !JSON_PROPERTIES.contains(&key.as_slice())
                        && !matches!(
                            key.as_slice(),
                            b"_BACKTRACE_TRIPLES"
                                | b"_CMAKE_TEST_BUILD_DEPENDS"
                                | b"TIMEOUT_AFTER_MATCH"
                        )
                })
                .map(|(key, value)| {
                    object([
                        ("name", Value::string(key.clone())),
                        ("value", Value::string(value.clone())),
                    ])
                }),
        )
        .collect()
}

fn resource_groups(value: &[u8]) -> Value {
    let mut groups = Vec::new();
    for group in list::split(value, list::Empty::Drop) {
        let mut requirements = group.split(|&byte| byte == b',').peekable();
        let count = requirements
            .peek()
            .and_then(|field| json_integer(field))
            .map_or(1, |count| {
                requirements.next();
                count.clamp(0, 4096) as usize
            });
        let requirements = requirements
            .filter_map(|field| {
                let separator = field.iter().position(|&byte| byte == b':')?;
                let (name, slots) = field.split_at(separator);
                Some(object([
                    (".type", Value::string(name)),
                    ("slots", Value::Int(json_integer(slots.get(1..)?)?)),
                ]))
            })
            .collect();
        let group = object([("requirements", Value::Array(requirements))]);
        groups.extend(std::iter::repeat_n(group, count.min(4096 - groups.len())));
    }
    Value::Array(groups)
}
