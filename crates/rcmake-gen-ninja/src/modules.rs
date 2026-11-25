//! Ninja rules and target metadata for C++ module scans.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, hash::HashAlgo, path_lex::Lexical};
use rcmake_cxxmodules::{
    BmiInstallInfo, ModmapFormat, ModuleExportInfo, ModuleSourceInfo, ModuleVisibility,
    ScannedSourceInfo, TargetDependInfo, tdi::write_tdi,
};
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{Host, path};
use rcmake_model::{DirId, FileSetKind, ProjectModel, UsageScope};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult, InstallActionKind, ResolvedTarget};

use crate::{generation_error, source_directory};

pub(crate) const RULES: &str = "
rule CXX_SCAN
  command = $COMMAND
  description = Scanning $in for C++ dependencies
  depfile = $DEP_FILE
  deps = gcc

rule CXX_SCAN_NO_DEPFILE
  command = $COMMAND
  description = Scanning $in for C++ dependencies

rule CXX_SCAN_RSP
  command = $COMMAND
  description = Scanning $in for C++ dependencies
  depfile = $DEP_FILE
  deps = gcc
  rspfile = $RSP_FILE
  rspfile_content = $RSP_ARGS

rule CXX_SCAN_RSP_NO_DEPFILE
  command = $COMMAND
  description = Scanning $in for C++ dependencies
  rspfile = $RSP_FILE
  rspfile_content = $RSP_ARGS

rule CXX_DYNDEP
  command = ${CMAKE_COMMAND} -E cmake_ninja_dyndep --tdi=$in_tdi --lang=CXX $MODMAP_FLAG --dd=$out @$out.rsp$MODULE_EXPORT_COMMANDS
  description = Generating C++ dyndep file $out
  rspfile = $out.rsp
  rspfile_content = $in_newline
  restat = 1

rule CXX_DYNDEP_WINDOWS
  command = $COMMAND
  description = Generating C++ dyndep file $out
  rspfile = $out.rsp
  rspfile_content = $in_newline
  restat = 1

rule CXX_COMPILE_SCANNED
  command = $COMMAND
  description = $DESC

rule CXX_COMPILE_SCANNED_DEPFILE
  command = $COMMAND
  description = $DESC
  depfile = $DEP_FILE
  deps = gcc
";

pub(crate) fn uses_modules(graph: &BuildGraph) -> bool {
    graph.steps.iter().any(|step| {
        matches!(
            step,
            BuildStep::Scan { .. }
                | BuildStep::Collate { .. }
                | BuildStep::Compile {
                    modmap: Some(_),
                    ..
                }
                | BuildStep::Compile {
                    dyndep: Some(_),
                    ..
                }
        )
    })
}

pub(crate) fn modmap_flag(
    result: &GenerationResult,
    build_dir: &Path,
) -> Result<BString, GenError> {
    let Some(format) = result
        .metadata
        .toolchains
        .get(b"CXX".as_slice())
        .and_then(|toolchain| toolchain.module_map_format.as_ref())
    else {
        if result
            .graphs
            .iter()
            .flat_map(|graph| &graph.steps)
            .any(|step| matches!(step, BuildStep::Collate { modmaps, .. } if !modmaps.is_empty()))
        {
            return Err(generation_error(
                build_dir,
                "The C++ toolchain has no module map format.",
            ));
        }
        return Ok(BString::default());
    };
    ModmapFormat::from_name(format).ok_or_else(|| {
        generation_error(
            build_dir,
            format!("Invalid C++ module map format: {format}"),
        )
    })?;
    Ok([b"--modmapfmt=".as_slice(), format].concat().into())
}

pub(crate) fn modules_json(dd: &BString) -> BString {
    rcmake_base::path_lex::cmake_style(&path::from_native(
        &path::to_native(dd).with_file_name("CXXModules.json"),
    ))
}

