use rcmake_base::{BString, path_lex::Lexical};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, InstallOptions, ProjectModel, Target};

use crate::{CompileUnit, InstallAction, InstallActionKind, InstallManifest};

use super::{file_action, join};

pub(super) struct ObjectPaths {
    built: BString,
    installer_from: BString,
    installed: BString,
}

impl ObjectPaths {
    pub(super) fn new(
        model: &ProjectModel,
        target: &Target,
        unit: &CompileUnit,
        destination: &[u8],
        custom_name: &[u8],
    ) -> Self {
        let name = super::super::artifacts::install_object_name(
            model,
            target,
            &unit.source.value,
            custom_name,
        );
        let built = join(&model.dir(DirId::ROOT).binary_dir, &unit.object);
        let installer_from = if model.generator == Some(rcmake_model::GeneratorKind::Ninja) {
            let dir = super::super::object_dir(model, target, &ConfigId::default());
            if let Some(rest) = unit
                .object
                .strip_prefix(dir.as_slice())
                .and_then(|rest| rest.strip_prefix(b"/"))
            {
                [dir.as_slice(), b"/.//", rest].concat().into()
            } else {
                built.clone()
            }
        } else {
            built.clone()
        };
        Self {
            built,
            installer_from,
            installed: join(destination, &name),
        }
    }

    pub(super) fn installer_path(&self, base: &[u8]) -> (BString, BString) {
        let installed = Lexical::new(&self.installed);
        let relative = if base.is_empty() {
            self.installed.clone()
        } else {
            installed
                .relative(Lexical::new(base))
                .unwrap_or_else(|| self.installed.clone())
        };
        (self.installer_from.clone(), relative)
    }

    pub(super) fn action(
        self,
        options: &InstallOptions,
        make_action: &impl Fn(&InstallOptions, &[u8], InstallActionKind) -> InstallAction,
    ) -> InstallAction {
        let mut options = options.clone();
        let path = Lexical::new(&self.installed);
        options.destination = Some(path.parent().into());
        options.rename = Some(path.filename().into());
        make_action(
            &options,
            b"",
            file_action(b"FILE", vec![self.built], &options),
        )
    }
}

pub(crate) fn exported_path(
    manifest: &InstallManifest,
    config: &ConfigId,
    built: &[u8],
) -> Option<BString> {
    manifest
        .actions
        .iter()
        .filter(|action| {
            action
                .configurations
                .iter()
                .any(|name| config.matches(name))
        })
        .find_map(|action| match &action.kind {
            InstallActionKind::Files {
                files,
                rename: Some(name),
                file_type,
                ..
            } if file_type == b"FILE" && files.iter().any(|path| path == built) => {
                Some(join(&action.destination, name))
            }
            _ => None,
        })
}
