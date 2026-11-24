use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::{BString, truth};
use rcmake_model::{CustomCommandKind, DirId, ProjectModel, PropScope};
use rcmake_resolve::BuildStep;

use super::{Graph, OrderOnly};
use crate::syntax::{Edge, Ninja};

impl Graph<'_> {
    pub(crate) fn connect_custom_dependencies(&mut self, model: Option<&ProjectModel>) {
        let Some(model) = model.filter(|_| self.config.is_none()) else {
            return;
        };
        let mut owners = model
            .commands()
            .filter(|(_, command)| command.kind == CustomCommandKind::Output)
            .map(|(id, command)| {
                let key = format!(
                    "_RCMAKE_CUSTOM_COMMAND_{}_DEPENDS_EXPLICIT_ONLY",
                    id.index()
                );
                let explicit = model
                    .dir(command.dir)
                    .props
                    .get(key.as_bytes())
                    .is_some_and(|value| truth::is_on(value));
                (!explicit).then(Vec::<usize>::new)
            })
            .collect::<Vec<_>>();
        for (index, target) in self.input.targets.iter().enumerate() {
            for &step in &target.steps {
                if let Some(Some(owners)) = owners.get_mut(step) {
                    owners.push(index);
                }
            }
        }
        for (step, owners) in owners.into_iter().enumerate() {
            let Some(owners) = owners else { continue };
            let mut closures = owners
                .into_iter()
                .map(|target| self.dependency_closure(target));
            let Some(mut dependencies) = closures.next() else {
                continue;
            };
            for closure in closures {
                dependencies.retain(|dependency| closure.contains(dependency));
            }
            if !dependencies.is_empty()
                && matches!(self.input.steps.get(step), Some(BuildStep::Custom(_)))
                && let Some(nodes) = self.steps.get_mut(step)
            {
                nodes.order_only.push(OrderOnly::Targets(Cow::Owned(
                    dependencies.into_iter().collect(),
                )));
            }
        }
    }

    fn dependency_closure(&self, owner: usize) -> BTreeSet<usize> {
        let mut visited = BTreeSet::from([owner]);
        let mut pending = vec![owner];
        while let Some(target) = pending.pop() {
            if let Some(target) = self.input.targets.get(target) {
                pending.extend(
                    target
                        .depends
                        .iter()
                        .copied()
                        .filter(|index| visited.insert(*index)),
                );
            }
        }
        visited.remove(&owner);
        visited
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
                        if let OrderOnly::CompilePrerequisites(index) = entry {
                            inputs.extend(
                                self.compile_prerequisites
                                    .get(*index)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|&step| self.steps.get(step))
                                    .flat_map(|step| &step.outputs),
                            );
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
