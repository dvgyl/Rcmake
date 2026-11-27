//! Stack traces, the `Locals` scope tree, and variable references
//! (`cmDebuggerVariablesHelper`, `cmDebuggerVariablesManager`).
//!
//! A variable reference belongs to one frame. It stays valid while the frame is on the
//! stack, as in CMake, and the adapter releases it when the frame pops. The values of a
//! node are read when the client asks for them, from the live state of the directory
//! that owns the frame.

use std::borrow::Cow;
use std::collections::HashMap;

use rcmake_base::BString;
use rcmake_eval::debug::DebugView;
use rcmake_json::Value;
use rcmake_model::{BT, DirId, Target, TestEntry, UsageField};

use crate::budget;
use crate::protocol::{FrameFormat, object};
use crate::session::{Frame, Shared};

/// Most variable references alive at one time.
const MAX_REFERENCES: usize = 1 << 20;

/// Most rows in one `variables` or `stackTrace` response.
const MAX_ENTRIES: usize = 100_000;

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

/// Bytes that [`Entry`] text keeps: borrowed from the interpreter state, or a short
/// owned text that the adapter makes (a count, an index, or a policy id).
type Text<'a> = Cow<'a, [u8]>;

/// Conversion to [`Text`].
trait IntoText<'a> {
    fn text(self) -> Text<'a>;
}

impl<'a> IntoText<'a> for &'a [u8] {
    fn text(self) -> Text<'a> {
        Cow::Borrowed(self)
    }
}

impl<'a> IntoText<'a> for &'a BString {
    fn text(self) -> Text<'a> {
        Cow::Borrowed(self.as_slice())
    }
}

impl<'a> IntoText<'a> for &'a str {
    fn text(self) -> Text<'a> {
        Cow::Borrowed(self.as_bytes())
    }
}

impl<'a> IntoText<'a> for String {
    fn text(self) -> Text<'a> {
        Cow::Owned(self.into_bytes())
    }
}

impl<'a> IntoText<'a> for BString {
    fn text(self) -> Text<'a> {
        Cow::Owned(self.to_vec())
    }
}

/// The node that a row opens. A named child keeps a borrowed name until the row is in
/// the response.
enum Child<'a> {
    Node(Node),
    Named(fn(BString) -> Node, &'a [u8]),
}

impl Child<'_> {
    fn node(self) -> Node {
        match self {
            Self::Node(node) => node,
            Self::Named(make, name) => make(name.into()),
        }
    }
}

/// One row of a `variables` response. It borrows its text, so that the rows of a large
/// node cost a small fixed size each until the page and the byte budget select them.
struct Entry<'a> {
    name: Text<'a>,
    /// The type of a cache entry, which its label shows as `NAME:TYPE`.
    cache_type: Option<&'static str>,
    value: Text<'a>,
    ty: &'static str,
    child: Option<Child<'a>>,
}

impl<'a> Entry<'a> {
    fn data(name: impl IntoText<'a>, value: impl IntoText<'a>) -> Self {
        Self {
            name: name.text(),
            cache_type: None,
            value: value.text(),
            ty: "string",
            child: None,
        }
    }

    fn flag(name: &'static str, value: bool) -> Self {
        Self {
            ty: "bool",
            ..Self::data(name, if value { "TRUE" } else { "FALSE" })
        }
    }

    fn collection(name: impl IntoText<'a>, value: impl IntoText<'a>, child: Child<'a>) -> Self {
        Self {
            ty: "collection",
            child: Some(child),
            ..Self::data(name, value)
        }
    }

    /// The bytes of the `name` field.
    fn label(&self) -> impl Iterator<Item = u8> + Clone + '_ {
        let (colon, ty): (&[u8], &[u8]) = match self.cache_type {
            Some(ty) => (b":", ty.as_bytes()),
            None => (b"", b""),
        };
        self.name.iter().chain(colon).chain(ty).copied()
    }
}

/// The rows that a request selects: skip `skip`, then take at most `take`. A request
/// gives them as `start` and `count`, or as `startFrame` and `levels`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Page {
    skip: usize,
    take: usize,
}

impl Page {
    /// Skip `start` rows. Take at most `count` rows (all if `None` or 0), and never more
    /// than [`MAX_ENTRIES`].
    pub(crate) fn new(start: u64, count: Option<u64>) -> Self {
        Self {
            skip: usize::try_from(start).unwrap_or(usize::MAX),
            take: count
                .filter(|&n| n > 0)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(usize::MAX)
                .min(MAX_ENTRIES),
        }
    }
}

