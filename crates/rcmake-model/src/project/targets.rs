use rcmake_base::BString;
use rcmake_diag::Backtrace;

use super::{ModelError, ProjectModel, push_arena};
use crate::{DirId, ImportedVisibility, Target, TargetBinding, TargetId, TargetOrigin};

impl ProjectModel {
    /// Iterate over global names and bindings in registration order.
    pub fn global_target_bindings(&self) -> impl Iterator<Item = (&BString, TargetBinding)> {
        self.global_targets
            .iter()
            .map(|(name, binding)| (name, *binding))
    }

    /// Target `id`.
    #[must_use]
    pub fn target(&self, id: TargetId) -> Option<&Target> {
        self.targets.get(id.index())
    }

    /// Mutable target `id`.
    pub fn target_mut(&mut self, id: TargetId) -> Option<&mut Target> {
        let target = self.targets.get_mut(id.index())?;
        target.mutation_epoch = target.mutation_epoch.wrapping_add(1);
        Some(target)
    }

    /// Monotonic counter incremented each time [`Self::target_mut`] grants mutable access
    /// to target `id`.
    #[must_use]
    pub fn target_mutation_epoch(&self, id: TargetId) -> Option<u64> {
        self.targets
            .get(id.index())
            .map(|target| target.mutation_epoch)
    }

    /// Append utility dependencies to target `id` without incrementing
    /// [`Self::target_mutation_epoch`], for callers maintaining an index over
    /// [`Target::utility_deps`].
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::UnknownObject`] if `id` does not refer to a target in this model.
    pub fn push_utility_deps(
        &mut self,
        id: TargetId,
        deps: impl IntoIterator<Item = crate::BT<BString>>,
    ) -> Result<(), ModelError> {
        let target = self
            .targets
            .get_mut(id.index())
            .ok_or(ModelError::UnknownObject)?;
        target.utility_deps.extend(deps);
        Ok(())
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
}
