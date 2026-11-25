//! Install entries in script order.

use std::collections::BTreeMap;

use rcmake_model::{DirId, InstallKind, InstallRule, ProjectModel, RuntimeDependencyInstall};

#[derive(Clone, Copy)]
pub(super) enum InstallEntry<'a> {
    Rule(usize, &'a InstallRule, Option<usize>),
    Directory(DirId),
    RuntimeSet(&'a RuntimeDependencyInstall),
}

pub(super) fn ordered_install_rules(
    model: &ProjectModel,
) -> Vec<(usize, &InstallRule, Option<usize>)> {
    ordered_entries(model)
        .into_iter()
        .filter_map(|entry| match entry {
            InstallEntry::Rule(index, rule, export) => Some((index, rule, export)),
            InstallEntry::Directory(_) | InstallEntry::RuntimeSet(_) => None,
        })
        .collect()
}

pub(super) fn ordered_entries(model: &ProjectModel) -> Vec<InstallEntry<'_>> {
    use rcmake_diag::{PolicyId, PolicyStatus};

    let mut entries: BTreeMap<_, Vec<_>> = BTreeMap::new();
    let mut export_count = 0;
    for (index, rule) in model.installs.iter().enumerate() {
        let included = !model.dir_excluded_from_all(rule.dir);
        let export = if included && matches!(rule.kind, InstallKind::Export { .. }) {
            let plan = export_count;
            export_count += 1;
            Some(plan)
        } else {
            None
        };
        if included {
            entries.entry(rule.dir).or_default().push((
                index,
                true,
                InstallEntry::Rule(index, rule, export),
            ));
        }
    }
    for set in &model.runtime_dependency_installs {
        if !model.dir_excluded_from_all(set.dir) {
            entries.entry(set.dir).or_default().push((
                set.position,
                false,
                InstallEntry::RuntimeSet(set),
            ));
        }
    }
    for (id, directory) in model.dirs().skip(1) {
        let Some(parent) = directory.parent else {
            continue;
        };
        if model.dir_excluded_from_all(id) {
            continue;
        }
        let interleave = model
            .dir(parent)
            .final_policies
            .is_none_or(|policies| policies.get(PolicyId::new(82)) == PolicyStatus::New);
        let position = if interleave {
            directory.install_position
        } else {
            usize::MAX
        };
        entries
            .entry(parent)
            .or_default()
            .push((position, false, InstallEntry::Directory(id)));
    }
    for directory in entries.values_mut() {
        directory.sort_by_key(|&(position, is_rule, _)| (position, is_rule));
    }
    let mut pending = vec![InstallEntry::Directory(DirId::ROOT)];
    let mut ordered = Vec::with_capacity(model.installs.len());
    while let Some(entry) = pending.pop() {
        match entry {
            InstallEntry::Rule(..) | InstallEntry::RuntimeSet(_) => ordered.push(entry),
            InstallEntry::Directory(id) => {
                ordered.push(entry);
                if let Some(items) = entries.remove(&id) {
                    pending.extend(items.into_iter().rev().map(|entry| entry.2));
                }
            }
        }
    }
    ordered
}