pub(crate) fn write_metadata(
    result: &GenerationResult,
    model: Option<&ProjectModel>,
    graphs: &[&BuildGraph],
    host: &dyn Host,
    build_dir: &Path,
    multi_config: bool,
) -> Result<Vec<BString>, GenError> {
    let source_dir = model
        .map(|model| &model.dir(DirId::ROOT).source_dir)
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || source_directory(result, build_dir),
            |dir| path::to_native(dir),
        );
    let cmake_path = |path| rcmake_base::path_lex::cmake_style(&path::from_native(path));
    let top_src = cmake_path(source_dir.as_path());
    let top_bld = cmake_path(build_dir);
    let targets: HashMap<_, _> = result
        .targets
        .iter()
        .map(|target| ((&target.config, &target.metadata.name), target))
        .collect();
    let toolchain = result.metadata.toolchains.get(b"CXX".as_slice());
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for graph in graphs {
        for step in &graph.steps {
            let BuildStep::Collate {
                target,
                dd,
                tdi,
                implicit_inputs,
                depend_info,
                ..
            } = step
            else {
                continue;
            };
            let rcmake_resolve::DependInfo::Cxx(depend_info) = depend_info.as_ref() else {
                return Err(GenError::Unimplemented("Fortran collation for Ninja"));
            };
            let file = absolute(&top_bld, tdi);
            let path = path::to_native(&file);
            if !seen.insert(file) {
                return Err(generation_error(
                    &path,
                    "Duplicate Ninja C++ dependency metadata file.",
                ));
            }
            let resolved = targets.get(&(&graph.config, target)).copied();
            let mut info = TargetDependInfo {
                language: "CXX".into(),
                config: graph.config.0.clone(),
                compiler_id: toolchain
                    .map(|toolchain| toolchain.compiler_id.clone())
                    .unwrap_or_default(),
                dir_cur_src: resolved.map_or_else(
                    || top_src.clone(),
                    |target| absolute(&top_src, &target.metadata.source_dir),
                ),
                dir_cur_bld: resolved.map_or_else(
                    || top_bld.clone(),
                    |target| absolute(&top_bld, &target.metadata.binary_dir),
                ),
                dir_top_src: top_src.clone(),
                dir_top_bld: top_bld.clone(),
                module_dir: absolute(&top_bld, parent(dd)),
                linked_target_dirs: implicit_inputs
                    .iter()
                    .filter(|input| Lexical::new(input).filename() == b"CXXModules.json")
                    .map(|input| absolute(&top_bld, parent(input)))
                    .collect(),
                ..TargetDependInfo::default()
            };
            if let Some(model) = model {
                let dir = resolved
                    .and_then(|target| model.target(target.id))
                    .map_or(DirId::ROOT, |target| target.dir);
                info.compiler_frontend_variant = model
                    .definition(dir, b"CMAKE_CXX_COMPILER_FRONTEND_VARIANT")
                    .cloned()
                    .unwrap_or_default();
                info.compiler_simulate_id = model
                    .definition(dir, b"CMAKE_CXX_SIMULATE_ID")
                    .cloned()
                    .unwrap_or_default();
            }
            if depend_info.language.is_empty() {
                if let Some(target) = resolved {
                    add_target_metadata(&mut info, target, model, &path, multi_config)?;
                }
            } else {
                info = depend_info.clone();
                if let Some(install) = &mut info.bmi_installation {
                    install.script_location = rcmake_resolve::lower::bmi_script_path(
                        &top_bld,
                        tdi,
                        &info.config,
                        multi_config,
                    );
                    if !install.permissions.is_empty() {
                        install.permissions =
                            [" ".as_bytes(), &install.permissions].concat().into();
                    }
                }
            }
            if let Some(raw) = resolved.and_then(|target| model?.target(target.id)) {
                info.exports = module_exports(result, raw, &top_bld, &graph.config.0);
            }
            if let Some(database) = &info.database_info
                && let Some(resolved) = resolved
            {
                let units = resolved
                    .compile_units
                    .iter()
                    .map(|unit| &unit.object)
                    .collect::<HashSet<_>>();
                let contents = rcmake_cxxmodules::tdi::write_database_template(
                    target,
                    &graph.config.0,
                    graph.steps.iter().filter_map(|step| {
                        let BuildStep::Compile {
                            object,
                            source,
                            invocation,
                            ..
                        } = step
                        else {
                            return None;
                        };
                        units
                            .contains(object)
                            .then(|| rcmake_cxxmodules::tdi::DatabaseUnit {
                                source,
                                object,
                                arguments: &invocation.argv,
                                working_directory: invocation
                                    .working_dir
                                    .as_deref()
                                    .map_or(top_bld.as_slice(), Vec::as_slice),
                                private: info.cxx_modules.get(object).is_none_or(|source| {
                                    source.visibility == ModuleVisibility::Private
                                }),
                            })
                    }),
                );
                let template = path::to_native(&database.template_path);
                if let Some(directory) = template.parent() {
                    host.create_dir_all(directory)
                        .map_err(|error| generation_error(directory, error.to_string()))?;
                }
                write_if_changed(host, &template, &contents)?;
                files.push(database.template_path.clone());
            }
            let contents =
                write_tdi(&info).map_err(|error| generation_error(&path, error.to_string()))?;
            if let Some(directory) = path.parent() {
                host.create_dir_all(directory)
                    .map_err(|error| generation_error(directory, error.to_string()))?;
            }
            write_if_changed(host, &path, &contents)?;
            files.push(tdi.clone());
        }
    }
    Ok(files)
}

