//! Build RPM packages with the bundled CPackRPM module.

use std::collections::BTreeSet;
use std::path::PathBuf;

use rcmake_base::{BString, hash::HashAlgo, truth};
use rcmake_eval::Interp;
use rcmake_host::{FileKind, path::to_native};

use super::ScratchDir;
use super::linux::{passes, verify_path};
use crate::config::CpackConfig;
use crate::stage::{component_dir, remove_entry};
use crate::{ComponentMembers, CpackError, PackageContext, PackageGenerator, error};

pub(super) struct Rpm;

impl PackageGenerator for Rpm {
    fn name(&self) -> &'static str {
        "RPM"
    }
    fn extension(&self) -> &'static str {
        ".rpm"
    }

    fn package(&self, _: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        Err(error("CPackRPM requires a CPack interpreter"))
    }

    fn package_with_interp(
        &self,
        cx: &PackageContext<'_>,
        _: &ComponentMembers,
        interp: &mut Interp,
    ) -> Result<Vec<PathBuf>, CpackError> {
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let mut passes = passes(&cfg, cx.grouping);
        let named = passes.iter().any(|pass| pass.name.is_some());
        let main = cfg
            .get(b"CPACK_RPM_MAIN_COMPONENT")
            .filter(|name| !truth::is_off(name));
        let main_pass = if let Some(main) = main.filter(|_| named) {
            let index = passes
                .iter()
                .position(|pass| {
                    pass.name
                        .as_ref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(main))
                })
                .ok_or_else(|| {
                    error(format!(
                        "CPackRPM: Main component {} was not found",
                        BString::from(main)
                    ))
                })?;
            let pass = passes.remove(index);
            passes.push(pass);
            true
        } else {
            false
        };
        let combined = cfg.on(b"CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE");
        if combined && named && !main_pass && passes.len() > 1 {
            return Err(error(
                "CPackRPM: CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE requires CPACK_RPM_MAIN_COMPONENT",
            ));
        }
        let component_debug = |name: &[u8]| {
            [name.to_vec(), name.to_ascii_uppercase()]
                .iter()
                .any(|name| {
                    cfg.on(&[b"CPACK_RPM_".as_slice(), name, b"_DEBUGINFO_PACKAGE"].concat())
                })
        };
        let global_debug = combined
            && (!named
                || (!cfg.on(b"CPACK_RPM_DEBUGINFO_PACKAGE")
                    && !passes
                        .iter()
                        .filter_map(|pass| pass.name.as_deref())
                        .any(|name| component_debug(name))));
        for dir in ["RPMS", "SRPMS", "SPECS"] {
            remove_entry(cx.host, &cx.output_dir.join(dir))?;
        }
        let artifacts = ScratchDir::for_generator(cx.host, cx.output_dir, "rpm")?;
        let mut packages = Vec::new();
        let mut names = BTreeSet::new();
        for (index, pass) in passes.iter().enumerate() {
            let display_name = pass.name.as_ref().map(|name| {
                if cfg.on(b"CPACK_RPM_USE_DISPLAY_NAME_IN_FILENAME") {
                    let key = [
                        b"CPACK_COMPONENT_GROUP_".as_slice(),
                        &name.to_ascii_uppercase(),
                        b"_DISPLAY_NAME",
                    ]
                    .concat();
                    cfg.get(&key).unwrap_or(name)
                } else {
                    name.as_slice()
                }
            });
            let part_name = pass.name.as_ref().map(|name| {
                if name.contains(&b':') {
                    HashAlgo::Md5.hex_digest(name).into()
                } else {
                    rcmake_host::path::from_native_cmake(&component_dir(name))
                }
            });
            let spec_only = combined && main_pass && index + 1 < passes.len();
            let result = pass.module(
                interp,
                cx,
                "RPM",
                display_name,
                &[
                    (b"CPACK_RPM_PACKAGE_COMPONENT_PART_NAME", part_name),
                    (
                        b"GENERATE_SPEC_PARTS",
                        Some(if spec_only { "ON" } else { "OFF" }.into()),
                    ),
                    (
                        b"CPACK_RPM_DEBUGINFO_PACKAGE",
                        if global_debug {
                            Some("ON".into())
                        } else {
                            cfg.get(b"CPACK_RPM_DEBUGINFO_PACKAGE").map(BString::from)
                        },
                    ),
                ],
            )?;
            if spec_only {
                continue;
            }
            if let Some(status) = result
                .get(b"_RCMAKE_CPACK_RPMBUILD_STATUS")
                .filter(|status| *status != b"0")
            {
                return Err(error(format!("rpmbuild failed: {}", BString::from(status))));
            }
            let outputs = result.list(b"GEN_CPACK_OUTPUT_FILES");
            if outputs.is_empty() {
                return Err(error("RPM package was not generated"));
            }
            for output in outputs {
                let path = to_native(&output);
                if !["RPMS", "SRPMS"]
                    .iter()
                    .any(|dir| path.starts_with(cx.output_dir.join(dir)))
                {
                    return Err(error(format!("Invalid RPM output: {}", path.display())));
                }
                verify_path(cx.host, cx.output_dir, &path, FileKind::File)?;
                let name = path
                    .file_name()
                    .ok_or_else(|| error("Missing RPM file name"))?;
                if !names.insert(name.to_owned()) {
                    return Err(error(format!("Duplicate RPM package: {}", path.display())));
                }
                let target = artifacts.path.join(name);
                cx.host.rename(&path, &target)?;
                packages.push((target, path));
            }
        }
        packages
            .into_iter()
            .map(|(scratch, path)| {
                cx.host.rename(&scratch, &path)?;
                Ok(path)
            })
            .collect()
    }
}
