use std::{collections::HashSet, path::Path};

use rcmake_base::{BString, list};
use rcmake_diag::{RenderOptions, format_paragraphs};
use rcmake_gen_common::{
    GenError, compile_commands_native::MultiConfigCompileDatabase, export::write_module_exports,
};
use rcmake_genex::ConfigId;
use rcmake_host::{Host, path};
use rcmake_model::{DirId, ProjectModel, TargetId};
use rcmake_resolve::GenerationResult;

use crate::graph::{self, Graph, qualified};
use crate::syntax::{Edge, Ninja};
use crate::{
    Programs, RULES_PATH, add_ctest_files, add_install_scripts,
    clean::clean_command,
    commands::Commands,
    config_artifacts, fortran, generation_error,
    install::{self, emit_config_utilities, emit_utilities},
    modules, output_prefix,
    prep::PrepTargets,
    prepare_directory,
    regenerate::{configuration_setting, configure_command, emit_regeneration},
    write_rules,
};

pub(super) fn generate_multi(
    result: &GenerationResult,
    host: &dyn Host,
    build_dir: &Path,
    model: Option<&ProjectModel>,
    commands: Commands,
    export_targets: Option<&HashSet<TargetId>>,
) -> Result<Option<MultiConfigCompileDatabase>, GenError> {
    let prefix = output_prefix(model, &commands);
    let mut configs = Vec::new();
    let mut names = HashSet::new();
    for config in &result.configs {
        let name = config.0.as_slice();
        if name.is_empty()
            || name
                .iter()
                .any(|byte| matches!(byte, b'/' | b'\\' | b'\n' | b'\r' | 0))
            || !names.insert(name)
        {
            return Err(generation_error(
                build_dir,
                "Invalid or duplicate Ninja configuration name.",
            ));
        }
        let input = result
            .graphs
            .iter()
            .find(|graph| graph.config == *config)
            .ok_or_else(|| {
                generation_error(build_dir, "A Ninja configuration has no build graph.")
            })?;
        let artifacts = config_artifacts(result, input);
        let mut graph = Graph::with_context(
            input,
            &artifacts,
            build_dir,
            Some(name),
            &prefix,
            commands.clone(),
            model,
            Some(result),
        )?;
        graph.add_aliases(&artifacts, model, result);
        graph.resolve_custom_inputs(model, host);
        graph.set_compile_metadata(result);
        graph.set_command_settings(model, host);
        graph.set_clean_files(model, &result.metadata.clean_files)?;
        let prep = PrepTargets::new(&graph, result, model, host)?;
        configs.push((graph, ConfigFiles::new(name, prep)));
    }

    prepare_directory(host, build_dir)?;
    for (graph, _) in &mut configs {
        graph.prepare_custom_scripts(host)?;
    }
    let selection = Selection::new(&result.configs, build_dir, |name| {
        model
            .and_then(|model| model.definition(DirId::ROOT, name))
            .cloned()
            .or_else(|| configuration_setting(host, build_dir, name))
    })?;
    let (default, default_files) = configs
        .get(selection.default)
        .ok_or_else(|| generation_error(build_dir, "Missing default Ninja configuration."))?;
    let cross: Vec<_> = selection
        .cross
        .iter()
        .filter_map(|&index| configs.get(index).map(|(graph, _)| graph))
        .collect();
    let defaults: Vec<_> = selection
        .defaults
        .iter()
        .filter_map(|&index| configs.get(index).map(|(graph, _)| graph))
        .collect();
    let mut manifests = vec![default.node(b"build.ninja")];
    for (_, files) in &configs {
        manifests.extend([
            default.node(&files.entry),
            default.node(&files.implementation),
        ]);
    }

    let graphs: Vec<_> = configs.iter().map(|(graph, _)| graph.input).collect();
    let prep_graphs: Vec<_> = configs.iter().map(|(graph, _)| graph).collect();
    crate::prep::warn_install_prep_components(&prep_graphs, model, host);
    let uses_modules = graphs.iter().any(|graph| modules::uses_modules(graph));
    let module_flag = if uses_modules {
        modules::modmap_flag(result, build_dir)?
    } else {
        BString::default()
    };
    prepare_directory(host, build_dir)?;
    crate::write_swift_output_maps(&graphs, host, build_dir)?;
    let verify_globs = rcmake_gen_common::glob::write_verification(model, build_dir, host)?;
    let exports = model
        .map(|model| write_module_exports(result, model, host, build_dir, true))
        .transpose()?
        .unwrap_or_default();
    manifests.extend(exports.iter().map(|job| default.node(&job.script)));
    manifests.extend(
        result
            .metadata
            .generated_files
            .iter()
            .map(|file| default.node(file)),
    );
    add_install_scripts(&mut manifests, default, result, model);
    add_ctest_files(&mut manifests, default, model);
    if uses_modules || graphs.iter().any(|graph| fortran::uses_fortran(graph)) {
        manifests.extend(
            modules::write_metadata(result, model, &graphs, host, build_dir, true)?
                .iter()
                .map(|file| default.node(file)),
        );
    }
    let has_cache = host
        .metadata(&build_dir.join("CMakeCache.txt"), rcmake_host::Follow::Yes)
        .is_ok();
    let programs = Programs::new(result, host, build_dir, model, &commands);
    let custom_clean = configs
        .iter()
        .any(|(graph, _)| !graph.clean_files.is_empty());
    write_rules(
        result,
        host,
        build_dir,
        uses_modules,
        model,
        &commands,
        &programs.ninja,
        custom_clean,
    )?;
    let regenerate = configure_command(result, build_dir, &programs.cmake, &commands, true);
    let rebuild = regenerate.clone();
    for (index, (graph, files)) in configs.iter().enumerate() {
        let mut ninja = graph.manifest();
        ninja.include(&graph.node(RULES_PATH.as_bytes()));
        let mut emitted = HashSet::new();
        let included = std::iter::once(index).chain(
            selection
                .cross
                .iter()
                .copied()
                .filter(|&other| other != index),
        );
        let owners = graph::CustomOwners::new(
            included
                .clone()
                .filter_map(|index| configs.get(index).map(|(graph, _)| graph)),
        );
        for (output_index, (output, files)) in included
            .clone()
            .filter_map(|selected| configs.get(selected).map(|config| (selected, config)))
        {
            let cross = (index != output_index)
                .then(|| {
                    result
                        .cross_graphs
                        .get(&(graph.input.config.clone(), output.input.config.clone()))
                })
                .flatten()
                .map(|input| {
                    let artifacts = config_artifacts(result, input);
                    let mut variant = Graph::with_context(
                        input,
                        &artifacts,
                        build_dir,
                        output.config,
                        &prefix,
                        commands.clone(),
                        model,
                        Some(result),
                    )?;
                    variant.add_aliases(&artifacts, model, result);
                    variant.resolve_custom_inputs(model, host);
                    variant.set_compile_metadata(result);
                    variant.set_command_settings(model, host);
                    variant.prepare_custom_scripts(host)?;
                    Ok::<_, GenError>(variant)
                })
                .transpose()?;
            let output_graph = cross.as_ref().unwrap_or(output);
            let custom_inputs =
                output_graph.inherited_custom_inputs(graph, &owners, !selection.cross.is_empty());
            let graph = output_graph;
            graph.emit_into(
                &mut ninja,
                &programs.cmake,
                &module_flag,
                &exports,
                Some(&mut emitted),
                &custom_inputs,
            )?;
            graph.emit_clean_inputs(&mut ninja);
            files.prep.emit(&mut ninja, graph);
            emit_config_utilities(&mut ninja, output, &programs, &rebuild);
        }
        if !cross.is_empty() {
            files.prep.emit_all_aliases(&mut ninja, &cross);
        }
        emit_clean(
            &mut ninja,
            graph,
            &files.implementation,
            &programs,
            included.filter_map(|selected| configs.get(selected).map(|(graph, _)| graph)),
            &cross,
        );
        if !cross.is_empty() {
            graph.emit_aliases(&mut ninja, Some(b"all"), &cross, &emitted);
        }
        emit_regeneration(
            &mut ninja,
            graph,
            &manifests,
            result,
            &regenerate,
            verify_globs.then_some(programs.cmake.as_slice()),
            has_cache,
        );
        crate::write_always(
            host,
            &build_dir.join(path::to_native(&files.implementation)),
            &ninja.0,
        )?;
        write_entry(
            host,
            build_dir,
            graph,
            files,
            &files.entry,
            (&[graph], &[]),
            &emitted,
            (&programs, &rebuild),
        )?;
        if index == selection.default {
            write_entry(
                host,
                build_dir,
                default,
                default_files,
                b"build.ninja",
                (&defaults, &cross),
                &emitted,
                (&programs, &rebuild),
            )?;
        }
    }
    let db = export_targets.map(|selected| {
        crate::compile_db::collect(&configs, selection.default, build_dir, selected)
    });
    Ok(db)
}

