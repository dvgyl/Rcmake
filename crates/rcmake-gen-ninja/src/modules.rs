//! Ninja rules and target metadata for C++ module scans.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_cxxmodules::{
    BmiInstallInfo, ModmapFormat, ModuleSourceInfo, ModuleVisibility, ScannedSourceInfo,
    TargetDependInfo, tdi::write_tdi,
};
use rcmake_gen_common::{GenError, write_if_changed};
use rcmake_host::{Host, path};
use rcmake_model::{DirId, FileSetKind, ProjectModel, UsageScope};
use rcmake_resolve::{BuildGraph, BuildStep, GenerationResult, ResolvedTarget};

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

rule CXX_DYNDEP
  command = ${CMAKE_COMMAND} -E cmake_ninja_dyndep --tdi=$in_tdi --lang=CXX $MODMAP_FLAG --dd=$out @$out.rsp
  description = Generating C++ dyndep file $out
  rspfile = $out.rsp
  rspfile_content = $in_newline
  restat = 1

rule CXX_COMPILE_SCANNED
  command = $COMMAND
  description = Building $out

rule CXX_COMPILE_SCANNED_DEPFILE
  command = $COMMAND
  description = Building $out
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
    path::from_native(&path::to_native(dd).with_file_name("CXXModules.json"))
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
    let top_src = path::from_native(&source_dir);
    let top_bld = path::from_native(build_dir);
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
                ..
            } = step
            else {
                continue;
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
            if let Some(target) = resolved {
                add_target_metadata(&mut info, target, model, &path, multi_config)?;
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
        let config = if info.config.is_empty() {
            b"noconfig"
        } else {
            info.config.as_slice()
        };
        let script =
            BString::from([b"install-cxx-module-bmi-".as_slice(), config, b".cmake"].concat());
        let mut permissions = BString::default();
        for permission in &install.permissions {
            permissions.push(b' ');
            permissions.extend_from_slice(permission);
        }
        let support_dir = tdi.parent().and_then(|dir| {
            if multi_config {
                dir.parent()
            } else {
                Some(dir)
            }
        });
        let script = path::to_native(&script);
        info.bmi_installation = Some(BmiInstallInfo {
            destination: install.destination.clone(),
            component: install.component.clone(),
            permissions,
            exclude_from_all: install.exclude_from_all,
            optional: install.optional,
            script_location: path::from_native(
                &support_dir.map_or(script.clone(), |dir| dir.join(&script)),
            ),
            ..BmiInstallInfo::default()
        });
    }
    Ok(())
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
