//! The arena-based [`ProjectModel`] and its scoped property API.

use indexmap::IndexMap;
use rcmake_base::{BString, list};
use rcmake_diag::Backtrace;

use crate::target::join_entries;
use crate::{
    AppendMode, BT, Cache, CacheEntry, CacheType, CommandId, CustomCommand, DirId, Directory,
    ExportSet, FileSet, FileSetId, GeneratorKind, GlobalState, ImportedVisibility, InstallRule,
    LinkItem, PropOrigin, PropScope, Target, TargetBinding, TargetId, TargetOrigin, TestEntry,
    TestId, UsageField, UsageProps,
};

/// Error of a [`ProjectModel`] operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// An arena has more items than its id type can address.
    #[error("too many project model objects")]
    ArenaFull,
    /// A target with this name is already visible.
    #[error("target name already exists")]
    DuplicateTarget(TargetId),
    /// A test with this name already exists in the directory.
    #[error("test name already exists")]
    DuplicateTest(TestId),
    /// The property owner does not exist (unknown target, test, or cache entry).
    #[error("property owner does not exist")]
    UnknownObject,
    /// The property can not be set (`NAME`, `TYPE`, ...).
    #[error("property is read-only")]
    ReadOnly,
    /// The value is not valid for the property (for example a cache `TYPE`).
    #[error("invalid property value")]
    InvalidValue,
}

/// Raw configure-time project state (ADR-007, layer 1 of ADR-009).
///
/// [`DirId::ROOT`] always exists. Normal targets, `IMPORTED GLOBAL` targets, and their
/// aliases have global names. Other imported targets and their aliases are visible in
/// the creating directory and in subdirectories that are created later.
#[derive(Clone, Debug, Default)]
pub struct ProjectModel {
    /// Cache entries (`CMakeCache.txt`).
    pub cache: Cache,
    /// Global properties and `define_property()` entries.
    pub global: GlobalState,
    /// Selected generator (`None` in script mode).
    pub generator: Option<GeneratorKind>,
    /// Install rules in call order.
    pub installs: Vec<InstallRule>,
    /// Export sets by name.
    pub exports: IndexMap<BString, ExportSet>,
    root: Directory,
    subdirs: Vec<Directory>,
    targets: Vec<Target>,
    commands: Vec<CustomCommand>,
    tests: Vec<TestEntry>,
    file_sets: Vec<FileSet>,
    global_targets: IndexMap<BString, TargetBinding>,
}

fn push_arena<T, I>(
    arena: &mut Vec<T>,
    item: T,
    make: fn(usize) -> Option<I>,
) -> Result<I, ModelError> {
    let id = make(arena.len()).ok_or(ModelError::ArenaFull)?;
    arena.push(item);
    Ok(id)
}

impl ProjectModel {
    // ----------------------------------------------------------------------------------
    // Directories.
    // ----------------------------------------------------------------------------------

    /// Look up a variable definition as seen at the end of configuring `dir`:
    /// normal variable snapshot first, else `CMakeCache.txt` value.
    #[must_use]
    pub fn definition(&self, dir: DirId, name: &[u8]) -> Option<&BString> {
        self.dir(dir)
            .vars
            .get(name)
            .or_else(|| self.cache.get(name).map(|e| &e.value))
    }

    /// Directory `id`. An id from a different model resolves to the root directory.
    #[must_use]
    pub fn dir(&self, id: DirId) -> &Directory {
        match id.index().checked_sub(1) {
            Some(i) => self.subdirs.get(i).unwrap_or(&self.root),
            None => &self.root,
        }
    }

    /// Mutable directory `id`. An id from a different model resolves to the root
    /// directory.
    pub fn dir_mut(&mut self, id: DirId) -> &mut Directory {
        match id.index().checked_sub(1) {
            Some(i) if i < self.subdirs.len() => self.subdirs.get_mut(i).unwrap_or(&mut self.root),
            _ => &mut self.root,
        }
    }

    /// All directories in creation order (the root first).
    pub fn dirs(&self) -> impl Iterator<Item = (DirId, &Directory)> {
        core::iter::once(&self.root)
            .chain(&self.subdirs)
            .enumerate()
            .filter_map(|(i, dir)| DirId::from_index(i).map(|id| (id, dir)))
    }

