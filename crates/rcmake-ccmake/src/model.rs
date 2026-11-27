//! Cache view model: filtering, sorting, new-entry tracking, search, and pending edits.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::BString;
use rcmake_model::CacheType;

use crate::{CacheSnapshot, EntryEdit, EntrySnapshot};

/// One displayable cache entry inside [`CacheView`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewEntry {
    /// Cache variable name.
    pub name: BString,
    /// Cache entry type (never `INTERNAL`, `STATIC`, or `UNINITIALIZED`).
    pub ty: CacheType,
    /// Current value (reflecting any local unconfigured edits).
    pub value: BString,
    /// Help string (`HELPSTRING`).
    pub help: BString,
    /// Whether marked `ADVANCED`.
    pub advanced: bool,
    /// Allowed choice values from the `STRINGS` property, if any.
    pub strings: Vec<BString>,
    /// Whether this entry was newly added by the most recent configure pass.
    pub is_new: bool,
}

/// Interactive view over a CMake cache with advanced filtering, sorting, search,
/// and queued edits.
#[derive(Clone, Debug, Default)]
pub struct CacheView {
    entries: Vec<ViewEntry>,
    known_keys: BTreeSet<BString>,
    advanced_mode: bool,
    pending_edits: BTreeMap<BString, EntryEdit>,
}

impl CacheView {
    /// Build a [`CacheView`] from an initial cache snapshot.
    ///
    /// Initial entries already on disk start with `is_new = false`, matching
    /// upstream `ccmake` after its startup `Configure(1)` pre-check.
    #[must_use]
    pub fn from_initial(snapshot: CacheSnapshot) -> Self {
        let mut view = Self::default();
        for entry in snapshot.entries {
            if is_hidden_type(entry.ty) {
                continue;
            }
            view.known_keys.insert(entry.name.clone());
            view.entries.push(ViewEntry {
                name: entry.name,
                ty: entry.ty,
                value: entry.value,
                help: entry.help,
                advanced: entry.advanced,
                strings: entry.strings,
                is_new: false,
            });
        }
        view.sort_entries();
        view
    }

    /// Reload entries after a configure run, marking any newly discovered entries
    /// (plus any explicitly reported in `explicit_new`) as new and sorting them first.
    ///
    /// Returns `true` if at least one new entry was introduced.
    pub fn reload_after_configure(
        &mut self,
        snapshot: CacheSnapshot,
        explicit_new: &BTreeSet<BString>,
    ) -> bool {
        let mut next_entries = Vec::new();
        let mut next_known = BTreeSet::new();
        let mut any_new = !explicit_new.is_empty();

        for entry in snapshot.entries {
            if is_hidden_type(entry.ty) {
                continue;
            }
            let is_new =
                explicit_new.contains(&entry.name) || !self.known_keys.contains(&entry.name);
            if is_new {
                any_new = true;
            }
            next_known.insert(entry.name.clone());
            next_entries.push(ViewEntry {
                name: entry.name,
                ty: entry.ty,
                value: entry.value,
                help: entry.help,
                advanced: entry.advanced,
                strings: entry.strings,
                is_new,
            });
        }

        self.entries = next_entries;
        self.known_keys = next_known;
        self.pending_edits.clear();
        self.sort_entries();
        any_new
    }

    /// Whether advanced cache entries are currently shown.
    #[must_use]
    pub const fn advanced_mode(&self) -> bool {
        self.advanced_mode
    }

    /// Toggle advanced mode and return the new state.
    pub fn toggle_advanced(&mut self) -> bool {
        self.advanced_mode = !self.advanced_mode;
        self.advanced_mode
    }

    /// Return references to all currently visible entries in display order.
    #[must_use]
    pub fn visible_entries(&self) -> Vec<&ViewEntry> {
        self.entries
            .iter()
            .filter(|entry| self.advanced_mode || !entry.advanced)
            .collect()
    }

