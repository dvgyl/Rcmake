use crate::{FileSetKind, ProjectModel, Target, TargetKind, UsageScope};

impl ProjectModel {
    /// Return whether a native generator can emit this target.
    #[must_use]
    pub fn is_in_build_system(&self, target: &Target) -> bool {
        if target.is_imported() {
            return false;
        }
        match target.kind {
            TargetKind::UnknownLibrary => false,
            TargetKind::InterfaceLibrary => {
                !target.sources.is_empty()
                    || target.file_sets.iter().any(|&id| {
                        self.file_set(id).is_some_and(|set| {
                            set.scope != UsageScope::Interface
                                && matches!(
                                    set.kind,
                                    FileSetKind::Headers | FileSetKind::CxxModules
                                )
                        })
                    })
            }
            _ => true,
        }
    }
}
