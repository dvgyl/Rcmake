//! Variable scope stack with explicit `Set`/`Unset` bindings and raise-then-set
//! `PARENT_SCOPE` semantics (ADR-005).

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::PolicyStatus;
use rcmake_model::{Binding, DirId, VarLayer, VarSnapshot};

use crate::util::FxBuildHasher;

pub(crate) const RECURSION_LIMIT: &[u8] = b"CMAKE_MAXIMUM_RECURSION_DEPTH";

pub(crate) fn parse_recursion_limit(value: &[u8]) -> Option<u32> {
    let value = core::str::from_utf8(value.trim_ascii_start())
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(u32::try_from(value).unwrap_or(u32::MAX))
}

#[derive(Clone, Copy, Debug, Default)]
enum RecursionBinding {
    #[default]
    Inherit,
    Unset,
    Defined(Option<u32>),
}

impl RecursionBinding {
    fn configured(self) -> Option<Option<u32>> {
        match self {
            Self::Defined(value) => Some(value),
            Self::Inherit | Self::Unset => None,
        }
    }
}

type Bindings = IndexMap<BString, Binding, FxBuildHasher>;

/// Outcome of an in-place list append on the innermost scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    /// Items were appended directly to the existing local `Binding::Set`.
    Appended,
    /// The variable is not currently a `Binding::Set` in an unshared innermost scope.
    NotLocal,
    /// Appending would exceed `max_value_bytes`.
    ValueTooLarge,
    /// Appending would exceed the live variable budget.
    BudgetExceeded,
}

/// Fixed cost of one binding for the live variable budget (map node and string headers).
const BINDING_OVERHEAD: usize = 64;

/// A write would make the live variables use more bytes than the limit allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BudgetExceeded;

/// Bytes that one binding of `name` adds to the live variable budget, or `None` on overflow.
fn binding_cost(name: &[u8], binding: &Binding) -> Option<usize> {
    let value = match binding {
        Binding::Set(value) => value.len(),
        Binding::Unset => 0,
    };
    name.len().checked_add(value)?.checked_add(BINDING_OVERHEAD)
}

/// Cost of a binding that the budget already admitted.
fn admitted_cost(name: &[u8], binding: &Binding) -> usize {
    binding_cost(name, binding).unwrap_or(0)
}

/// Remove the cost of a replaced binding, then add the cost of the new binding.
fn charge(total: usize, removed: usize, added: usize) -> Option<usize> {
    total.checked_sub(removed)?.checked_add(added)
}

fn recursion_binding(binding: &Binding) -> RecursionBinding {
    match binding {
        Binding::Unset => RecursionBinding::Unset,
        Binding::Set(value) => RecursionBinding::Defined(parse_recursion_limit(value)),
    }
}

#[derive(Debug)]
enum ScopeBindings {
    Local(Bindings),
    Shared(Box<VarLayer>),
}

impl Default for ScopeBindings {
    fn default() -> Self {
        Self::Local(Bindings::default())
    }
}

impl ScopeBindings {
    fn get(&self, name: &[u8], shard: &mut Option<usize>) -> Option<&Binding> {
        match self {
            Self::Local(bindings) => bindings.get(name),
            Self::Shared(bindings) => bindings.get_at(
                name,
                *shard.get_or_insert_with(|| VarLayer::shard_index(name)),
            ),
        }
    }

