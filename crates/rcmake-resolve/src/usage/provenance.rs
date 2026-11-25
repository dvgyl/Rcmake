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
        self.entries.extend(other.entries);
        self.chains.extend(other.chains);
    }
}
