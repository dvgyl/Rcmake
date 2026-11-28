//! Build RPM packages with the bundled CPackRPM module.

use std::collections::BTreeSet;
use std::path::PathBuf;

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_eval::Interp;
use rcmake_host::{FileKind, path::to_native};

use super::ScratchDir;
use super::linux::{passes, verify_path_in_root};
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
        let canon_work = cx.expected_work_root()?;
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };
        let mut passes = passes(&cfg, cx.grouping);
        let named = passes.iter().any(|pass| pass.name.is_some());
        let main = cfg.get(b"CPACK_RPM_MAIN_COMPONENT");
        let main_pass = if let Some(main) = main.filter(|_| named) {
            let index = passes
                .iter()
                .position(|pass| {
                    !main.is_empty()
                        && pass
                            .name
                            .as_ref()
                            .is_some_and(|name| name.eq_ignore_ascii_case(main))
                })
                .ok_or_else(|| error("CPACK_RPM_MAIN_COMPONENT set to non existing component."))?;
            let pass = passes.remove(index);
            passes.push(pass);
            true
        } else {
            false
        };
        let combined = cfg.on(b"CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE");
        let component_count = {
            let mut components = BTreeSet::new();
            let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
            for [_, _, selection, _] in projects.as_chunks::<4>().0 {
                components.extend(crate::stage::package_components(&cfg, selection));
            }
            components.len()
        };
        if combined && named && !main_pass && component_count > 1 {
            return Err(error(
                "CPACK_RPM_MAIN_COMPONENT not set but it is mandatory with CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE being set.",
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
                verify_path_in_root(cx.host, cx.output_dir, &canon_work, &path, FileKind::File)?;
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

#[cfg(test)]
mod tests {
    use rcmake_base::BString;
    use rcmake_host::path::from_native_cmake;
    use rcmake_host::{Host, Stream, WriteMode};

    use crate::generators::create_unrestricted;
    use crate::generators::tests::package_host;

    fn run_rpm_component_case(extra_config: &str) -> (i32, String) {
        let host = package_host();
        let root = host.initial_cwd();
        let build = root.join("build");
        host.create_dir_all(&build).unwrap();
        host.write(
            &build.join("cmake_install.cmake"),
            br#"if(CMAKE_INSTALL_COMPONENT STREQUAL "applications")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/foo/a.txt" "a")
elseif(CMAKE_INSTALL_COMPONENT STREQUAL "headers")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/bar/b.txt" "b")
endif()
"#,
            WriteMode::Truncate,
        )
        .unwrap();
        let config = root.join("CPackConfig.cmake");
        let build_str = from_native_cmake(&build);
        host.write(
            &config,
            format!(
                r#"set(CPACK_GENERATOR RPM)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_PACKAGE_NAME sample)
set(CPACK_PACKAGE_VERSION "0.1.1")
set(CPACK_PACKAGE_FILE_NAME "sample-0.1.1-Linux")
set(CPACK_RPM_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL "applications;headers")
set(CPACK_INSTALL_CMAKE_PROJECTS "{build_str};sample;ALL;/")
{extra_config}
"#
            )
            .as_bytes(),
            WriteMode::Truncate,
        )
        .unwrap();
        let argv = [
            BString::from("cpack"),
            BString::from("--config"),
            from_native_cmake(&config),
        ];
        let code = crate::main_with_factory(&argv, host.clone(), create_unrestricted);
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        (code, stderr)
    }

    #[test]
    fn empty_rpm_main_component_reports_non_existing_component() {
        let (code, stderr) = run_rpm_component_case(r#"set(CPACK_RPM_MAIN_COMPONENT "")"#);
        assert_eq!(code, 1);
        assert!(
            stderr.contains("CPACK_RPM_MAIN_COMPONENT set to non existing component."),
            "{stderr}"
        );
    }

    #[test]
    fn single_debuginfo_without_main_component_reports_cmake_message() {
        let (code, stderr) =
            run_rpm_component_case(r#"set(CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE ON)"#);
        assert_eq!(code, 1);
        assert!(
            stderr.contains(
                "CPack Error: CPACK_RPM_MAIN_COMPONENT not set but it is mandatory with CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE being set."
            ),
            "{stderr}"
        );
    }

    #[test]
    fn single_debuginfo_grouped_two_components_still_requires_main_component() {
        let (code, stderr) = run_rpm_component_case(
            r#"set(CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE ON)
set(CPACK_COMPONENTS_GROUPING ONE_PER_GROUP)
set(CPACK_COMPONENT_APPLICATIONS_GROUP shared)
set(CPACK_COMPONENT_HEADERS_GROUP shared)"#,
        );
        assert_eq!(code, 1);
        assert!(
            stderr.contains(
                "CPack Error: CPACK_RPM_MAIN_COMPONENT not set but it is mandatory with CPACK_RPM_DEBUGINFO_SINGLE_PACKAGE being set."
            ),
            "{stderr}"
        );
    }
}
