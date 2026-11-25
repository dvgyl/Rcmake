//! Restore validated targets from a configure journal.
#![allow(unreachable_pub)]

use std::collections::{BTreeMap, BTreeSet};

use crate::{DirId, ProjectModel, Target, TargetBinding, TargetId, TestEntry, TestId};

impl ProjectModel {
    /// Compute the identifier that replay will assign to a new target.
    pub fn replay_target_id(&self, offset: usize) -> Option<TargetId> {
        TargetId::from_index(self.targets.len().checked_add(offset)?)
    }

    /// Insert the targets for a replayed directory. Check all names and arena
    /// positions before the first insertion. A failed check leaves the model intact.
    pub fn restore_directory_targets(&mut self, dir: DirId, targets: Vec<Target>) -> bool {
        self.restore_directory_objects(dir, targets, Vec::new(), Vec::new())
    }

    /// Restore local declarations after validation of all names and arena positions.
    pub fn restore_directory_objects(
        &mut self,
        dir: DirId,
        targets: Vec<Target>,
        aliases: Vec<(rcmake_base::BString, rcmake_base::BString)>,
        tests: Vec<TestEntry>,
    ) -> bool {
        if dir.index() > self.subdirs.len() {
            return false;
        }
        let mut names = BTreeMap::new();
        let mut ids = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let Some(id) = self.replay_target_id(index) else {
                return false;
            };
            if target.dir != dir
                || target.is_imported()
                || !target.file_sets.is_empty()
                || !target.event_commands.is_empty()
                || self.find_target(dir, &target.name).is_some()
                || names.insert(&target.name, id).is_some()
            {
                return false;
            }
            ids.push(id);
        }
        let mut alias_names = BTreeSet::new();
        let mut bindings = Vec::with_capacity(aliases.len());
        for (alias, target) in &aliases {
            let Some(&id) = names.get(target) else {
                return false;
            };
            if names.contains_key(alias)
                || !alias_names.insert(alias)
                || self.find_target(dir, alias).is_some()
            {
                return false;
            }
            bindings.push(id);
        }
        let mut test_names = BTreeSet::new();
        let mut test_ids = Vec::with_capacity(tests.len());
        for (offset, test) in tests.iter().enumerate() {
            let Some(id) = self
                .tests
                .len()
                .checked_add(offset)
                .and_then(TestId::from_index)
            else {
                return false;
            };
            if test.dir != dir
                || test.is_discovery()
                || !test_names.insert(&test.name)
                || self.find_test(dir, &test.name).is_some()
            {
                return false;
            }
            test_ids.push(id);
        }
        for (target, id) in targets.into_iter().zip(ids) {
            self.global_targets
                .insert(target.name.clone(), TargetBinding::Target(id));
            self.dir_mut(dir).targets.push(id);
            self.targets.push(target);
        }
        for ((alias, _), id) in aliases.into_iter().zip(bindings) {
            self.global_targets.insert(alias, TargetBinding::Alias(id));
        }
        for (test, id) in tests.into_iter().zip(test_ids) {
            self.dir_mut(dir).tests.insert(test.name.clone(), id);
            self.tests.push(test);
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