    /// Bind `name` after `admit` accepts the binding that the new one replaces.
    ///
    /// If `admit` returns an error, the bindings do not change.
    fn set_checked<E>(
        &mut self,
        name: &[u8],
        value: Binding,
        admit: impl FnOnce(Option<&Binding>) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Local(bindings) => {
                if let Some(current) = bindings.get_mut(name) {
                    admit(Some(current))?;
                    *current = value;
                } else {
                    admit(None)?;
                    bindings.insert(name.into(), value);
                }
            }
            Self::Shared(bindings) => {
                admit(bindings.get_at(name, VarLayer::shard_index(name)))?;
                bindings.set(name, value);
            }
        }
        Ok(())
    }

    fn iter(&self) -> impl Iterator<Item = (&BString, &Binding)> {
        let (local, shared) = match self {
            Self::Local(bindings) => (Some(bindings), None),
            Self::Shared(bindings) => (None, Some(bindings.as_ref())),
        };
        local
            .into_iter()
            .flat_map(Bindings::iter)
            .chain(shared.into_iter().flat_map(VarLayer::iter))
    }

    fn into_bindings(self) -> Bindings {
        match self {
            Self::Local(bindings) => bindings,
            Self::Shared(bindings) => bindings.into_bindings().collect(),
        }
    }

    fn snapshot(&mut self) -> VarLayer {
        match self {
            Self::Local(bindings) => {
                let mut shared = VarLayer::default();
                shared.extend(std::mem::take(bindings));
                let snapshot = shared.clone();
                *self = Self::Shared(Box::new(shared));
                snapshot
            }
            Self::Shared(bindings) => bindings.as_ref().clone(),
        }
    }
}

#[derive(Debug)]
struct Scope {
    id: u64,
    directory: Option<DirId>,
    bindings: ScopeBindings,
    /// Sum of [`binding_cost`] over `bindings`.
    bytes: usize,
    parent_policy: Option<(usize, PolicyStatus)>,
    recursion_limit: RecursionBinding,
}

impl Scope {
    fn new(
        id: u64,
        directory: Option<DirId>,
        parent_policy: Option<(usize, PolicyStatus)>,
    ) -> Self {
        Self {
            id,
            directory,
            bindings: ScopeBindings::default(),
            bytes: 0,
            parent_policy,
            recursion_limit: RecursionBinding::Inherit,
        }
    }

    /// Bind `name` in this scope if the live total `live` stays within `limit`.
    ///
    /// Return the new live total. On an error, nothing changes.
    fn bind(
        &mut self,
        name: &[u8],
        binding: Binding,
        live: usize,
        limit: usize,
    ) -> Result<usize, BudgetExceeded> {
        let added = binding_cost(name, &binding).ok_or(BudgetExceeded)?;
        let bytes = self.bytes;
        let mut totals = (live, bytes);
        self.bindings.set_checked(name, binding, |old| {
            let removed = old.map_or(0, |old| admitted_cost(name, old));
            let live = charge(live, removed, added)
                .filter(|&live| live <= limit)
                .ok_or(BudgetExceeded)?;
            totals = (live, charge(bytes, removed, added).ok_or(BudgetExceeded)?);
            Ok(())
        })?;
        self.bytes = totals.1;
        Ok(totals.0)
    }

    /// Cost of the binding of `name` in this scope, or zero.
    fn cost_of(&self, name: &[u8]) -> usize {
        self.bindings
            .get(name, &mut None)
            .map_or(0, |binding| admitted_cost(name, binding))
    }
}

/// Stack of variable scopes. Index `0` is the outermost (directory) scope.
#[derive(Debug)]
pub(crate) struct VarStack {
    scopes: Vec<Scope>,
    next_id: u64,
    recursion_limit: Option<Option<u32>>,
    /// Sum of `Scope::bytes` over all scopes.
    live: usize,
}

impl Default for VarStack {
    fn default() -> Self {
        Self {
            scopes: vec![Scope::new(0, Some(DirId::ROOT), None)],
            next_id: 1,
            recursion_limit: None,
            live: 0,
        }
    }
}

impl VarStack {
    /// Bytes of all live bindings in all scopes, as counted by [`binding_cost`].
    pub(crate) fn live_bytes(&self) -> usize {
        self.live
    }

    /// Number of active scopes.
    pub(crate) fn depth(&self) -> usize {
        self.scopes.len()
    }

    /// Get the parsed normal value. A defined but invalid value uses the default limit.
    pub(crate) fn recursion_limit(&self) -> Option<Option<u32>> {
        self.recursion_limit
    }

    fn update_recursion_limit(&mut self) {
        self.recursion_limit = self
            .scopes
            .iter()
            .rev()
            .find(|scope| !matches!(scope.recursion_limit, RecursionBinding::Inherit))
            .and_then(|scope| scope.recursion_limit.configured());
    }

