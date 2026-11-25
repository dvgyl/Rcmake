//! Export C++ module file sets and write their per-configuration import files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, hash::HashAlgo, list, path_lex::Lexical};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{
    Host, ReadRange,
    path::{from_native, to_native},
};
use rcmake_model::{
    DirId, FileSet, FileSetKind, InstallKind, ProjectModel, Target, UsageField, UsageScope,
};
use rcmake_resolve::{
    BuildStep, ExportPlan, GenerationResult, InstallAction, InstallActionKind, InstallManifest,
    usage::UsageResolver,
};

use super::{
    export_output_directory, output_error, render_main, render_property, render_relative_path,
    start_properties, write_output,
};
use crate::{
    GenError, genex,
    install_script::{
        self, InstallScriptOptions,
        file_sets::{options_for_set, set_files},
    },
    quote, quoted_content, write_if_changed,
};

/// Files that a C++ module collation step creates for one export.
pub struct ModuleExportJob {
    /// Dyndep file that identifies the collation step.
    pub dyndep: BString,
    /// CMake script that reads the scan results.
    pub script: BString,
    /// Import file that the script writes.
    pub output: BString,
}

/// Write C++ module export indices and prepare the per-target collation jobs.
///
/// # Errors
///
/// Returns an error if export paths or file sets are invalid, or if a file cannot
/// be read or written.
pub fn write_module_exports(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
    multi_config: bool,
) -> Result<Vec<ModuleExportJob>, GenError> {
    let mut jobs = Vec::new();
    let mut install = InstallManifest::default();
    let mut usage = UsageResolver::new(model);
    for (index, plan) in result.exports.iter().enumerate() {
        let Some(directory) = plan
            .cxx_modules_directory
            .as_ref()
            .filter(|dir| !dir.is_empty())
        else {
            continue;
        };
        let directory = safe_directory(directory, plan)?;
        let members = module_export_members(model, plan);
        let action = result.install.actions.iter().find(
            |action| matches!(action.kind, InstallActionKind::Export { plan } if plan == index),
        );
        let destination = action.map_or(b"".as_slice(), |action| &action.destination);
        let main_path = if plan.install {
            export_output_directory(
                plan,
                &bin.join("CMakeFiles/Export").join(index.to_string()),
                destination,
            )
            .join(to_native(&plan.file))
        } else {
            bin.join(to_native(&plan.file))
        };
        let root = main_path.with_file_name(to_native(&directory));
        let key = if plan.set.is_empty() {
            let names: Vec<_> = members
                .iter()
                .flat_map(|target| target.name.iter().copied())
                .collect();
            let mut digest = HashAlgo::Sha3_512.hex_digest(&names);
            digest.truncate(12);
            BString::from(digest)
        } else {
            plan.set.clone()
        };
        validate_file_component(&key, plan)?;
        let index = ModuleExportIndex {
            root,
            directory,
            stem: [b"cxx-modules-".as_slice(), &key].concat().into(),
            plan,
            action,
        };
        let mut main = BString::default();
        let mut config_files: BTreeMap<&BString, (&ConfigId, BString)> = result
            .configs
            .iter()
            .filter(|config| {
                action.is_none_or(|action| {
                    action.configurations.is_empty()
                        || action
                            .configurations
                            .iter()
                            .any(|value| config.matches(value))
                })
            })
            .map(|config| (&config.0, (config, BString::default())))
            .collect();
        for (config, _) in config_files.values() {
            validate_file_component(label(config), plan)?;
        }

        for target in members {
            let raw_name = target.props.get(b"EXPORT_NAME").unwrap_or(&target.name);
            validate_file_component(raw_name, plan)?;
            let target_name = BString::from([plan.namespace.as_slice(), raw_name].concat());
            let mut target_stem = BString::from("target-");
            for &byte in raw_name.iter() {
                match byte {
                    b'_' => target_stem.extend_from_slice(b"__"),
                    b':' => target_stem.extend_from_slice(b"_c"),
                    _ => target_stem.push(byte),
                }
            }
            let mut scans_by_config = BTreeMap::new();
            for set in module_sets(model, target).filter(|set| set.scope != UsageScope::Private) {
                let files = render_file_set(
                    &mut main,
                    plan,
                    model,
                    target,
                    &target_name,
                    set,
                    config_files.values().map(|(config, _)| *config),
                    &mut usage,
                )?;
                for (config, sources) in files {
                    let Some(graph) = result.graphs.iter().find(|graph| graph.config == *config)
                    else {
                        continue;
                    };
                    let Some((dyndep, scans, bmi_destination)) =
                        graph.steps.iter().find_map(|step| match step {
                            BuildStep::Collate {
                                target: name,
                                dd,
                                ddis,
                                depend_info,
                                ..
                            } if name == &target.name => {
                                let rcmake_resolve::DependInfo::Cxx(info) = depend_info.as_ref()
                                else {
                                    return None;
                                };
                                Some((
                                    dd,
                                    ddis,
                                    info.bmi_installation
                                        .as_ref()
                                        .map(|install| install.destination.as_slice())
                                        .filter(|destination| !destination.is_empty()),
                                ))
                            }
                            _ => None,
                        })
                    else {
                        continue;
                    };
                    let scans_for_config = scans_by_config
                        .entry(&config.0)
                        .or_insert_with(|| (config, dyndep, Vec::new(), bmi_destination));
                    scans_for_config
                        .2
                        .extend(graph.steps.iter().filter_map(|step| {
                            match step {
                                BuildStep::Scan { source, ddi, .. } if scans.contains(ddi) => {
                                    sources
                                        .get(source)
                                        .map(|path| (bin.join(to_native(ddi)), path.clone()))
                                }
                                _ => None,
                            }
                        }));
                }
            }
            for (config, dyndep, scans, bmi_destination) in scans_by_config.into_values() {
                if scans.is_empty() {
                    continue;
                }
                let name = filename(&target_stem, label(config));
                let output = index.root.join(to_native(&name));
                let module_table = bin
                    .join(to_native(dyndep))
                    .with_file_name("CXXModules.json");
                if let Some((_, contents)) = config_files.get_mut(&config.0) {
                    include(contents, &name, false);
                }
                let script = bin.join("CMakeFiles/ModuleExports").join(format!(
                    "{}.cmake",
                    HashAlgo::Sha256.hex_digest(&from_native(&output))
                ));
                write_property_script(
                    host,
                    &script,
                    &output,
                    &module_table,
                    &target_name,
                    config,
                    &scans,
                    plan.install,
                    bmi_destination,
                )?;
                jobs.push(ModuleExportJob {
                    dyndep: dyndep.clone(),
                    script: from_native(&script),
                    output: from_native(&output),
                });
                add_install(
                    &mut install,
                    action,
                    &index.directory,
                    Some(config),
                    &output,
                );
            }
            if let Some(features) = target.usage.fields.get(&UsageField::CompileFeatures) {
                let features = list::join(features.iter().map(|entry| &entry.value));
                start_properties(&mut main, &target_name);
                render_property(
                    &mut main,
                    b"IMPORTED_CXX_MODULES_COMPILE_FEATURES",
                    &features,
                    plan.install,
                );
                main.extend_from_slice(b")\n");
            }
        }
        let index_path = index.write(host, config_files, multi_config, &mut install)?;
        include(&mut main, &index_path, false);
        let prefix = model
            .definition(DirId::ROOT, b"CMAKE_INSTALL_PREFIX")
            .map_or(b"/usr/local".as_slice(), |value| value.as_slice());
        write_output(
            host,
            &main_path,
            &render_main(plan, destination, Some(prefix), &main)?,
        )?;
    }
    write_module_install(host, bin, &install)?;
    Ok(jobs)
}

