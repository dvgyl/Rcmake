use std::collections::HashSet;

use rcmake_base::{BString, list};
use rcmake_diag::{Backtrace, Location, MessageKind};
use rcmake_model::TargetId;

use super::{UsageResolver, WalkValue, definition};

impl UsageResolver<'_> {
    pub(super) fn log_usage(&self, id: TargetId, property: &[u8], result: &WalkValue) {
        if !self.debug_properties {
            return;
        }
        let label = match property {
            b"SOURCES" => "sources",
            b"INCLUDE_DIRECTORIES" => "includes",
            b"COMPILE_FEATURES" => "compile features",
            _ => return,
        };
        let (Some(paths), Some(target)) = (self.source_paths(), self.model.target(id)) else {
            return;
        };
        if !definition(self.model, target.dir, b"CMAKE_DEBUG_TARGET_PROPERTIES")
            .is_some_and(|names| list::iter(names, list::Empty::Drop).any(|name| name == property))
        {
            return;
        }
        let mut groups: Vec<(&Backtrace, Vec<BString>)> = Vec::new();
        let mut seen = HashSet::new();
        let dir = self.model.dir(target.dir);
        for (owner, entries) in &result.contributions {
            let link = result
                .chains
                .as_ref()
                .and_then(|chains| chains.get(owner))
                .and_then(|hops| hops.first());
            for entry in entries.iter() {
                let value = if property == b"SOURCES" {
                    paths
                        .find(dir, &entry.value)
                        .unwrap_or_else(|| entry.value.clone())
                } else {
                    entry.value.clone()
                };
                if !seen.insert(value.clone()) {
                    continue;
                }
                let origin = link.map_or(&entry.backtrace, |hop| &hop.link_bt);
                if let Some((_, values)) = groups
                    .last_mut()
                    .filter(|(group, _)| group.frames().eq(origin.frames()))
                {
                    values.push(value);
                } else {
                    groups.push((origin, vec![value]));
                }
            }
        }
        for (origin, values) in groups {
            let mut body = BString::from(format!("Used {label} for target {}:\n", target.name));
            for value in values {
                body.extend_from_slice(b" * ");
                body.extend_from_slice(&value);
                body.push(b'\n');
            }
            paths.warn_once(
                self.model,
                MessageKind::Log,
                None,
                Location::Backtrace(origin.clone()),
                body,
            );
        }
    }
}
