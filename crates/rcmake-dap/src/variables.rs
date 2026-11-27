//! Stack traces, the `Locals` scope tree, and variable references
//! (`cmDebuggerVariablesHelper`, `cmDebuggerVariablesManager`).
//!
//! A variable reference belongs to one frame. It stays valid while the frame is on the
//! stack, as in CMake, and the adapter releases it when the frame pops. The values of a
//! node are read when the client asks for them, from the live state of the directory
//! that owns the frame.

use std::collections::HashMap;

use rcmake_base::BString;
use rcmake_eval::debug::DebugView;
use rcmake_json::Value;
use rcmake_model::{BT, DirId, Target, TestEntry, UsageField};

use crate::protocol::{FrameFormat, object};
use crate::session::{Frame, Shared};

/// Most variable references alive at one time.
const MAX_REFERENCES: usize = 1 << 20;

/// Most entries in one `variables` response.
const MAX_ENTRIES: usize = 100_000;

/// Most value bytes in one `variables` response.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// A list in a target node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TargetList {
    PolicyMap,
    Properties,
    IncludeDirectories,
    Sources,
    CompileDefinitions,
    CompileFeatures,
    CompileOptions,
    LinkDirectories,
    LinkOptions,
}

impl TargetList {
    const ALL: [Self; 9] = [
        Self::PolicyMap,
        Self::Properties,
        Self::IncludeDirectories,
        Self::Sources,
        Self::CompileDefinitions,
        Self::CompileFeatures,
        Self::CompileOptions,
        Self::LinkDirectories,
        Self::LinkOptions,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::PolicyMap => "PolicyMap",
            Self::Properties => "Properties",
            Self::IncludeDirectories => "IncludeDirectories",
            Self::Sources => "Sources",
            Self::CompileDefinitions => "CompileDefinitions",
            Self::CompileFeatures => "CompileFeatures",
            Self::CompileOptions => "CompileOptions",
            Self::LinkDirectories => "LinkDirectories",
            Self::LinkOptions => "LinkOptions",
        }
    }

    const fn field(self) -> Option<UsageField> {
        match self {
            Self::IncludeDirectories => Some(UsageField::IncludeDirectories),
            Self::CompileDefinitions => Some(UsageField::CompileDefinitions),
            Self::CompileFeatures => Some(UsageField::CompileFeatures),
            Self::CompileOptions => Some(UsageField::CompileOptions),
            Self::LinkDirectories => Some(UsageField::LinkDirectories),
            Self::LinkOptions => Some(UsageField::LinkOptions),
            Self::PolicyMap | Self::Properties | Self::Sources => None,
        }
    }
}

/// A node of the scope tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    Root,
    Locals,
    Directories,
    Cache,
    CacheEntry(BString),
    Targets,
    Target(BString),
    TargetList(BString, TargetList),
    Tests,
    Test(BString),
    TestCommand(BString),
    TestProperties(BString),
}

/// Variable references of the live frames.
#[derive(Default)]
pub(crate) struct Registry {
    nodes: HashMap<i64, (i64, Node)>,
    by_node: HashMap<(i64, Node), i64>,
    by_frame: HashMap<i64, Vec<i64>>,
    next: i64,
}

impl Registry {
    /// The reference of `node` of `frame`. Allocate one if needed.
    fn reference(&mut self, frame: i64, node: Node) -> Option<i64> {
        if let Some(&reference) = self.by_node.get(&(frame, node.clone())) {
            return Some(reference);
        }
        if self.nodes.len() >= MAX_REFERENCES {
            return None;
        }
        self.next = self.next.saturating_add(1);
        let reference = self.next;
        self.nodes.insert(reference, (frame, node.clone()));
        self.by_node.insert((frame, node), reference);
        self.by_frame.entry(frame).or_default().push(reference);
        Some(reference)
    }

    /// The reference of the `Locals` scope of `frame`.
    pub(crate) fn scope(&mut self, frame: i64) -> Option<i64> {
        self.reference(frame, Node::Root)
    }

    fn node(&self, reference: i64) -> Option<(i64, Node)> {
        self.nodes.get(&reference).cloned()
    }

