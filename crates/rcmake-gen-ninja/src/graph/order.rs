use std::{borrow::Cow, collections::HashSet};

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_resolve::BuildStep;

use super::{Graph, OrderOnly, is_library_symlink};
use crate::generation_error;

pub(super) struct Prerequisite {
    step: usize,
    output: usize,
}

impl Graph<'_> {
    pub(super) fn connect_targets(&mut self) -> Result<(), GenError> {
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
            let has_prerequisites = !prerequisites.is_empty();
            self.compile_prerequisites.push(prerequisites);
            for &index in &target.steps {
                let Some(step) = self.input.steps.get(index) else {
                    return Err(generation_error(
                        self.build_dir,
                        "Invalid Ninja build step index.",
                    ));
                };
                if matches!(
                    step,
                    BuildStep::Compile { .. }
                        | BuildStep::Scan { .. }
                        | BuildStep::DeviceLink { .. }
                ) && let Some(nodes) = self.steps.get_mut(index)
                    && has_prerequisites
                {
                    nodes
                        .order_only
                        .push(OrderOnly::CompilePrerequisites(target_index));
                }
                // Link and utility commands must wait for target dependencies.
                if !dependencies.is_empty()
                    && self.producers.get(alias) != Some(&index)
                    && let Some(nodes) = self.steps.get_mut(index)
                    && (matches!(step, BuildStep::Link { .. })
                        || nodes.link
                        || self.input.utility_steps.contains(&index))
                {
                    nodes
                        .order_only
                        .push(OrderOnly::Targets(Cow::Borrowed(dependencies)));
                    self.order_only_alias_bytes =
                        self.order_only_alias_bytes.saturating_add(alias_bytes);
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
                        .filter(|&index| index != producer)
                        .map(OrderOnly::StepOutputs),
                );
            }
        }
        Ok(())
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
                if is_library_symlink(custom) {
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