struct ModuleExportIndex<'a> {
    root: PathBuf,
    directory: BString,
    stem: BString,
    plan: &'a ExportPlan,
    action: Option<&'a InstallAction>,
}

impl ModuleExportIndex<'_> {
    fn write(
        &self,
        host: &dyn Host,
        config_files: BTreeMap<&BString, (&ConfigId, BString)>,
        multi_config: bool,
        install: &mut InstallManifest,
    ) -> Result<BString, GenError> {
        let mut index_contents = BString::default();
        if self.plan.install {
            index_contents.extend_from_slice(
                b"file(GLOB _cmake_cxx_module_includes \"${CMAKE_CURRENT_LIST_DIR}/",
            );
            quoted_content(&mut index_contents, &self.stem);
            index_contents.extend_from_slice(
                br#"-*.cmake")
foreach(_cmake_cxx_module_include IN LISTS _cmake_cxx_module_includes)
  include("${_cmake_cxx_module_include}")
endforeach()
unset(_cmake_cxx_module_include)
unset(_cmake_cxx_module_includes)
"#,
            );
        }
        for (config, contents) in config_files.into_values() {
            let name = filename(&self.stem, label(config));
            if !self.plan.install {
                include(&mut index_contents, &name, multi_config);
            }
            let path = self.root.join(to_native(&name));
            write_output(host, &path, &contents)?;
            add_install(install, self.action, &self.directory, Some(config), &path);
        }
        let index_file = filename(&self.stem, b"");
        let path = self.root.join(to_native(&index_file));
        write_output(host, &path, &index_contents)?;
        add_install(install, self.action, &self.directory, None, &path);

        Ok(Lexical::new(&self.directory).append(&index_file))
    }
}

