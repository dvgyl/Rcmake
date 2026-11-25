//! Restore validated targets from a configure journal.
#![allow(unreachable_pub)]

use std::collections::BTreeSet;

use crate::{DirId, ProjectModel, Target, TargetBinding, TargetId};

impl ProjectModel {
    /// Insert the targets for a replayed directory. Check all names and arena
    /// positions before the first insertion. A failed check leaves the model intact.
    pub fn restore_directory_targets(&mut self, dir: DirId, targets: Vec<Target>) -> bool {
        if dir.index() > self.subdirs.len() {
            return false;
        }
        let mut names = BTreeSet::new();
        let mut ids = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let Some(id) = self
                .targets
                .len()
                .checked_add(index)
                .and_then(TargetId::from_index)
            else {
                return false;
            };
            if target.dir != dir
                || target.is_imported()
                || !target.file_sets.is_empty()
                || !target.event_commands.is_empty()
                || self.find_target(dir, &target.name).is_some()
                || !names.insert(&target.name)
            {
                return false;
            }
            ids.push(id);
        }
        for (target, id) in targets.into_iter().zip(ids) {
            self.global_targets
                .insert(target.name.clone(), TargetBinding::Target(id));
            self.dir_mut(dir).targets.push(id);
            self.targets.push(target);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TargetKind, TargetOrigin};

    fn target(name: &str) -> Target {
        Target::new(
            name.into(),
            TargetKind::StaticLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Default::default(),
            Default::default(),
        )
    }

    #[test]
    fn a_late_name_collision_leaves_all_target_tables_intact() -> Result<(), crate::ModelError> {
        let mut project = ProjectModel::default();
        let old = project.add_target(target("existing"))?;
        assert!(
            !project
                .restore_directory_targets(DirId::ROOT, vec![target("first"), target("existing")])
        );
        assert!(project.find_target(DirId::ROOT, b"first").is_none());
        assert_eq!(project.dir(DirId::ROOT).targets, [old]);
        assert_eq!(project.targets().count(), 1);
        assert!(
            project.restore_directory_targets(DirId::ROOT, vec![target("first"), target("second")])
        );
        assert_eq!(project.dir(DirId::ROOT).targets.len(), 3);
        Ok(())
    }
}
