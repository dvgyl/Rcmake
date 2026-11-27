use std::{borrow::Cow, collections::HashSet};

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_model::TargetKind;
use rcmake_resolve::BuildStep;

use super::{Graph, OrderOnly, is_executable_symlink, is_library_symlink};
use crate::generation_error;

pub(super) struct Prerequisite {
    step: usize,
    output: usize,
}

impl Graph<'_> {
    pub(super) fn connect_targets(&mut self) -> Result<(), GenError> {
        // First pass: assign `cmake_object_order_depends_target_<name>[_<CONFIG>]`
        // for every target that compiles/scans/device-links sources.
        for (target_index, target) in self.input.targets.iter().enumerate() {
            let has_compile = target.steps.iter().any(|&index| {
                self.input.swift_actions.contains_key(&index)
                    || matches!(
                        self.input.steps.get(index),
                        Some(
                            BuildStep::Compile { .. }
                                | BuildStep::Scan { .. }
                                | BuildStep::DeviceLink { .. }
                        )
                    )
            });
            let is_normal = self.target_meta.get(target_index).is_some_and(|meta| {
                matches!(
                    meta.kind,
                    TargetKind::Executable
                        | TargetKind::SharedLibrary
                        | TargetKind::StaticLibrary
                        | TargetKind::ModuleLibrary
                        | TargetKind::ObjectLibrary
                )
            });
            if is_normal || has_compile {
                let mut name = b"cmake_object_order_depends_target_".to_vec();
                name.extend_from_slice(&target.name);
                if let Some(config) = self.config {
                    name.push(b'_');
                    name.extend_from_slice(&config.to_ascii_uppercase());
                }
                let node = self.node(&name);
                if let Some(meta) = self.target_meta.get_mut(target_index) {
                    meta.order_depends_target = Some(node);
                }
            }
        }

        for (target_index, (target, alias)) in
            self.input.targets.iter().zip(&self.aliases).enumerate()
        {
            let dependencies = target.depends.as_slice();
            let alias_bytes = dependencies
                .iter()
                .try_fold(0usize, |bytes, &index| {
                    self.aliases
                        .get(index)
                        .map(|name| bytes.saturating_add(name.len()).saturating_add(1))
                })
                .ok_or_else(|| {
                    generation_error(self.build_dir, "Invalid Ninja target dependency index.")
                })?;
            let prerequisites = self.custom_prerequisites(target_index);
            self.compile_prerequisites.push(prerequisites);

            if let Some(order_target) = self
                .target_meta
                .get(target_index)
                .and_then(|meta| meta.order_depends_target.clone())
            {
                let mut order_inputs: Vec<BString> = Vec::new();
                for &dep_index in dependencies {
                    let Some(dep_meta) = self.target_meta.get(dep_index) else {
                        continue;
                    };
                    if matches!(
                        dep_meta.kind,
                        TargetKind::SharedLibrary
                            | TargetKind::StaticLibrary
                            | TargetKind::ModuleLibrary
                            | TargetKind::ObjectLibrary
                    ) {
                        if let Some(dep_order) = &dep_meta.order_depends_target {
                            order_inputs.push(dep_order.clone());
                        } else {
                            order_inputs.extend(dep_meta.artifact_outputs.iter().cloned());
                        }
                    } else {
                        order_inputs.extend(dep_meta.artifact_outputs.iter().cloned());
                    }
                }
                let (public_outs, private_outs) = self.own_custom_outputs(target_index);
                order_inputs.extend(public_outs);
                order_inputs.sort();
                order_inputs.dedup();
                if order_inputs.is_empty() {
                    let dot = if self.prefix.is_empty() {
                        BString::from(".")
                    } else {
                        BString::from([self.prefix, b"/."].concat())
                    };
                    order_inputs.push(dot);
                }
                let private = if !private_outs.is_empty() {
                    let mut private_name = order_target.to_vec();
                    private_name.extend_from_slice(b"_private");
                    let mut private_inputs = Vec::with_capacity(1 + private_outs.len());
                    private_inputs.push(order_target);
                    private_inputs.extend(private_outs);
                    private_inputs.sort();
                    private_inputs.dedup();
                    Some((BString::from(private_name), private_inputs))
                } else {
                    None
                };
                if let Some(meta) = self.target_meta.get_mut(target_index) {
                    meta.order_depends_inputs = order_inputs;
                    if let Some((private_name, private_inputs)) = private {
                        meta.order_depends_private = Some(private_name);
                        meta.order_depends_private_inputs = private_inputs;
                    }
                }
            }

            let has_order_target = self
                .target_meta
                .get(target_index)
                .is_some_and(|meta| meta.order_depends_target.is_some());
            for &index in &target.steps {
                let Some(step) = self.input.steps.get(index) else {
                    return Err(generation_error(
                        self.build_dir,
                        "Invalid Ninja build step index.",
                    ));
                };
                if (self.input.swift_actions.contains_key(&index)
                    || matches!(
                        step,
                        BuildStep::Compile { .. }
                            | BuildStep::Scan { .. }
                            | BuildStep::DeviceLink { .. }
                    ))
                    && let Some(nodes) = self.steps.get_mut(index)
                    && has_order_target
                {
                    nodes.order_only.push(OrderOnly::ObjectOrder(target_index));
                }
                // Link and utility commands must wait for target dependencies.
                // Utility commands (custom targets with COMMAND) inherit the
                // transitive target dependency closure like cmLocalNinjaGenerator::
                // WriteCustomCommandBuildStatement -> AppendTargetDependsClosure.
                if !dependencies.is_empty()
                    && self.producers.get(alias) != Some(&index)
                    && let Some(nodes) = self.steps.get_mut(index)
                {
                    if self.input.utility_steps.contains(&index) {
                        nodes
                            .order_only
                            .push(OrderOnly::TargetClosure(Cow::Borrowed(dependencies)));
                        self.order_only_alias_bytes =
                            self.order_only_alias_bytes.saturating_add(alias_bytes);
                    } else if matches!(step, BuildStep::Link { .. }) || nodes.link {
                        nodes
                            .order_only
                            .push(OrderOnly::Targets(Cow::Borrowed(dependencies)));
                        self.order_only_alias_bytes =
                            self.order_only_alias_bytes.saturating_add(alias_bytes);
                    }
                }
            }
            if let Some(&producer) = self.producers.get(alias)
                && let Some(nodes) = self.steps.get_mut(producer)
            {
                // A real output can also be the target name, such as a root executable.
                if !dependencies.is_empty() {
                    nodes
                        .order_only
                        .push(OrderOnly::Targets(Cow::Borrowed(dependencies)));
                    self.order_only_alias_bytes =
                        self.order_only_alias_bytes.saturating_add(alias_bytes);
                }
                nodes.order_only.extend(
                    target
                        .steps
                        .iter()
                        .copied()
                        .filter(|&index| {
                            index != producer
                                && !matches!(
                                    self.input.steps.get(index),
                                    Some(
                                        BuildStep::Compile { .. }
                                            | BuildStep::Scan { .. }
                                            | BuildStep::Collate { .. }
                                    )
                                )
                        })
                        .map(OrderOnly::StepOutputs),
                );
            }
        }
        Ok(())
    }

    fn own_custom_outputs(&self, target_index: usize) -> (Vec<BString>, Vec<BString>) {
        let mut public = Vec::new();
        let mut private = Vec::new();
        let Some(target) = self.input.targets.get(target_index) else {
            return (public, private);
        };
        let file_sets = self.input.file_set_order.get(&target_index);
        for &step in &target.steps {
            if self.input.utility_steps.contains(&step)
                || self.input.swift_actions.contains_key(&step)
                || !self.step_belongs_to_target_dir(target_index, step)
            {
                continue;
            }
            let Some(BuildStep::Custom(custom)) = self.input.steps.get(step) else {
                continue;
            };
            let Some(nodes) = self.steps.get(step).filter(|nodes| !nodes.link) else {
                continue;
            };
            if is_library_symlink(custom) || is_executable_symlink(custom) {
                continue;
            }
            if let Some(file_sets) = file_sets {
                for name in custom.outputs.iter().chain(&custom.byproducts) {
                    if file_sets.independent.contains(name) {
                        continue;
                    }
                    let node = self.node(name);
                    if file_sets.public.contains(name) {
                        public.push(node);
                    } else {
                        private.push(node);
                    }
                }
            } else {
                public.extend(nodes.outputs.iter().chain(&nodes.byproducts).cloned());
            }
        }
        (public, private)
    }

    pub(super) fn compile_prerequisite_paths(
        &self,
        target: usize,
    ) -> impl Iterator<Item = &BString> {
        self.compile_prerequisites
            .get(target)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let step = self.steps.get(entry.step)?;
                if let Some(byproduct) = entry.output.checked_sub(step.outputs.len()) {
                    step.byproducts.get(byproduct)
                } else {
                    step.outputs.get(entry.output)
                }
            })
    }

    fn custom_prerequisites(&self, owner: usize) -> Vec<Prerequisite> {
        let mut pending = vec![owner];
        let mut visited = HashSet::new();
        let mut seen = HashSet::new();
        let mut files = Vec::new();
        while let Some(index) = pending.pop() {
            if !visited.insert(index) {
                continue;
            }
            let Some(target) = self.input.targets.get(index) else {
                continue;
            };
            let file_sets = self.input.file_set_order.get(&index);
            for &step in &target.steps {
                let Some(BuildStep::Custom(custom)) = self.input.steps.get(step) else {
                    continue;
                };
                let Some(nodes) = self.steps.get(step).filter(|nodes| !nodes.link) else {
                    continue;
                };
                if is_library_symlink(custom) || is_executable_symlink(custom) {
                    continue;
                }
                if let Some(file_sets) = file_sets {
                    let outputs = custom.outputs.iter().zip(&nodes.outputs).enumerate();
                    let byproducts = custom
                        .byproducts
                        .iter()
                        .zip(&nodes.byproducts)
                        .enumerate()
                        .map(|(index, paths)| (nodes.outputs.len() + index, paths));
                    for (output, (name, node)) in outputs.chain(byproducts) {
                        if !file_sets.independent.contains(name)
                            && (index == owner || file_sets.public.contains(name))
                            && seen.insert(node)
                        {
                            files.push(Prerequisite { step, output });
                        }
                    }
                } else {
                    files.extend(
                        nodes
                            .outputs
                            .iter()
                            .enumerate()
                            .filter(|(_, node)| seen.insert(node))
                            .map(|(output, _)| Prerequisite { step, output }),
                    );
                }
            }
            pending.extend(target.depends.iter().rev().copied());
        }
        files
    }
}
