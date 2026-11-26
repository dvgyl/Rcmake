use rcmake_diag::PolicyStatus;
use rcmake_genex::{ConfigId, EvalContext, GenexHost, evaluate, parse};
use rcmake_model::ProjectModel;

use super::{SourcePaths, absolute_path};
use crate::{
    ResolveError,
    file_sets::{file_generate_contexts, file_generate_policy},
    usage::UsageResolver,
};

impl SourcePaths<'_> {
    pub(super) fn index_file_generations(
        &mut self,
        model: &ProjectModel,
        config: &ConfigId,
        usage: &mut UsageResolver<'_>,
    ) -> Result<(), ResolveError> {
        let props = &model.global.props;
        let Some(count) = props
            .get(b"_RCMAKE_FILE_GENERATE_COUNT")
            .and_then(|value| number(value))
        else {
            return Ok(());
        };
        let contexts = file_generate_contexts(model, std::slice::from_ref(config));
        let directories: Vec<_> = model.dirs().collect();
        for index in 0..count {
            let key = format!("_RCMAKE_FILE_GENERATE_{index}");
            let get = |field: &str| props.get(format!("{key}_{field}").as_bytes());
            let directory = get("DIRECTORY")
                .and_then(|value| number(value))
                .and_then(|dir| directories.get(dir));
            let (Some(&(id, dir)), Some(output)) = (directory, get("ARG1")) else {
                break;
            };
            let base = if file_generate_policy(model, id, index) == PolicyStatus::New {
                &dir.binary_dir
            } else {
                &dir.source_dir
            };
            let ast = parse(output)?;

            let mut options = (4..).map_while(|arg| get(&format!("ARG{arg}")));
            let mut target = None;
            while let Some(option) = options.next() {
                match option.as_slice() {
                    b"TARGET" => {
                        target = options
                            .next()
                            .and_then(|name| usage.host(id).find_target(name));
                        break;
                    }
                    b"CONDITION" | b"NEWLINE_STYLE" => {
                        options.next();
                    }
                    _ => {}
                }
            }
            for context in &contexts {
                let context = EvalContext {
                    current: target,
                    head: target,
                    ..(*context).clone()
                };
                usage.source_reads = Some(Default::default());
                let mut host = usage.host(id);
                host.cmp0189 = get("CMP0189").map(|value| {
                    if value == "New" {
                        PolicyStatus::New
                    } else {
                        PolicyStatus::Old
                    }
                });
                let result = evaluate(&ast, &context, &mut host);
                let reads = usage
                    .source_reads
                    .take()
                    .map_or_else(Default::default, |reads| {
                        std::mem::take(&mut *reads.borrow_mut())
                    });
                let output = result?.0;
                if !output.is_empty() {
                    let path = absolute_path(base, &output);
                    if let Some(index) = self.index_mut() {
                        index
                            .generated
                            .entry(path.clone())
                            .or_default()
                            .insert(dir.binary_dir.clone());
                        if !reads.is_empty() {
                            index
                                .output_source_reads
                                .entry(path)
                                .or_default()
                                .extend(reads);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn number(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

pub(crate) fn traced_sources(
    model: &ProjectModel,
    configs: &[ConfigId],
    graphs: &[crate::BuildGraph],
) -> Result<
    indexmap::IndexMap<rcmake_model::TargetId, indexmap::IndexSet<rcmake_base::BString>>,
    ResolveError,
> {
    use indexmap::{IndexMap, IndexSet};
    use rcmake_base::{BString, ByteSlice, hash::HashAlgo};

    let mut sources = IndexMap::<_, IndexSet<_>>::new();
    if model
        .global
        .props
        .get(b"_RCMAKE_FILE_GENERATE_COUNT")
        .is_none()
    {
        return Ok(sources);
    }
    let mut rule_files = IndexMap::new();
    let mut usage = UsageResolver::new(model);
    for graph in graphs {
        for (&id, steps) in &graph.custom_source_steps {
            for index in steps {
                let Some(owners) = graph.custom_output_owners.get(index) else {
                    continue;
                };
                if !owners.targets.contains(&id) {
                    continue;
                }
                if let Some(main) = graph.custom_main_sources.get(index) {
                    sources.entry(id).or_default().insert(main.clone());
                    continue;
                }
                let rule = match rule_files.entry(owners.command) {
                    indexmap::map::Entry::Occupied(entry) => entry.into_mut(),
                    indexmap::map::Entry::Vacant(entry) => {
                        let Some(command) = model.command(owners.command) else {
                            continue;
                        };
                        let Some(output) = command.outputs.first() else {
                            continue;
                        };
                        let dir = model.dir(command.dir);
                        let simple = output.value.replace(b"$<CONFIG>", b"(CONFIG)");
                        let mut path = if !simple.windows(2).any(|part| part == b"$<") {
                            absolute_path(&dir.binary_dir, &simple)
                        } else {
                            let mut evaluated = IndexSet::new();
                            for config in configs {
                                let (outputs, _) = crate::lower::custom::command_outputs(
                                    model, command, config, &mut usage,
                                )?;
                                evaluated.extend(outputs.into_iter().take(1));
                            }
                            if evaluated.len() == 1 {
                                evaluated.pop().unwrap_or_default()
                            } else {
                                let hash = HashAlgo::Sha256.hex_digest(&output.value);
                                let short = hash.get(..16).unwrap_or(&hash);
                                absolute_path(
                                    &dir.binary_dir,
                                    format!("CMakeFiles/{short}").as_bytes(),
                                )
                            }
                        };
                        if let Some(config_dir) = model
                            .definition(command.dir, b"CMAKE_CFG_INTDIR")
                            .filter(|dir| dir.starts_with(b"$"))
                        {
                            path = BString::from(path.replace(config_dir, b"/CMakeFiles"));
                        }
                        path.extend_from_slice(b".rule");
                        entry.insert(path)
                    }
                };
                sources.entry(id).or_default().insert(rule.clone());
            }
        }
    }
    Ok(sources)
}
