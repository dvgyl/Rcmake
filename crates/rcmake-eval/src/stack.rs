//! Layered policy and diagnostic stacks with weak write-through entries and
//! per-snapshot balance checks (ADR-005).

/// One stack entry. Each entry holds the complete effective state, so a lookup reads
/// only the top entry.
#[derive(Clone, Copy, Debug)]
struct Entry<T> {
    state: T,
    /// A weak entry (function or macro call) forwards updates to the entry below it.
    weak: bool,
}

/// A stack of complete state copies (`PolicyMap` or `DiagState`) above a strong root.
///
/// A snapshot (file, function call, macro call, or variable block) records [`Self::len`]
/// when it starts. `cmake_policy(POP)` and `cmake_diagnostic(POP)` can not pop below
/// that length.
#[derive(Debug)]
pub(crate) struct LayeredStack<T: Copy> {
    root: T,
    above: Vec<Entry<T>>,
}

impl<T: Copy> LayeredStack<T> {
    /// Create a stack with one strong root entry.
    pub(crate) fn new(root: T) -> Self {
        Self {
            root,
            above: Vec::new(),
        }
    }

    /// Current number of entries, including the root.
    pub(crate) fn len(&self) -> usize {
        self.above.len() + 1
    }

    /// Visit every entry from the strong root up to the top with its weak write-through flag.
    pub(crate) fn for_each_entry(&self, mut visit: impl FnMut(&T, bool)) {
        visit(&self.root, false);
        for entry in &self.above {
            visit(&entry.state, entry.weak);
        }
    }

    /// Effective state (top entry).
    pub(crate) fn top(&self) -> &T {
        self.above.last().map_or(&self.root, |e| &e.state)
    }

    /// Parent state (the entry immediately below the top entry), if one exists.
    pub(crate) fn parent(&self) -> Option<&T> {
        match self.above.len() {
            0 => None,
            1 => Some(&self.root),
            len => self.above.get(len - 2).map(|e| &e.state),
        }
    }

    /// Mutable access to the top entry only (no write-through).
    pub(crate) fn top_mut(&mut self) -> &mut T {
        match self.above.last_mut() {
            Some(e) => &mut e.state,
            None => &mut self.root,
        }
    }

    /// Push a strong copy of the current state (`cmake_policy(PUSH)`, `include()`,
    /// `block()`).
    pub(crate) fn push_copy(&mut self) {
        let state = *self.top();
        self.above.push(Entry { state, weak: false });
    }

    /// Push a weak entry with recorded state (function or macro invocation).
    pub(crate) fn push_weak(&mut self, state: T) {
        self.above.push(Entry { state, weak: true });
    }

    /// Pop the top entry if the stack is longer than `base`. The root is never popped.
    pub(crate) fn pop_above(&mut self, base: usize) -> bool {
        self.len() > base && self.above.pop().is_some()
    }

    /// Apply `update` to the top entry and to every entry below it until (and
    /// including) the first strong entry.
    pub(crate) fn update(&mut self, mut update: impl FnMut(&mut T)) {
        let _ = self.try_update(|state| {
            update(state);
            Ok::<(), ()>(())
        });
    }

    /// Fallible variant of [`Self::update`]. Stops at the first error.
    pub(crate) fn try_update<E>(
        &mut self,
        mut update: impl FnMut(&mut T) -> Result<(), E>,
    ) -> Result<(), E> {
        for entry in self.above.iter_mut().rev() {
            update(&mut entry.state)?;
            if !entry.weak {
                return Ok(());
            }
        }
        update(&mut self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_entries_write_through_to_first_strong_entry() {
        let mut s = LayeredStack::new(0u32);
        s.push_copy();
        s.push_weak(5);
        s.update(|v| *v = 9);
        assert_eq!(*s.top(), 9);
        assert!(s.pop_above(0));
        assert_eq!(*s.top(), 9);
        assert!(s.pop_above(0));
        assert_eq!(*s.top(), 0);
    }

    #[test]
    fn weak_entry_on_root_writes_root() {
        let mut s = LayeredStack::new(1u8);
        s.push_weak(2);
        s.update(|v| *v += 10);
        assert_eq!(*s.top(), 12);
        assert!(s.pop_above(1));
        assert_eq!(*s.top(), 11);
    }

    #[test]
    fn pop_respects_base_and_root() {
        let mut s = LayeredStack::new(1u8);
        let base = s.len();
        assert!(!s.pop_above(base));
        s.push_copy();
        assert!(s.pop_above(base));
        assert!(!s.pop_above(0));
        assert_eq!(s.len(), 1);
        *s.top_mut() = 4;
        assert_eq!(*s.top(), 4);
    }
}
