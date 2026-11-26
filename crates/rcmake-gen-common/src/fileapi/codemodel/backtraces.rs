//! Build backtrace graphs for directory and target replies.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_diag::Backtrace;
use rcmake_model::BT;

use super::super::{Members, Object, UInt, Value, array, members, object, relative, strings};
use super::number;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BacktraceNode {
    file: usize,
    line: u32,
    command: Option<usize>,
    parent: Option<usize>,
}

pub(super) struct Backtraces<'a> {
    source: &'a [u8],
    files: Vec<BString>,
    file_indexes: BTreeMap<BString, usize>,
    commands: Vec<BString>,
    command_indexes: BTreeMap<BString, usize>,
    nodes: Vec<BacktraceNode>,
    node_indexes: BTreeMap<BacktraceNode, usize>,
}

impl<'a> Backtraces<'a> {
    pub(super) fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            files: Vec::new(),
            file_indexes: BTreeMap::new(),
            commands: Vec::new(),
            command_indexes: BTreeMap::new(),
            nodes: Vec::new(),
            node_indexes: BTreeMap::new(),
        }
    }

    pub(super) fn add(&mut self, trace: &Backtrace) -> Option<usize> {
        let frames: Vec<_> = trace.frames().collect();
        let mut parent = None;
        for frame in frames.into_iter().rev() {
            let file = intern(
                &mut self.files,
                &mut self.file_indexes,
                relative(&frame.file, self.source),
            );
            let command = frame
                .name
                .as_deref()
                .map(|name| intern_slice(&mut self.commands, &mut self.command_indexes, name));
            if parent.is_none() && frame.line != 0 {
                parent = Some(self.node(BacktraceNode {
                    file,
                    line: 0,
                    command: None,
                    parent: None,
                }));
            }
            parent = Some(self.node(BacktraceNode {
                file,
                line: frame.line,
                command,
                parent,
            }));
        }
        parent
    }

    fn node(&mut self, node: BacktraceNode) -> usize {
        *self.node_indexes.entry(node).or_insert_with(|| {
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
    indexes: &mut BTreeMap<BString, usize>,
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
    indexes: &mut BTreeMap<BString, usize>,
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
