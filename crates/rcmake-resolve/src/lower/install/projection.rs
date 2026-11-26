use std::collections::BTreeSet;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_genex::ConfigId;
use rcmake_model::TargetId;

use super::{Group, InstallAction, InstallManifest, join};
use crate::{ArtifactSet, NamelinkMode, TargetInstaller};

pub(super) struct Projection<'a> {
    pub rule: usize,
    pub target: TargetId,
    pub config: &'a ConfigId,
}

impl Projection<'_> {
    pub(super) fn add(
        &self,
        manifest: &mut InstallManifest,
        action: &InstallAction,
        import_library: bool,
        namelink: NamelinkMode,
        paths: Vec<(BString, BString)>,
    ) {
        if !paths.is_empty() {
            self.push(manifest, action, import_library, namelink, paths);
        }
    }

    fn push(
        &self,
        manifest: &mut InstallManifest,
        action: &InstallAction,
        import_library: bool,
        namelink: NamelinkMode,
        paths: Vec<(BString, BString)>,
    ) {
        if !action.configurations.is_empty()
            && !action
                .configurations
                .iter()
                .any(|config| self.config.matches(config))
        {
            return;
        }
        manifest.target_installers.push(TargetInstaller {
            rule: self.rule,
            target: self.target,
            config: self.config.clone(),
            destination: action.destination.clone(),
            component: action.component.clone(),
            optional: action.optional,
            exclude_from_all: action.exclude_from_all,
            import_library,
            namelink,
            paths,
        });
    }

    pub(super) fn library(
        &self,
        manifest: &mut InstallManifest,
        action: &InstallAction,
        bin: &[u8],
        (artifacts, group, import_library): (&ArtifactSet, Option<&Group<'_>>, bool),
    ) {
        let Some(output) = artifacts.linker_file.as_ref().or(artifacts.file.as_ref()) else {
            return;
        };
        let mut seen = BTreeSet::new();
        let versions = artifacts
            .file
            .iter()
            .chain(&artifacts.soname_file)
            .filter(|path| *path != output && seen.insert(*path))
            .collect::<Vec<_>>();
        let only = group.is_some_and(|group| group.namelink_only);
        let skip = group.is_some_and(|group| group.namelink_skip);
        if !only {
            self.push(
                manifest,
                action,
                import_library,
                if versions.is_empty() {
                    NamelinkMode::None
                } else {
                    NamelinkMode::Skip
                },
                if versions.is_empty() {
                    vec![file_path(bin, output)]
                } else {
                    versions.iter().map(|path| file_path(bin, path)).collect()
                },
            );
        }
        if !skip {
            let mut action = action.clone();
            if let Some(component) = group.and_then(|group| group.namelink_component) {
                action.component = component.into();
            }
            let paths = if versions.is_empty() {
                Vec::new()
            } else {
                vec![file_path(bin, output)]
            };
            self.push(manifest, &action, import_library, NamelinkMode::Only, paths);
        }
    }
}

pub(super) fn file_path(bin: &[u8], path: &[u8]) -> (BString, BString) {
    (join(bin, path), Lexical::new(path).filename().into())
}
