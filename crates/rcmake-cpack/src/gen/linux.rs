//! Work directories and module inputs for the Linux package generators.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, truth, version::Version,
};
use rcmake_eval::Interp;
use rcmake_host::{FileKind, Follow, Host, path::from_native_cmake};

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
    fn filename_override<'a>(&self, cfg: &'a CpackConfig, generator: &str) -> Option<&'a [u8]> {
        let name = self.name.as_ref()?;
        let upper = BString::from(name.to_ascii_uppercase());
        let candidates = if generator == "DEB" {
            vec![
                format!("CPACK_DEBIAN_{upper}_FILE_NAME"),
                "CPACK_DEBIAN_FILE_NAME".to_owned(),
            ]
        } else {
            vec![
                format!("CPACK_RPM_{name}_FILE_NAME"),
                format!("CPACK_RPM_{upper}_FILE_NAME"),
                "CPACK_RPM_FILE_NAME".to_owned(),
            ]
        };
        candidates.iter().find_map(|key| {
            cfg.get(key.as_bytes())
                .filter(|value| !truth::is_off(value))
        })
    }

    pub(super) fn module(
        &self,
        interp: &mut Interp,
        cx: &PackageContext<'_>,
        generator: &str,
        filename_suffix: Option<&[u8]>,
        extra: &[(&[u8], Option<BString>)],
    ) -> Result<CpackConfig, CpackError> {
        let canon_work = cx.expected_work_root()?;
        let canon_staging = cx.expected_staging_root()?;
        let directory = self.suffix.as_ref().map_or_else(
            || cx.staging_dir.to_owned(),
            |suffix| cx.staging_dir.join(suffix),
        );
        verify_path_in_root(
            cx.host,
            cx.staging_dir,
            &canon_staging,
            &directory,
            FileKind::Dir,
        )?;
        verify_path_in_root(
            cx.host,
            cx.output_dir,
            &canon_work,
            &directory,
            FileKind::Dir,
        )?;
        let mut filename = BString::from(cx.file_name);
        if let Some(suffix) = filename_suffix {
            filename.push(b'-');
            filename.extend_from_slice(suffix);
        }
        let extension = if generator == "DEB" { b".deb" } else { b".rpm" };
        filename.extend_from_slice(extension);
        let package = match safe_name(&filename) {
            Ok(path) => path,
            Err(invalid_name) => {
                let cfg = snapshot(interp);
                let Some(override_name) = self.filename_override(&cfg, generator) else {
                    return Err(invalid_name);
                };
                // Linux package modules apply their own rules to the final name.
                // Give the module a valid temporary path before it applies them.
                if matches!(override_name, b"DEB-DEFAULT" | b"RPM-DEFAULT") {
                    component_dir(&filename)
                } else {
                    safe_name(override_name)?
                }
            }
        };
        let proposed = cx.output_dir.join(package);
        let component = format!("CPACK_{generator}_PACKAGE_COMPONENT");
        let part = format!("{component}_PART_PATH");
        let part_path = self
            .suffix
            .as_ref()
            .map(|suffix| BString::from([b"/".as_slice(), &from_native_cmake(suffix)].concat()));
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
                Some(from_native_cmake(&script_path)),
            ),
            (b"_RCMAKE_CPACK_RPMBUILD_STATUS", None),
            (
                b"CPACK_TEMPORARY_DIRECTORY",
                Some(from_native_cmake(&directory)),
            ),
            (b"CPACK_OUTPUT_FILE_NAME", Some(filename)),
            (
                b"CPACK_TEMPORARY_PACKAGE_FILE_NAME",
                Some(from_native_cmake(&proposed)),
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

pub(super) fn verify_path_in_root(
    host: &dyn Host,
    root: &Path,
    expected_canon_root: &Path,
    path: &Path,
    kind: FileKind,
) -> Result<(), CpackError> {
    let canon_root = crate::source::verify_expected_root(host, root, expected_canon_root)?;
    if !path.starts_with(root)
        || host.metadata_fresh(path, Follow::No)?.kind != kind
        || !host.canonicalize_fresh(path)?.starts_with(&canon_root)
    {
        return Err(error(format!(
            "Invalid CPack work path: {}",
            path.display()
        )));
    }
    Ok(())
}
