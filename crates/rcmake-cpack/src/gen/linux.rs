//! Work directories and module inputs for the Linux package generators.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, version::Version,
};
use rcmake_eval::Interp;
use rcmake_host::{FileKind, Follow, Host, path::from_native};

use crate::config::{CpackConfig, absolute, script, snapshot};
use crate::stage::{component_dir, component_group, package_components};
use crate::{ComponentGrouping, CpackError, PackageContext, error, safe_name};

pub(super) struct Pass {
    pub name: Option<BString>,
    suffix: Option<PathBuf>,
}

pub(super) fn passes(cfg: &CpackConfig, grouping: ComponentGrouping) -> Vec<Pass> {
    let mut groups = BTreeSet::new();
    let mut orphans = BTreeSet::new();
    let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
    for [_, _, selection, _] in projects.as_chunks::<4>().0 {
        for name in package_components(cfg, selection) {
            if grouping == ComponentGrouping::OnePerGroup
                && let Some(group) = component_group(cfg, &name)
            {
                groups.insert(BString::from(group));
            } else {
                orphans.insert(name);
            }
        }
    }
    if groups.is_empty() && orphans.is_empty() {
        return vec![Pass {
            name: None,
            suffix: None,
        }];
    }
    if grouping == ComponentGrouping::AllComponentsInOne {
        return vec![Pass {
            name: None,
            suffix: Some("ALL_COMPONENTS_IN_ONE".into()),
        }];
    }
    groups
        .into_iter()
        .chain(orphans)
        .map(|name| Pass {
            suffix: Some(component_dir(&name)),
            name: Some(name),
        })
        .collect()
}

impl Pass {
    pub(super) fn module(
        &self,
        interp: &mut Interp,
        cx: &PackageContext<'_>,
        generator: &str,
        filename_suffix: Option<&[u8]>,
        extra: &[(&[u8], Option<BString>)],
    ) -> Result<CpackConfig, CpackError> {
        let directory = self.suffix.as_ref().map_or_else(
            || cx.staging_dir.to_owned(),
            |suffix| cx.staging_dir.join(suffix),
        );
        verify_path(cx.host, cx.output_dir, &directory, FileKind::Dir)?;
        let mut filename = BString::from(cx.file_name);
        if let Some(suffix) = filename_suffix {
            filename.push(b'-');
            filename.extend_from_slice(suffix);
        }
        filename.extend_from_slice(if generator == "DEB" { b".deb" } else { b".rpm" });
        let proposed = cx.output_dir.join(safe_name(&filename)?);
        let component = format!("CPACK_{generator}_PACKAGE_COMPONENT");
        let part = format!("{component}_PART_PATH");
        let part_path = self
            .suffix
            .as_ref()
            .map(|suffix| BString::from([b"/".as_slice(), &from_native(suffix)].concat()));
        let root = interp.cx().var_or_empty(b"CMAKE_ROOT");
        let script_path = absolute(&cx.host.initial_cwd(), &root).join(if generator == "DEB" {
            "Modules/Internal/CPack/CPackDeb.cmake"
        } else {
            "Modules/Internal/CPack/CPackRPM.cmake"
        });
        let variables = [
            (b"CMAKE_BINARY_DIR".as_slice(), None),
            (
                b"_RCMAKE_CPACK_INTERNAL_MODULE",
                Some(from_native(&script_path)),
            ),
            (b"_RCMAKE_CPACK_RPMBUILD_STATUS", None),
            (b"CPACK_TEMPORARY_DIRECTORY", Some(from_native(&directory))),
            (b"CPACK_OUTPUT_FILE_NAME", Some(filename)),
            (
                b"CPACK_TEMPORARY_PACKAGE_FILE_NAME",
                Some(from_native(&proposed)),
            ),
            (component.as_bytes(), self.name.clone()),
            (part.as_bytes(), part_path),
        ];
        let before = snapshot(interp);
        let mut context = interp.cx();
        for key in before.vars.keys().filter(|key| key.starts_with(b"GEN_")) {
            context.unset_var(key);
        }
        for (key, value) in variables.iter().chain(extra) {
            if let Some(value) = value {
                context.set_var(key, value.clone());
            } else {
                context.unset_var(key);
            }
        }
        context.push_policy_scope();
        let version = Version::from_tuple(
            CMAKE_VERSION_MAJOR,
            CMAKE_VERSION_MINOR,
            CMAKE_VERSION_PATCH,
        );
        let result = context
            .apply_policy_version(&version)
            .map_err(|err| error(err.to_string()))
            .and_then(|()| script(interp, &script_path))
            .map(|()| snapshot(interp));
        let pop = interp
            .cx()
            .pop_policy_scope()
            .map_err(|_| error("Cannot restore CPack policies"));
        let mut context = interp.cx();
        for (key, _) in variables.iter().chain(extra) {
            if let Some(value) = before.get(key) {
                context.set_var(key, value.into());
            } else {
                context.unset_var(key);
            }
        }
        result.and_then(|vars| pop.map(|()| vars))
    }
}

pub(super) fn verify_path(
    host: &dyn Host,
    root: &Path,
    path: &Path,
    kind: FileKind,
) -> Result<(), CpackError> {
    if !path.starts_with(root)
        || host.metadata(path, Follow::No)?.kind != kind
        || !host
            .canonicalize(path)?
            .starts_with(host.canonicalize(root)?)
    {
        return Err(error(format!(
            "Invalid CPack work path: {}",
            path.display()
        )));
    }
    Ok(())
}
