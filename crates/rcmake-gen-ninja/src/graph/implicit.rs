use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, truth};
use rcmake_model::{CommandId, DirId, ProjectModel, PropScope, TargetId};
use rcmake_resolve::BuildStep;

use super::{Graph, OrderOnly};
use crate::syntax::{Edge, Ninja};

#[derive(Default)]
pub(crate) struct CustomOwners(BTreeMap<CommandId, BTreeSet<TargetId>>);

impl CustomOwners {
    pub(crate) fn new<'a, 'graph>(graphs: impl IntoIterator<Item = &'a Graph<'graph>>) -> Self
    where
        'graph: 'a,
    {
        let mut result = Self::default();
        for graph in graphs {
            for owners in graph.input.custom_output_owners.values() {
                result
                    .0
                    .entry(owners.command)
                    .or_default()
                    .extend(owners.targets.iter().copied());
            }
        }
        result
    }
}

impl Graph<'_> {
    pub(crate) fn inherited_custom_inputs(
        &self,
        file_graph: &Self,
        owners: &CustomOwners,
        cross: bool,
    ) -> BTreeMap<usize, Vec<BString>> {
        self.input
            .custom_output_owners
            .iter()
            .filter_map(|(&step, local)| {
                if local.explicit_only {
                    return None;
                }
                let targets = owners.0.get(&local.command)?;
                let mut closures =
                    targets
                        .iter()
                        .filter_map(|&id| self.target_index(id))
                        .map(|index| {
                            self.owner_dependencies(
                                file_graph,
                                index,
                                local.command,
                                cross && local.shared_output,
                            )
                        });
                let mut common = closures.next()?;
                for dependencies in closures {
                    common.retain(|node| dependencies.contains(node));
                }
                (!common.is_empty()).then(|| (step, common.into_iter().collect()))
            })
            .collect()
    }

    fn target_index(&self, id: TargetId) -> Option<usize> {
        self.input
            .target_ids
            .iter()
            .position(|target| *target == id)
    }

    fn concrete_node(&self, index: usize) -> Option<BString> {
        self.target_meta
            .get(index)
            .and_then(|meta| meta.artifact_outputs.first().cloned())
            .or_else(|| self.aliases.get(index).cloned())
    }

    fn per_config(&self, index: usize) -> bool {
        self.input.targets.get(index).is_some_and(|target| {
            target
                .steps
                .iter()
                .any(|step| !self.input.native_custom_steps.contains(step))
        }) || self.artifact_targets.contains(&index)
    }

    fn owns_command(&self, index: usize, command: CommandId) -> bool {
        self.input.targets.get(index).is_some_and(|target| {
            target.steps.iter().any(|step| {
                self.input
                    .custom_output_owners
                    .get(step)
                    .is_some_and(|owners| owners.command == command)
            })
        })
    }

    fn reaches_command(&self, index: usize, command: CommandId) -> bool {
        let mut visited = BTreeSet::new();
        let mut pending = vec![index];
        while let Some(index) = pending.pop() {
            if !visited.insert(index) {
                continue;
            }
            if self.owns_command(index, command) {
                return true;
            }
            if let Some(target) = self.input.targets.get(index) {
                pending.extend(target.depends.iter().copied());
            }
        }
        false
    }

    fn owner_dependencies(
        &self,
        file_graph: &Self,
        root: usize,
        command: CommandId,
        shared: bool,
    ) -> BTreeSet<BString> {
        let mut pending = vec![(false, root)];
        let mut visited = BTreeSet::from([(false, root)]);
        let mut nodes = BTreeSet::new();
        while let Some((file_config, index)) = pending.pop() {
            let graph = if file_config { file_graph } else { self };
            let Some(target) = graph.input.targets.get(index) else {
                continue;
            };
            let mut add = |file_config, dependency| {
                if visited.insert((file_config, dependency)) {
                    let graph = if file_config { file_graph } else { self };
                    if !graph.reaches_command(dependency, command) {
                        nodes.extend(graph.concrete_node(dependency));
                    }
                    // A target that needs this command can have prerequisites of its own.
                    pending.push((file_config, dependency));
                }
            };
            let command_targets: Vec<_> = target
                .steps
                .iter()
                .filter_map(|step| {
                    if graph
                        .input
                        .custom_output_owners
                        .get(step)
                        .is_some_and(|owners| owners.command == command)
                    {
                        return None;
                    }
                    let BuildStep::Custom(step) = graph.input.steps.get(*step)? else {
                        return None;
                    };
                    Some(step.target_dependencies.iter())
                })
                .flatten()
                .collect();

            for &dependency in &target.depends {
                let ordinary = !shared
                    || file_config
                    || !graph.per_config(index)
                    || !graph.per_config(dependency);
                if ordinary {
                    add(file_config, dependency);
                }
            }
            for (target, config) in command_targets {
                let file_config = config == &file_graph.input.config;
                let graph = if file_config { file_graph } else { self };
                if let Some(dependency) = graph.target_index(*target) {
                    add(file_config, dependency);
                }
            }
        }
        nodes
    }

    pub(crate) fn emit_assumed_sources(&self, ninja: &mut Ninja, model: Option<&ProjectModel>) {
        let Some(model) = model else { return };
        let mut sources = BTreeMap::<&BString, BTreeSet<&BString>>::new();
        for target in &self.input.targets {
            let Some(target_id) = model.find_target(DirId::ROOT, &target.name) else {
                continue;
            };
            let Some(owner) = model.target(target_id) else {
                continue;
            };
            for &index in &target.steps {
                let Some(BuildStep::Compile { source, .. } | BuildStep::Scan { source, .. }) =
                    self.input.steps.get(index)
                else {
                    continue;
                };
                if self.has_target(&self.node(source))
                    || !model
                        .get_prop(
                            &PropScope::Source {
                                dir: owner.dir,
                                path: source.clone(),
                            },
                            b"GENERATED",
                        )
                        .is_some_and(|value| truth::is_on(&value))
                {
                    continue;
                }
                let inputs = sources.entry(source).or_default();
                if let Some(nodes) = self.steps.get(index) {
                    for entry in &nodes.order_only {
                        if let OrderOnly::ObjectOrder(index) = entry {
                            inputs.extend(self.compile_prerequisite_paths(*index));
                        }
                    }
                }
            }
        }
        for (source, inputs) in sources {
            let relative = self.node(source);
            let outputs = if relative == *source {
                vec![relative]
            } else {
                vec![relative, source.clone()]
            };
            ninja.edge(
                b"phony",
                Edge {
                    outputs: &outputs,
                    order_only: &inputs.into_iter().collect::<Vec<_>>(),
                    ..Edge::default()
                },
            );
        }
    }
}
