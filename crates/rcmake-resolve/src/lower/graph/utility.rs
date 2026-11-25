use rcmake_base::BString;
use rcmake_model::{GeneratorKind, TargetEvent};

use crate::{BuildGraph, BuildStep, CustomStep, ResolveError, usage::UsageResolver};

use super::{Commands, TargetPlan, command_path, push};
use crate::lower::custom::remove_own_outputs;

impl TargetPlan<'_> {
    pub(super) fn add_utility_commands(
        &mut self,
        commands: &Commands<'_>,
        usage: &mut UsageResolver<'_>,
        graph: &mut BuildGraph,
    ) -> Result<(), ResolveError> {
        let multi = self.model.generator == Some(GeneratorKind::NinjaMultiConfig);
        let events @ [pre_build, post_build] =
            [Some(TargetEvent::PreBuild), Some(TargetEvent::PostBuild)];
        let both = [None, pre_build, post_build];
        let mut utility =
            self.utility_recipe(commands, usage, if multi { &[None] } else { &both })?;
        if multi {
            let mut after = self.utility_recipe(commands, usage, &events)?;
            if after != CustomStep::default() {
                after.depends.extend(utility.outputs.iter().cloned());
                if after.comment.is_none() {
                    after.comment =
                        Some(format!("Running utility command for {}", self.target.name).into());
                }
                let path: BString = [
                    b"CMakeFiles/".as_slice(),
                    &self.config.0,
                    b"/",
                    &self.target.name,
                    b".util",
                ]
                .concat()
                .into();
                let dir = &self.model.dir(self.target.dir).binary_dir;
                after
                    .outputs
                    .insert(0, command_path(self.model, dir, &path));
                self.add_utility_recipe(commands, graph, std::mem::take(&mut utility));
                utility = after;
            }
        }
        self.add_utility_recipe(commands, graph, utility);
        Ok(())
    }

    fn utility_recipe(
        &mut self,
        commands: &Commands<'_>,
        usage: &mut UsageResolver<'_>,
        events: &[Option<TargetEvent>],
    ) -> Result<CustomStep, ResolveError> {
        commands.events(
            self.resolved.id,
            events,
            usage,
            &mut self.steps,
            &mut self.dependencies,
            self.ide_steps.as_mut(),
        )
    }

    fn add_utility_recipe(
        &mut self,
        commands: &Commands<'_>,
        graph: &mut BuildGraph,
        mut recipe: CustomStep,
    ) {
        if recipe == CustomStep::default() {
            return;
        }
        remove_own_outputs(&mut recipe);
        recipe
            .depends
            .retain(|dep| dep != &self.target.name || commands.producer(dep).is_some());
        self.dependencies.shift_remove(&self.resolved.id);
        let index = push(&mut graph.steps, BuildStep::Custom(recipe));
        self.steps.insert(index);
        graph.utility_steps.insert(index);
    }
}
