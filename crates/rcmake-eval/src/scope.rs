//! Variable scope stack with explicit `Set`/`Unset` bindings and raise-then-set
//! `PARENT_SCOPE` semantics (ADR-005).

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_diag::PolicyStatus;
use rcmake_model::DirId;

/// One binding in a variable scope.
///
/// An [`Binding::Unset`] entry shadows bindings in outer scopes. A name that has no entry
/// in a scope is looked up in the next outer scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The variable is defined with this value (the value can be empty).
    Set(BString),
    /// The variable is explicitly undefined in this scope.
    Unset,
}

#[derive(Debug)]
struct Scope {
    id: u64,
    directory: Option<DirId>,
    bindings: BTreeMap<BString, Binding>,
    parent_policy: Option<(usize, PolicyStatus)>,
}

/// Stack of variable scopes. Index `0` is the outermost (directory) scope.
#[derive(Debug)]
pub(crate) struct VarStack {
    scopes: Vec<Scope>,
    next_id: u64,
}

impl Default for VarStack {
    fn default() -> Self {
        Self {
            scopes: vec![Scope {
                id: 0,
                directory: Some(DirId::ROOT),
                bindings: BTreeMap::new(),
                parent_policy: None,
            }],
            next_id: 1,
        }
    }
}

impl VarStack {
    /// Number of active scopes.
    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.scopes.len()
    }

    /// Find the innermost binding for `name`.
    pub(crate) fn lookup(&self, name: &[u8]) -> Option<&Binding> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
    }

    /// Value of the innermost normal binding, if it is [`Binding::Set`].
    pub(crate) fn get(&self, name: &[u8]) -> Option<&BString> {
        self.get_from(name, false)
    }

    pub(crate) fn scope_identity(&self, parent: bool) -> Option<(u32, u64, Option<DirId>)> {
        let depth = self.scopes.len().checked_sub(1 + usize::from(parent))?;
        let scope = self.scopes.get(depth)?;
        Some((u32::try_from(depth).ok()?, scope.id, scope.directory))
    }

    pub(crate) fn get_from(&self, name: &[u8], parent: bool) -> Option<&BString> {
        let binding = self
            .scopes
            .iter()
            .rev()
            .skip(usize::from(parent))
            .find_map(|scope| scope.bindings.get(name));
        match binding {
            Some(Binding::Set(v)) => Some(v),
            Some(Binding::Unset) | None => None,
        }
    }

    /// Define `name` in the innermost scope.
    pub(crate) fn set(&mut self, name: &[u8], value: BString) {
        self.insert_top(name, Binding::Set(value));
    }

    /// Undefine `name` in the innermost scope (shadows outer bindings).
    pub(crate) fn unset(&mut self, name: &[u8]) {
        self.insert_top(name, Binding::Unset);
    }

    fn insert_top(&mut self, name: &[u8], binding: Binding) {
        if let Some(top) = self.scopes.last_mut() {
            if let Some(current) = top.bindings.get_mut(name) {
                *current = binding;
            } else {
                top.bindings.insert(BString::from(name), binding);
            }
        }
    }

    /// Push a new empty scope.
    pub(crate) fn push(&mut self) {
        self.push_boundary(None, None);
    }

    pub(crate) fn push_boundary(
        &mut self,
        directory: Option<DirId>,
        parent_policy: Option<(usize, PolicyStatus)>,
    ) {
        self.scopes.push(Scope {
            id: self.next_id,
            directory,
            bindings: BTreeMap::new(),
            parent_policy,
        });
        self.next_id = self.next_id.saturating_add(1);
    }

    /// Pop the innermost scope. The outermost scope is never removed.
    pub(crate) fn pop(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    pub(crate) fn update_parent_policy(&mut self, depth: usize, status: PolicyStatus) {
        for scope in &mut self.scopes {
            if let Some((level, saved)) = &mut scope.parent_policy
                && *level == depth
            {
                *saved = status;
            }
        }
    }

    /// Install the captured bindings in the caller and its eligible enclosing scopes.
    pub(crate) fn propagate_capture(&mut self) -> bool {
        let Some(capture) = self.scopes.pop() else {
            return false;
        };
        let mut bindings = capture.bindings;
        bindings.retain(|name, _| !name.is_empty());
        if bindings.is_empty() {
            return false;
        }
        let mut scopes = self.scopes.iter_mut().rev();
        let Some(caller) = scopes.next() else {
            return false;
        };
        caller.bindings.extend(bindings.clone());
        let mut policy = caller.parent_policy;
        for parent in scopes {
            match policy.map(|(_, status)| status) {
                Some(PolicyStatus::New) => {
                    parent.bindings.extend(bindings.clone());
                    policy = parent.parent_policy;
                }
                Some(PolicyStatus::Warn) => return true,
                Some(PolicyStatus::Old) | None => break,
            }
        }
        false
    }

    /// Raise-then-set `PARENT_SCOPE`: copy the current effective binding of `name` into
    /// the innermost scope (so that its view does not change), then set (`Some`) or unset
    /// (`None`) `name` in the parent scope.
    ///
    /// Returns `false` when the innermost scope has no parent.
    pub(crate) fn set_parent(&mut self, name: &[u8], value: Option<BString>) -> bool {
        let len = self.scopes.len();
        if len < 2 {
            return false;
        }
        let current = self.lookup(name).cloned().unwrap_or(Binding::Unset);
        self.insert_top(name, current);
        if let Some(parent) = self.scopes.get_mut(len - 2) {
            let binding = value.map_or(Binding::Unset, Binding::Set);
            parent.bindings.insert(BString::from(name), binding);
        }
        true
    }

    /// All names with a [`Binding::Set`] value visible from the innermost scope, sorted.
    pub(crate) fn visible_names(&self) -> Vec<BString> {
        let mut seen: BTreeMap<&BString, bool> = BTreeMap::new();
        for scope in self.scopes.iter().rev() {
            for (name, binding) in &scope.bindings {
                seen.entry(name)
                    .or_insert(matches!(binding, Binding::Set(_)));
            }
        }
        seen.into_iter()
            .filter(|&(_, set)| set)
            .map(|(name, _)| name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_shadows_outer_binding() {
        let mut s = VarStack::default();
        s.set(b"a", "1".into());
        s.push();
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"1"[..]));
        s.unset(b"a");
        assert_eq!(s.get(b"a"), None);
        assert_eq!(s.lookup(b"a"), Some(&Binding::Unset));
        s.pop();
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"1"[..]));
    }

    #[test]
    fn raise_then_set_keeps_child_view() {
        let mut s = VarStack::default();
        s.set(b"a", "outer".into());
        s.push();
        assert!(s.set_parent(b"a", Some("new".into())));
        // The child still sees the value it had before the raise.
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"outer"[..]));
        // An undefined name is localized as an explicit unset.
        assert!(s.set_parent(b"b", Some("x".into())));
        assert_eq!(s.lookup(b"b"), Some(&Binding::Unset));
        s.pop();
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"new"[..]));
        assert_eq!(s.get(b"b").map(|v| v.as_slice()), Some(&b"x"[..]));
    }

    #[test]
    fn root_scope_has_no_parent() {
        let mut s = VarStack::default();
        assert!(!s.set_parent(b"a", None));
        assert_eq!(s.depth(), 1);
        s.pop();
        assert_eq!(s.depth(), 1);
    }

    #[test]
    fn capture_keeps_equal_values_and_unsets_at_each_new_boundary() {
        let mut stack = VarStack::default();
        stack.set(b"unset", "prior".into());
        stack.push_boundary(None, Some((1, PolicyStatus::Warn)));
        stack.set(b"same", "inherited".into());
        stack.push_boundary(None, Some((2, PolicyStatus::New)));
        stack.push();
        stack.set(b"same", "inherited".into());
        stack.unset(b"unset");
        assert!(stack.propagate_capture());

        stack.pop();
        assert_eq!(stack.get(b"same"), Some(&"inherited".into()));
        assert_eq!(stack.lookup(b"unset"), Some(&Binding::Unset));
        stack.pop();
        assert_eq!(stack.get(b"same"), None);
        assert_eq!(stack.get(b"unset"), Some(&"prior".into()));
    }

    #[test]
    fn visible_names_respect_shadowing() {
        let mut s = VarStack::default();
        s.set(b"a", "1".into());
        s.set(b"b", "2".into());
        s.push();
        s.unset(b"a");
        s.set(b"c", BString::default());
        assert_eq!(
            s.visible_names(),
            vec![BString::from("b"), BString::from("c")]
        );
    }
}