/// Bytes of a `stackTrace` body other than its rows:
/// `{"stackFrames":[],"totalFrames":<i64>}`.
const STACK_BODY_BYTES: usize = 64;

/// Bytes of a `variables` body other than its rows: `{"variables":[]}`.
const VARIABLES_BODY_BYTES: usize = 16;

/// Bytes of one `variables` row other than its name and value strings, with its comma.
/// The largest row has `kind` `property`, `type` `collection`, and a 20-character
/// reference.
const VARIABLE_ROW_BYTES: usize = 160;

/// The parameter text of a frame name.
#[derive(Clone, Copy)]
enum Params<'a> {
    /// No parentheses.
    Hidden,
    /// `(a, b)`, or `()` for no values.
    Values(&'a [BString]),
    /// `(...)`: the values do not fit in the response.
    Omitted,
}

/// `stackTrace` response body for `frames` (outermost first). The rows are innermost
/// first.
///
/// The size of each row is known before its name is copied. A frame whose argument
/// values do not fit in the remaining `budget` bytes of JSON text shows `(...)`, and
/// the rows stop at the first frame that does not fit at all. `totalFrames` is the full
/// stack size, so that a client can ask for later frames with `startFrame`.
pub(crate) fn stack_trace(
    frames: &[Frame],
    format: FrameFormat,
    page: Page,
    budget: usize,
) -> Value {
    let mut remaining = budget.saturating_sub(STACK_BODY_BYTES);
    let mut list = Vec::new();
    for frame in frames.iter().rev().skip(page.skip).take(page.take) {
        let params = match (format.parameters, format.parameter_values) {
            (false, _) => Params::Hidden,
            (true, true) => Params::Values(&frame.args),
            (true, false) => Params::Values(&[]),
        };
        let row = frame_row(frame, params, format.line, remaining).or_else(|| {
            matches!(params, Params::Values(args) if !args.is_empty())
                .then(|| frame_row(frame, Params::Omitted, format.line, remaining))
                .flatten()
        });
        let Some((row, used)) = row else {
            break;
        };
        remaining = remaining.saturating_sub(used);
        list.push(row);
    }
    let total = i64::try_from(frames.len()).unwrap_or(i64::MAX);
    object([
        ("stackFrames", Value::Array(list)),
        ("totalFrames", Value::Int(total)),
    ])
}

/// The bytes of a frame name: `name(a, b) Line: n`.
fn frame_name<'a>(
    frame: &'a Frame,
    params: Params<'a>,
    line: &'a str,
) -> impl Iterator<Item = u8> + Clone + 'a {
    let (open, args, close): (&[u8], &[BString], &[u8]) = match params {
        Params::Hidden => (b"", &[], b""),
        Params::Values(args) => (b"(", args, b")"),
        Params::Omitted => (b"(...)", &[], b""),
    };
    let values = args.iter().enumerate().flat_map(|(index, arg)| {
        let separator: &[u8] = if index > 0 { b", " } else { b"" };
        separator.iter().chain(arg.iter())
    });
    frame
        .name
        .iter()
        .chain(open)
        .chain(values)
        .chain(close)
        .chain(line.as_bytes())
        .copied()
}

/// The `stackTrace` row of `frame` and its bytes with a comma, if they fit in `limit`.
fn frame_row(
    frame: &Frame,
    params: Params<'_>,
    line: bool,
    limit: usize,
) -> Option<(Value, usize)> {
    let line = if line {
        format!(" Line: {}", frame.line)
    } else {
        String::new()
    };
    let name = frame_name(frame, params, &line);
    let mut row = object([
        ("column", Value::Int(1)),
        ("id", Value::Int(frame.id)),
        ("line", Value::Int(i64::from(frame.line))),
        ("name", Value::string("")),
        ("source", crate::protocol::source(&frame.file)),
    ]);
    // The row with an empty name (`""`), and its comma.
    let fixed = budget::value_len(&row).saturating_sub(1);
    let name_len = budget::string_len(name.clone(), limit.checked_sub(fixed)?)?;
    if let Value::Object(map) = &mut row {
        map.insert("name".into(), Value::string(name.collect::<Vec<u8>>()));
    }
    Some((row, fixed.saturating_add(name_len)))
}

fn is_directory_key(name: &[u8]) -> bool {
    name.ends_with(b"_DIR") || name.ends_with(b"_DIRECTORY")
}

