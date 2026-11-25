use indexmap::IndexSet;
use rcmake_base::list;
use rcmake_genex::ConfigId;
use rcmake_model::{CommandId, DirId, GeneratorKind, ProjectModel};

use crate::{CrossConfiguration, ResolveError, lower, usage::UsageResolver};

#[derive(Default)]
pub(super) struct CrossPlan<'a> {
    commands: &'a [ConfigId],
    outputs: IndexSet<ConfigId>,
    shared_outputs: IndexSet<CommandId>,
}

impl<'a> CrossPlan<'a> {
    pub(super) fn new(model: &ProjectModel, configs: &'a [ConfigId]) -> Result<Self, ResolveError> {
        let Some(setting) = lower::definition(model, DirId::ROOT, b"CMAKE_CROSS_CONFIGS")
            .filter(|_| model.generator == Some(GeneratorKind::NinjaMultiConfig))
        else {
            return Ok(Self::default());
        };
        let values = list::split(setting, list::Empty::Drop);
        let outputs: IndexSet<_> = configs
            .iter()
            .filter(|config| setting == "all" || values.contains(&config.0))
            .cloned()
            .collect();
        if outputs.is_empty() || !model.commands().any(|_| true) {
            return Ok(Self::default());
        }
        let mut shared_outputs = IndexSet::new();
        let mut usage = UsageResolver::new(model);
        for (id, command) in model.commands() {
            let has_plain_output = command
                .outputs
                .iter()
                .chain(&command.byproducts)
                .any(|output| !output.value.windows(2).any(|part| part == b"$<"));
            if has_plain_output {
                shared_outputs.insert(id);
                continue;
            }
            let mut seen = IndexSet::new();
            for config in configs {
                let (outputs, byproducts) =
                    lower::command_outputs(model, command, config, &mut usage)?;
                let paths: IndexSet<_> = outputs.into_iter().chain(byproducts).collect();
                if paths.into_iter().any(|path| !seen.insert(path)) {
                    shared_outputs.insert(id);
                    break;
                }
            }
        }
        Ok(Self {
            commands: configs,
            outputs,
            shared_outputs,
        })
    }

    pub(super) fn commands_for(
        &self,
        output: &ConfigId,
    ) -> impl Iterator<Item = CrossConfiguration<'_>> {
        self.commands
            .iter()
            .filter(move |command| *command != output && self.outputs.contains(output))
            .map(|command| CrossConfiguration {
                command,
                shared_outputs: &self.shared_outputs,
            })
    }

    pub(super) fn native_for<'b>(&'b self, config: &'b ConfigId) -> Option<CrossConfiguration<'b>> {
        (!self.commands.is_empty()).then_some(CrossConfiguration {
            command: config,
            shared_outputs: &self.shared_outputs,
        })
    }
}
