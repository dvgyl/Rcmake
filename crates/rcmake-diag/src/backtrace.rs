//! Persistent immutable call stacks (`Backtrace`) matching CMake's stack trace model.

use std::sync::Arc;

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

#[derive(Debug)]
struct BtNode {
    frame: Frame,
    parent: Option<Arc<BtNode>>,
}

/// Persistent linked call stack, cheap to clone for targets and deferred calls.
#[derive(Clone, Default, Debug)]
pub struct Backtrace(Option<Arc<BtNode>>);

impl Backtrace {
    /// Return a new backtrace with `frame` pushed on top.
    #[must_use]
    pub fn push(&self, frame: Frame) -> Self {
        Self(Some(Arc::new(BtNode {
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
    }
}

struct BacktraceIter<'a> {
    cur: Option<&'a BtNode>,
}

impl<'a> Iterator for BacktraceIter<'a> {
    type Item = &'a Frame;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.cur?;
        self.cur = node.parent.as_deref();
        Some(&node.frame)
    }
}