fn count(n: usize) -> String {
    n.to_string()
}

/// `[0]`, `[1]`, ... entries of a list, unsorted.
fn indexed<'a>(items: impl Iterator<Item = &'a [u8]>) -> Vec<Entry<'a>> {
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

fn policy_entries(policies: &rcmake_diag::PolicyMap) -> Vec<Entry<'static>> {
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

/// The property rows of `props`: borrowed values, sorted later.
fn property_entries(props: &rcmake_model::PropertyMap) -> Vec<Entry<'_>> {
    props
        .names()
        .into_iter()
        .map(|key| {
            let value = props.get(&key).map_or(Text::Borrowed(b""), IntoText::text);
            Entry {
                value,
                ..Entry::data(key, "")
            }
        })
        .collect()
}

/// The rows of `node` of a frame in `dir` at `line`, and whether they sort by name.
fn rows<'a>(view: &DebugView<'a>, dir: DirId, line: u32, node: &Node) -> (Vec<Entry<'a>>, bool) {
    match node {
        Node::Root => {
            let vars = view.variables(dir);
            let directories = vars.keys().filter(|name| is_directory_key(name)).count();
            let mut rows = vec![
                Entry {
                    ty: "int",
                    ..Entry::data("CurrentLine", line.to_string())
                },
                Entry::collection("Locals", count(vars.len()), Child::Node(Node::Locals)),
                Entry::collection(
                    "Directories",
                    count(directories),
                    Child::Node(Node::Directories),
                ),
                Entry::collection(
                    "CacheVariables",
                    count(view.cache().count()),
                    Child::Node(Node::Cache),
                ),
            ];
            let targets = view.targets(dir).count();
            if targets > 0 {
                rows.push(Entry::collection(
                    "Targets",
                    count(targets),
                    Child::Node(Node::Targets),
                ));
            }
            let tests = view.tests(dir).count();
            if tests > 0 {
                rows.push(Entry::collection(
                    "Tests",
                    count(tests),
                    Child::Node(Node::Tests),
                ));
            }
            (rows, true)
        }
        Node::Locals => (
            view.variables(dir)
                .into_iter()
                .map(|(name, value)| Entry::data(name, value))
                .collect(),
            true,
        ),
        Node::Directories => (
            view.variables(dir)
                .into_iter()
                .filter(|(name, _)| is_directory_key(name))
                .map(|(name, value)| Entry::data(name, value))
                .collect(),
            true,
        ),
        Node::Cache => (
            view.cache()
                .map(|(name, entry)| Entry {
                    cache_type: Some(entry.ty.as_str()),
                    ..Entry::collection(
                        name,
                        &entry.value,
                        Child::Named(Node::CacheEntry, name.as_slice()),
                    )
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
                rows.push(Entry::data("HELPSTRING", &entry.help));
            }
            rows.extend(property_entries(&entry.props));
            rows.push(Entry::data("TYPE", entry.ty.as_str()));
            rows.push(Entry::data("VALUE", &entry.value));
            (rows, true)
        }
        Node::Targets => (
            view.targets(dir)
                .map(|target| {
                    Entry::collection(
                        &target.name,
                        target.kind.as_str(),
                        Child::Named(Node::Target, target.name.as_slice()),
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
                Entry::data("Name", &target.name),
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
                    String::new()
                } else {
                    count(size)
                };
                rows.push(Entry::collection(
                    list.name(),
                    value,
                    Child::Node(Node::TargetList(name.clone(), list)),
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
                TargetList::Properties => (property_entries(&target.props), true),
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
                .map(|test| {
                    Entry::collection(
                        &test.name,
                        "",
                        Child::Named(Node::Test, test.name.as_slice()),
                    )
                })
                .collect(),
            true,
        ),
        Node::Test(name) => {
            let Some(test) = find_test(view, dir, name) else {
                return (Vec::new(), true);
            };
            let mut rows = vec![
                Entry::flag("CommandExpandLists", test.command_expand_lists),
                Entry::data("Name", &test.name),
                Entry::flag("OldStyle", test.old_style),
            ];
            if !test.command.is_empty() {
                rows.push(Entry::collection(
                    "Command",
                    count(test.command.len()),
                    Child::Node(Node::TestCommand(name.clone())),
                ));
            }
            let props = test.props.names();
            if !props.is_empty() {
                rows.push(Entry::collection(
                    "Properties",
                    count(props.len()),
                    Child::Node(Node::TestProperties(name.clone())),
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
            find_test(view, dir, name).map_or_else(Vec::new, |test| property_entries(&test.props)),
            true,
        ),
    }
}

fn find_test<'a>(view: &DebugView<'a>, dir: DirId, name: &[u8]) -> Option<&'a TestEntry> {
    view.tests(dir).find(|test| test.name == name)
}

/// The `variables` rows of `page` that fit in `budget` bytes of JSON text, in order,
/// each without its `variablesReference`, and the node that each row opens.
///
/// The size of each row is known before its name and value are copied, and the rows stop
/// at the first row that does not fit.
fn page_rows(
    rows: Vec<Entry<'_>>,
    page: Page,
    types: bool,
    budget: usize,
) -> Vec<(Value, Option<Node>)> {
    let mut remaining = budget.saturating_sub(VARIABLES_BODY_BYTES);
    let mut out = Vec::new();
    for row in rows.into_iter().skip(page.skip).take(page.take) {
        let Some(limit) = remaining.checked_sub(VARIABLE_ROW_BYTES) else {
            break;
        };
        let Some(name_len) = budget::string_len(row.label(), limit) else {
            break;
        };
        let Some(value_len) = budget::bytes_len(&row.value, limit.saturating_sub(name_len)) else {
            break;
        };
        remaining = limit.saturating_sub(name_len).saturating_sub(value_len);
        let kind = if row.ty == "collection" {
            "property"
        } else {
            "data"
        };
        let mut item = object([
            ("name", Value::string(row.label().collect::<Vec<u8>>())),
            (
                "presentationHint",
                object([
                    ("kind", Value::string(kind)),
                    ("visibility", Value::string("private")),
                ]),
            ),
            ("value", Value::string(row.value.into_owned())),
        ]);
        if types && let Value::Object(map) = &mut item {
            map.insert("type".into(), Value::string(row.ty));
        }
        out.push((item, row.child.map(Child::node)));
    }
    out
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
        rows.sort_by(|a, b| a.label().cmp(b.label()));
    }
    let page = page_rows(
        rows,
        Page::new(start.unwrap_or(0), limit),
        types,
        budget::MAX_RESPONSE_BYTES,
    );
    // The lock is held only to give out the references of the selected rows.
    let mut state = shared.lock();
    let out = page
        .into_iter()
        .map(|(mut item, child)| {
            let reference = child
                .and_then(|child| state.registry.reference(frame, child))
                .unwrap_or(0);
            if let Value::Object(map) = &mut item {
                map.insert("variablesReference".into(), Value::Int(reference));
            }
            item
        })
        .collect();
    drop(state);
    object([("variables", Value::Array(out))])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(count: i64, arg: &[u8]) -> Vec<Frame> {
        (1..=count)
            .map(|id| Frame {
                id,
                file: "/w/main.cmake".into(),
                line: 3,
                name: "f".into(),
                args: vec![arg.into(), b"b".as_slice().into()],
                directory: DirId::ROOT,
            })
            .collect()
    }

    const FULL: FrameFormat = FrameFormat {
        parameters: true,
        parameter_values: true,
        line: true,
    };

    fn names(body: &Value) -> Vec<String> {
        body.get("stackFrames")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn stack_trace_counts_escaped_bytes_against_the_budget() {
        // 4 KiB of control bytes is 24 KiB of JSON text. A count of raw bytes would let
        // about 60 frames show their values in 256 KiB.
        let budget = 256 * 1024;
        let stack = frames(60, &[0x01; 4096]);
        let body = stack_trace(&stack, FULL, Page::new(0, None), budget);
        assert!(rcmake_json::write_compact(&body).len() <= budget);
        assert_eq!(body.get("totalFrames").and_then(Value::as_i64), Some(60));
        let names = names(&body);
        assert_eq!(names.len(), 60);
        let shown = names.iter().filter(|name| name.contains('\u{1}')).count();
        assert!((5..=10).contains(&shown), "{shown}");
        assert!(names[shown..].iter().all(|name| name == "f(...) Line: 3"));
        // An exact budget for one row keeps that row.
        let one = frames(1, b"x");
        let exact =
            rcmake_json::write_compact(&stack_trace(&one, FULL, Page::new(0, None), budget)).len();
        let body = stack_trace(&one, FULL, Page::new(0, None), exact + STACK_BODY_BYTES);
        assert_eq!(super::tests::names(&body), ["f(x, b) Line: 3"]);
        let body = stack_trace(&one, FULL, Page::new(0, None), 16);
        assert!(super::tests::names(&body).is_empty());
    }

    #[test]
    fn stack_trace_pages_before_it_copies_frames() {
        let stack = frames(10, b"a");
        let ids = |body: &Value| -> Vec<i64> {
            body.get("stackFrames")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| row.get("id")?.as_i64())
                        .collect()
                })
                .unwrap_or_default()
        };
        let budget = budget::MAX_RESPONSE_BYTES;
        assert_eq!(
            ids(&stack_trace(&stack, FULL, Page::new(2, Some(3)), budget)),
            [8, 7, 6]
        );
        assert_eq!(
            ids(&stack_trace(&stack, FULL, Page::new(0, None), budget)).len(),
            10
        );
        assert!(
            ids(&stack_trace(
                &stack,
                FULL,
                Page::new(u64::MAX, None),
                budget
            ))
            .is_empty()
        );
        let body = stack_trace(&stack, FrameFormat::default(), Page::new(9, None), budget);
        assert_eq!(super::tests::names(&body), ["f"]);
        assert_eq!(body.get("totalFrames").and_then(Value::as_i64), Some(10));
    }

    #[test]
    fn variable_rows_page_within_the_escaped_byte_budget() {
        // A lone invalid UTF-8 byte is six bytes of JSON text (`�`).
        let value = [0xff_u8; 1000];
        let names: Vec<String> = (0..1000).map(|index| format!("V{index:04}")).collect();
        let rows = || -> Vec<Entry<'_>> {
            names
                .iter()
                .map(|name| {
                    Entry::collection(name.as_str(), value.as_slice(), Child::Node(Node::Locals))
                })
                .collect()
        };
        let budget = 256 * 1024;
        let page = page_rows(rows(), Page::new(0, None), true, budget);
        let items: Vec<Value> = page
            .into_iter()
            .map(|(mut item, child)| {
                assert_eq!(child, Some(Node::Locals));
                if let Value::Object(map) = &mut item {
                    map.insert("variablesReference".into(), Value::Int(i64::MAX));
                }
                item
            })
            .collect();
        // A count of raw bytes would let about 250 rows into 256 KiB.
        assert!((30..=45).contains(&items.len()), "{}", items.len());
        let body = object([("variables", Value::Array(items))]);
        assert!(rcmake_json::write_compact(&body).len() <= budget);
        let page = page_rows(rows(), Page::new(5, Some(3)), false, budget);
        let names: Vec<&str> = page
            .iter()
            .filter_map(|(item, _)| item.get("name")?.as_str())
            .collect();
        assert_eq!(names, ["V0005", "V0006", "V0007"]);
        assert!(page_rows(rows(), Page::new(1000, None), false, budget).is_empty());
    }

    #[test]
    fn row_overheads_are_upper_bounds() {
        let row = object([
            ("name", Value::string("")),
            (
                "presentationHint",
                object([
                    ("kind", Value::string("property")),
                    ("visibility", Value::string("private")),
                ]),
            ),
            ("type", Value::string("collection")),
            ("value", Value::string("")),
            ("variablesReference", Value::Int(i64::MIN)),
        ]);
        // Without the two empty strings (4 bytes), with a comma (1 byte).
        assert!(budget::value_len(&row) - 3 <= VARIABLE_ROW_BYTES);
        let body = object([
            ("stackFrames", Value::Array(Vec::new())),
            ("totalFrames", Value::Int(i64::MIN)),
        ]);
        assert!(budget::value_len(&body) <= STACK_BODY_BYTES);
        let body = object([("variables", Value::Array(Vec::new()))]);
        assert!(budget::value_len(&body) <= VARIABLES_BODY_BYTES);
    }

    #[test]
    fn cache_labels_sort_with_their_type() {
        let mut rows = [
            Entry {
                cache_type: Some("STRING"),
                ..Entry::data("FOO", "")
            },
            Entry {
                cache_type: Some("STRING"),
                ..Entry::data("FOO2", "")
            },
        ];
        rows.sort_by(|a, b| a.label().cmp(b.label()));
        let labels: Vec<Vec<u8>> = rows.iter().map(|row| row.label().collect()).collect();
        assert_eq!(labels, [b"FOO2:STRING".to_vec(), b"FOO:STRING".to_vec()]);
    }
}
