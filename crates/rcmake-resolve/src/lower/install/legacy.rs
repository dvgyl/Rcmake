use indexmap::IndexMap;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_model::{ProjectModel, TargetKind};

use crate::{InstallAction, InstallActionKind, InstallEntry, InstallManifest};

pub(super) fn append_scripts(model: &ProjectModel, manifest: &mut InstallManifest) {
    let mut scripts = IndexMap::<_, Vec<_>>::new();
    for (_, target) in model
        .targets()
        .filter(|(_, target)| !target.is_imported() && target.kind != TargetKind::InterfaceLibrary)
    {
        for key in [b"PRE_INSTALL_SCRIPT".as_slice(), b"POST_INSTALL_SCRIPT"] {
            if let Some(path) = target.props.get(key) {
                scripts.entry(target.dir).or_default().push(path);
            }
        }
    }
    for (id, files) in scripts {
        let Some(entries) = manifest.directories.get_mut(&id) else {
            continue;
        };
        let start = manifest.actions.len();
        for path in files {
            manifest.actions.push(InstallAction {
                component: Default::default(),
                all_components: false,
                exclude_from_all: false,
                destination: Default::default(),
                configurations: Vec::new(),
                kind: InstallActionKind::Script(path.clone()),
                optional: false,
                message: Default::default(),
            });
        }
        let index = if model
            .dir(id)
            .final_policies
            .is_some_and(|policies| policies.get(PolicyId::new(82)) != PolicyStatus::New)
        {
            entries
                .iter()
                .position(|entry| matches!(entry, InstallEntry::Child(_)))
                .unwrap_or(entries.len())
        } else {
            entries.len()
        };
        entries.insert(index, InstallEntry::Actions(start..manifest.actions.len()));
    }
}
