//! Build NuGet packages with the bundled CPackNuGet module.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, list, version::Version,
};
use rcmake_eval::Interp;
use rcmake_host::{
    FileKind,
    path::{from_native_cmake, to_native},
};

use super::installer::is_windows_host;
use super::linux::verify_path_in_root;
use crate::config::{CpackConfig, absolute, script, snapshot};
use crate::stage::{
    component_dir_for_host, component_group, package_components, relative_install_path,
};
use crate::{
    ComponentGrouping, ComponentMembers, CpackError, PackageContext, PackageGenerator, error,
};

pub(super) struct NuGet;

impl PackageGenerator for NuGet {
    fn name(&self) -> &'static str {
        "NuGet"
    }

    fn extension(&self) -> &'static str {
        ".nupkg"
    }

    fn package(&self, _: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError> {
        Err(error("CPackNuGet requires a CPack interpreter"))
    }

    fn package_with_interp(
        &self,
        cx: &PackageContext<'_>,
        members: &ComponentMembers,
        interp: &mut Interp,
    ) -> Result<Vec<PathBuf>, CpackError> {
        let canon_work = cx.expected_work_root()?;
        let cfg = CpackConfig {
            vars: cx.vars.clone(),
        };

        let mut groups: BTreeMap<BString, Vec<BString>> = BTreeMap::new();
        let mut orphans: BTreeSet<BString> = BTreeSet::new();
        let mut all_components: BTreeSet<BString> = BTreeSet::new();

        let mut record_component = |group: Option<&[u8]>, rel_dir: BString| {
            all_components.insert(rel_dir.clone());
            if cx.grouping == ComponentGrouping::OnePerGroup
                && let Some(group) = group
            {
                let entry = groups.entry(BString::from(group)).or_default();
                if !entry.contains(&rel_dir) {
                    entry.push(rel_dir);
                }
            } else {
                orphans.insert(rel_dir);
            }
        };

        if !members.is_empty() {
            for component in members {
                let group = component
                    .group
                    .as_ref()
                    .map(|g| g.as_slice())
                    .or_else(|| component_group(&cfg, &component.name));
                for staged in &component.staged_roots {
                    let rel = staged.root.strip_prefix(cx.staging_dir).map_err(|_| {
                        error(format!(
                            "CPack NuGet component staging root escapes staging directory: {}",
                            staged.root.display()
                        ))
                    })?;
                    if !rel.as_os_str().is_empty() {
                        record_component(group, from_native_cmake(rel));
                    }
                }
            }
        } else {
            let windows_rules = is_windows_host(cx.host);
            let projects = cfg.list(b"CPACK_INSTALL_CMAKE_PROJECTS");
            for [_, _, selection, subdir] in projects.as_chunks::<4>().0 {
                let subdir = relative_install_path(subdir)?;
                for name in package_components(&cfg, selection) {
                    let rel = subdir.join(component_dir_for_host(&name, windows_rules));
                    record_component(component_group(&cfg, &name), from_native_cmake(&rel));
                }
            }
        }

        let mut extra_vars: Vec<(BString, Option<BString>)> = vec![
            (b"CMAKE_BINARY_DIR".as_slice().into(), None),
            (
                b"CPACK_TEMPORARY_DIRECTORY".as_slice().into(),
                Some(from_native_cmake(cx.staging_dir)),
            ),
            (b"GEN_CPACK_OUTPUT_FILES".as_slice().into(), None),
            (b"GEN_CPACK_NUGET_PACKAGE_FILES".as_slice().into(), None),
            (
                b"GEN_CPACK_NUGET_SYMBOL_PACKAGE_FILES".as_slice().into(),
                None,
            ),
            (b"CPACK_NUGET_GROUPS".as_slice().into(), None),
            (b"CPACK_NUGET_COMPONENTS".as_slice().into(), None),
        ];

        if all_components.is_empty() {
            extra_vars.push((
                b"CPACK_NUGET_ORDINAL_MONOLITIC".as_slice().into(),
                Some("ON".into()),
            ));
            extra_vars.push((
                b"CPACK_NUGET_ALL_IN_ONE".as_slice().into(),
                Some("OFF".into()),
            ));
        } else if cx.grouping == ComponentGrouping::AllComponentsInOne {
            extra_vars.push((
                b"CPACK_NUGET_ORDINAL_MONOLITIC".as_slice().into(),
                Some("OFF".into()),
            ));
            extra_vars.push((
                b"CPACK_NUGET_ALL_IN_ONE".as_slice().into(),
                Some("ON".into()),
            ));
            extra_vars.push((
                b"CPACK_NUGET_COMPONENTS".as_slice().into(),
                Some(list::join(all_components.iter())),
            ));
        } else {
            extra_vars.push((
                b"CPACK_NUGET_ORDINAL_MONOLITIC".as_slice().into(),
                Some("OFF".into()),
            ));
            extra_vars.push((
                b"CPACK_NUGET_ALL_IN_ONE".as_slice().into(),
                Some("OFF".into()),
            ));
            if !groups.is_empty() {
                extra_vars.push((
                    b"CPACK_NUGET_GROUPS".as_slice().into(),
                    Some(list::join(groups.keys())),
                ));
                for (group, comps) in &groups {
                    let group_up = make_c_identifier(group).to_ascii_uppercase();
                    let key =
                        [b"CPACK_NUGET_".as_slice(), &group_up, b"_GROUP_COMPONENTS"].concat();
                    extra_vars.push((key.into(), Some(list::join(comps.iter()))));
                }
            }
            if !orphans.is_empty() {
                extra_vars.push((
                    b"CPACK_NUGET_COMPONENTS".as_slice().into(),
                    Some(list::join(orphans.iter())),
                ));
            }
        }

        let root = interp.cx().var_or_empty(b"CMAKE_ROOT");
        let script_path =
            absolute(&cx.host.initial_cwd(), &root).join("Modules/Internal/CPack/CPackNuGet.cmake");

        let before = snapshot(interp);
        {
            let mut context = interp.cx();
            for (key, value) in &extra_vars {
                if let Some(value) = value {
                    context.set_var(key, value.clone());
                } else {
                    context.unset_var(key);
                }
            }
            context.push_policy_scope();
        }

        let version = Version::from_tuple(
            CMAKE_VERSION_MAJOR,
            CMAKE_VERSION_MINOR,
            CMAKE_VERSION_PATCH,
        );
        let run_result = interp
            .cx()
            .apply_policy_version(&version)
            .map_err(|err| error(err.to_string()))
            .and_then(|()| script(interp, &script_path))
            .map(|()| snapshot(interp));
        let pop = interp
            .cx()
            .pop_policy_scope()
            .map_err(|_| error("Cannot restore CPack policies"));

        {
            let mut context = interp.cx();
            for (key, _) in &extra_vars {
                if let Some(value) = before.get(key) {
                    context.set_var(key, value.into());
                } else {
                    context.unset_var(key);
                }
            }
        }

        let result = run_result.and_then(|vars| pop.map(|()| vars))?;
        let outputs = result.list(b"GEN_CPACK_OUTPUT_FILES");
        if outputs.is_empty() {
            return Err(error("NuGet package was not generated"));
        }

        let mut packages = Vec::new();
        let mut seen = BTreeSet::new();
        for output in outputs {
            let path = to_native(&output);
            verify_path_in_root(cx.host, cx.output_dir, &canon_work, &path, FileKind::File)?;
            if seen.insert(path.clone()) {
                packages.push(path);
            }
        }
        Ok(packages)
    }
}