pub(crate) struct ConfigFiles {
    entry: BString,
    implementation: BString,
    prep: PrepTargets,
}

impl ConfigFiles {
    fn new(config: &[u8], prep: PrepTargets) -> Self {
        let entry = [b"build-", config, b".ninja"].concat().into();
        let implementation = [b"CMakeFiles/impl-", config, b".ninja"].concat().into();
        Self {
            entry,
            implementation,
            prep,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_entry(
    host: &dyn Host,
    build_dir: &Path,
    graph: &Graph<'_>,
    files: &ConfigFiles,
    entry: &[u8],
    (aliases, all): (&[&Graph<'_>], &[&Graph<'_>]),
    emitted: &HashSet<BString>,
    (programs, rebuild): (&Programs, &[u8]),
) -> Result<(), GenError> {
    let mut ninja = Ninja::default();
    // The include path can contain a literal pipe.
    ninja.0.extend_from_slice(b"ninja_pipe = |\n");
    ninja.include(&graph.node(&files.implementation));
    graph.emit_aliases(&mut ninja, None, aliases, emitted);
    if !graph.has_target(&"clean".into()) {
        let targets = aliases
            .iter()
            .map(|graph| qualified(&graph.node(b"clean"), graph.config))
            .collect::<Vec<_>>();
        ninja.phony(&graph.node(b"clean"), &targets);
    }
    files.prep.emit_aliases(&mut ninja, graph, aliases);
    emit_utilities(
        &mut ninja,
        graph,
        entry,
        programs,
        rebuild,
        install::Scope::Entry { aliases, all },
    );
    if graph.prefix.is_empty() {
        ninja.0.extend_from_slice(b"default all\n");
    }
    crate::write_always(host, &build_dir.join(path::to_native(entry)), &ninja.0)
}

pub(crate) struct Selection {
    pub(crate) default: usize,
    pub(crate) cross: Vec<usize>,
    pub(crate) defaults: Vec<usize>,
}

impl Selection {
    pub(crate) fn new(
        configs: &[ConfigId],
        build_dir: &Path,
        setting: impl Fn(&[u8]) -> Option<BString>,
    ) -> Result<Self, GenError> {
        let Some(first) = configs.first() else {
            return Err(generation_error(
                build_dir,
                "Ninja Multi-Config requires at least one configuration.",
            ));
        };
        let index = |name: &[u8]| configs.iter().position(|config| config.0 == name);
        let requested = setting(b"CMAKE_DEFAULT_BUILD_TYPE").filter(|value| !value.is_empty());
        let default_name = requested.as_ref().unwrap_or(&first.0);
        let default = index(default_name).ok_or_else(|| config_error(format!(
            "The configuration specified by CMAKE_DEFAULT_BUILD_TYPE ({default_name}) is not present in CMAKE_CONFIGURATION_TYPES"
        )))?;
        let sorted = |mut selection: Vec<usize>| {
            selection.sort_unstable_by_key(|&index| configs.get(index).map(|config| &config.0));
            selection
        };
        let all = sorted((0..configs.len()).collect());
        let cross_setting = setting(b"CMAKE_CROSS_CONFIGS").unwrap_or_default();
        let cross = sorted(parse(&cross_setting, &all, index).ok_or_else(|| {
            config_error("CMAKE_CROSS_CONFIGS is not a subset of CMAKE_CONFIGURATION_TYPES")
        })?);
        let defaults_setting = setting(b"CMAKE_DEFAULT_CONFIGS").filter(|value| !value.is_empty());
        let defaults_name = defaults_setting.as_ref().unwrap_or(default_name);
        if defaults_name != default_name && (default_name.is_empty() || cross.is_empty()) {
            return Err(config_error(
                "CMAKE_DEFAULT_CONFIGS cannot be used without CMAKE_DEFAULT_BUILD_TYPE or CMAKE_CROSS_CONFIGS",
            ));
        }
        let defaults = sorted(
            parse(defaults_name, &cross, |name| {
                index(name).filter(|config| *config == default || cross.contains(config))
            })
            .ok_or_else(|| {
                config_error("CMAKE_DEFAULT_CONFIGS is not a subset of CMAKE_CROSS_CONFIGS")
            })?,
        );
        Ok(Self {
            default,
            cross,
            defaults,
        })
    }
}

fn config_error(message: impl AsRef<str>) -> GenError {
    let body = format_paragraphs(
        message.as_ref().as_bytes(),
        2,
        RenderOptions::default().wrap_width,
    );
    GenError::Invalid(
        [b"CMake Error:\n".as_slice(), &body, b"\n\n"]
            .concat()
            .into(),
    )
}

fn parse(
    value: &[u8],
    all: &[usize],
    index: impl Fn(&[u8]) -> Option<usize>,
) -> Option<Vec<usize>> {
    let names = list::split(value, list::Empty::Drop);
    if matches!(names.as_slice(), [name] if name == "all") {
        return Some(all.to_vec());
    }
    let mut selected = Vec::new();
    for name in names {
        if name == "all" {
            return None;
        }
        let index = index(&name)?;
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    Some(selected)
}

pub(crate) fn emit_clean<'a>(
    ninja: &mut Ninja,
    graph: &Graph<'_>,
    implementation: &[u8],
    programs: &Programs,
    included: impl Iterator<Item = &'a Graph<'a>>,
    cross: &[&Graph<'_>],
) {
    let mut file_arg = b"-f ".to_vec();
    graph
        .commands
        .word(&graph.node(implementation), &mut file_arg);
    let mut base = programs.ninja.clone();
    base.extend_from_slice(b" ");
    base.extend_from_slice(&file_arg);
    base.extend_from_slice(b" -t clean");
    let mut emit = |name, configs: &[&Graph<'_>]| {
        let mut targets = Vec::new();
        for config in configs {
            if !targets.is_empty() {
                targets.push(b' ');
            }
            graph.commands.word(
                &qualified(&graph.node(b"CMakeFiles/rcmake-clean"), config.config),
                &mut targets,
            );
        }
        let has_additional = configs.iter().any(|graph| !graph.clean_files.is_empty());
        if !has_additional {
            ninja.edge(
                b"CLEAN",
                Edge {
                    outputs: &[qualified(&graph.node(b"clean"), Some(name))],
                    ..Edge::default()
                },
            );
            ninja.binding(b"FILE_ARG", &file_arg);
            ninja.binding(b"TARGETS", &targets);
        } else {
            let mut command = base.clone();
            command.push(b' ');
            command.extend_from_slice(&targets);
            let command = clean_command(
                graph,
                &programs.cmake,
                command,
                configs.iter().flat_map(|graph| &graph.clean_files),
            );
            ninja.edge(
                b"CLEAN_CUSTOM",
                Edge {
                    outputs: &[qualified(&graph.node(b"clean"), Some(name))],
                    ..Edge::default()
                },
            );
            ninja.binding(b"COMMAND", &command);
        }
        // Default aliases can run multiple clean edges that share outputs.
        ninja.binding(b"pool", b"console");
    };
    for graph in included {
        if let Some(config) = graph.config {
            emit(config, &[graph]);
        }
    }
    if !cross.is_empty() {
        emit(b"all", cross);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_aliases_use_the_cross_set_for_all() -> Result<(), GenError> {
        let configs = [ConfigId("Release".into()), ConfigId("Debug".into())];
        let selection = Selection::new(&configs, Path::new("/build"), |name| match name {
            b"CMAKE_DEFAULT_BUILD_TYPE" => Some("".into()),
            b"CMAKE_CROSS_CONFIGS" => Some("Debug".into()),
            b"CMAKE_DEFAULT_CONFIGS" => Some("all".into()),
            _ => None,
        })?;
        assert_eq!(selection.default, 0);
        assert_eq!(selection.cross, [1]);
        assert_eq!(selection.defaults, [1]);
        Ok(())
    }

    #[test]
    fn default_alias_must_match_the_entry_manifest_without_cross_configs() {
        let configs = [ConfigId("Release".into()), ConfigId("Debug".into())];
        let select = |name| {
            Selection::new(&configs, Path::new("/build"), |setting| {
                (setting == b"CMAKE_DEFAULT_CONFIGS").then(|| BString::from(name))
            })
        };
        assert!(select("Release").is_ok());
        assert!(select("Debug").is_err());
    }
}
