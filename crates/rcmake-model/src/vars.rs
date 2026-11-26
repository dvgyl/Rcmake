// SPDX-License-Identifier: MIT OR Apache-2.0

//! Scoped variables shared between evaluation and the directory model.

use std::collections::BTreeMap;
use std::sync::Arc;

use rcmake_base::BString;

const SHARDS: usize = 64;

type Bindings = BTreeMap<BString, Binding>;

/// A value or an unset marker in one variable scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// The variable has this value.
    Set(BString),
    /// The variable is undefined and hides any outer binding.
    Unset,
}

/// One variable scope. A write copies only the affected shared shard.
#[derive(Clone, Debug)]
pub struct VarLayer {
    shards: [Option<Arc<Bindings>>; SHARDS],
}

impl Default for VarLayer {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| None),
        }
    }
}

impl VarLayer {
    /// Get the shard index for a variable name.
    #[must_use]
    pub fn shard_index(name: &[u8]) -> usize {
        let hash = name.iter().fold(2_166_136_261_u32, |hash, &byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
        });
        (hash as usize) % SHARDS
    }

    /// Look up a binding with an index from `shard_index`.
    #[must_use]
    pub fn get_at(&self, name: &[u8], shard: usize) -> Option<&Binding> {
        self.shards.get(shard)?.as_ref()?.get(name)
    }

    /// Insert or replace a binding.
    pub fn set(&mut self, name: &[u8], binding: Binding) -> Option<Binding> {
        let index = Self::shard_index(name);
        let shard = self.shards.get_mut(index)?;
        let bindings = Arc::make_mut(shard.get_or_insert_with(Arc::default));
        if let Some(previous) = bindings.get_mut(name) {
            Some(std::mem::replace(previous, binding))
        } else {
            bindings.insert(name.into(), binding)
        }
    }

    /// Iterate over this scope's bindings.
    pub fn iter(&self) -> impl Iterator<Item = (&BString, &Binding)> {
        self.shards.iter().flatten().flat_map(|shard| shard.iter())
    }

    /// Take the bindings from this scope.
    pub fn into_bindings(self) -> impl Iterator<Item = (BString, Binding)> {
        self.shards
            .into_iter()
            .flatten()
            .flat_map(Arc::unwrap_or_clone)
    }
}

impl Extend<(BString, Binding)> for VarLayer {
    fn extend<T: IntoIterator<Item = (BString, Binding)>>(&mut self, iter: T) {
        for (name, binding) in iter {
            self.set(&name, binding);
        }
    }
}

/// A frozen view of the normal variables visible when a directory ends.
#[derive(Clone, Debug, Default)]
pub struct VarSnapshot {
    layers: Arc<[VarLayer]>,
}

impl VarSnapshot {
    /// Capture scope layers in order from the outermost to the innermost.
    #[must_use]
    pub fn from_layers(layers: impl IntoIterator<Item = VarLayer>) -> Self {
        Self {
            layers: layers.into_iter().collect(),
        }
    }

    /// Get the visible value for a name.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<&BString> {
        let shard = VarLayer::shard_index(name);
        match self
            .layers
            .iter()
            .rev()
            .find_map(|layer| layer.get_at(name, shard))
        {
            Some(Binding::Set(value)) => Some(value),
            _ => None,
        }
    }

    /// Return whether this view has a value for the name.
    #[must_use]
    pub fn contains_key(&self, name: &[u8]) -> bool {
        self.get(name).is_some()
    }

    /// Insert a value into the innermost layer of this view.
    pub fn insert(&mut self, name: BString, value: BString) -> Option<BString> {
        let old = self.get(&name).cloned();
        if self.layers.is_empty() {
            self.layers = Arc::from([VarLayer::default()]);
        }
        if let Some(top) = Arc::make_mut(&mut self.layers).last_mut() {
            top.set(&name, Binding::Set(value));
        }
        old
    }

    /// Remove the visible value for a name.
    pub fn remove(&mut self, name: &[u8]) -> Option<BString> {
        let old = self.get(name)?.clone();
        if let Some(top) = Arc::make_mut(&mut self.layers).last_mut() {
            top.set(name, Binding::Unset);
        }
        Some(old)
    }

    /// Iterate over the visible variables in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&BString, &BString)> {
        let mut visible = BTreeMap::new();
        for layer in self.layers.iter().rev() {
            for (name, binding) in layer.iter() {
                visible.entry(name).or_insert(binding);
            }
        }
        visible
            .into_iter()
            .filter_map(|(name, binding)| match binding {
                Binding::Set(value) => Some((name, value)),
                Binding::Unset => None,
            })
    }

    /// Iterate over the names of the visible variables.
    pub fn keys(&self) -> impl Iterator<Item = &BString> {
        self.iter().map(|(name, _)| name)
    }

    /// Count the visible variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Return whether the view contains no visible variables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

impl Extend<(BString, BString)> for VarSnapshot {
    fn extend<T: IntoIterator<Item = (BString, BString)>>(&mut self, iter: T) {
        for (name, value) in iter {
            self.insert(name, value);
        }
    }
}

impl PartialEq for VarSnapshot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.layers, &other.layers) || self.iter().eq(other.iter())
    }
}

impl Eq for VarSnapshot {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_keeps_previous_bindings_after_scope_writes() {
        let mut outer = VarLayer::default();
        outer.set(b"A", Binding::Set("original".into()));
        outer.set(b"B", Binding::Set("hidden".into()));
        let mut inner = VarLayer::default();
        inner.set(b"B", Binding::Unset);
        inner.set(b"C", Binding::Set("inner".into()));
        let earlier = VarSnapshot::from_layers([outer.clone(), inner.clone()]);

        outer.set(b"A", Binding::Set("new".into()));
        inner.set(b"B", Binding::Set("visible".into()));
        let later = VarSnapshot::from_layers([outer, inner]);
        assert_eq!(earlier.get(b"A"), Some(&BString::from("original")));
        assert_eq!(earlier.get(b"B"), None);
        assert_eq!(later.get(b"A"), Some(&BString::from("new")));
        assert_eq!(later.get(b"B"), Some(&BString::from("visible")));
        assert_eq!(
            earlier
                .keys()
                .map(|name| name.as_slice())
                .collect::<Vec<_>>(),
            [b"A".as_slice(), b"C"]
        );
        assert_eq!(
            later.keys().map(|name| name.as_slice()).collect::<Vec<_>>(),
            [b"A".as_slice(), b"B", b"C"]
        );
    }
}