    /// Release the references of a popped frame.
    pub(crate) fn release_frame(&mut self, frame: i64) {
        if let Some(references) = self.by_frame.remove(&frame) {
            for reference in references {
                if let Some(entry) = self.nodes.remove(&reference) {
                    self.by_node.remove(&entry);
                }
            }
        }
    }
}

/// One row of a `variables` response.
struct Entry {
    name: BString,
    value: BString,
    ty: &'static str,
    child: Option<Node>,
}

impl Entry {
    fn data(name: impl Into<BString>, value: impl Into<BString>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            ty: "string",
            child: None,
        }
    }

    fn flag(name: &str, value: bool) -> Self {
        Self {
            name: name.into(),
            value: if value { "TRUE" } else { "FALSE" }.into(),
            ty: "bool",
            child: None,
        }
    }

    fn collection(name: impl Into<BString>, value: impl Into<BString>, child: Node) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            ty: "collection",
            child: Some(child),
        }
    }
}

/// `stackTrace` response body for `frames` (innermost first).
pub(crate) fn stack_trace(frames: &[Frame], format: FrameFormat) -> Value {
    let list: Vec<Value> = frames
        .iter()
        .rev()
        .map(|frame| {
            let mut name = frame.name.to_vec();
            if format.parameters {
                name.push(b'(');
                if format.parameter_values && !frame.args.is_empty() {
                    for (index, arg) in frame.args.iter().enumerate() {
                        if index > 0 {
                            name.extend_from_slice(b", ");
                        }
                        name.extend_from_slice(arg);
                    }
                }
                name.push(b')');
            }
            if format.line {
                name.extend_from_slice(format!(" Line: {}", frame.line).as_bytes());
            }
            object([
                ("column", Value::Int(1)),
                ("id", Value::Int(frame.id)),
                ("line", Value::Int(i64::from(frame.line))),
                ("name", Value::string(name)),
                ("source", crate::protocol::source(&frame.file)),
            ])
        })
        .collect();
    let total = i64::try_from(list.len()).unwrap_or(i64::MAX);
    object([
        ("stackFrames", Value::Array(list)),
        ("totalFrames", Value::Int(total)),
    ])
}

fn is_directory_key(name: &[u8]) -> bool {
    name.ends_with(b"_DIR") || name.ends_with(b"_DIRECTORY")
}

fn count(n: usize) -> BString {
    n.to_string().into()
}

/// `[0]`, `[1]`, ... entries of a list, unsorted.
fn indexed<'a>(items: impl Iterator<Item = &'a [u8]>) -> Vec<Entry> {
    items
        .enumerate()
        .map(|(index, item)| Entry::data(format!("[{index}]"), item))
        .collect()
}

fn target_list(target: &Target, list: TargetList) -> Vec<&BT<BString>> {
    match (list, list.field()) {
        (TargetList::Sources, _) => target.sources.iter().collect(),
        (_, Some(field)) => target.usage.entries(field).iter().collect(),
        _ => Vec::new(),
    }
}

fn policy_entries(policies: &rcmake_diag::PolicyMap) -> Vec<Entry> {
    (0..=rcmake_diag::generated_policies::MAX_POLICY_ID)
        .filter_map(|number| {
            let id = rcmake_diag::PolicyId::new(number);
            let status = match policies.get(id) {
                rcmake_diag::PolicyStatus::Old => "OLD",
                rcmake_diag::PolicyStatus::New => "NEW",
                rcmake_diag::PolicyStatus::Warn => return None,
            };
            Some(Entry::data(format!("CMP{number:04}"), status))
        })
        .collect()
}

