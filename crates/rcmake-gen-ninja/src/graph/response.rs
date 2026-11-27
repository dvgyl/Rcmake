use rcmake_base::{BString, hash::HashAlgo, shell::quote};
use rcmake_gen_common::{GenError, link_response};
use rcmake_host::{Host, path};
use rcmake_resolve::{BuildStep, Invocation};

use super::{Graph, StepNodes, plan::EdgePlan};
use crate::{generation_error, response::response_rule, write_if_changed};

pub(super) struct LinkCommand {
    command: BString,
    files: Vec<BString>,
    native_response: Option<(BString, BString)>,
}

impl LinkCommand {
    pub(super) fn bind(&self, plan: &mut EdgePlan) {
        plan.bind(b"COMMAND", self.command.clone());
        plan.implicit_inputs.extend(self.files.iter().cloned());
        if let Some((file, contents)) = &self.native_response
            && let Some(rule) = response_rule(&plan.rule)
        {
            plan.rule = std::borrow::Cow::Borrowed(rule);
            plan.bind(b"RSP_FILE", file.clone());
            plan.bind(b"RSP_ARGS", contents.clone());
        }
    }
}

impl Graph<'_> {
    pub(super) fn prepared_link(&self, nodes: &StepNodes<'_>) -> Option<&LinkCommand> {
        self.link_commands.get(nodes.outputs.first()?)
    }

    pub(super) fn prepare_link_commands(&mut self, host: &dyn Host) -> Result<(), GenError> {
        for (step_index, (step, nodes)) in self.input.steps.iter().zip(&self.steps).enumerate() {
            let Some(output) = nodes.outputs.first() else {
                continue;
            };
            let (raw, invocations, custom) = match step {
                BuildStep::Link {
                    step, invocations, ..
                } => {
                    let invocations = self
                        .exe_symlinks
                        .get(&step_index)
                        .and_then(|sym| invocations.get(..sym.split_at))
                        .unwrap_or(invocations.as_slice());
                    (&step.output, invocations, false)
                }
                BuildStep::Custom(step)
                    if step
                        .invocations
                        .iter()
                        .any(|invocation| invocation.link_response.is_some()) =>
                {
                    (output, step.invocations.as_slice(), !nodes.link)
                }
                _ => continue,
            };
            let ordinary = self.link_command_line(invocations, custom);
            let dir = self.target_dir_for_output(raw);
            if !self.use_response(dir, &ordinary) {
                continue;
            }
            let mut revised = invocations.to_vec();
            let mut files = Vec::new();
            for (index, (invocation, revised)) in invocations.iter().zip(&mut revised).enumerate() {
                if invocation
                    .link_response
                    .as_ref()
                    .is_some_and(|response| response.archive_batch)
                    && !self.force_response(dir)
                    && !self.command_exceeds_budget(
                        &self.command_line(std::slice::from_ref(invocation), custom),
                    )
                {
                    continue;
                }
                if let Some((invocation, mut inputs)) = link_response::rewrite(
                    invocation,
                    (raw, &self.input.config.0, index),
                    self.build_dir,
                    |_, _, setting| setting.unwrap_or(true),
                ) {
                    *revised = invocation;
                    files.append(&mut inputs);
                }
            }
            let mut command = self.link_command_line(&revised, custom);
            let mut implicit = Vec::new();
            let mut scripted = false;
            if self.command_exceeds_budget(&command) {
                if self.commands.windows() {
                    // Keep the existing model-free response path for hand-built graphs.
                    if files.is_empty() && self.command_settings.model.is_none() {
                        continue;
                    }
                    let command_too_long = revised.iter().any(|invocation| {
                        let command = self.command_line(std::slice::from_ref(invocation), custom);
                        String::from_utf8_lossy(&command).encode_utf16().count() >= 8180
                    });
                    if command_too_long {
                        return Err(self.link_command_too_long(raw));
                    }
                    let root = path::from_native(self.build_dir);
                    if let Some(script) =
                        self.commands
                            .custom_script(&revised, &root, !self.prefix.is_empty())
                    {
                        let digest = HashAlgo::Sha256.hex_digest(&script);
                        let name = format!("CMakeFiles/rcmake-link-{digest}.cmd");
                        write_if_changed(host, &self.build_dir.join(&name), &script)?;
                        let file = self.node(name.as_bytes());
                        let argument = [b"\"".as_slice(), &file, b"\""].concat();
                        command = self.commands.cmd(&argument);
                        implicit.push(file);
                        scripted = true;
                    } else {
                        return Err(self.link_command_too_long(raw));
                    }
                } else {
                    command = self.posix_link_script(host, raw, &revised, custom, &mut implicit)?;
                    scripted = true;
                }
            }
            let native = if !custom
                && !scripted
                && let [file] = files.as_slice()
            {
                Some((self.node(&file.path), file.contents.clone()))
            } else {
                for file in &files {
                    file.write(host, self.build_dir)?;
                    implicit.push(self.node(&file.path));
                }
                None
            };
            if !files.is_empty() || scripted {
                self.link_commands.insert(
                    output.clone(),
                    LinkCommand {
                        command: command.into(),
                        files: implicit,
                        native_response: native,
                    },
                );
            }
        }
        Ok(())
    }

    fn posix_link_script(
        &self,
        host: &dyn Host,
        output: &BString,
        invocations: &[Invocation],
        custom: bool,
        implicit: &mut Vec<BString>,
    ) -> Result<Vec<u8>, GenError> {
        let mut script = b"set -e\n".to_vec();
        for invocation in invocations {
            let command = self.command_line(std::slice::from_ref(invocation), custom);
            if self.command_exceeds_budget(&command) {
                return Err(self.link_command_too_long(output));
            }
            // Isolate explicit directory changes and shell variables to this command.
            script.extend_from_slice(b"( ");
            script.extend_from_slice(&command);
            script.extend_from_slice(b" )\n");
        }
        let digest = HashAlgo::Sha256.hex_digest(&script);
        let name = format!("CMakeFiles/rcmake-link-{digest}.sh");
        let file = self.build_dir.join(&name);
        write_if_changed(host, &file, &script)?;
        implicit.push(self.node(name.as_bytes()));
        let path = quote(&path::from_native(&file));
        Ok([
            b"sh -c 'exec sh \"$1\"' ".as_slice(),
            digest.as_bytes(),
            b" ",
            &path,
        ]
        .concat())
    }

    fn link_command_too_long(&self, output: &BString) -> GenError {
        generation_error(
            self.build_dir,
            format!(
                "The link command for {output} exceeds the shell argument limit. Check CMAKE_<LANG>_RESPONSE_FILE_LINK_FLAG and CMAKE_<LANG>_USE_RESPONSE_FILE_FOR_OBJECTS."
            ),
        )
    }
}

impl Graph<'_> {
    pub(crate) fn prepare_custom_scripts(&mut self, host: &dyn Host) -> Result<(), GenError> {
        self.prepare_link_commands(host)?;
        let build = path::from_native(self.build_dir);
        for (step, nodes) in self.input.steps.iter().zip(&self.steps) {
            let BuildStep::Custom(custom) = step else {
                continue;
            };
            let Some(output) = nodes.outputs.first() else {
                continue;
            };
            if self.link_commands.contains_key(output) {
                continue;
            }
            let Some(script) =
                self.commands
                    .custom_script(&custom.invocations, &build, !self.prefix.is_empty())
            else {
                continue;
            };
            let digest = rcmake_base::hash::HashAlgo::Sha256.hex_digest(&script);
            let name = format!("CMakeFiles/rcmake-custom-{digest}.cmd");
            crate::write_if_changed(host, &self.build_dir.join(&name), &script)?;
            self.custom_scripts
                .insert(output.clone(), self.node(name.as_bytes()));
        }
        Ok(())
    }
}
