use rcmake_base::BString;
use rcmake_model::{GeneratorKind, TargetEvent};

use crate::{BuildStep, CustomStep, ResolveError, usage::UsageResolver};

use super::{Commands, GraphSink, TargetPlan, command_path};
use crate::lower::custom::remove_own_outputs;

impl TargetPlan<'_> {
    pub(super) fn add_utility_commands(
        &mut self,
        commands: &Commands<'_>,
        usage: &mut UsageResolver<'_>,
        sink: &mut GraphSink<'_>,
    ) -> Result<(), ResolveError> {
        let ninja = self.model.generator.is_some_and(GeneratorKind::is_ninja);
        let multi = self.model.generator == Some(GeneratorKind::NinjaMultiConfig);
        let events @ [pre_build, post_build] =
            [Some(TargetEvent::PreBuild), Some(TargetEvent::PostBuild)];
        let both = [None, pre_build, post_build];
        let mut utility =
            self.utility_recipe(commands, usage, if ninja { &[None] } else { &both })?;
        if ninja {
            let mut after = self.utility_recipe(commands, usage, &events)?;
            if after != CustomStep::default() {
                after.depends.extend(utility.outputs.iter().cloned());
                after.depends.extend(utility.byproducts.iter().cloned());
                if after.comment.is_none() {
                    after.comment =
                        Some(format!("Running utility command for {}", self.target.name).into());
                }
                let path: BString = if multi {
                    [
                        b"CMakeFiles/".as_slice(),
                        &self.config.0,
                        b"/",
                        &self.target.name,
                        b".util",
                    ]
                    .concat()
                    .into()
                } else {
                    [b"CMakeFiles/".as_slice(), &self.target.name, b".util"]
                        .concat()
                        .into()
                };
                let dir = &self.model.dir(self.target.dir).binary_dir;
                after
                    .outputs
                    .insert(0, command_path(self.model, dir, &path));
                self.add_utility_recipe(commands, sink, std::mem::take(&mut utility));
                utility = after;
            }
        }
        self.add_utility_recipe(commands, sink, utility);
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
            (
                &mut self.steps,
                &mut self.dependencies,
                &mut self.trace_roots,
            ),
            self.ide_steps.as_mut(),
        )
    }

    fn add_utility_recipe(
        &mut self,
        commands: &Commands<'_>,
        sink: &mut GraphSink<'_>,
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
        let index = sink.push(BuildStep::Custom(recipe));
        self.steps.insert(index);
        sink.utility_steps.insert(index);
    }
}