/// The rows of `node` of a frame in `dir` at `line`, and whether they sort by name.
fn rows(view: &DebugView<'_>, dir: DirId, line: u32, node: &Node) -> (Vec<Entry>, bool) {
    match node {
        Node::Root => {
            let vars = view.variables(dir);
            let directories = vars.keys().filter(|name| is_directory_key(name)).count();
            let mut rows = vec![
                Entry {
                    name: "CurrentLine".into(),
                    value: line.to_string().into(),
                    ty: "int",
                    child: None,
                },
                Entry::collection("Locals", count(vars.len()), Node::Locals),
                Entry::collection("Directories", count(directories), Node::Directories),
                Entry::collection("CacheVariables", count(view.cache().count()), Node::Cache),
            ];
            let targets = view.targets(dir).count();
            if targets > 0 {
                rows.push(Entry::collection("Targets", count(targets), Node::Targets));
            }
            let tests = view.tests(dir).count();
            if tests > 0 {
                rows.push(Entry::collection("Tests", count(tests), Node::Tests));
            }
            (rows, true)
        }
        Node::Locals => (
            view.variables(dir)
                .into_iter()
                .map(|(name, value)| Entry::data(name.clone(), value.clone()))
                .collect(),
            true,
        ),
        Node::Directories => (
            view.variables(dir)
                .into_iter()
                .filter(|(name, _)| is_directory_key(name))
                .map(|(name, value)| Entry::data(name.clone(), value.clone()))
                .collect(),
            true,
        ),
        Node::Cache => (
            view.cache()
                .map(|(name, entry)| {
                    let mut label = name.to_vec();
                    label.push(b':');
                    label.extend_from_slice(entry.ty.as_str().as_bytes());
                    Entry::collection(label, entry.value.clone(), Node::CacheEntry(name.clone()))
                })
                .collect(),
            true,
        ),
        Node::CacheEntry(name) => {
            let Some((_, entry)) = view.cache().find(|(key, _)| *key == name) else {
                return (Vec::new(), true);
            };
            let mut rows = Vec::new();
            if entry.advanced {
                rows.push(Entry::data("ADVANCED", "1"));
            }
            if !entry.help.is_empty() {
                rows.push(Entry::data("HELPSTRING", entry.help.clone()));
            }
            for prop in entry.props.names() {
                let value = entry.props.get(&prop).cloned().unwrap_or_default();
                rows.push(Entry::data(prop, value));
            }
            rows.push(Entry::data("TYPE", entry.ty.as_str()));
            rows.push(Entry::data("VALUE", entry.value.clone()));
            (rows, true)
        }
        Node::Targets => (
            view.targets(dir)
                .map(|target| {
                    Entry::collection(
                        target.name.clone(),
                        target.kind.as_str(),
                        Node::Target(target.name.clone()),
                    )
                })
                .collect(),
            true,
        ),
        Node::Target(name) => {
            let Some(target) = view.targets(dir).find(|target| target.name == *name) else {
                return (Vec::new(), true);
            };
            let prop = |key: &[u8]| {
                target
                    .props
                    .get(key)
                    .is_some_and(|value| rcmake_base::truth::is_on(value))
            };
            let executable = matches!(target.kind, rcmake_model::TargetKind::Executable);
            let mut rows = vec![
                Entry::flag("IsAIX", false),
                Entry::flag("IsAndroidGuiExecutable", false),
                Entry::flag(
                    "IsAppBundleOnApple",
                    executable && cfg!(target_os = "macos") && prop(b"MACOSX_BUNDLE"),
                ),
                Entry::flag("IsDLLPlatform", cfg!(windows)),
                Entry::flag(
                    "IsExecutableWithExports",
                    executable && prop(b"ENABLE_EXPORTS"),
                ),
                Entry::flag(
                    "IsFrameworkOnApple",
                    target.kind.is_library() && cfg!(target_os = "macos") && prop(b"FRAMEWORK"),
                ),
                Entry::flag("IsImported", target.is_imported()),
                Entry::flag(
                    "IsImportedGloballyVisible",
                    target.is_imported() && target.is_global(),
                ),
                Entry::flag("IsPerConfig", true),
                Entry::data("Name", target.name.clone()),
                Entry::data("Type", target.kind.as_str()),
            ];
            for list in TargetList::ALL {
                let size = match list {
                    TargetList::PolicyMap => 1,
                    TargetList::Properties => target.props.names().len(),
                    _ => target_list(target, list).len(),
                };
                if size == 0 {
                    continue;
                }
                let value = if list == TargetList::PolicyMap {
                    BString::default()
                } else {
                    count(size)
                };
                rows.push(Entry::collection(
                    list.name(),
                    value,
                    Node::TargetList(name.clone(), list),
                ));
            }
            (rows, true)
        }
        Node::TargetList(name, list) => {
            let Some(target) = view.targets(dir).find(|target| target.name == *name) else {
                return (Vec::new(), true);
            };
            match list {
                TargetList::PolicyMap => (policy_entries(&target.policies.policies), true),
                TargetList::Properties => (
                    target
                        .props
                        .names()
                        .into_iter()
                        .map(|key| {
                            let value = target.props.get(&key).cloned().unwrap_or_default();
                            Entry::data(key, value)
                        })
                        .collect(),
                    true,
                ),
                _ => (
                    indexed(
                        target_list(target, *list)
                            .into_iter()
                            .map(|item| item.value.as_slice()),
                    ),
                    false,
                ),
            }
        }
        Node::Tests => (
            view.tests(dir)
                .map(|test| Entry::collection(test.name.clone(), "", Node::Test(test.name.clone())))
                .collect(),
            true,
        ),
        Node::Test(name) => {
            let Some(test) = find_test(view, dir, name) else {
                return (Vec::new(), true);
            };
            let mut rows = vec![
                Entry::flag("CommandExpandLists", test.command_expand_lists),
                Entry::data("Name", test.name.clone()),
                Entry::flag("OldStyle", test.old_style),
            ];
            if !test.command.is_empty() {
                rows.push(Entry::collection(
                    "Command",
                    count(test.command.len()),
                    Node::TestCommand(name.clone()),
                ));
            }
            let props = test.props.names();
            if !props.is_empty() {
                rows.push(Entry::collection(
                    "Properties",
                    count(props.len()),
                    Node::TestProperties(name.clone()),
                ));
            }
            (rows, true)
        }
        Node::TestCommand(name) => (
            find_test(view, dir, name).map_or_else(Vec::new, |test| {
                indexed(test.command.iter().map(|item| item.as_slice()))
            }),
            false,
        ),
        Node::TestProperties(name) => (
            find_test(view, dir, name).map_or_else(Vec::new, |test| {
                test.props
                    .names()
                    .into_iter()
                    .map(|key| {
                        let value = test.props.get(&key).cloned().unwrap_or_default();
                        Entry::data(key, value)
                    })
                    .collect()
            }),
            true,
        ),
    }
}

