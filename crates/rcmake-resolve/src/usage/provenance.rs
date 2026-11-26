use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::{BT, TargetId};

use crate::{ChainKey, ChainKind, Hop, ProvenanceEntry, ResolveChains};

#[derive(Default)]
pub(crate) struct Provenance {
    pub(crate) entries: Vec<ProvenanceEntry>,
    pub(crate) chains: ResolveChains,
    ordinals: IndexMap<(TargetId, ConfigId, ChainKind), u32>,
}

impl Provenance {
    pub(crate) fn record(
        &mut self,
        target: TargetId,
        config: &ConfigId,
        kind: ChainKind,
        value: &BT<BString>,
        hops: Vec<Hop>,
        link_targets: Vec<TargetId>,
    ) {
        let ordinal = self
            .ordinals
            .entry((target, config.clone(), kind))
            .or_default();
        let key = ChainKey {
            target,
            config: config.clone(),
            kind,
            ordinal: *ordinal,
        };
        *ordinal = ordinal.saturating_add(1);
        self.chains.insert(key.clone(), hops);
        self.entries.push(ProvenanceEntry {
            key,
            value: value.clone(),
            link_targets,
        });
    }

    pub(crate) fn extend(&mut self, other: Self) {
        if other.entries.is_empty() && other.chains.is_empty() {
            return;
        }
        let mut bases = IndexMap::new();
        for (key, count) in other.ordinals {
            let slot = self.ordinals.entry(key.clone()).or_default();
            let base = *slot;
            *slot = base.saturating_add(count);
            if base != 0 {
                bases.insert(key, base);
            }
        }
        if bases.is_empty() {
            self.entries.extend(other.entries);
            self.chains.extend(other.chains);
        } else {
            for mut entry in other.entries {
                let lookup = (entry.key.target, entry.key.config.clone(), entry.key.kind);
                if let Some(&base) = bases.get(&lookup) {
                    entry.key.ordinal = entry.key.ordinal.saturating_add(base);
                }
                self.entries.push(entry);
            }
            for (mut key, hops) in other.chains {
                let lookup = (key.target, key.config.clone(), key.kind);
                if let Some(&base) = bases.get(&lookup) {
                    key.ordinal = key.ordinal.saturating_add(base);
                }
                self.chains.insert(key, hops);
            }
        }
    }
}
