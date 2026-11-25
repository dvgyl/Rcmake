//! The arena-based [`ProjectModel`] and its scoped property API.

/// Snapshot helpers for incremental configure replay.
pub mod replay;

mod items;
mod membership;
mod props;
mod target_properties;
mod targets;

use std::collections::{BTreeMap, BTreeSet};

use indexmap::{IndexMap, IndexSet};
use rcmake_base::BString;
use rcmake_diag::Backtrace;

use crate::{
    BuildExport, Cache, CustomCommand, DirId, Directory, ExportDependencySetup, ExportSet, FileSet,
    FileSetId, GeneratorKind, GeneratorSpec, GlobCheck, GlobKey, GlobalState, InstallRule,
    PropertyMap, PropertyPrint, Target, TargetBinding, TargetId, TestEntry, TestId,
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
}

#[cfg(test)]
mod tests;