fn add_target_metadata(
    info: &mut TargetDependInfo,
    target: &ResolvedTarget,
    model: Option<&ProjectModel>,
    tdi: &Path,
    multi_config: bool,
) -> Result<(), GenError> {
    let Some(modules) = &target.cxx_modules else {
        return Ok(());
    };
    if !modules.module_dir.is_empty() {
        info.module_dir = absolute(&info.dir_top_bld, &modules.module_dir);
    }
    let mut includes = HashSet::new();
    for group in target
        .compile_groups
        .iter()
        .filter(|group| group.language == "CXX")
    {
        for include in &group.includes {
            if includes.insert(&include.path.value) {
                info.include_dirs.push(include.path.value.clone());
            }
        }
    }
    for module in &modules.units {
        let unit = target
            .compile_units
            .get(module.unit)
            .ok_or_else(|| generation_error(tdi, "Invalid C++ module compile unit index."))?;
        let mut source = ModuleSourceInfo {
            source: absolute(&info.dir_cur_src, &unit.source.value),
            file_set: module.file_set.clone(),
            visibility: if module.public {
                ModuleVisibility::Public
            } else {
                ModuleVisibility::Private
            },
            bmi_only: module.bmi_only,
            ..ModuleSourceInfo::default()
        };
        if let Some(group) = target.compile_groups.get(unit.group) {
            source.compile_options = group.flags.iter().map(|flag| flag.value.clone()).collect();
            source.definitions = group
                .defines
                .iter()
                .map(|define| define.value.clone())
                .collect();
            source.include_directories = group
                .includes
                .iter()
                .map(|include| include.path.value.clone())
                .collect();
        }
        if let Some(model) = model
            && let Some(raw) = model.target(target.id)
            && let Some(set) = raw
                .file_sets
                .iter()
                .filter_map(|&id| model.file_set(id))
                .find(|set| set.kind == FileSetKind::CxxModules && set.name == module.file_set)
        {
            source.visibility = match set.scope {
                UsageScope::Private => ModuleVisibility::Private,
                UsageScope::Public => ModuleVisibility::Public,
                UsageScope::Interface => ModuleVisibility::Interface,
            };
            let source_dir = path::to_native(parent(&source.source));
            let mut base_dirs = set
                .base_dirs
                .iter()
                .map(|base| absolute(&info.dir_cur_src, &base.value));
            source.relative_directory = base_dirs
                .find_map(|base| source_dir.strip_prefix(path::to_native(&base)).ok())
                .map_or_else(BString::default, path::from_native);
        }
        info.cxx_modules.insert(unit.object.clone(), source);
    }
    for unit in target.compile_units.iter().filter(|unit| unit.scan) {
        if !info.cxx_modules.contains_key(&unit.object) {
            info.sources.insert(
                unit.object.clone(),
                ScannedSourceInfo {
                    source: absolute(&info.dir_cur_src, &unit.source.value),
                    language: "CXX".into(),
                },
            );
        }
    }
    if let Some(install) = &modules.bmi_install {
        let mut permissions = BString::default();
        for permission in &install.permissions {
            permissions.push(b' ');
            permissions.extend_from_slice(permission);
        }
        info.bmi_installation = Some(BmiInstallInfo {
            destination: install.destination.clone(),
            component: install.component.clone(),
            permissions,
            exclude_from_all: install.exclude_from_all,
            optional: install.optional,
            script_location: rcmake_resolve::lower::bmi_script_path(
                &info.dir_top_bld,
                &path::from_native(tdi),
                &info.config,
                multi_config,
            ),
            ..BmiInstallInfo::default()
        });
    }
    Ok(())
}

fn module_exports(
    result: &GenerationResult,
    target: &rcmake_model::Target,
    top_bld: &[u8],
    config: &[u8],
) -> Vec<ModuleExportInfo> {
    let name = target.props.get(b"EXPORT_NAME").unwrap_or(&target.name);
    let filesystem_name = filesystem_export_name(name);
    result
        .exports
        .iter()
        .enumerate()
        .filter_map(|(index, plan)| {
            let directory = plan
                .cxx_modules_directory
                .as_ref()
                .filter(|dir| !dir.is_empty())?;
            if !plan
                .targets
                .iter()
                .any(|export| &export.name == name && export.config_properties.contains_key(config))
            {
                return None;
            }
            let (destination, export_prefix) = if plan.install {
                let install = result.install.actions.iter().find(|action| {
                matches!(action.kind, InstallActionKind::Export { plan } if plan == index)
            })?;
                let hash = HashAlgo::Md5.hex_digest(&install.destination);
                (
                    install.destination.clone(),
                    absolute(top_bld, format!("CMakeFiles/Export/{hash}").as_bytes()),
                )
            } else {
                let dir = absolute(top_bld, parent(&plan.file));
                (dir.clone(), dir)
            };
            Some(ModuleExportInfo {
                export_name: name.clone(),
                filesystem_export_name: filesystem_name.clone(),
                namespace: plan.namespace.clone(),
                destination,
                cxx_module_info_dir: directory.clone(),
                export_prefix,
                install: plan.install,
            })
        })
        .collect()
}

fn filesystem_export_name(name: &[u8]) -> BString {
    let mut result = BString::default();
    for &byte in name {
        match byte {
            b':' => result.extend_from_slice(b"_c"),
            b'_' => result.extend_from_slice(b"__"),
            _ => result.push(byte),
        }
    }
    result
}

fn absolute(base: &[u8], name: &[u8]) -> BString {
    if name.is_empty() {
        return base.into();
    }
    Lexical::new(&Lexical::new(base).append(name)).normalize()
}

fn parent(file: &[u8]) -> &[u8] {
    Lexical::new(file).parent()
}
