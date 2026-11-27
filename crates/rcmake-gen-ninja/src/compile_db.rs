//! Native per-configuration `compile_commands.json` extraction for Ninja Multi-Config.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rcmake_base::{BString, ByteSlice};
use rcmake_gen_common::compile_commands::clean_compile_shell;
use rcmake_gen_common::compile_commands_native::{
    CommandEncoding, CompileRow, ConfigDatabase, MultiConfigCompileDatabase, absolute_path,
    strip_co_compile_argv, strip_depfile_argv,
};
use rcmake_host::path;
use rcmake_model::TargetId;
use rcmake_resolve::{BuildStep, Invocation};

use crate::graph::Graph;
use crate::multi::ConfigFiles;

pub(crate) fn collect(
    configs: &[(Graph<'_>, ConfigFiles)],
    default_index: usize,
    build_dir: &Path,
    selected: &HashSet<TargetId>,
) -> MultiConfigCompileDatabase {
    let build_root = path::from_native_cmake(build_dir);
    let mut databases = Vec::with_capacity(configs.len());

    for (graph, _) in configs {
        let config_name = graph.input.config.0.clone();
        let mut step_owner: HashMap<usize, TargetId> = HashMap::new();
        for (&target_id, target) in graph.input.target_ids.iter().zip(&graph.input.targets) {
            if !selected.contains(&target_id) {
                continue;
            }
            for &step_idx in &target.steps {
                step_owner.entry(step_idx).or_insert(target_id);
            }
        }

        let mut rows = Vec::new();
        for (step_idx, step) in graph.input.steps.iter().enumerate() {
            let BuildStep::Compile {
                object,
                source,
                invocation,
                ..
            } = step
            else {
                continue;
            };
            let Some(&target_id) = step_owner.get(&step_idx) else {
                continue;
            };
            let directory = invocation
                .working_dir
                .as_deref()
                .map_or_else(|| build_root.clone(), |dir| absolute_path(&build_root, dir));
            let encoding = encode_invocation(graph, invocation);
            if let Some(action) = graph.input.swift_actions.get(&step_idx) {
                let sources = if action.abs_sources.is_empty() {
                    action.sources.as_slice()
                } else {
                    action.abs_sources.as_slice()
                };
                if sources.is_empty() {
                    rows.push(CompileRow {
                        config: config_name.clone(),
                        target: target_id,
                        directory,
                        file: absolute_path(&build_root, source),
                        output: absolute_path(&build_root, object),
                        invocation: encoding,
                    });
                } else {
                    for (idx, swift_source) in sources.iter().enumerate() {
                        let row_object = if action.single_output {
                            action.outputs.first().unwrap_or(object)
                        } else {
                            action.outputs.get(idx).unwrap_or(object)
                        };
                        rows.push(CompileRow {
                            config: config_name.clone(),
                            target: target_id,
                            directory: directory.clone(),
                            file: absolute_path(&build_root, swift_source),
                            output: absolute_path(&build_root, row_object),
                            invocation: encoding.clone(),
                        });
                    }
                }
            } else {
                rows.push(CompileRow {
                    config: config_name.clone(),
                    target: target_id,
                    directory,
                    file: absolute_path(&build_root, source),
                    output: absolute_path(&build_root, object),
                    invocation: encoding,
                });
            }
        }

        databases.push(ConfigDatabase {
            config: config_name,
            rows,
        });
    }

    MultiConfigCompileDatabase {
        configs: databases,
        default_index,
        diagnostics: Vec::new(),
    }
}

fn encode_invocation(graph: &Graph<'_>, invocation: &Invocation) -> CommandEncoding {
    if is_complete_argv(graph, invocation) {
        let stripped = strip_co_compile_argv(&invocation.argv);
        let cleaned = strip_depfile_argv(stripped);
        if !cleaned.is_empty() {
            return CommandEncoding::Arguments(cleaned);
        }
    }
    if !invocation.shell.is_empty() {
        return CommandEncoding::Command(clean_compile_shell(&invocation.shell));
    }
    let command = graph
        .commands
        .argv(&strip_depfile_argv(strip_co_compile_argv(&invocation.argv)));
    CommandEncoding::Command(BString::from(command))
}

fn is_complete_argv(graph: &Graph<'_>, invocation: &Invocation) -> bool {
    if invocation.argv.is_empty() {
        return false;
    }
    if graph.commands.raw_script(invocation).is_some() {
        return false;
    }
    if matches!(
        invocation.argv.as_slice(),
        [program, option, _] if program.as_slice() == b"sh" && option.as_slice() == b"-c"
    ) {
        return false;
    }
    if invocation.shell.find(b"&&").is_some() && !invocation.argv.iter().any(|arg| arg == b"&&") {
        // Multi-command shell chain that is not represented directly in argv.
        return false;
    }
    true
}