fn make_c_identifier(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 1);
    if raw.first().is_some_and(u8::is_ascii_digit) {
        out.push(b'_');
    }
    for &byte in raw {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            out.push(byte);
        } else {
            out.push(b'_');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_host::path::from_native_cmake;
    use rcmake_host::{Host, ProcessResult, ReadRange, RunOutput, WriteMode};

    use crate::generators::tests::package_host;

    const NUGET_CMAKE: &[u8] =
        include_bytes!("../../../../modules/cmake-4.5/Modules/Internal/CPack/CPackNuGet.cmake");
    const NUGET_NUSPEC_IN: &[u8] = include_bytes!(
        "../../../../modules/cmake-4.5/Modules/Internal/CPack/CPack.NuGet.nuspec.in"
    );

    #[test]
    fn nuget_generator_renders_nuspec_and_publishes_nupkg_and_snupkg() {
        let host = package_host();
        let root = host.initial_cwd();
        let payload = root.join("payload");
        host.create_dir_all(&payload.join("lib/net4")).unwrap();
        host.write(
            &payload.join("lib/net4/sample.dll"),
            b"dll",
            WriteMode::Truncate,
        )
        .unwrap();

        let fake_nuget = root.join(format!("nuget{}", std::env::consts::EXE_SUFFIX));
        host.write(&fake_nuget, b"#!/bin/sh\n", WriteMode::Truncate)
            .unwrap();
        host.set_permissions(&fake_nuget, 0o755).unwrap();
        host.env_set(b"PATH", Some(from_native_cmake(&root).as_slice()));

        for (rel, bytes) in [
            ("Modules/Internal/CPack/CPackNuGet.cmake", NUGET_CMAKE),
            (
                "Modules/Internal/CPack/CPack.NuGet.nuspec.in",
                NUGET_NUSPEC_IN,
            ),
        ] {
            host.seed_file(&root.join("cmake-root").join(rel), bytes)
                .unwrap();
        }

        let host_for_cmd = Arc::clone(&host);
        host.set_command_handler(move |_, req, _| {
            let cmd = req.pipeline.first().unwrap();
            assert!(cmd.args.iter().any(|a| a == "pack"));
            assert!(cmd.args.iter().any(|a| a == "-Symbols"));
            assert!(cmd.args.iter().any(|a| a == "snupkg"));
            let nuspec = req.cwd.join("CPack.NuGet.nuspec");
            let spec = host_for_cmd
                .read(&nuspec, ReadRange::All { limit: 1024 * 1024 })
                .unwrap();
            let spec_str = String::from_utf8_lossy(&spec);
            assert!(spec_str.contains("<id>GeneratorTest</id>"), "{spec_str}");
            assert!(spec_str.contains("<version>1.2.3</version>"), "{spec_str}");
            assert!(
                spec_str.contains("<group targetFramework=\".NETFramework4\">"),
                "{spec_str}"
            );
            assert!(
                spec_str.contains("<group targetFramework=\"net6.0\" />"),
                "{spec_str}"
            );
            host_for_cmd
                .write(
                    &req.cwd.join("GeneratorTest.1.2.3.nupkg"),
                    b"nupkg",
                    WriteMode::Truncate,
                )
                .unwrap();
            host_for_cmd
                .write(
                    &req.cwd.join("GeneratorTest.1.2.3.snupkg"),
                    b"snupkg",
                    WriteMode::Truncate,
                )
                .unwrap();
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })
        .unwrap();

        let config = root.join("CPackConfig.cmake");
        let cmake_root = from_native_cmake(&root.join("cmake-root"));
        let payload_str = from_native_cmake(&payload);
        host.write(
            &config,
            format!(
                r#"set(CMAKE_ROOT "{cmake_root}")
set(CPACK_GENERATOR NuGet)
set(CPACK_PACKAGE_NAME GeneratorTest)
set(CPACK_PACKAGE_VERSION "1.2.3")
set(CPACK_PACKAGE_FILE_NAME "GeneratorTest-1.2.3-win64")
set(CPACK_PACKAGE_VENDOR "ACME Inc")
set(CPACK_NUGET_PACKAGE_OWNERS "ACME Inc")
set(CPACK_PACKAGE_DESCRIPTION "A NuGet package")
set(CPACK_PACKAGE_DESCRIPTION_SUMMARY "Summary")
set(CPACK_PACKAGE_HOMEPAGE_URL "https://www.example.com")
set(CPACK_NUGET_PACKAGE_REPOSITORY_URL "https://github.com/example/nugetlib.git")
set(CPACK_NUGET_PACKAGE_REPOSITORY_TYPE git)
set(CPACK_NUGET_PACKAGE_LICENSE_EXPRESSION "MIT")
set(CPACK_NUGET_SYMBOL_PACKAGE ON)
set(CPACK_NUGET_PACKAGE_TFMS "net4;net6.0")
set(CPACK_NUGET_PACKAGE_DEPENDENCIES_net4 "Foo")
set(CPACK_NUGET_PACKAGE_DEPENDENCIES_net4_Foo_VERSION "1.23")
set(CPACK_INSTALLED_DIRECTORIES "{payload_str};/")
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
        assert_eq!(crate::main(&argv, host.clone()), 0);
        assert!(
            host.metadata(
                &root.join("GeneratorTest.1.2.3.nupkg"),
                rcmake_host::Follow::Yes
            )
            .is_ok()
        );
        assert!(
            host.metadata(
                &root.join("GeneratorTest.1.2.3.snupkg"),
                rcmake_host::Follow::Yes
            )
            .is_ok()
        );
    }

    #[test]
    fn nuget_component_install_preserves_staged_subdir_and_runtime_windows_rules() {
        let host = package_host();
        let mut sys = host.system();
        sys.sysname = "Windows".into();
        host.set_system(sys).unwrap();
        let root = host.initial_cwd();
        let build = root.join("build");
        host.create_dir_all(&build).unwrap();
        host.write(
            &build.join("cmake_install.cmake"),
            br#"if(CMAKE_INSTALL_COMPONENT STREQUAL "aux")
  file(WRITE "${CMAKE_INSTALL_PREFIX}/lib/net4/a.dll" "dll")
endif()
"#,
            WriteMode::Truncate,
        )
        .unwrap();

        let fake_nuget = root.join(format!("nuget{}", std::env::consts::EXE_SUFFIX));
        host.write(&fake_nuget, b"#!/bin/sh\n", WriteMode::Truncate)
            .unwrap();
        host.set_permissions(&fake_nuget, 0o755).unwrap();
        host.env_set(b"PATH", Some(from_native_cmake(&root).as_slice()));

        for (rel, bytes) in [
            ("Modules/Internal/CPack/CPackNuGet.cmake", NUGET_CMAKE),
            (
                "Modules/Internal/CPack/CPack.NuGet.nuspec.in",
                NUGET_NUSPEC_IN,
            ),
        ] {
            host.seed_file(&root.join("cmake-root").join(rel), bytes)
                .unwrap();
        }

        let host_for_cmd = Arc::clone(&host);
        host.set_command_handler(move |_, req, _| {
            let nuspec = req.cwd.join("CPack.NuGet.nuspec");
            let spec = host_for_cmd
                .read(&nuspec, ReadRange::All { limit: 1024 * 1024 })
                .unwrap();
            let spec_str = String::from_utf8_lossy(&spec);
            assert!(
                spec_str.contains("sub/_aux") || spec_str.contains("sub\\_aux"),
                "expected preserved subdir with Windows-safe component dir in nuspec: {spec_str}"
            );
            host_for_cmd
                .write(
                    &req.cwd.join("CompTest.1.0.0.nupkg"),
                    b"nupkg",
                    WriteMode::Truncate,
                )
                .unwrap();
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })
        .unwrap();

        let config = root.join("CPackConfig.cmake");
        let cmake_root = from_native_cmake(&root.join("cmake-root"));
        let build_str = from_native_cmake(&build);
        host.write(
            &config,
            format!(
                r#"set(CMAKE_ROOT "{cmake_root}")
set(CPACK_GENERATOR NuGet)
set(CPACK_CMAKE_GENERATOR Ninja)
set(CPACK_PACKAGE_NAME CompTest)
set(CPACK_PACKAGE_VERSION "1.0.0")
set(CPACK_PACKAGE_FILE_NAME "CompTest-1.0.0-win64")
set(CPACK_PACKAGE_VENDOR "ACME")
set(CPACK_PACKAGE_DESCRIPTION "Desc")
set(CPACK_NUGET_COMPONENT_INSTALL ON)
set(CPACK_COMPONENTS_ALL_IN_ONE_PACKAGE ON)
set(CPACK_COMPONENTS_ALL "aux")
set(CPACK_INSTALL_CMAKE_PROJECTS "{build_str};CompTest;ALL;sub")
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
        assert_eq!(crate::main(&argv, host.clone()), 0);
        assert!(
            host.metadata(&root.join("CompTest.1.0.0.nupkg"), rcmake_host::Follow::Yes)
                .is_ok()
        );
    }
}