    /// Number of currently visible entries.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| self.advanced_mode || !entry.advanced)
            .count()
    }

    /// Look up the visible entry at `visible_idx`.
    #[must_use]
    pub fn visible_get(&self, visible_idx: usize) -> Option<&ViewEntry> {
        self.entries
            .iter()
            .filter(|entry| self.advanced_mode || !entry.advanced)
            .nth(visible_idx)
    }

    /// Find the visible index of an entry by exact name, if currently visible.
    #[must_use]
    pub fn visible_index_of(&self, name: &[u8]) -> Option<usize> {
        self.entries
            .iter()
            .filter(|entry| self.advanced_mode || !entry.advanced)
            .position(|entry| entry.name.as_slice() == name)
    }

    /// Update the value of the visible entry at `visible_idx` and record an [`EntryEdit::Set`].
    ///
    /// Returns `true` if the value changed.
    pub fn set_visible_value(&mut self, visible_idx: usize, value: BString) -> bool {
        let Some(pos) = self.actual_index(visible_idx) else {
            return false;
        };
        let Some(entry) = self.entries.get_mut(pos) else {
            return false;
        };
        if entry.value == value {
            return false;
        }
        entry.value = value.clone();
        self.pending_edits.insert(
            entry.name.clone(),
            EntryEdit::Set {
                name: entry.name.clone(),
                ty: entry.ty,
                value,
            },
        );
        true
    }

    /// Delete the visible entry at `visible_idx` and queue an [`EntryEdit::Remove`].
    ///
    /// Returns the removed entry's name if one existed.
    pub fn remove_visible(&mut self, visible_idx: usize) -> Option<BString> {
        let pos = self.actual_index(visible_idx)?;
        if pos >= self.entries.len() {
            return None;
        }
        let removed = self.entries.remove(pos);
        self.known_keys.remove(&removed.name);
        self.pending_edits.insert(
            removed.name.clone(),
            EntryEdit::Remove {
                name: removed.name.clone(),
            },
        );
        Some(removed.name)
    }

    /// All pending edits in deterministic name order for the next configure run.
    #[must_use]
    pub fn pending_edits(&self) -> Vec<EntryEdit> {
        self.pending_edits.values().cloned().collect()
    }

    /// Forget the pending edits after a configure consumed them.
    pub fn clear_pending_edits(&mut self) {
        self.pending_edits.clear();
    }

    /// Find the next visible entry index whose name contains `query` (ASCII case-insensitive),
    /// starting after `current` and wrapping around.
    #[must_use]
    pub fn search_next(&self, current: usize, query: &str) -> Option<usize> {
        self.search_impl(current, query, false)
    }

    /// Find the previous visible entry index whose name contains `query` (ASCII case-insensitive),
    /// starting before `current` and wrapping around.
    #[must_use]
    pub fn search_prev(&self, current: usize, query: &str) -> Option<usize> {
        self.search_impl(current, query, true)
    }

    fn search_impl(&self, current: usize, query: &str, reverse: bool) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        let visible = self.visible_entries();
        let count = visible.len();
        if count == 0 {
            return None;
        }
        let needle: Vec<u8> = query.bytes().map(|b| b.to_ascii_lowercase()).collect();
        let start = current.min(count.saturating_sub(1));
        for step in 1..=count {
            let idx = if reverse {
                (start + count - (step % count)) % count
            } else {
                (start + step) % count
            };
            if let Some(entry) = visible.get(idx)
                && contains_ascii_ci(entry.name.as_slice(), &needle)
            {
                return Some(idx);
            }
        }
        None
    }

    fn actual_index(&self, visible_idx: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.advanced_mode || !entry.advanced)
            .nth(visible_idx)
            .map(|(idx, _)| idx)
    }

    fn sort_entries(&mut self) {
        self.entries
            .sort_by(|a, b| b.is_new.cmp(&a.is_new).then_with(|| a.name.cmp(&b.name)));
    }
}

/// Whether `ty` is hidden from the interactive cache view.
#[must_use]
pub const fn is_hidden_type(ty: CacheType) -> bool {
    matches!(
        ty,
        CacheType::Internal | CacheType::Static | CacheType::Uninitialized
    )
}

fn contains_ascii_ci(haystack: &[u8], lower_needle: &[u8]) -> bool {
    if lower_needle.is_empty() {
        return true;
    }
    if lower_needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(lower_needle.len()).any(|window| {
        window
            .iter()
            .zip(lower_needle.iter())
            .all(|(&h, &n)| h.to_ascii_lowercase() == n)
    })
}

