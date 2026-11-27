//! Build backtrace graphs for directory and target replies.

use std::collections::HashMap;

use rcmake_base::BString;
use rcmake_diag::{Backtrace, Frame};
use rcmake_model::BT;

use super::super::{Members, Object, UInt, Value, array, members, object, relative, strings};
use super::number;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct BacktraceNode {
    file: usize,
    line: u32,
    command: Option<usize>,
    parent: Option<usize>,
}

/// Identity of a `backtraceGraph` node.
///
/// CMake keys nodes by the address of the `cmListFileContext` stack entry. Each
/// command call has its own entry, so two calls with equal file, line, command,
/// and caller are different nodes (for example one `install()` line that a
/// `foreach()` loop runs three times). A frame without a line and a command only
/// names a listfile; it is equal to all other frames of that file and caller.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum NodeKey {
    Call(u64),
    File(BacktraceNode),
}

pub(super) struct Backtraces<'a> {
    source: &'a [u8],
    files: Vec<BString>,
    file_indexes: HashMap<BString, usize>,
    raw_file_indexes: HashMap<BString, usize>,
    commands: Vec<BString>,
    command_indexes: HashMap<BString, usize>,
    nodes: Vec<BacktraceNode>,
    node_indexes: HashMap<NodeKey, usize>,
}

impl<'a> Backtraces<'a> {
    pub(super) fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            files: Vec::new(),
            file_indexes: HashMap::new(),
            raw_file_indexes: HashMap::new(),
            commands: Vec::new(),
            command_indexes: HashMap::new(),
            nodes: Vec::new(),
            node_indexes: HashMap::new(),
        }
    }

    pub(super) fn add(&mut self, trace: &Backtrace) -> Option<usize> {
        let mut iter = trace.frame_nodes();
        self.add_iter(&mut iter)
    }

    fn intern_raw_file(&mut self, raw: &[u8]) -> usize {
        if let Some(&index) = self.raw_file_indexes.get(raw) {
            return index;
        }
        let rel = relative(raw, self.source);
        let index = intern(&mut self.files, &mut self.file_indexes, rel);
        self.raw_file_indexes.insert(BString::from(raw), index);
        index
    }

    fn add_iter<'f>(
        &mut self,
        frames: &mut impl Iterator<Item = (u64, &'f Frame)>,
    ) -> Option<usize> {
        let (id, top) = frames.next()?;
        let key = (top.line != 0 || top.name.as_deref().is_some_and(|name| !name.is_empty()))
            .then_some(NodeKey::Call(id));
        if let Some(&index) = key.as_ref().and_then(|key| self.node_indexes.get(key)) {
            return Some(index);
        }
        let file = self.intern_raw_file(&top.file);
        let command = top
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(|name| intern_slice(&mut self.commands, &mut self.command_indexes, name));
        let parent = if let Some(parent) = self.add_iter(frames) {
            Some(parent)
        } else if top.line != 0 {
            let sentinel = BacktraceNode {
                file,
                line: 0,
                command: None,
                parent: None,
            };
            Some(self.node(NodeKey::File(sentinel), sentinel))
        } else {
            None
        };
        let node = BacktraceNode {
            file,
            line: top.line,
            command,
            parent,
        };
        Some(self.node(key.unwrap_or(NodeKey::File(node)), node))
    }

    /// Return the index of `key`, and append `node` (after its parents) if it is new.
    fn node(&mut self, key: NodeKey, node: BacktraceNode) -> usize {
        *self.node_indexes.entry(key).or_insert_with(|| {
            let index = self.nodes.len();
            self.nodes.push(node);
            index
        })
    }

    pub(super) fn tag(&mut self, body: &mut Members, trace: &Backtrace) {
        if let Some(index) = self.add(trace) {
            body.insert("backtrace".into(), number(index));
        }
    }

    pub(super) fn tagged_value(&mut self, body: &mut Members, key: &str, item: &BT<BString>) {
        body.insert(key.into(), Value::string(item.value.clone()));
        self.tag(body, &item.backtrace);
    }

    pub(super) fn into_json(self) -> Value {
        let nodes = self.nodes.into_iter().map(|node| {
            let mut body = members([("file", number(node.file))]);
            if node.line != 0 {
                body.insert("line".into(), UInt(node.line.into()));
            }
            if let Some(command) = node.command {
                body.insert("command".into(), number(command));
            }
            if let Some(parent) = node.parent {
                body.insert("parent".into(), number(parent));
            }
            Object(body)
        });
        object([
            ("files", strings(&self.files)),
            ("commands", strings(&self.commands)),
            ("nodes", array(nodes)),
        ])
    }
}

fn intern(
    values: &mut Vec<BString>,
    indexes: &mut HashMap<BString, usize>,
    value: BString,
) -> usize {
    if let Some(&index) = indexes.get(&value) {
        return index;
    }
    let index = values.len();
    values.push(value.clone());
    indexes.insert(value, index);
    index
}

fn intern_slice(
    values: &mut Vec<BString>,
    indexes: &mut HashMap<BString, usize>,
    value: &[u8],
) -> usize {
    if let Some(&index) = indexes.get(value) {
        return index;
    }
    let index = values.len();
    let owned = BString::from(value);
    values.push(owned.clone());
    indexes.insert(owned, index);
    index
}