fn module_export_members<'a>(model: &'a ProjectModel, plan: &ExportPlan) -> Vec<&'a Target> {
    plan.targets
        .iter()
        .filter_map(|exported| {
            let matches_name = |target: &Target| {
                target.props.get(b"EXPORT_NAME").unwrap_or(&target.name) == &exported.name
            };
            if let Some(set) = model.exports.get(&plan.set) {
                set.targets
                    .iter()
                    .filter_map(|&id| model.target(id))
                    .find(|target| matches_name(target))
            } else {
                model
                    .targets()
                    .map(|(_, target)| target)
                    .find(|target| !target.is_imported() && matches_name(target))
            }
        })
        .collect()
}

fn module_sets<'a>(
    model: &'a ProjectModel,
    target: &'a Target,
) -> impl Iterator<Item = &'a FileSet> {
    target
        .file_sets
        .iter()
        .filter_map(|&id| model.file_set(id))
        .filter(|set| set.kind == FileSetKind::CxxModules)
}

fn safe_directory(directory: &[u8], plan: &ExportPlan) -> Result<BString, GenError> {
    if directory.contains(&0) {
        return Err(output_error(
            &plan.file,
            "Invalid C++ module export directory.",
        ));
    }
    // CMake appends the module directory to the directory of the export file.
    let relative = Lexical::new(directory).relative_part();
    Ok(Lexical::new(relative).normalize())
}

fn validate_file_component(value: &[u8], plan: &ExportPlan) -> Result<(), GenError> {
    if value
        .iter()
        .any(|byte| matches!(byte, b'/' | b'\\' | b'\n' | b'\r' | 0))
    {
        return Err(output_error(
            &plan.file,
            "Invalid name in a C++ module export file.",
        ));
    }
    Ok(())
}

fn label(config: &ConfigId) -> &[u8] {
    if config.0.is_empty() {
        b"noconfig"
    } else {
        &config.0
    }
}

fn filename(stem: &[u8], suffix: &[u8]) -> BString {
    let mut file = BString::from(stem);
    if !suffix.is_empty() {
        file.push(b'-');
        file.extend_from_slice(suffix);
    }
    file.extend_from_slice(b".cmake");
    file
}

fn include(out: &mut BString, file: &[u8], optional: bool) {
    out.extend_from_slice(b"include(\"${CMAKE_CURRENT_LIST_DIR}/");
    quoted_content(out, file);
    out.push(b'"');
    if optional {
        out.extend_from_slice(b" OPTIONAL");
    }
    out.extend_from_slice(b")\n");
}

