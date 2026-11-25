//! The arena-based [`ProjectModel`] and its scoped property API.

/// Snapshot helpers for incremental configure replay.
pub mod replay;

mod membership;
mod target_properties;

use std::collections::{BTreeMap, BTreeSet};

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list};
use rcmake_diag::Backtrace;

use crate::{
    AppendMode, BT, BuildExport, Cache, CacheEntry, CacheType, CommandId, CustomCommand, DirId,
    Directory, ExportDependencySetup, ExportSet, FileSet, FileSetId, GeneratorKind, GeneratorSpec,
    GlobCheck, GlobKey, GlobalState, ImportedVisibility, InstallRule, PropOrigin, PropScope,
    PropertyMap, PropertyPrint, Target, TargetBinding, TargetId, TargetOrigin, TestEntry, TestId,
    UsageField, UsageProps,
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
    /// Raw and parsed `-T`, `-A`, and instance values of [`ProjectModel::generator`].
    pub generator_spec: GeneratorSpec,
    /// Install rules in call order.
    pub installs: Vec<InstallRule>,
    /// Named runtime dependency set installers in call order.
    pub runtime_dependency_installs: Vec<crate::RuntimeDependencyInstall>,
    /// Properties by install file name.
    pub install_props: BTreeMap<BString, PropertyMap>,
    /// Export sets by name.
    pub exports: IndexMap<BString, ExportSet>,
    /// `export()` calls that write build-tree files, in call order.
    pub build_exports: Vec<BuildExport>,
    /// XCFramework alternatives by export set and target name.
    pub export_xcframeworks: IndexMap<BString, IndexMap<BString, BString>>,
    /// Package setup options by export set and package name, in call order.
    pub export_dependencies: IndexMap<BString, IndexMap<BString, ExportDependencySetup>>,
    /// Package names in the order in which lookups began.
    pub package_lookups: IndexSet<BString>,
    /// Rules that ask the build tool to verify glob results.
    pub glob_checks: IndexMap<GlobKey, GlobCheck>,
    /// Property print requests in call order.
    pub property_prints: Vec<PropertyPrint>,
    globally_generated: BTreeSet<BString>,
    custom_rules: BTreeMap<Option<DirId>, IndexMap<BString, PropertyMap>>,
    file_set_rule_dirs: BTreeMap<FileSetId, DirId>,
    root: Directory,
    subdirs: Vec<Directory>,
    targets: Vec<Target>,
    commands: Vec<CustomCommand>,
    tests: Vec<TestEntry>,
    file_sets: Vec<FileSet>,
    global_targets: IndexMap<BString, TargetBinding>,
    languages: crate::LanguageRegistry,
    prop_ledger: Option<Box<crate::PropLedger>>,
    prop_seq: u64,
    alias_declarations: Vec<(DirId, BString, bool, Backtrace)>,
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
    /// Get the project cache.
    #[must_use]
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Get the mutable project cache.
    pub fn cache_mut(&mut self) -> &mut Cache {
        &mut self.cache
    }

    /// Get the enabled language registry.
    #[must_use]
    pub fn languages(&self) -> &crate::LanguageRegistry {
        &self.languages
    }

    /// Get the mutable language registry.
    pub fn languages_mut(&mut self) -> &mut crate::LanguageRegistry {
        &mut self.languages
    }

    /// Get the property history, if enabled.
    #[must_use]
    pub fn prop_ledger(&self) -> Option<&crate::PropLedger> {
        self.prop_ledger.as_deref()
    }

    /// Set the property history for this model.
    pub fn set_prop_ledger(&mut self, ledger: Option<Box<crate::PropLedger>>) {
        self.prop_ledger = ledger;
    }

    /// Iterate over global names and bindings in registration order.
    pub fn global_target_bindings(&self) -> impl Iterator<Item = (&BString, TargetBinding)> {
        self.global_targets
            .iter()
            .map(|(name, binding)| (name, *binding))
    }

    fn record_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        op: crate::PropOp,
        old: Option<BString>,
        origin: PropOrigin<'_>,
    ) {
        if self.prop_ledger.is_none() {
            return;
        }
        let new = self.get_prop(scope, name);
        if let Some(ledger) = &mut self.prop_ledger {
            self.prop_seq = self.prop_seq.saturating_add(1);
            ledger.record(
                scope,
                name,
                crate::PropEvent::new(
                    self.prop_seq,
                    op,
                    old.as_ref().map(|v| v.as_slice()),
                    new.as_ref().map(|v| v.as_slice()),
                    origin.backtrace,
                ),
            );
        }
    }

    /// Update target properties with their native storage rules and record the writes.
    pub fn update_target_properties<T>(
        &mut self,
        id: TargetId,
        append: impl IntoIterator<Item = (&'static [u8], AppendMode)>,
        origin: PropOrigin<'_>,
        update: impl FnOnce(&mut Target) -> T,
    ) -> Result<T, ModelError> {
        let scope = PropScope::Target(id);
        let previous: Vec<_> = if self.prop_ledger.is_some() {
            append
                .into_iter()
                .map(|(name, mode)| (name, mode, self.get_prop(&scope, name)))
                .collect()
        } else {
            Vec::new()
        };
        let result = update(self.target_mut(id).ok_or(ModelError::UnknownObject)?);
        for (name, mode, value) in previous {
            self.record_prop(&scope, name, crate::PropOp::Append(mode), value, origin);
        }
        Ok(result)
    }

    /// Look up a variable definition as seen at the end of configuring `dir`:
    /// normal variable snapshot first, else `CMakeCache.txt` value.
    #[must_use]
    pub fn definition(&self, dir: DirId, name: &[u8]) -> Option<&BString> {
        self.dir(dir)
            .vars
            .get(name)
            .or_else(|| self.cache.get(name).map(|e| &e.value))
    }

    /// Find the owner of a visible rule. A global rule has no directory owner.
    #[must_use]
    pub fn custom_rule_owner(&self, dir: DirId, name: &[u8]) -> Option<Option<DirId>> {
        self.ancestors(dir).map(Some).chain([None]).find(|owner| {
            self.custom_rules
                .get(owner)
                .is_some_and(|rules| rules.contains_key(name))
        })
    }

    /// Get the declared properties of a visible custom rule.
    #[must_use]
    pub fn custom_rule(&self, dir: DirId, name: &[u8]) -> Option<&PropertyMap> {
        self.custom_rules
            .get(&self.custom_rule_owner(dir, name)?)?
            .get(name)
    }

    /// Record the directory that selected a rule for a file set.
    pub fn set_file_set_rule_dir(&mut self, set: FileSetId, dir: DirId) {
        self.file_set_rule_dirs.insert(set, dir);
    }

    /// Get the directory that selected a rule for a file set.
    #[must_use]
    pub fn file_set_rule_dir(&self, set: FileSetId) -> Option<DirId> {
        self.file_set_rule_dirs.get(&set).copied()
    }

    /// Return the local rule names in declaration order.
    pub fn directory_rules(&self, dir: DirId) -> impl Iterator<Item = &BString> {
        self.custom_rules
            .get(&Some(dir))
            .into_iter()
            .flat_map(|rules| rules.keys())
    }

    /// Store properties for a custom rule in the specified scope.
    pub fn store_custom_rule(&mut self, dir: Option<DirId>, name: BString, props: PropertyMap) {
        self.custom_rules
            .entry(dir)
            .or_default()
            .insert(name, props);
    }

    /// Remove a custom rule from its specified scope.
    pub fn remove_custom_rule(&mut self, dir: Option<DirId>, name: &[u8]) {
        if let Some(rules) = self.custom_rules.get_mut(&dir) {
            rules.shift_remove(name);
        }
    }

    /// Mark an absolute file path as generated in this directory and the project.
    pub fn mark_generated(&mut self, dir: DirId, path: BString) {
        self.globally_generated.insert(path.clone());
        let directory = self.dir_mut(dir);
        directory.generated_sources.insert(path.clone());
        directory
            .source_props
            .entry(path)
            .or_default()
            .set("GENERATED".into(), "1".into());
    }

    /// Check whether any directory marked this absolute file path as generated.
    #[must_use]
    pub fn is_globally_generated(&self, path: &[u8]) -> bool {
        self.globally_generated.contains(path)
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

    /// Return `dir` and its ancestors in child-to-parent order.
    /// Stop if the parent index does not decrease.
    pub fn ancestors(&self, dir: DirId) -> impl Iterator<Item = DirId> + '_ {
        core::iter::successors(Some(dir), |&id| {
            self.dir(id)
                .parent
                .filter(|parent| parent.index() < id.index())
        })
    }

    /// Check if a directory or one of its ancestors is excluded from the default build.
    #[must_use]
    pub fn dir_excluded_from_all(&self, dir: DirId) -> bool {
        self.ancestors(dir).any(|id| self.dir(id).exclude_from_all)
    }

    /// Check if a target is excluded from the default build.
    /// An explicit target property overrides the directory setting.
    #[must_use]
    pub fn excluded_from_all(&self, target: &Target) -> bool {
        target.props.get(b"EXCLUDE_FROM_ALL").map_or_else(
            || self.dir_excluded_from_all(target.dir),
            |value| rcmake_base::truth::is_on(value),
        )
    }

    /// Set the source and binary directory of the root directory.
    pub fn set_root_dirs(&mut self, source_dir: BString, binary_dir: BString) {
        let root = &mut self.root;
        let property = b"INCLUDE_REGULAR_EXPRESSION";
        if root.source_dir.is_empty() && root.props.get(property).is_none() {
            root.props.set(property.as_slice().into(), "^.*$".into());
        }
        root.source_dir = source_dir;
        root.binary_dir = binary_dir;
    }

    /// Add a subdirectory with the inherited properties and local targets of `parent`.
    pub fn add_directory(
        &mut self,
        parent: DirId,
        source_dir: BString,
        binary_dir: BString,
        backtrace: Backtrace,
    ) -> Result<DirId, ModelError> {
        const INHERITED: &[&[u8]] = &[
            b"IMPLICIT_DEPENDS_INCLUDE_TRANSFORM",
            b"INCLUDE_REGULAR_EXPRESSION",
            b"LABELS",
            b"LINK_LIBRARIES",
            b"SYSTEM",
            b"_RCMAKE_INCLUDE_REGEX_COMPLAIN",
        ];
        let parent_dir = self.dir(parent);
        let mut dir = Directory {
            parent: Some(parent),
            install_position: self.installs.len(),
            source_dir,
            binary_dir,
            backtrace,
            usage: parent_dir.usage.clone(),
            local_targets: parent_dir.local_targets.clone(),
            ..Directory::default()
        };
        for &name in INHERITED {
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

    /// List the names that alias a target.
    pub fn target_aliases(&self, id: TargetId) -> impl Iterator<Item = &BString> {
        self.global_targets
            .iter()
            .chain(
                self.dirs()
                    .flat_map(|(_, directory)| &directory.local_targets),
            )
            .filter_map(move |(name, binding)| {
                (*binding == TargetBinding::Alias(id)).then_some(name)
            })
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

    /// Record an alias declaration when property provenance is enabled.
    pub fn record_alias_declaration(
        &mut self,
        dir: DirId,
        name: &BString,
        global: bool,
        trace: Backtrace,
    ) {
        if self.prop_ledger.is_some() {
            self.alias_declarations
                .push((dir, name.clone(), global, trace));
        }
    }

    /// Get the recorded alias declarations in evaluation order.
    pub fn alias_declarations(&self) -> impl Iterator<Item = (DirId, &BString, bool, &Backtrace)> {
        self.alias_declarations
            .iter()
            .map(|(dir, name, global, trace)| (*dir, name, *global, trace))
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
    /// directory already has a test with the same name. A `discover_tests()` entry
    /// ([`crate::TestKind::Discover`]) goes into [`Directory::discoveries`] and never
    /// conflicts with a name.
    pub fn add_test(&mut self, test: TestEntry) -> Result<TestId, ModelError> {
        let dir = test.dir;
        if test.is_discovery() {
            let id = push_arena(&mut self.tests, test, TestId::from_index)?;
            self.dir_mut(dir).discoveries.push(id);
            return Ok(id);
        }
        if let Some(&existing) = self.dir(dir).tests.get(&test.name) {
            return Err(ModelError::DuplicateTest(existing));
        }
        let name = test.name.clone();
        let id = push_arena(&mut self.tests, test, TestId::from_index)?;
        self.dir_mut(dir).tests.insert(name, id);
        Ok(id)
    }

    /// Tests and `discover_tests()` entries of directory `dir` in call order.
    pub fn dir_tests(&self, dir: DirId) -> impl Iterator<Item = (TestId, &TestEntry)> {
        let directory = self.dir(dir);
        let mut ids: Vec<TestId> = directory
            .tests
            .values()
            .chain(&directory.discoveries)
            .copied()
            .collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.test(id).map(|test| (id, test)))
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

    /// Names of all install components (`get_cmake_property(COMPONENTS)`), sorted. A
    /// rule without `COMPONENT` uses `CMAKE_INSTALL_DEFAULT_COMPONENT_NAME` of its
    /// directory, else `Unspecified`. Empty when the project has no install rules.
    #[must_use]
    pub fn install_components(&self) -> BTreeSet<BString> {
        self.installs
            .iter()
            .map(|rule| match &rule.options.component {
                Some(component) => component.clone(),
                None => self
                    .definition(rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                    .filter(|name| !name.is_empty())
                    .cloned()
                    .unwrap_or_else(|| BString::from("Unspecified")),
            })
            .collect()
    }

    // ----------------------------------------------------------------------------------
    // Scoped properties.
    // ----------------------------------------------------------------------------------

    /// Set (`Some`) or unset (`None`) property `name` on `scope`.
    pub fn set_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: Option<BString>,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        let old = self
            .prop_ledger
            .as_ref()
            .and_then(|_| self.get_prop(scope, name));
        let op = if value.is_some() {
            crate::PropOp::Set
        } else {
            crate::PropOp::Unset
        };
        self.set_prop_value(scope, name, value, origin)?;
        self.record_prop(scope, name, op, old, origin);
        Ok(())
    }

    fn set_prop_value(
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
                if name == b"EXCLUDE_FROM_ALL" {
                    dir.exclude_from_all = value
                        .as_ref()
                        .is_some_and(|value| rcmake_base::truth::is_on(value));
                }
                match UsageField::from_name(name).filter(|f| f.on_directory()) {
                    Some(field) => set_usage(&mut dir.usage, field, value, origin.backtrace),
                    None => set_map(&mut dir.props, name, value),
                }
                Ok(())
            }
            PropScope::Target(id) => {
                if target_properties::is_scoped_link(name) {
                    return self.write_link_property(*id, name, value, origin, false);
                }
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                target_properties::set_target_prop(target, name, value, origin)
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
            PropScope::FileSet(id) => {
                let file_set = self.file_set_mut(*id).ok_or(ModelError::UnknownObject)?;
                set_map(&mut file_set.props, name, value);
                Ok(())
            }
            PropScope::Install { file } => {
                if let Some(value) = value {
                    self.install_props
                        .entry(file.clone())
                        .or_default()
                        .set(name.into(), value);
                } else if let Some(props) = self.install_props.get_mut(file) {
                    props.unset(name);
                    if props.names().is_empty() {
                        self.install_props.remove(file);
                    }
                }
                Ok(())
            }
        }
    }

    /// Append `value` to property `name` on `scope` (`set_property(APPEND)` or
    /// `APPEND_STRING`). Keep empty entries for target `SOURCES`.
    pub fn append_prop(
        &mut self,
        scope: &PropScope,
        name: &[u8],
        value: &[u8],
        mode: AppendMode,
        origin: PropOrigin<'_>,
    ) -> Result<(), ModelError> {
        let target_sources = matches!(scope, PropScope::Target(_)) && name == b"SOURCES";
        if value.is_empty() && !target_sources {
            return Ok(());
        }
        let previous = self
            .prop_ledger
            .as_ref()
            .and_then(|_| self.get_prop(scope, name));
        if let PropScope::Target(id) = scope {
            if target_properties::is_scoped_link(name) {
                self.write_link_property(*id, name, Some(value.into()), origin, true)?;
                self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
                return Ok(());
            }
            if target_properties::is_direct_link(name) {
                let target = self.target_mut(*id).ok_or(ModelError::UnknownObject)?;
                target.append_direct_link(name, value, origin.backtrace);
                self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
                return Ok(());
            }
        }
        if (target_sources || mode == AppendMode::List)
            && self.append_entry(scope, name, value, origin)?
        {
            self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
            return Ok(());
        }
        let old = match scope {
            PropScope::Source { dir, path } if name == b"GENERATED" => self
                .dir(*dir)
                .source_props
                .get(path)
                .and_then(|props| props.get(name))
                .cloned(),
            _ => self.get_prop(scope, name),
        };
        let new = match (old, mode) {
            (Some(old), AppendMode::List) if !old.is_empty() => list::join([old.as_slice(), value]),
            (Some(old), AppendMode::String) => [old.as_slice(), value].concat().into(),
            _ => BString::from(value),
        };
        self.set_prop_value(scope, name, Some(new), origin)?;
        self.record_prop(scope, name, crate::PropOp::Append(mode), previous, origin);
        Ok(())
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
                    _ => return Ok(false),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
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
mod tests;