fn find_test<'a>(view: &DebugView<'a>, dir: DirId, name: &[u8]) -> Option<&'a TestEntry> {
    view.tests(dir).find(|test| test.name == name)
}

/// Answer `variables` for `reference` on the evaluator thread.
pub(crate) fn respond(
    shared: &Shared,
    view: &DebugView<'_>,
    reference: i64,
    start: Option<u64>,
    limit: Option<u64>,
) -> Value {
    let found = {
        let state = shared.lock();
        state.registry.node(reference).and_then(|(frame, node)| {
            state.frame(frame).map(|found| {
                (
                    frame,
                    found.directory,
                    found.line,
                    node,
                    state.variable_types,
                )
            })
        })
    };
    let Some((frame, dir, line, node, types)) = found else {
        return object([("variables", Value::Array(Vec::new()))]);
    };
    let (mut rows, sorted) = rows(view, dir, line, &node);
    // Targets skip empty string rows (`SetIgnoreEmptyStringEntries`).
    if matches!(node, Node::Target(_)) {
        rows.retain(|row| !(row.ty == "string" && row.value.is_empty()));
    }
    if sorted {
        rows.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let skip = usize::try_from(start.unwrap_or(0)).unwrap_or(usize::MAX);
    let take = limit
        .filter(|&n| n > 0)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(usize::MAX)
        .min(MAX_ENTRIES);
    let mut bytes = 0usize;
    let mut out = Vec::new();
    let mut state = shared.lock();
    for row in rows.into_iter().skip(skip).take(take) {
        bytes = bytes.saturating_add(row.name.len().saturating_add(row.value.len()));
        if bytes > MAX_RESPONSE_BYTES {
            break;
        }
        let reference = row
            .child
            .and_then(|child| state.registry.reference(frame, child))
            .unwrap_or(0);
        let kind = if row.ty == "collection" {
            "property"
        } else {
            "data"
        };
        let mut item = object([
            ("name", Value::string(row.name)),
            (
                "presentationHint",
                object([
                    ("kind", Value::string(kind)),
                    ("visibility", Value::string("private")),
                ]),
            ),
            ("value", Value::string(row.value)),
            ("variablesReference", Value::Int(reference)),
        ]);
        if types && let Value::Object(map) = &mut item {
            map.insert("type".into(), Value::string(row.ty));
        }
        out.push(item);
    }
    drop(state);
    object([("variables", Value::Array(out))])
}