    /// Set the source and binary directory of the root directory.
    pub fn set_root_dirs(&mut self, source_dir: BString, binary_dir: BString) {
        self.root.source_dir = source_dir;
        self.root.binary_dir = binary_dir;
    }

    /// Add a subdirectory of `parent` (`add_subdirectory()`). As in
    /// `cmMakefile::InitializeFromParent`, the new directory copies the parent's usage
    /// requirements, its `LABELS`, `LINK_LIBRARIES`, and
    /// `IMPLICIT_DEPENDS_INCLUDE_TRANSFORM` properties, and its visible local imported
    /// targets and aliases. The caller sets `exclude_from_all` and `system`.
    pub fn add_directory(
        &mut self,
        parent: DirId,
        source_dir: BString,
        binary_dir: BString,
        backtrace: Backtrace,
    ) -> Result<DirId, ModelError> {
        const INHERITED: [&[u8]; 3] = [
            b"IMPLICIT_DEPENDS_INCLUDE_TRANSFORM",
            b"LABELS",
            b"LINK_LIBRARIES",
        ];
        let parent_dir = self.dir(parent);
        let mut dir = Directory {
            parent: Some(parent),
            source_dir,
            binary_dir,
            backtrace,
            usage: parent_dir.usage.clone(),
            local_targets: parent_dir.local_targets.clone(),
            ..Directory::default()
        };
        for name in INHERITED {
            if let Some(value) = parent_dir.props.get(name) {
                dir.props.set(name.into(), value.clone());
            }
        }
        let id = DirId::from_index(self.subdirs.len() + 1).ok_or(ModelError::ArenaFull)?;
        self.subdirs.push(dir);
        self.dir_mut(parent).children.push(id);
        Ok(id)
    }

    // ----------------------------------------------------------------------------------
    // Targets.
    // ----------------------------------------------------------------------------------

    /// Target `id`.
    #[must_use]
    pub fn target(&self, id: TargetId) -> Option<&Target> {
        self.targets.get(id.index())
    }

    /// Mutable target `id`.
    pub fn target_mut(&mut self, id: TargetId) -> Option<&mut Target> {
        self.targets.get_mut(id.index())
    }

    /// All targets in creation order.
    pub fn targets(&self) -> impl Iterator<Item = (TargetId, &Target)> {
        self.targets
            .iter()
            .enumerate()
            .filter_map(|(i, t)| TargetId::from_index(i).map(|id| (id, t)))
    }

    /// What `name` resolves to from directory `dir` (`cmMakefile::FindTargetToUse`):
    /// local imported targets and aliases first, then global names.
    #[must_use]
    pub fn lookup_target(&self, dir: DirId, name: &[u8]) -> Option<TargetBinding> {
        self.dir(dir)
            .local_targets
            .get(name)
            .or_else(|| self.global_targets.get(name))
            .copied()
    }

    /// The real target that `name` names from directory `dir` (aliases resolved).
    #[must_use]
    pub fn find_target(&self, dir: DirId, name: &[u8]) -> Option<TargetId> {
        self.lookup_target(dir, name).map(TargetBinding::id)
    }

    /// Add `target` and register its name. Returns
    /// [`ModelError::DuplicateTarget`] when the name is already visible from
    /// `target.dir`.
    pub fn add_target(&mut self, target: Target) -> Result<TargetId, ModelError> {
        if let Some(existing) = self.find_target(target.dir, &target.name) {
            return Err(ModelError::DuplicateTarget(existing));
        }
        let dir = target.dir;
        let name = target.name.clone();
        let global = target.is_global();
        let id = push_arena(&mut self.targets, target, TargetId::from_index)?;
        if global {
            self.global_targets.insert(name, TargetBinding::Target(id));
        } else {
            self.dir_mut(dir)
                .local_targets
                .insert(name, TargetBinding::Target(id));
        }
        self.dir_mut(dir).targets.push(id);
        Ok(id)
    }

    /// Add `ALIAS` name `alias` for `target`, created in directory `dir`. An alias of a
    /// non-global imported target is local to `dir`.
    pub fn add_alias(
        &mut self,
        dir: DirId,
        alias: BString,
        target: TargetId,
    ) -> Result<(), ModelError> {
        if let Some(existing) = self.find_target(dir, &alias) {
            return Err(ModelError::DuplicateTarget(existing));
        }
        let global = self
            .target(target)
            .ok_or(ModelError::UnknownObject)?
            .is_global();
        let binding = TargetBinding::Alias(target);
        if global {
            self.global_targets.insert(alias, binding);
        } else {
            self.dir_mut(dir).local_targets.insert(alias, binding);
        }
        Ok(())
    }

