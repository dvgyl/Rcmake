//! Restore validated targets from a configure journal.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use rcmake_base::BString;

use crate::{DirId, ProjectModel, PropertyMap, Target, TargetBinding, TargetId, TestEntry, TestId};

/// Directory property that holds the source registration records of a directory in
/// configure call order. Each record is `R` (relative) or `A` (absolute), the
/// normalized path, and a NUL byte. The resolver reads it for ambiguous source names.
pub const SOURCE_ORDER_PROPERTY: &[u8] = b"\0rcmake-source-order";

/// The source state of one directory before a listfile runs.
#[derive(Clone, Debug)]
pub struct DirectorySourceMark {
    dir: DirId,
    order: BString,
    sources: IndexMap<BString, PropertyMap>,
    generated: BTreeSet<BString>,
}

/// The source registrations that one listfile added to its directory: the appended
/// records of [`SOURCE_ORDER_PROPERTY`]. Each absolute record also declares a source
/// property entry, in record order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectorySources(pub BString);

/// Split source registration records. Return `None` for a malformed record.
fn source_records(bytes: &[u8]) -> Option<Vec<(bool, &[u8])>> {
    let mut records = Vec::new();
    let mut rest = bytes;
    while let Some((&tag, tail)) = rest.split_first() {
        let end = tail.iter().position(|&byte| byte == 0)?;
        let (path, tail) = tail.split_at(end);
        let absolute = match tag {
            b'A' => true,
            b'R' => false,
            _ => return None,
        };
        if path.is_empty() {
            return None;
        }
        records.push((absolute, path));
        rest = tail.get(1..)?;
    }
    Some(records)
}

impl ProjectModel {
    /// Compute the identifier that replay will assign to a new target.
    pub fn replay_target_id(&self, offset: usize) -> Option<TargetId> {
        TargetId::from_index(self.targets.len().checked_add(offset)?)
    }

    /// Insert the targets for a replayed directory. Check all names and arena
    /// positions before the first insertion. A failed check leaves the model intact.
    #[cfg(test)]
    fn restore_directory_targets(&mut self, dir: DirId, targets: Vec<Target>) -> bool {
        self.restore_directory_objects(
            dir,
            targets,
            Vec::new(),
            Vec::new(),
            &DirectorySources::default(),
        )
    }

    /// Save the source state of `dir` before a listfile runs.
    #[must_use]
    pub fn directory_source_mark(&self, dir: DirId) -> DirectorySourceMark {
        let directory = self.dir(dir);
        DirectorySourceMark {
            dir,
            order: directory
                .props
                .get(SOURCE_ORDER_PROPERTY)
                .cloned()
                .unwrap_or_default(),
            sources: directory.source_props.clone(),
            generated: directory.generated_sources.clone(),
        }
    }

    /// Return the source registrations added to the directory of `mark` since the
    /// mark. Return `None` when the listfile changed the directory sources in another
    /// way, which replay cannot restore: an edit of an earlier record or source
    /// property, a generated source, or a source entry without a matching record.
    #[must_use]
    pub fn directory_sources_since(&self, mark: &DirectorySourceMark) -> Option<DirectorySources> {
        let directory = self.dir(mark.dir);
        let order = directory
            .props
            .get(SOURCE_ORDER_PROPERTY)
            .map_or(&[][..], |value| value.as_slice());
        let added = order.strip_prefix(mark.order.as_slice())?;
        let records = source_records(added)?;
        if directory.generated_sources != mark.generated
            || directory.source_props.len() < mark.sources.len()
            || directory
                .source_props
                .iter()
                .zip(&mark.sources)
                .any(|(now, before)| now != before)
        {
            return None;
        }
        let declared: BTreeSet<&[u8]> = records
            .iter()
            .filter(|(absolute, _)| *absolute)
            .map(|(_, path)| *path)
            .collect();
        let fresh_valid = directory
            .source_props
            .iter()
            .skip(mark.sources.len())
            .all(|(path, props)| declared.contains(path.as_slice()) && props.names().is_empty());
        let declared_present = declared
            .iter()
            .all(|path| directory.source_props.contains_key(*path));
        (fresh_valid && declared_present).then(|| DirectorySources(added.into()))
    }