/// Convenience helper for building a single [`EntrySnapshot`] in tests or drivers.
#[must_use]
pub fn snapshot_entry(
    name: impl Into<BString>,
    ty: CacheType,
    value: impl Into<BString>,
    help: impl Into<BString>,
    advanced: bool,
    strings: Vec<BString>,
) -> EntrySnapshot {
    EntrySnapshot {
        name: name.into(),
        ty,
        value: value.into(),
        help: help.into(),
        advanced,
        strings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> CacheSnapshot {
        CacheSnapshot {
            entries: vec![
                snapshot_entry("INTERNAL_VAR", CacheType::Internal, "1", "", false, vec![]),
                snapshot_entry("STATIC_VAR", CacheType::Static, "1", "", false, vec![]),
                snapshot_entry(
                    "UNINIT_VAR",
                    CacheType::Uninitialized,
                    "1",
                    "",
                    false,
                    vec![],
                ),
                snapshot_entry("ZEBRA", CacheType::String, "z", "zebra help", false, vec![]),
                snapshot_entry("ALPHA", CacheType::Bool, "OFF", "alpha help", false, vec![]),
                snapshot_entry("ADV_FOO", CacheType::String, "a", "adv help", true, vec![]),
                snapshot_entry(
                    "FOO_BAR",
                    CacheType::String,
                    "fb",
                    "foo help",
                    false,
                    vec![],
                ),
                snapshot_entry(
                    "FOO_BAZ",
                    CacheType::String,
                    "fz",
                    "baz help",
                    false,
                    vec![],
                ),
            ],
        }
    }

    #[test]
    fn hides_internal_static_uninitialized_and_filters_advanced() {
        let mut view = CacheView::from_initial(sample_snapshot());
        let names: Vec<_> = view
            .visible_entries()
            .into_iter()
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(names, vec!["ALPHA", "FOO_BAR", "FOO_BAZ", "ZEBRA"]);

        assert!(view.toggle_advanced());
        let names_adv: Vec<_> = view
            .visible_entries()
            .into_iter()
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(
            names_adv,
            vec!["ADV_FOO", "ALPHA", "FOO_BAR", "FOO_BAZ", "ZEBRA"]
        );
    }

    #[test]
    fn new_entries_sort_first_and_are_marked() {
        let mut view = CacheView::from_initial(sample_snapshot());
        let mut next = sample_snapshot();
        next.entries.push(snapshot_entry(
            "NEW_ZZZ",
            CacheType::Bool,
            "ON",
            "new",
            false,
            vec![],
        ));
        next.entries.push(snapshot_entry(
            "NEW_AAA",
            CacheType::String,
            "x",
            "new",
            false,
            vec![],
        ));
        let added = view.reload_after_configure(next, &BTreeSet::new());
        assert!(added);
        let items: Vec<_> = view
            .visible_entries()
            .into_iter()
            .map(|e| (e.name.to_string(), e.is_new))
            .collect();
        assert_eq!(
            items,
            vec![
                ("NEW_AAA".to_owned(), true),
                ("NEW_ZZZ".to_owned(), true),
                ("ALPHA".to_owned(), false),
                ("FOO_BAR".to_owned(), false),
                ("FOO_BAZ".to_owned(), false),
                ("ZEBRA".to_owned(), false),
            ]
        );

        // Second reload with same entries clears new marks.
        let mut same = sample_snapshot();
        same.entries.push(snapshot_entry(
            "NEW_ZZZ",
            CacheType::Bool,
            "ON",
            "new",
            false,
            vec![],
        ));
        same.entries.push(snapshot_entry(
            "NEW_AAA",
            CacheType::String,
            "x",
            "new",
            false,
            vec![],
        ));
        assert!(!view.reload_after_configure(same, &BTreeSet::new()));
        assert!(view.visible_entries().iter().all(|e| !e.is_new));
    }

    #[test]
    fn search_wraps_forward_and_backward() {
        let view = CacheView::from_initial(sample_snapshot());
        // Visible: 0=ALPHA, 1=FOO_BAR, 2=FOO_BAZ, 3=ZEBRA
        assert_eq!(view.search_next(0, "foo"), Some(1));
        assert_eq!(view.search_next(1, "foo"), Some(2));
        assert_eq!(view.search_next(2, "foo"), Some(1));
        assert_eq!(view.search_prev(1, "foo"), Some(2));
        assert_eq!(view.search_prev(2, "foo"), Some(1));
        assert_eq!(view.search_next(0, "NO_MATCH"), None);
    }
}