fn install_set_destination(
    model: &ProjectModel,
    plan: &ExportPlan,
    set: &FileSet,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<BString>, GenError> {
    for &index in model
        .exports
        .get(&plan.set)
        .into_iter()
        .flat_map(|set| &set.install_rules)
    {
        let Some(rule) = model.installs.get(index) else {
            continue;
        };
        let InstallKind::Targets { targets, .. } = &rule.kind else {
            continue;
        };
        if !targets.contains(&set.target) {
            continue;
        }
        let Some(options) = options_for_set(model, index, &set.name) else {
            continue;
        };
        if !options.configurations.is_empty()
            && !options
                .configurations
                .iter()
                .any(|value| config.matches(value))
        {
            continue;
        }
        if let Some(destination) = options.destination {
            let context = EvalContext {
                config: config.clone(),
                ..EvalContext::default()
            };
            return genex(usage, rule.dir, &context, &destination, None).map(Some);
        }
    }
    Ok(None)
}

type SourcePaths = BTreeMap<BString, BString>;

#[allow(clippy::too_many_arguments)]
fn render_file_set<'a>(
    out: &mut BString,
    plan: &ExportPlan,
    model: &ProjectModel,
    target: &Target,
    name: &[u8],
    set: &FileSet,
    configs: impl Iterator<Item = &'a ConfigId>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<(&'a ConfigId, SourcePaths)>, GenError> {
    let mut sources = Vec::new();
    let mut entries = Vec::new();
    for config in configs {
        let roots = set_files(model, usage, target, set, config)?;
        let destination = if plan.install {
            let Some(destination) = install_set_destination(model, plan, set, config, usage)?
            else {
                continue;
            };
            Some(destination)
        } else {
            None
        };
        let mut bases = BTreeSet::new();
        let mut paths = BTreeMap::new();
        for (base, files) in roots {
            bases.insert(destination.clone().unwrap_or(base));
            for (source, relative) in files {
                let path = destination.as_ref().map_or_else(
                    || source.clone(),
                    |base| Lexical::new(base).append(&relative),
                );
                paths.insert(source, path);
            }
        }
        entries.push((config, bases, paths.values().cloned().collect::<Vec<_>>()));
        sources.push((config, paths));
    }
    if entries.is_empty() {
        return Ok(sources);
    }
    out.extend_from_slice(b"if(NOT CMAKE_VERSION VERSION_LESS \"3.28\")\n  target_sources(");
    quote(out, name);
    out.extend_from_slice(b" INTERFACE FILE_SET ");
    quote(out, &set.name);
    out.extend_from_slice(b" TYPE CXX_MODULES");
    let multi = entries.len() > 1;
    for (key, bases) in [(b" BASE_DIRS".as_slice(), true), (b" FILES", false)] {
        if !entries.iter().any(|(_, dirs, paths)| {
            if bases {
                !dirs.is_empty()
            } else {
                !paths.is_empty()
            }
        }) {
            continue;
        }
        out.extend_from_slice(key);
        for (config, dirs, paths) in &entries {
            let paths: Vec<_> = if bases {
                dirs.iter().collect()
            } else {
                paths.iter().collect()
            };
            for path in paths {
                out.extend_from_slice(b" \"");
                if multi {
                    out.extend_from_slice(b"$<$<CONFIG:");
                    quoted_content(out, &config.0);
                    out.extend_from_slice(b">:");
                }
                if plan.install {
                    render_relative_path(out, path, false);
                } else {
                    quoted_content(out, path);
                }
                if multi {
                    out.push(b'>');
                }
                out.push(b'"');
            }
        }
    }
    out.extend_from_slice(b")\nendif()\n");
    Ok(sources)
}