    /// Promote an imported target to global visibility (`IMPORTED_GLOBAL`).
    pub fn set_imported_global(&mut self, id: TargetId) -> Result<(), ModelError> {
        let target = self
            .targets
            .get_mut(id.index())
            .ok_or(ModelError::UnknownObject)?;
        if !target.is_imported() {
            return Err(ModelError::InvalidValue);
        }
        target.origin = TargetOrigin::Imported(ImportedVisibility::Global);
        let name = target.name.clone();
        self.global_targets.insert(name, TargetBinding::Target(id));
        Ok(())
    }

    // ----------------------------------------------------------------------------------
    // Custom commands, tests, file sets, installs.
    // ----------------------------------------------------------------------------------

    /// Add a custom command.
    pub fn add_command(&mut self, command: CustomCommand) -> Result<CommandId, ModelError> {
        push_arena(&mut self.commands, command, CommandId::from_index)
    }

    /// Custom command `id`.
    #[must_use]
    pub fn command(&self, id: CommandId) -> Option<&CustomCommand> {
        self.commands.get(id.index())
    }

    /// Mutable custom command `id` (`add_custom_command(APPEND)`).
    pub fn command_mut(&mut self, id: CommandId) -> Option<&mut CustomCommand> {
        self.commands.get_mut(id.index())
    }