    /// Find the innermost binding for `name`.
    pub(crate) fn lookup(&self, name: &[u8]) -> Option<&Binding> {
        let mut shard = None;
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name, &mut shard))
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
        let mut shard = None;
        let binding = self
            .scopes
            .iter()
            .rev()
            .skip(usize::from(parent))
            .find_map(|scope| scope.bindings.get(name, &mut shard));
        match binding {
            Some(Binding::Set(v)) => Some(v),
            Some(Binding::Unset) | None => None,
        }
    }

    /// Define `name` in the innermost scope if the live total stays within `limit`.
    pub(crate) fn set(
        &mut self,
        name: &[u8],
        value: BString,
        limit: usize,
    ) -> Result<(), BudgetExceeded> {
        self.insert_top(name, Binding::Set(value), limit)
    }

    /// Append `;`-separated `items` directly to `name` when it is already a `Binding::Set`
    /// in an unshared innermost scope. Enforces `max_value_bytes` and `live_limit` before
    /// mutating anything, avoiding O(N²) cloning in `list(APPEND)` loops.
    pub(crate) fn append_local_in_place(
        &mut self,
        name: &[u8],
        items: &[rcmake_expr::ExpandedArg],
        max_value_bytes: usize,
        live_limit: usize,
    ) -> AppendOutcome {
        if items.is_empty() || name == RECURSION_LIMIT {
            return AppendOutcome::NotLocal;
        }
        let Some(top) = self.scopes.last_mut() else {
            return AppendOutcome::NotLocal;
        };
        let ScopeBindings::Local(bindings) = &mut top.bindings else {
            return AppendOutcome::NotLocal;
        };
        let Some(Binding::Set(existing)) = bindings.get_mut(name) else {
            return AppendOutcome::NotLocal;
        };
        let existing_len = existing.len();
        let separators = items.len() - 1 + usize::from(existing_len > 0);
        let joined = items
            .iter()
            .try_fold(existing_len, |len, item| len.checked_add(item.value.len()))
            .and_then(|len| len.checked_add(separators));
        let Some(joined) = joined.filter(|&len| len <= max_value_bytes) else {
            return AppendOutcome::ValueTooLarge;
        };
        let delta = joined - existing_len;
        let Some(new_live) = self
            .live
            .checked_add(delta)
            .filter(|&live| live <= live_limit)
        else {
            return AppendOutcome::BudgetExceeded;
        };
        let Some(new_scope_bytes) = top.bytes.checked_add(delta) else {
            return AppendOutcome::BudgetExceeded;
        };
        let has_value = existing_len > 0;
        existing.reserve(delta);
        for (i, item) in items.iter().enumerate() {
            if has_value || i != 0 {
                existing.push(b';');
            }
            existing.extend_from_slice(&item.value);
        }
        top.bytes = new_scope_bytes;
        self.live = new_live;
        AppendOutcome::Appended
    }

    /// Undefine `name` in the innermost scope (shadows outer bindings) if the live total
    /// stays within `limit`.
    pub(crate) fn unset(&mut self, name: &[u8], limit: usize) -> Result<(), BudgetExceeded> {
        self.insert_top(name, Binding::Unset, limit)
    }

    fn insert_top(
        &mut self,
        name: &[u8],
        binding: Binding,
        limit: usize,
    ) -> Result<(), BudgetExceeded> {
        let Some(top) = self.scopes.last_mut() else {
            return Ok(());
        };
        let recursion = (name == RECURSION_LIMIT).then(|| recursion_binding(&binding));
        self.live = top.bind(name, binding, self.live, limit)?;
        if let Some(recursion) = recursion {
            top.recursion_limit = recursion;
            self.recursion_limit = top.recursion_limit.configured();
        }
        Ok(())
    }

    /// Cost of the binding of `name` in the scope at `index`, or zero.
    fn cost_at(&self, index: usize, name: &[u8]) -> usize {
        self.scopes
            .get(index)
            .map_or(0, |scope| scope.cost_of(name))
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
        self.scopes
            .push(Scope::new(self.next_id, directory, parent_policy));
        self.next_id = self.next_id.saturating_add(1);
    }

    /// Pop the innermost scope. The outermost scope is never removed.
    pub(crate) fn pop(&mut self) {
        if self.scopes.len() > 1
            && let Some(scope) = self.scopes.pop()
        {
            self.live = self.live.saturating_sub(scope.bytes);
            if !matches!(scope.recursion_limit, RecursionBinding::Inherit) {
                self.update_recursion_limit();
            }
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
    ///
    /// The complete charge is checked against `limit` before any binding is copied. On an
    /// error, the captured scope is gone and no enclosing scope changes.
    pub(crate) fn propagate_capture(&mut self, limit: usize) -> Result<bool, BudgetExceeded> {
        let Some(capture) = self.scopes.pop() else {
            return Ok(false);
        };
        self.live = self.live.saturating_sub(capture.bytes);
        if !matches!(capture.recursion_limit, RecursionBinding::Inherit) {
            self.update_recursion_limit();
        }
        let mut bindings = capture.bindings.into_bindings();
        bindings.retain(|name, binding| {
            let value = match binding {
                Binding::Set(value) => Some(&*value),
                Binding::Unset => None,
            };
            !name.is_empty() && self.get(name) != value
        });
        if bindings.is_empty() {
            return Ok(false);
        }
        let Some(caller) = self.scopes.len().checked_sub(1) else {
            return Ok(false);
        };
        // The caller, then each enclosing scope that the policy chain admits.
        let mut targets = vec![caller];
        let mut policy = self
            .scopes
            .get(caller)
            .and_then(|scope| scope.parent_policy);
        let mut warn = false;
        for index in (0..caller).rev() {
            match policy.map(|(_, status)| status) {
                Some(PolicyStatus::New) => {
                    targets.push(index);
                    policy = self.scopes.get(index).and_then(|scope| scope.parent_policy);
                }
                Some(PolicyStatus::Warn) => {
                    warn = true;
                    break;
                }
                Some(PolicyStatus::Old) | None => break,
            }
        }
        let mut projected = self.live;
        for &index in &targets {
            for (name, binding) in &bindings {
                let added = binding_cost(name, binding).ok_or(BudgetExceeded)?;
                projected =
                    charge(projected, self.cost_at(index, name), added).ok_or(BudgetExceeded)?;
            }
        }
        if projected > limit {
            return Err(BudgetExceeded);
        }
        let recursion = bindings.get(RECURSION_LIMIT).map(recursion_binding);
        for &index in &targets {
            let Some(scope) = self.scopes.get_mut(index) else {
                continue;
            };
            for (name, binding) in &bindings {
                self.live = scope.bind(name, binding.clone(), self.live, usize::MAX)?;
            }
            if let Some(recursion) = recursion {
                scope.recursion_limit = recursion;
            }
        }
        if recursion.is_some() {
            self.update_recursion_limit();
        }
        Ok(warn)
    }

    /// Check that `PARENT_SCOPE` writes for all `names`, in their order, keep the live total
    /// within `limit` after every single write.
    ///
    /// Each name copies its current binding into the innermost scope, then its value into
    /// the parent scope. A later name can free bytes, but an earlier write must still fit.
    /// A repeated name adds nothing. Nothing changes.
    pub(crate) fn preflight_raise(
        &self,
        names: &[BString],
        limit: usize,
    ) -> Result<(), BudgetExceeded> {
        let len = self.scopes.len();
        let (Some(top), Some(parent)) = (len.checked_sub(1), len.checked_sub(2)) else {
            return Ok(());
        };
        let within = |live: Option<usize>| live.filter(|&live| live <= limit).ok_or(BudgetExceeded);
        let mut seen = BTreeSet::new();
        let mut projected = self.live;
        for name in names.iter().map(|name| name.as_slice()) {
            if name.is_empty() || !seen.insert(name) {
                continue;
            }
            let current = self
                .lookup(name)
                .map_or_else(
                    || binding_cost(name, &Binding::Unset),
                    |binding| binding_cost(name, binding),
                )
                .ok_or(BudgetExceeded)?;
            projected = within(charge(projected, self.cost_at(top, name), current))?;
            projected = within(charge(projected, self.cost_at(parent, name), current))?;
        }
        Ok(())
    }

    /// Raise-then-set `PARENT_SCOPE`: copy the current effective binding of `name` into
    /// the innermost scope (so that its view does not change), then set (`Some`) or unset
    /// (`None`) `name` in the parent scope.
    ///
    /// Returns `false` when the innermost scope has no parent. Both writes are checked
    /// against `limit` before the current binding is copied. On an error, nothing changes.
    pub(crate) fn set_parent(
        &mut self,
        name: &[u8],
        value: Option<BString>,
        limit: usize,
    ) -> Result<bool, BudgetExceeded> {
        let len = self.scopes.len();
        let (Some(top), Some(parent)) = (len.checked_sub(1), len.checked_sub(2)) else {
            return Ok(false);
        };
        let binding = value.map_or(Binding::Unset, Binding::Set);
        let unset_cost = binding_cost(name, &Binding::Unset).ok_or(BudgetExceeded)?;
        let current_cost = self
            .lookup(name)
            .map_or(Some(unset_cost), |current| binding_cost(name, current));
        let added = binding_cost(name, &binding).ok_or(BudgetExceeded)?;
        let projected = current_cost
            .and_then(|current| charge(self.live, self.cost_at(top, name), current))
            .and_then(|live| charge(live, self.cost_at(parent, name), added))
            .ok_or(BudgetExceeded)?;
        if projected > limit {
            return Err(BudgetExceeded);
        }
        let current = self.lookup(name).cloned().unwrap_or(Binding::Unset);
        self.insert_top(name, current, usize::MAX)?;
        if let Some(scope) = self.scopes.get_mut(parent) {
            let recursion = (name == RECURSION_LIMIT).then(|| recursion_binding(&binding));
            self.live = scope.bind(name, binding, self.live, usize::MAX)?;
            if let Some(recursion) = recursion {
                scope.recursion_limit = recursion;
            }
        }
        Ok(true)
    }

    pub(crate) fn snapshot(&mut self) -> VarSnapshot {
        VarSnapshot::from_layers(
            self.scopes
                .iter_mut()
                .map(|scope| scope.bindings.snapshot()),
        )
    }

    /// All names with a [`Binding::Set`] value visible from the innermost scope, sorted.
    pub(crate) fn visible_names(&self) -> Vec<BString> {
        let mut seen: BTreeMap<&BString, bool> = BTreeMap::new();
        for scope in self.scopes.iter().rev() {
            for (name, binding) in scope.bindings.iter() {
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

    impl VarStack {
        /// Count the live bytes again from the bindings of every scope.
        fn recount(&self) -> usize {
            self.scopes
                .iter()
                .flat_map(|scope| scope.bindings.iter())
                .map(|(name, binding)| admitted_cost(name, binding))
                .sum()
        }

        fn put(&mut self, name: &[u8], value: BString) {
            assert_eq!(self.set(name, value, usize::MAX), Ok(()));
        }

        fn clear(&mut self, name: &[u8]) {
            assert_eq!(self.unset(name, usize::MAX), Ok(()));
        }

        fn raise(&mut self, name: &[u8], value: Option<BString>) -> bool {
            self.set_parent(name, value, usize::MAX) == Ok(true)
        }

        fn capture(&mut self) -> bool {
            self.propagate_capture(usize::MAX) == Ok(true)
        }
    }

    #[test]
    fn refused_writes_change_nothing() {
        let mut stack = VarStack::default();
        stack.put(b"a", "1".into());
        let before = stack.live_bytes();
        assert_eq!(
            stack.set(b"a", "x".repeat(100).into(), before + 10),
            Err(BudgetExceeded)
        );
        assert_eq!(stack.unset(b"new", before), Err(BudgetExceeded));
        assert_eq!(stack.live_bytes(), before);
        assert_eq!(stack.get(b"a"), Some(&"1".into()));
        assert_eq!(stack.lookup(b"new"), None);
        // A replacement that frees bytes passes at the current total.
        assert_eq!(stack.set(b"a", BString::default(), before), Ok(()));
        assert_eq!(stack.live_bytes(), stack.recount());
    }

    #[test]
    fn refused_parent_scope_write_changes_neither_scope() {
        let mut stack = VarStack::default();
        stack.put(b"big", "x".repeat(1000).into());
        stack.push();
        let before = stack.live_bytes();
        assert_eq!(
            stack.set_parent(b"big", Some("y".repeat(1000).into()), before + 500),
            Err(BudgetExceeded)
        );
        assert_eq!(stack.live_bytes(), before);
        assert_eq!(stack.cost_at(1, b"big"), 0);
        stack.pop();
        assert_eq!(stack.get(b"big"), Some(&"x".repeat(1000).into()));
    }

    #[test]
    fn refused_capture_changes_no_enclosing_scope() {
        let mut stack = VarStack::default();
        stack.push_boundary(None, Some((1, PolicyStatus::New)));
        stack.push();
        stack.put(b"lang", "z".repeat(1000).into());
        let limit = stack.live_bytes();
        assert_eq!(stack.propagate_capture(limit), Err(BudgetExceeded));
        assert_eq!(stack.live_bytes(), 0);
        assert_eq!(stack.recount(), 0);
        assert_eq!(stack.get(b"lang"), None);
    }

    #[test]
    fn refused_raise_preflight_changes_no_name() {
        let mut stack = VarStack::default();
        stack.push();
        stack.put(b"small", "1".into());
        stack.put(b"big", "x".repeat(1000).into());
        let before = stack.live_bytes();
        let names = [
            BString::from("small"),
            BString::from("big"),
            BString::from("big"),
        ];
        assert_eq!(
            stack.preflight_raise(&names, before + 500),
            Err(BudgetExceeded)
        );
        assert_eq!(stack.preflight_raise(&names, before + 2000), Ok(()));
        assert_eq!(stack.live_bytes(), before);
        stack.pop();
        assert_eq!(stack.lookup(b"small"), None);
    }

    #[test]
    fn raise_preflight_checks_each_step_in_caller_order() {
        let mut stack = VarStack::default();
        stack.put(b"b", "x".repeat(1000).into());
        stack.push();
        stack.put(b"a", "y".repeat(600).into());
        stack.put(b"b", BString::default());
        let names = |order: [&str; 2]| order.map(BString::from);
        // Raising `a` first goes over the limit before raising `b` frees the large value.
        assert_eq!(
            stack.preflight_raise(&names(["a", "b"]), 2000),
            Err(BudgetExceeded)
        );
        // The final total is the same in the other order, and every step fits.
        assert_eq!(stack.preflight_raise(&names(["b", "a"]), 2000), Ok(()));
    }

    #[test]
    fn charge_reports_overflow_instead_of_saturating() {
        assert_eq!(charge(usize::MAX, 0, 1), None);
        assert_eq!(charge(5, 6, 0), None);
        assert_eq!(charge(usize::MAX, 1, 1), Some(usize::MAX));
    }

    #[test]
    fn live_bytes_follow_replacements_unsets_and_pops() {
        let mut stack = VarStack::default();
        stack.put(b"a", "xx".into());
        assert_eq!(stack.live_bytes(), 1 + 2 + BINDING_OVERHEAD);
        stack.put(b"a", "xxxx".into());
        assert_eq!(stack.live_bytes(), 1 + 4 + BINDING_OVERHEAD);
        stack.clear(b"a");
        assert_eq!(stack.live_bytes(), 1 + BINDING_OVERHEAD);
        let root = stack.live_bytes();

        stack.push();
        stack.put(b"local", "value".into());
        let _ = stack.snapshot();
        stack.put(b"local", "longer value".into());
        stack.put(b"other", "1".into());
        assert_eq!(stack.live_bytes(), stack.recount());
        stack.pop();
        assert_eq!(stack.live_bytes(), root);
    }

    #[test]
    fn live_bytes_count_parent_scope_and_captured_copies() {
        let mut stack = VarStack::default();
        stack.put(b"keep", "root".into());
        stack.push_boundary(None, Some((1, PolicyStatus::New)));
        assert!(stack.raise(b"up", Some("value".into())));
        assert_eq!(stack.live_bytes(), stack.recount());

        stack.push_boundary(None, Some((2, PolicyStatus::New)));
        stack.push();
        stack.put(b"lang", "C".into());
        stack.put(b"keep", "changed".into());
        assert!(!stack.capture());
        assert_eq!(stack.live_bytes(), stack.recount());

        stack.pop();
        stack.pop();
        assert_eq!(stack.live_bytes(), stack.recount());
        assert_eq!(stack.get(b"lang"), Some(&"C".into()));
        assert_eq!(stack.get(b"up"), Some(&"value".into()));
    }

    #[test]
    fn snapshot_keeps_visible_values_after_later_scope_writes() {
        let mut stack = VarStack::default();
        stack.put(b"outer", "before".into());
        stack.put(b"hidden", "outer".into());
        stack.push();
        stack.clear(b"hidden");
        let captured = stack.snapshot();
        stack.put(b"outer", "after".into());
        stack.pop();
        stack.put(b"outer", "next".into());
        let next = stack.snapshot();

        assert_eq!(captured.get(b"outer"), Some(&"before".into()));
        assert_eq!(captured.get(b"hidden"), None);
        assert_eq!(next.get(b"outer"), Some(&"next".into()));
        assert_eq!(next.get(b"hidden"), Some(&"outer".into()));
    }

    #[test]
    fn unset_shadows_outer_binding() {
        let mut s = VarStack::default();
        s.put(b"a", "1".into());
        s.push();
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"1"[..]));
        s.clear(b"a");
        assert_eq!(s.get(b"a"), None);
        assert_eq!(s.lookup(b"a"), Some(&Binding::Unset));
        s.pop();
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"1"[..]));
    }

    #[test]
    fn raise_then_set_keeps_child_view() {
        let mut s = VarStack::default();
        s.put(b"a", "outer".into());
        s.push();
        assert!(s.raise(b"a", Some("new".into())));
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"outer"[..]));
        assert!(s.raise(b"b", Some("x".into())));
        assert_eq!(s.lookup(b"b"), Some(&Binding::Unset));
        s.pop();
        assert_eq!(s.get(b"a").map(|v| v.as_slice()), Some(&b"new"[..]));
        assert_eq!(s.get(b"b").map(|v| v.as_slice()), Some(&b"x"[..]));
    }

    #[test]
    fn root_scope_has_no_parent() {
        let mut s = VarStack::default();
        assert!(!s.raise(b"a", None));
        assert_eq!(s.depth(), 1);
        s.pop();
        assert_eq!(s.depth(), 1);
    }

    #[test]
    fn capture_propagates_only_changed_values_and_unsets() {
        let mut stack = VarStack::default();
        stack.put(b"unset", "prior".into());
        stack.push_boundary(None, Some((1, PolicyStatus::Warn)));
        stack.put(b"same", "inherited".into());
        stack.push_boundary(None, Some((2, PolicyStatus::New)));
        stack.push();
        stack.put(b"same", "inherited".into());
        stack.clear(b"unset");
        assert!(stack.capture());

        assert!(
            !stack
                .scopes
                .last()
                .is_some_and(|scope| scope.bindings.get(b"same", &mut None).is_some())
        );
        stack.pop();
        assert_eq!(stack.lookup(b"unset"), Some(&Binding::Unset));
        stack.pop();
        assert_eq!(stack.get(b"same"), None);
        assert_eq!(stack.get(b"unset"), Some(&"prior".into()));
    }

    #[test]
    fn capture_keeps_function_local_changes_out_of_the_root() {
        let mut stack = VarStack::default();
        stack.put(b"E", "rootE".into());
        stack.put(b"U", "rootU".into());
        stack.push_boundary(None, Some((1, PolicyStatus::New)));
        stack.put(b"E", "fval".into());
        stack.clear(b"U");
        stack.push();
        stack.put(b"E", "fval".into());
        stack.clear(b"U");
        assert!(!stack.capture());
        stack.pop();
        assert_eq!(stack.get(b"E"), Some(&"rootE".into()));
        assert_eq!(stack.get(b"U"), Some(&"rootU".into()));
    }

    #[test]
    fn visible_names_respect_shadowing() {
        let mut s = VarStack::default();
        s.put(b"a", "1".into());
        s.put(b"b", "2".into());
        s.push();
        s.clear(b"a");
        s.put(b"c", BString::default());
        assert_eq!(
            s.visible_names(),
            vec![BString::from("b"), BString::from("c")]
        );
    }

    #[test]
    fn append_local_in_place_updates_live_bytes_and_respects_limits() {
        let item = |s: &str| rcmake_expr::ExpandedArg {
            value: BString::from(s),
            kind: rcmake_syntax::ArgKind::Unquoted,
            line: 1,
        };
        let mut s = VarStack::default();
        assert_eq!(
            s.append_local_in_place(b"missing", &[item("a")], 1024, usize::MAX),
            AppendOutcome::NotLocal
        );
        s.put(b"list", BString::default());
        assert_eq!(
            s.append_local_in_place(b"list", &[], 1024, usize::MAX),
            AppendOutcome::NotLocal
        );
        assert_eq!(
            s.append_local_in_place(b"list", &[item("a"), item("bb")], 1024, usize::MAX),
            AppendOutcome::Appended
        );
        assert_eq!(s.get(b"list"), Some(&BString::from("a;bb")));
        assert_eq!(s.live_bytes(), s.recount());

        assert_eq!(
            s.append_local_in_place(b"list", &[item("ccc")], 1024, usize::MAX),
            AppendOutcome::Appended
        );
        assert_eq!(s.get(b"list"), Some(&BString::from("a;bb;ccc")));
        assert_eq!(s.live_bytes(), s.recount());

        // Exact max_value_bytes boundary rejects without mutating value or live bytes.
        let before = s.live_bytes();
        assert_eq!(
            s.append_local_in_place(b"list", &[item("dd")], 10, usize::MAX),
            AppendOutcome::ValueTooLarge
        );
        assert_eq!(s.get(b"list"), Some(&BString::from("a;bb;ccc")));
        assert_eq!(s.live_bytes(), before);

        // Live budget boundary rejects without mutating value or live bytes.
        assert_eq!(
            s.append_local_in_place(b"list", &[item("dddd")], 1024, before + 2),
            AppendOutcome::BudgetExceeded
        );
        assert_eq!(s.get(b"list"), Some(&BString::from("a;bb;ccc")));
        assert_eq!(s.live_bytes(), before);

        // Unset, parent-only, recursion limit, and shared snapshots all report NotLocal.
        s.push();
        assert_eq!(
            s.append_local_in_place(b"list", &[item("child")], 1024, usize::MAX),
            AppendOutcome::NotLocal
        );
        s.clear(b"list");
        assert_eq!(
            s.append_local_in_place(b"list", &[item("child")], 1024, usize::MAX),
            AppendOutcome::NotLocal
        );
        s.put(RECURSION_LIMIT, "10".into());
        assert_eq!(
            s.append_local_in_place(RECURSION_LIMIT, &[item("20")], 1024, usize::MAX),
            AppendOutcome::NotLocal
        );
        s.put(b"local", "x".into());
        let _snap = s.snapshot();
        assert_eq!(
            s.append_local_in_place(b"local", &[item("y")], 1024, usize::MAX),
            AppendOutcome::NotLocal
        );
        s.pop();
        assert_eq!(s.get(b"list"), Some(&BString::from("a;bb;ccc")));
        assert_eq!(s.live_bytes(), s.recount());
    }
}