#[allow(clippy::too_many_arguments)]
fn write_property_script(
    host: &dyn Host,
    script: &Path,
    output: &Path,
    table: &Path,
    target: &[u8],
    config: &ConfigId,
    scans: &[(PathBuf, BString)],
    install: bool,
    bmi_destination: Option<&[u8]>,
) -> Result<(), GenError> {
    let mut out = BString::from("cmake_minimum_required(VERSION 3.28)\n");
    let mut property = BString::from("set_property(TARGET ");
    quote(&mut property, target);
    property.extend_from_slice(b" PROPERTY IMPORTED_CXX_MODULES_");
    property.extend_from_slice(&super::config_label(&config.0));
    out.extend_from_slice(b"set(_properties ");
    quote(&mut out, &property);
    out.extend_from_slice(b")\n");
    if !install || bmi_destination.is_some() {
        out.extend_from_slice(b"file(READ ");
        quote(&mut out, &from_native(table));
        out.extend_from_slice(b" _module_table)\n");
    }
    if install && let Some(destination) = bmi_destination {
        out.extend_from_slice(b"set(_bmi_destination ");
        quote(&mut out, destination);
        out.extend_from_slice(b")\nset(_bmi_prefix ");
        quote(
            &mut out,
            if to_native(destination).is_absolute() {
                b""
            } else {
                b"${_IMPORT_PREFIX}/"
            },
        );
        out.extend_from_slice(b")\n");
    }
    out.extend_from_slice(
        br#"function(escape_property value output)
  string(REPLACE "\\" "\\\\" value "${value}")
  string(REPLACE "\"" "\\\"" value "${value}")
  string(REPLACE "$" "\\$" value "${value}")
  string(REPLACE ";" "\\;" value "${value}")
  set("${output}" "${value}" PARENT_SCOPE)
endfunction()
function(add_modules scan source prefix)
  file(READ "${scan}" contents)
  string(JSON count ERROR_VARIABLE error LENGTH "${contents}" rules 0 provides)
  if(error OR count EQUAL 0)
    return()
  endif()
  math(EXPR last "${count} - 1")
  escape_property("${source}" source)
  foreach(index RANGE "${last}")
    string(JSON name GET "${contents}" rules 0 provides "${index}" logical-name)
    set(bmi_suffix "")
    if(DEFINED _module_table)
      string(JSON bmi GET "${_module_table}" modules "${name}" bmi)
      if(DEFINED _bmi_destination)
        get_filename_component(bmi "${bmi}" NAME)
        set(bmi "${_bmi_destination}/${bmi}")
      endif()
      escape_property("${bmi}" bmi)
      set(bmi_suffix ",${_bmi_prefix}${bmi}")
    endif()
    escape_property("${name}" name)
    string(APPEND _properties "\n  \"${name}=${prefix}${source}${bmi_suffix}\"")
  endforeach()
  set(_properties "${_properties}" PARENT_SCOPE)
endfunction()
"#,
    );
    for (scan, source) in scans {
        out.extend_from_slice(b"add_modules(");
        quote(&mut out, &from_native(scan));
        out.push(b' ');
        quote(&mut out, source);
        out.push(b' ');
        quote(
            &mut out,
            if install && !to_native(source).is_absolute() {
                b"${_IMPORT_PREFIX}/"
            } else {
                b""
            },
        );
        out.extend_from_slice(b")\n");
    }
    out.extend_from_slice(b"file(WRITE ");
    quote(&mut out, &from_native(output));
    out.extend_from_slice(b" \"${_properties}\n)\n\")\n");
    write_output(host, script, &out)
}

fn add_install(
    manifest: &mut InstallManifest,
    action: Option<&InstallAction>,
    directory: &[u8],
    config: Option<&ConfigId>,
    file: &Path,
) {
    let Some(action) = action else {
        return;
    };
    let mut action = action.clone();
    action.destination = Lexical::new(&action.destination).append(directory);
    if let Some(config) = config {
        action.configurations = vec![config.0.clone()];
    }
    action.kind = InstallActionKind::Files {
        files: vec![from_native(file)],
        file_type: "FILE".into(),
        permissions: Vec::new(),
        rename: None,
    };
    manifest.actions.push(action);
}

fn write_module_install(
    host: &dyn Host,
    bin: &Path,
    manifest: &InstallManifest,
) -> Result<(), GenError> {
    if manifest.actions.is_empty() {
        return Ok(());
    }
    // The CLI writes the main install script before it calls the generator.
    // Add the module files before the script records the installed paths.
    let path = bin.join("cmake_install.cmake");
    let mut script = host
        .read(
            &path,
            ReadRange::All {
                limit: 64 * 1024 * 1024,
            },
        )
        .map_err(|error| output_error(&from_native(&path), error.to_string()))?;
    let begin = b"# C++ module export files\n";
    let end = b"# End of C++ module export files\n";
    let footer = b"if(CMAKE_INSTALL_COMPONENT)\n  if(CMAKE_INSTALL_COMPONENT MATCHES ";
    let position = script
        .windows(footer.len())
        .rposition(|part| part == footer)
        .ok_or_else(|| {
            output_error(
                &from_native(&path),
                "Cannot find the install manifest in the install script.",
            )
        })?;
    let (actions, _) = script.split_at(position);
    let start = actions
        .strip_suffix(end)
        .and_then(|actions| actions.windows(begin.len()).rposition(|part| part == begin))
        .unwrap_or(position);
    let mut include = BString::from(begin);
    include.extend_from_slice(&install_script::render_with_options(
        manifest,
        &from_native(bin),
        &InstallScriptOptions {
            top_level: false,
            ..Default::default()
        },
    )?);
    include.extend_from_slice(end);
    script.splice(start..position, include.iter().copied());
    write_if_changed(host, &path, &script)
}
