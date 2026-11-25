//! Variable scope stack with explicit `Set`/`Unset` bindings and raise-then-set
//! `PARENT_SCOPE` semantics (ADR-005).

use std::collections::BTreeMap;

use rcmake_base::BString;
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
        self.push_scope(None);
    }

    pub(crate) fn push_directory(&mut self, dir: DirId) {
        self.push_scope(Some(dir));
    }

    fn push_scope(&mut self, directory: Option<DirId>) {
        self.scopes.push(Scope {
            id: self.next_id,
            directory,
            bindings: BTreeMap::new(),
        });
        self.next_id = self.next_id.saturating_add(1);
    }

    /// Pop the innermost scope. The outermost scope is never removed.
    pub(crate) fn pop(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
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
