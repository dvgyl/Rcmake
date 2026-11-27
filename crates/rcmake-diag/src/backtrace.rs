//! Persistent immutable call stacks (`Backtrace`) matching CMake's stack trace model.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Source of call-instance identities. Each [`Backtrace::push`] takes the next value.
static NEXT_NODE_ID: AtomicU64 = AtomicU64::new(1);

/// Kind of stack frame in a CMake backtrace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// Ordinary command or function/macro invocation.
    Command,
    /// Deferred call executed at directory end (`cmake_language(DEFER)`).
    Deferred,
    /// Top-level file or module inclusion frame.
    ModuleInclude,
}

/// One frame in a CMake call stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// File path bytes.
    pub file: Arc<[u8]>,
    /// 1-based line number (`0` when no specific line applies).
    pub line: u32,
    /// Original-case command name as written in the listfile, if applicable.
    pub name: Option<Arc<[u8]>>,
    /// Classification of this frame.
    pub kind: FrameKind,
}

struct BtNode {
    /// Call-instance identity, unique for the process lifetime (`cmConstStack` node identity).
    id: u64,
    frame: Frame,
    parent: Option<Arc<BtNode>>,
}

/// Print the frames only. The identity depends on the order of all pushes in
/// the process, so it is not part of the deterministic output.
impl std::fmt::Debug for BtNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtNode")
            .field("frame", &self.frame)
            .field("parent", &self.parent)
            .finish()
    }
}

/// Persistent linked call stack, cheap to clone for targets and deferred calls.
#[derive(Clone, Default, Debug)]
pub struct Backtrace(Option<Arc<BtNode>>);

impl Backtrace {
    /// Return a new backtrace with `frame` pushed on top.
    ///
    /// The new top frame gets a new call-instance identity, also when an equal
    /// frame was pushed before. Clones of the result share that identity.
    #[must_use]
    pub fn push(&self, frame: Frame) -> Self {
        Self(Some(Arc::new(BtNode {
            id: NEXT_NODE_ID.fetch_add(1, Ordering::Relaxed),
            frame,
            parent: self.0.clone(),
        })))
    }

    /// Return the most recent frame, if any.
    #[must_use]
    pub fn top(&self) -> Option<&Frame> {
        self.0.as_ref().map(|n| &n.frame)
    }

    /// Return `true` when no frames are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Iterate frames from most recent call first to outermost caller.
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        BacktraceIter {
            cur: self.0.as_deref(),
        }
        .map(|node| &node.frame)
    }

    /// Return `true` if both backtraces come from the same [`Self::push`] call
    /// (or both are empty). Equal frames of two calls are not the same call.
    #[must_use]
    pub fn same_call(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(left), Some(right)) => left.id == right.id,
            (None, None) => true,
            _ => false,
        }
    }

    /// Iterate `(identity, frame)` pairs from most recent call first to outermost caller.
    ///
    /// Two frames have the same identity only if they come from the same
    /// [`Self::push`] call. Use the identity as a lookup key only: its value
    /// depends on the order of all pushes in the process.
    pub fn frame_nodes(&self) -> impl Iterator<Item = (u64, &Frame)> {
        BacktraceIter {
            cur: self.0.as_deref(),
        }
        .map(|node| (node.id, &node.frame))
    }
}

struct BacktraceIter<'a> {
    cur: Option<&'a BtNode>,
}

impl<'a> Iterator for BacktraceIter<'a> {
    type Item = &'a BtNode;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.cur?;
        self.cur = node.parent.as_deref();
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::{Backtrace, Frame, FrameKind};

    fn frame(line: u32) -> Frame {
        Frame {
            file: b"/s/CMakeLists.txt".as_slice().into(),
            line,
            name: Some(b"install".as_slice().into()),
            kind: FrameKind::Command,
        }
    }

    #[test]
    fn push_gives_each_call_instance_its_own_identity() {
        let root = Backtrace::default();
        let first = root.push(frame(5));
        let second = root.push(frame(5));
        let ids = |trace: &Backtrace| trace.frame_nodes().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(first.top(), second.top());
        assert_ne!(ids(&first), ids(&second));
        assert_eq!(ids(&first), ids(&first.clone()));
        let nested = first.push(frame(9));
        assert_eq!(ids(&nested).get(1), ids(&first).first());
        assert!(
            nested
                .frames()
                .eq(nested.frame_nodes().map(|(_, frame)| frame))
        );
    }
}