    /// Restore local declarations after validation of all names and arena positions.
    /// Supply targets and tests in their original creation order. `sources` holds the
    /// source registrations of the same listfile.
    pub fn restore_directory_objects(
        &mut self,
        dir: DirId,
        targets: Vec<Target>,
        aliases: Vec<(rcmake_base::BString, rcmake_base::BString)>,
        tests: Vec<TestEntry>,
        sources: &DirectorySources,
    ) -> bool {
        if dir.index() > self.subdirs.len() {
            return false;
        }
        let Some(records) = source_records(&sources.0) else {
            return false;
        };
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
        let directory = self.dir_mut(dir);
        for (absolute, path) in records {
            if absolute {
                directory.source_props.entry(path.into()).or_default();
            }
        }
        if !sources.0.is_empty() {
            directory
                .props
                .append_string(SOURCE_ORDER_PROPERTY.into(), &sources.0);
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

    fn test(name: &str) -> TestEntry {
        TestEntry {
            name: name.into(),
            dir: DirId::ROOT,
            backtrace: Default::default(),
            command: Vec::new(),
            configurations: Vec::new(),
            working_directory: None,
            command_expand_lists: false,
            old_style: false,
            policies: Default::default(),
            props: Default::default(),
            build_depends: Vec::new(),
            kind: crate::TestKind::Single,
        }
    }

    #[test]
    fn restores_targets_and_tests_in_order_after_an_atomic_collision()
    -> Result<(), crate::ModelError> {
        let mut model = ProjectModel::default();
        let existing = model.add_test(test("existing"))?;
        assert!(!model.restore_directory_objects(
            DirId::ROOT,
            vec![target("second"), target("first")],
            vec![("alias".into(), "first".into())],
            vec![test("test_second"), test("existing")],
            &DirectorySources(b"A/src/a.c\0".as_slice().into()),
        ));
        assert!(model.dir(DirId::ROOT).source_props.is_empty());
        assert_eq!(model.targets().count(), 0);
        assert!(model.find_target(DirId::ROOT, b"alias").is_none());
        assert_eq!(
            model
                .dir_tests(DirId::ROOT)
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [existing]
        );
        assert!(model.restore_directory_objects(
            DirId::ROOT,
            vec![target("second"), target("first")],
            Vec::new(),
            vec![test("test_second"), test("test_first")],
            &DirectorySources::default(),
        ));
        assert_eq!(
            model
                .targets()
                .map(|(_, target)| target.name.as_slice())
                .collect::<Vec<_>>(),
            [b"second".as_slice(), b"first"]
        );
        assert_eq!(
            model
                .dir_tests(DirId::ROOT)
                .map(|(_, test)| test.name.as_slice())
                .collect::<Vec<_>>(),
            [b"existing".as_slice(), b"test_second", b"test_first"]
        );
        Ok(())
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

    /// Apply the same writes that `add_library(x STATIC a.c /abs/b.c)` makes.
    fn register(model: &mut ProjectModel, records: &[(&[u8], bool)]) {
        let directory = model.dir_mut(DirId::ROOT);
        for (path, absolute) in records {
            if *absolute {
                directory.source_props.entry((*path).into()).or_default();
            }
            let mut entry = vec![if *absolute { b'A' } else { b'R' }];
            entry.extend_from_slice(path);
            entry.push(0);
            directory
                .props
                .append_string(SOURCE_ORDER_PROPERTY.into(), &entry);
        }
    }

    #[test]
    fn restored_source_registrations_equal_the_live_directory_state() {
        let records: &[(&[u8], bool)] =
            &[(b"a.c", false), (b"/src/b.c", true), (b"/src/a.c", true)];
        let mut live = ProjectModel::default();
        register(&mut live, &[(b"/src/a.c", true)]);
        let mark = live.directory_source_mark(DirId::ROOT);
        register(&mut live, records);
        let Some(added) = live.directory_sources_since(&mark) else {
            panic!("the registrations are restorable");
        };

        let mut replayed = ProjectModel::default();
        register(&mut replayed, &[(b"/src/a.c", true)]);
        assert!(replayed.restore_directory_objects(
            DirId::ROOT,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &added
        ));
        let (live, replayed) = (live.dir(DirId::ROOT), replayed.dir(DirId::ROOT));
        assert_eq!(live.props, replayed.props);
        assert_eq!(
            live.source_props.keys().collect::<Vec<_>>(),
            replayed.source_props.keys().collect::<Vec<_>>()
        );
        assert_eq!(live.source_props, replayed.source_props);
    }

    #[test]
    fn unrestorable_source_changes_and_malformed_records_are_rejected() {
        let mut model = ProjectModel::default();
        register(&mut model, &[(b"/src/a.c", true)]);
        let mark = model.directory_source_mark(DirId::ROOT);
        model
            .dir_mut(DirId::ROOT)
            .source_props
            .entry("/src/a.c".into())
            .or_default()
            .set("COMPILE_DEFINITIONS".into(), "X".into());
        assert!(model.directory_sources_since(&mark).is_none());

        let mut model = ProjectModel::default();
        let mark = model.directory_source_mark(DirId::ROOT);
        model
            .dir_mut(DirId::ROOT)
            .source_props
            .entry("/src/undeclared.c".into())
            .or_default();
        assert!(model.directory_sources_since(&mark).is_none());

        for malformed in [&b"Xa.c\0"[..], b"Ra.c", b"R\0", b"A"] {
            let mut model = ProjectModel::default();
            assert!(!model.restore_directory_objects(
                DirId::ROOT,
                vec![target("t")],
                Vec::new(),
                Vec::new(),
                &DirectorySources(malformed.into())
            ));
            assert_eq!(model.targets().count(), 0);
            assert!(model.dir(DirId::ROOT).props.names().is_empty());
        }
    }
}
