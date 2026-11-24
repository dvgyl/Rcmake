use std::collections::{BTreeSet, HashSet};

use super::{BString, Graph, Ninja, qualified};

impl Graph<'_> {
    pub(crate) fn emit_entry_aliases(&self, ninja: &mut Ninja) {
        let existing = if self.config.is_some() {
            HashSet::new()
        } else {
            self.extra_aliases
                .keys()
                .cloned()
                .chain([self.node(b"all")])
                .collect()
        };
        self.emit_aliases(ninja, None, &[self], &existing);
    }

    pub(crate) fn emit_aliases(
        &self,
        ninja: &mut Ninja,
        suffix: Option<&[u8]>,
        configurations: &[&Self],
        reserved: &HashSet<BString>,
    ) {
        let extra: BTreeSet<_> = configurations
            .iter()
            .flat_map(|graph| graph.extra_aliases.keys())
            .collect();
        let names = self
            .input
            .targets
            .iter()
            .map(|target| self.node(&target.name))
            .chain([self.node(b"all")])
            .chain(extra.into_iter().cloned());
        for name in names {
            let output = qualified(&name, suffix);
            if self.producers.contains_key(&output)
                || reserved.contains(&output)
                || self.aliases.contains(&output)
            {
                continue;
            }
            let inputs = configurations
                .iter()
                .filter_map(|graph| {
                    if name == self.node(b"all") || graph.extra_aliases.contains_key(&name) {
                        Some(qualified(&name, graph.config))
                    } else {
                        graph
                            .names
                            .get(&name)
                            .and_then(|&index| graph.aliases.get(index).cloned())
                    }
                })
                .collect::<Vec<_>>();
            if !inputs.is_empty() {
                ninja.phony(&output, &inputs);
            }
        }
    }
}