    /// All custom commands in creation order.
    pub fn commands(&self) -> impl Iterator<Item = (CommandId, &CustomCommand)> {
        self.commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| CommandId::from_index(i).map(|id| (id, c)))
    }

    /// Add a test to its directory. Returns [`ModelError::DuplicateTest`] when the
    /// directory already has a test with the same name.
    pub fn add_test(&mut self, test: TestEntry) -> Result<TestId, ModelError> {
        if let Some(&existing) = self.dir(test.dir).tests.get(&test.name) {
            return Err(ModelError::DuplicateTest(existing));
        }
        let dir = test.dir;
        let name = test.name.clone();
        let id = push_arena(&mut self.tests, test, TestId::from_index)?;
        self.dir_mut(dir).tests.insert(name, id);
        Ok(id)
    }

    /// Test `id`.
    #[must_use]
    pub fn test(&self, id: TestId) -> Option<&TestEntry> {
        self.tests.get(id.index())
    }

    /// Mutable test `id`.
    pub fn test_mut(&mut self, id: TestId) -> Option<&mut TestEntry> {
        self.tests.get_mut(id.index())
    }

    /// Test `name` of directory `dir` (`if(TEST)` looks only in the current directory).
    #[must_use]
    pub fn find_test(&self, dir: DirId, name: &[u8]) -> Option<TestId> {
        self.dir(dir).tests.get(name).copied()
    }

    /// Add a file set and attach it to its target.
    pub fn add_file_set(&mut self, file_set: FileSet) -> Result<FileSetId, ModelError> {
        let target = file_set.target;
        if self.target(target).is_none() {
            return Err(ModelError::UnknownObject);
        }
        let id = push_arena(&mut self.file_sets, file_set, FileSetId::from_index)?;
        if let Some(t) = self.target_mut(target) {
            t.file_sets.push(id);
        }
        Ok(id)
    }

    /// File set `id`.
    #[must_use]
    pub fn file_set(&self, id: FileSetId) -> Option<&FileSet> {
        self.file_sets.get(id.index())
    }

    /// Mutable file set `id`.
    pub fn file_set_mut(&mut self, id: FileSetId) -> Option<&mut FileSet> {
        self.file_sets.get_mut(id.index())
    }

    /// Add an install rule. Returns its index in [`Self::installs`].
    pub fn add_install(&mut self, rule: InstallRule) -> usize {
        self.installs.push(rule);
        self.installs.len() - 1
    }

    // ----------------------------------------------------------------------------------
    // Scoped properties.
    // ----------------------------------------------------------------------------------

    /// Value of property `name` on `scope`, without inheritance. `None` when not set.
    #[must_use]
    pub fn get_prop(&self, scope: &PropScope, name: &[u8]) -> Option<BString> {
        match scope {
            PropScope::Global => self.global.props.get(name).cloned(),
            PropScope::Directory(id) => self.directory_prop(*id, name),
            PropScope::Target(id) => self.target_prop(self.target(*id)?, name),
            PropScope::Source { dir, path } => {
                self.dir(*dir).source_props.get(path)?.get(name).cloned()
            }
            PropScope::Test { dir, name: test } => {
                let id = self.find_test(*dir, test)?;
                self.test(id)?.props.get(name).cloned()
            }
            PropScope::Cache(entry) => cache_prop(self.cache.get(entry)?, name),
        }
    }

    /// Value of property `name` with `INHERITED` lookup: object, then its directory,
    /// then parent directories, then global. A set value (also an empty one) stops the
    /// walk. The caller checks that the property is defined `INHERITED`.
    #[must_use]
    pub fn get_prop_inherited(&self, scope: &PropScope, name: &[u8]) -> Option<BString> {
        let mut scope = scope.clone();
        loop {
            if let Some(value) = self.get_prop(&scope, name) {
                return Some(value);
            }
            scope = match scope {
                PropScope::Target(id) => PropScope::Directory(self.target(id)?.dir),
                PropScope::Source { dir, .. } | PropScope::Test { dir, .. } => {
                    PropScope::Directory(dir)
                }
                PropScope::Directory(id) => match self.dir(id).parent {
                    Some(parent) => PropScope::Directory(parent),
                    None => PropScope::Global,
                },
                PropScope::Global | PropScope::Cache(_) => return None,
            };
        }
    }

    /// Set (`Some`) or unset (`None`) property `name` on `scope`.
    pub fn set_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: Option<BString>,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        match scope {
            PropScope::Global => {
                set_map(&mut self.global.props, name, value);
                Ok(())
            }
            PropScope::Directory(id) => {
                let dir = self.dir_mut(*id);
                match UsageField::from_name(name).filter(|f| f.on_directory()) {
                    Some(field) => set_usage(&mut dir.usage, field, value, origin.backtrace),
                    None => set_map(&mut dir.props, name, value),
                }
                Ok(())
            }
            PropScope::Target(id) => {
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                set_target_prop(target, name, value, origin)
            }
            PropScope::Source { dir, path } => {
                let props = self
                    .dir_mut(*dir)
                    .source_props
                    .entry(path.clone())
                    .or_default();
                set_map(props, name, value);
                Ok(())
            }
            PropScope::Test { dir, name: test } => {
                let id = self
                    .find_test(*dir, test)
                    .ok_or(ModelError::UnknownObject)?;
                let test = self.test_mut(id).ok_or(ModelError::UnknownObject)?;
                set_map(&mut test.props, name, value);
                Ok(())
            }
            PropScope::Cache(entry) => {
                let entry = self.cache.get_mut(entry).ok_or(ModelError::UnknownObject)?;
                set_cache_prop(entry, name, value)
            }
        }
    }

    /// Append `value` to property `name` on `scope` (`set_property(APPEND)` or
    /// `APPEND_STRING`). An empty value changes nothing.
    pub fn append_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: &[u8],
        mode: AppendMode,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        if value.is_empty() {
            return Ok(());
        }
        if mode == AppendMode::List && self.append_entry(scope, name, value, origin)? {
            return Ok(());
        }
        let old = self.get_prop(scope, name);
        let new = match (old, mode) {
            (Some(old), AppendMode::List) if !old.is_empty() => list::join([old.as_slice(), value]),
            (Some(old), AppendMode::String) => [old.as_slice(), value].concat().into(),
            _ => BString::from(value),
        };
        self.set_prop(scope, name, Some(new), origin)
    }

    /// Append one backtrace-tagged entry to a usage, `SOURCES`, or link property.
    /// Returns `false` when `name` is a plain property.
    fn append_entry(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: &[u8],
        origin: PropOrigin<'_>,
    ) -> Result<bool, ModelError> {
        let entry = BT::new(BString::from(value), origin.backtrace.clone());
        let field = UsageField::from_name(name);
        match scope {
            PropScope::Directory(id) => match field.filter(|f| f.on_directory()) {
                Some(field) => self.dir_mut(*id).usage.push(field, entry),
                None => return Ok(false),
            },
            PropScope::Target(id) => {
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                let usage = &mut target.usage;
                match (field, name) {
                    (Some(field), _) => usage.push(field, entry),
                    (None, b"SOURCES") => target.sources.push(entry),
                    (None, b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES") => {
                        let items = if name == b"LINK_LIBRARIES" {
                            &mut usage.link_libraries
                        } else {
                            &mut usage.interface_link_libraries
                        };
                        items.get_or_insert_default().push(LinkItem {
                            text: entry,
                            lookup_dir: origin.dir,
                        });
                    }
                    _ => return Ok(false),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn directory_prop(&self, id: DirId, name: &[u8]) -> Option<BString> {
        let dir = self.dir(id);
        let names = |ids: &mut dyn Iterator<Item = TargetId>| {
            list::join(
                ids.filter_map(|t| self.target(t))
                    .map(|t| t.name.as_slice()),
            )
        };
        Some(match name {
            b"SOURCE_DIR" => dir.source_dir.clone(),
            b"BINARY_DIR" => dir.binary_dir.clone(),
            b"PARENT_DIRECTORY" => dir
                .parent
                .map(|p| self.dir(p).source_dir.clone())
                .unwrap_or_default(),
            b"SUBDIRECTORIES" => list::join(
                dir.children
                    .iter()
                    .map(|c| self.dir(*c).source_dir.as_slice()),
            ),
            b"BUILDSYSTEM_TARGETS" => names(
                &mut dir
                    .targets
                    .iter()
                    .copied()
                    .filter(|t| self.target(*t).is_some_and(|t| !t.is_imported())),
            ),
            b"IMPORTED_TARGETS" => names(
                &mut dir
                    .targets
                    .iter()
                    .copied()
                    .filter(|t| self.target(*t).is_some_and(Target::is_imported)),
            ),
            b"TESTS" => list::join(dir.tests.keys()),
            _ => {
                return match UsageField::from_name(name).filter(|f| f.on_directory()) {
                    Some(field) => dir.usage.joined(field),
                    None => dir.props.get(name).cloned(),
                };
            }
        })
    }

    fn target_prop(&self, target: &Target, name: &[u8]) -> Option<BString> {
        let flag = |on: bool| BString::from(if on { "TRUE" } else { "FALSE" });
        match name {
            b"NAME" => Some(target.name.clone()),
            b"TYPE" => Some(target.kind.as_str().into()),
            b"IMPORTED" => Some(flag(target.is_imported())),
            b"IMPORTED_GLOBAL" => Some(flag(matches!(
                target.origin,
                TargetOrigin::Imported(ImportedVisibility::Global)
            ))),
            b"SOURCE_DIR" => Some(self.dir(target.dir).source_dir.clone()),
            b"BINARY_DIR" => Some(self.dir(target.dir).binary_dir.clone()),
            b"SOURCES" => (!target.sources.is_empty())
                .then(|| join_entries(target.sources.iter().map(|s| s.value.as_slice()))),
            b"LINK_LIBRARIES" => link_value(target.usage.link_libraries.as_deref()),
            b"INTERFACE_LINK_LIBRARIES" => {
                link_value(target.usage.interface_link_libraries.as_deref())
            }
            _ => match UsageField::from_name(name) {
                Some(field) => target.usage.joined(field),
                None => target.props.get(name).cloned(),
            },
        }
    }
}

fn link_value(items: Option<&[LinkItem]>) -> Option<BString> {
    items.map(|items| join_entries(items.iter().map(|i| i.text.value.as_slice())))
}

fn set_map(props: &mut crate::PropertyMap, name: &[u8], value: Option<BString>) {
    match value {
        Some(value) => props.set(name.into(), value),
        None => props.unset(name),
    }
}

fn set_usage(usage: &mut UsageProps, field: UsageField, value: Option<BString>, bt: &Backtrace) {
    match value {
        Some(value) => {
            usage.fields.insert(field, Vec::new());
            usage.push(field, BT::new(value, bt.clone()));
        }
        None => {
            usage.fields.shift_remove(&field);
        }
    }
}

fn set_target_prop(
    target: &mut Target,
    name: &[u8],
    value: Option<BString>,
    origin: PropOrigin<'_>,
) -> Result<(), ModelError> {
    let bt = |v: BString| BT::new(v, origin.backtrace.clone());
    match name {
        b"NAME" | b"TYPE" | b"IMPORTED" | b"SOURCE_DIR" | b"BINARY_DIR" => {
            return Err(ModelError::ReadOnly);
        }
        b"SOURCES" => {
            target.sources = value
                .into_iter()
                .filter(|v| !v.is_empty())
                .map(bt)
                .collect();
        }
        b"LINK_LIBRARIES" | b"INTERFACE_LINK_LIBRARIES" => {
            let items = value.map(|v| {
                let mut items = Vec::new();
                if !v.is_empty() {
                    items.push(LinkItem {
                        text: bt(v),
                        lookup_dir: origin.dir,
                    });
                }
                items
            });
            if name == b"LINK_LIBRARIES" {
                target.usage.link_libraries = items;
            } else {
                target.usage.interface_link_libraries = items;
            }
        }
        _ => match UsageField::from_name(name) {
            Some(field) => set_usage(&mut target.usage, field, value, origin.backtrace),
            None => set_map(&mut target.props, name, value),
        },
    }
    Ok(())
}

fn cache_prop(entry: &CacheEntry, name: &[u8]) -> Option<BString> {
    match name {
        b"VALUE" => Some(entry.value.clone()),
        b"TYPE" => Some(entry.ty.as_str().into()),
        b"HELPSTRING" => Some(entry.help.clone()),
        b"ADVANCED" => entry
            .props
            .get(name)
            .cloned()
            .or_else(|| entry.advanced.then(|| "1".into())),
        _ => entry.props.get(name).cloned(),
    }
}

fn set_cache_prop(
    entry: &mut CacheEntry,
    name: &[u8],
    value: Option<BString>,
) -> Result<(), ModelError> {
    match name {
        b"VALUE" => entry.value = value.unwrap_or_default(),
        b"HELPSTRING" => entry.help = value.unwrap_or_default(),
        b"TYPE" => {
            let value = value.unwrap_or_default();
            entry.ty = CacheType::parse(&value)
                .filter(|ty| ty.as_str().as_bytes() == value.as_slice())
                .ok_or(ModelError::InvalidValue)?;
        }
        b"ADVANCED" => {
            entry.advanced = value.as_ref().is_some_and(|v| rcmake_base::truth::is_on(v));
            set_map(&mut entry.props, name, value);
        }
        _ => set_map(&mut entry.props, name, value),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rcmake_diag::PolicyMap;

    use super::*;
    use crate::TargetKind;

    fn origin(bt: &Backtrace) -> PropOrigin<'_> {
        PropOrigin {
            dir: DirId::ROOT,
            backtrace: bt,
        }
    }

    fn target(name: &str, dir: DirId, origin: TargetOrigin) -> Target {
        Target::new(
            name.into(),
            TargetKind::StaticLibrary,
            origin,
            dir,
            Backtrace::default(),
            PolicyMap::default(),
        )
    }

    #[test]
    fn local_imported_targets_are_scoped_to_directory_and_later_children() {
        let mut model = ProjectModel::default();
        let early = model
            .add_directory(
                DirId::ROOT,
                "/s/early".into(),
                "/b/early".into(),
                Backtrace::default(),
            )
            .unwrap();
        let local = TargetOrigin::Imported(ImportedVisibility::Local);
        let imp = model.add_target(target("imp", DirId::ROOT, local)).unwrap();
        model.add_alias(DirId::ROOT, "ns::imp".into(), imp).unwrap();
        let late = model
            .add_directory(
                DirId::ROOT,
                "/s/late".into(),
                "/b/late".into(),
                Backtrace::default(),
            )
            .unwrap();
        assert_eq!(model.find_target(DirId::ROOT, b"ns::imp"), Some(imp));
        assert_eq!(model.find_target(late, b"imp"), Some(imp));
        assert_eq!(model.find_target(early, b"imp"), None);

        model.set_imported_global(imp).unwrap();
        assert_eq!(model.find_target(early, b"imp"), Some(imp));

        let normal = model
            .add_target(target("lib", late, TargetOrigin::Normal))
            .unwrap();
        assert_eq!(model.find_target(early, b"lib"), Some(normal));
        assert_eq!(
            model.add_target(target("lib", DirId::ROOT, TargetOrigin::Normal)),
            Err(ModelError::DuplicateTarget(normal))
        );
        assert_eq!(
            model.get_prop(&PropScope::Directory(late), b"BUILDSYSTEM_TARGETS"),
            Some("lib".into())
        );
        assert_eq!(
            model.get_prop(&PropScope::Directory(DirId::ROOT), b"SUBDIRECTORIES"),
            Some("/s/early;/s/late".into())
        );
    }

    #[test]
    fn new_directory_copies_only_initialize_from_parent_state() {
        let bt = Backtrace::default();
        let mut model = ProjectModel::default();
        let root = PropScope::Directory(DirId::ROOT);
        for (name, value) in [
            ("LABELS", "l"),
            ("OTHER", "o"),
            ("INCLUDE_DIRECTORIES", "/i"),
        ] {
            model
                .set_prop(&root, name.as_bytes(), Some(value.into()), origin(&bt))
                .unwrap();
        }
        let child = model
            .add_directory(DirId::ROOT, "/s/c".into(), "/b/c".into(), bt.clone())
            .unwrap();
        let child = PropScope::Directory(child);
        assert_eq!(model.get_prop(&child, b"LABELS"), Some("l".into()));
        assert_eq!(model.get_prop(&child, b"OTHER"), None);
        assert_eq!(
            model.get_prop(&child, b"INCLUDE_DIRECTORIES"),
            Some("/i".into())
        );
        assert_eq!(
            model.get_prop(&child, b"PARENT_DIRECTORY"),
            Some(BString::default())
        );
    }

    #[test]
    fn inherited_lookup_stops_at_explicit_empty_value() {
        let bt = Backtrace::default();
        let mut model = ProjectModel::default();
        let child = model
            .add_directory(DirId::ROOT, "/s/c".into(), "/b/c".into(), bt.clone())
            .unwrap();
        let t = model
            .add_target(target("t", child, TargetOrigin::Normal))
            .unwrap();
        model
            .set_prop(&PropScope::Global, b"P", Some("global".into()), origin(&bt))
            .unwrap();
        assert_eq!(
            model.get_prop_inherited(&PropScope::Target(t), b"P"),
            Some("global".into())
        );
        model
            .set_prop(
                &PropScope::Directory(DirId::ROOT),
                b"P",
                Some("root".into()),
                origin(&bt),
            )
            .unwrap();
        assert_eq!(
            model.get_prop_inherited(&PropScope::Target(t), b"P"),
            Some("root".into())
        );
        model
            .set_prop(
                &PropScope::Directory(child),
                b"P",
                Some(BString::default()),
                origin(&bt),
            )
            .unwrap();
        assert_eq!(
            model.get_prop_inherited(&PropScope::Target(t), b"P"),
            Some(BString::default())
        );
        assert_eq!(model.get_prop(&PropScope::Target(t), b"P"), None);
    }

    #[test]
    fn usage_properties_keep_one_entry_per_append() {
        let bt = Backtrace::default();
        let mut model = ProjectModel::default();
        let t = model
            .add_target(target("t", DirId::ROOT, TargetOrigin::Normal))
            .unwrap();
        let scope = PropScope::Target(t);
        for value in ["a", "", "b"] {
            model
                .append_prop(
                    &scope,
                    b"INCLUDE_DIRECTORIES",
                    value.as_bytes(),
                    AppendMode::List,
                    origin(&bt),
                )
                .unwrap();
        }
        model
            .append_prop(
                &scope,
                b"LINK_LIBRARIES",
                b"m",
                AppendMode::List,
                origin(&bt),
            )
            .unwrap();
        let target = model.target(t).unwrap();
        assert_eq!(
            target.usage.entries(UsageField::IncludeDirectories).len(),
            2
        );
        assert_eq!(
            model.get_prop(&scope, b"INCLUDE_DIRECTORIES"),
            Some("a;b".into())
        );
        assert_eq!(model.get_prop(&scope, b"LINK_LIBRARIES"), Some("m".into()));
        assert_eq!(
            model.get_prop(&scope, b"TYPE"),
            Some("STATIC_LIBRARY".into())
        );
        assert_eq!(
            model.set_prop(&scope, b"NAME", Some("x".into()), origin(&bt)),
            Err(ModelError::ReadOnly)
        );
        model
            .append_prop(
                &scope,
                b"COMPILE_FLAGS",
                b"-a",
                AppendMode::String,
                origin(&bt),
            )
            .unwrap();
        model
            .append_prop(
                &scope,
                b"COMPILE_FLAGS",
                b" -b",
                AppendMode::String,
                origin(&bt),
            )
            .unwrap();
        assert_eq!(
            model.get_prop(&scope, b"COMPILE_FLAGS"),
            Some("-a -b".into())
        );
    }
}
