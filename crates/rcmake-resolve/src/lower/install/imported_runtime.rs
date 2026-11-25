//! Install the runtime artifacts of imported targets.

use rcmake_base::truth;
use rcmake_genex::{ArtifactKind, ArtifactPart};
use rcmake_model::{InstallKind, InstallOptions, TargetKind};

use super::{
    BString, Backtrace, ConfigId, DirId, InstallActionKind, InstallRule, Lexical, ProjectModel,
    UsageResolver, eval_destination, file_action, join, matches_config,
};
use crate::lower::{artifacts::is_dll_platform, framework::wrapper_root, is_darwin};
use crate::usage::artifacts::{imported_for_config, imported_property};

pub(super) fn lower<E>(
    model: &ProjectModel,
    rule: &InstallRule,
    name: &[u8],
    configs: &[ConfigId],
    eval: &mut impl FnMut(DirId, &ConfigId, &Backtrace, &[u8]) -> Result<BString, E>,
    mut emit: impl FnMut(&InstallOptions, InstallActionKind),
) -> Result<(), E> {
    let Some(id) = model.find_target(rule.dir, name) else {
        return Ok(());
    };
    let Some(target) = model.target(id).filter(|target| target.is_imported()) else {
        return Ok(());
    };
    let usage = UsageResolver::new(model);
    let default = ConfigId::default();
    let multi = configs.len() > 1
        || model
            .generator
            .is_some_and(|generator| generator.multi_config());
    let configs = if configs.is_empty() {
        std::slice::from_ref(&default)
    } else {
        configs
    };
    for config in configs
        .iter()
        .filter(|config| matches_config(&rule.options, config))
    {
        let mut options = rule.options.clone();
        eval_destination(&mut options, rule.dir, config, rule, eval)?;
        if multi {
            options.configurations = vec![config.0.clone()];
        }
        let path = if let Some(path) =
            usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config)
        {
            path
        } else {
            let (InstallKind::Files(files) | InstallKind::Programs(files)) = &rule.kind else {
                continue;
            };
            let Some(file) = files.first() else { continue };
            let path = eval(rule.dir, config, &file.backtrace, &file.value)?;
            if path.is_empty() {
                continue;
            }
            path
        };
        if is_darwin(model, target.dir) {
            let layout = crate::ide::bundle_kind(target);
            let wrapper = layout.root(&path).or_else(|| {
                let suffix = match target.kind {
                    TargetKind::Executable => b".app".as_slice(),
                    TargetKind::SharedLibrary => b".framework",
                    TargetKind::ModuleLibrary => b".bundle",
                    _ => return None,
                };
                wrapper_root(&path, suffix)
            });
            if let Some(root) = wrapper {
                let mut directory_options = super::directory_options(&[], &options.permissions);
                directory_options.push("USE_SOURCE_PERMISSIONS".into());
                emit(
                    &options,
                    InstallActionKind::Directory {
                        dirs: vec![root.into()],
                        options: directory_options,
                    },
                );
                continue;
            }
        }
        let file_type = match target.kind {
            TargetKind::Executable => b"EXECUTABLE".as_slice(),
            TargetKind::ModuleLibrary => b"MODULE_LIBRARY",
            TargetKind::SharedLibrary => b"SHARED_LIBRARY",
            _ => continue,
        };
        let mut files = vec![path.clone()];
        if target.kind == TargetKind::SharedLibrary
            && !is_dll_platform(model, target.dir)
            && let Some((_, selected)) = imported_property(target, b"IMPORTED_LOCATION", config)
        {
            let property = |key| {
                imported_for_config(target, key, &selected)
                    .or_else(|| imported_for_config(target, key, b""))
            };
            if !property(b"IMPORTED_NO_SONAME").is_some_and(|value| truth::is_on(&value))
                && let Some(soname) = property(b"IMPORTED_SONAME").filter(|value| !value.is_empty())
            {
                let soname = join(
                    Lexical::new(&path).parent(),
                    Lexical::new(&soname).filename(),
                );
                if soname != path {
                    files.push(soname);
                }
            }
        }
        emit(&options, file_action(file_type, files, &options));
    }
    Ok(())
}
