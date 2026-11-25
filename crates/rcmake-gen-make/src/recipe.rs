use super::*;
use rcmake_resolve::Invocation;

pub(super) fn write_step(
    out: &mut Vec<u8>,
    step: &Step<'_>,
    file: &[u8],
    languages: &std::collections::HashMap<&[u8], &[u8]>,
) -> Result<(), GenError> {
    for path in std::iter::once(&step.goal)
        .chain(&step.inputs)
        .chain(&step.outputs)
        .chain(&step.depfile)
    {
        check_make_path(path)?;
    }
    if step.always {
        phony(
            out,
            &step.goal,
            step.inputs.iter().map(|value| value.as_slice()),
        );
    } else {
        rule(
            out,
            &step.goal,
            step.inputs
                .iter()
                .map(|value| value.as_slice())
                .chain([file]),
        );
    }

    let mut commands = Vec::new();
    let directories = step
        .outputs
        .iter()
        .chain(&step.depfile)
        .filter_map(|output| path::to_native(output).parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect::<BTreeSet<_>>();
    for directory in directories {
        let mut command = b"mkdir -p -- ".to_vec();
        make_recipe_text(&mut command, &quote_always(&path::from_native(&directory)))?;
        commands.push(command);
    }
    let (message, invocations) = match step.action {
        BuildStep::Compile {
            object, invocation, ..
        } => (
            rcmake_resolve::progress::compile_description(
                languages.get(object.as_slice()).copied(),
                &step.goal,
            ),
            std::slice::from_ref(invocation),
        ),
        BuildStep::Link {
            step: link,
            invocations,
            ..
        } => (
            rcmake_resolve::progress::link_description(link, &step.goal),
            invocations.as_slice(),
        ),
        BuildStep::Custom(custom) => (
            custom
                .comment
                .as_ref()
                .or(step.default_comment.as_ref())
                .map_or_else(Vec::new, |comment| comment.to_vec()),
            custom.invocations.as_slice(),
        ),
        BuildStep::DeviceLink { .. } => {
            return Err(GenError::Unimplemented("device linking for Unix Makefiles"));
        }
        // Step collection rejects module steps before recipes are written.
        BuildStep::Scan { .. } | BuildStep::Collate { .. } => (Vec::new(), [].as_slice()),
    };
    let invocations = step.revised_invocations.as_deref().unwrap_or(invocations);
    if !message.is_empty() {
        progress::report(out, &message, step.progress)?;
    }
    let custom = matches!(step.action, BuildStep::Custom(_));
    if step.jobserver_aware {
        for command in commands {
            out.push_str(b"\t$(SILENT)");
            out.push_str(&command);
            out.push(b'\n');
        }
        for invocation in invocations {
            if invocation
                .argv
                .first()
                .is_some_and(|program| program == b"$(CMAKE_COMMAND)")
            {
                out.push_str(b"\t@+");
                if let Some(directory) = &invocation.working_dir {
                    out.push_str(b"cd ");
                    make_recipe_text(out, &quote_always(directory))?;
                    out.push_str(b" && ");
                }
                out.push_str(b"$(CMAKE_COMMAND)");
                for arg in invocation.argv.iter().skip(1) {
                    out.push(b' ');
                    make_recipe_text(out, &rcmake_base::shell::quote(arg))?;
                }
                out.push(b'\n');
            } else if let Some(command) = recipe_invocation(invocation, custom)? {
                out.push_str(b"\t+$(SILENT)");
                out.push_str(&command);
                out.push(b'\n');
            }
        }
    } else {
        for invocation in invocations {
            commands.extend(recipe_invocation(invocation, custom)?);
        }
        let separate = matches!(step.action, BuildStep::Link { .. })
            || invocations
                .iter()
                .any(|invocation| invocation.link_response.is_some())
            || step.revised_invocations.is_some();
        for (index, command) in commands.iter().enumerate() {
            if separate {
                out.push_str(b"\t$(SILENT)");
                out.push_str(command);
                out.push(b'\n');
                continue;
            }
            if index == 0 {
                out.push_str(b"\t$(SILENT)");
            } else {
                out.push_str(b" && \\\n\t  ");
            }
            out.push_str(command);
        }
        if !separate && !commands.is_empty() {
            out.push(b'\n');
        }
    }

    for output in step.outputs.iter().filter(|output| **output != step.goal) {
        if step.symbolic_outputs.contains(output) {
            phony(out, output, [step.goal.as_slice()]);
            out.push_str(b"\t\n");
            continue;
        }
        if !step.always {
            // Older GNU Make needs one recipe to own all outputs.
            out.push_str(b"ifeq ($(wildcard ");
            make_word(out, output, Word::Wildcard);
            out.push_str(b"),)\n");
            rule(out, &step.goal, [FORCE]);
            out.push_str(b"endif\n");
        }
        rule(out, output, [step.goal.as_slice()]);
        // A recipe makes Make read the new file timestamp after the producer runs.
        out.push_str(b"\t$(SILENT)test -e ");
        let output = quote_always(output);
        make_recipe_text(out, &output)?;
        out.push_str(b" || test -L ");
        make_recipe_text(out, &output)?;
        out.push(b'\n');
    }
    if let Some(depfile) = &step.depfile {
        out.push_str(b"-include ");
        make_word(out, depfile, Word::Input);
        out.push(b'\n');
    }
    Ok(())
}

pub(super) fn shell_invocation(invocation: &Invocation) -> Option<Vec<u8>> {
    if invocation.shell.is_empty() && invocation.argv.is_empty() {
        return None;
    }
    let mut command = shell_command_prefix(invocation);
    if invocation.shell.is_empty() {
        for (index, arg) in invocation.argv.iter().enumerate() {
            if index != 0 {
                command.push(b' ');
            }
            command.push_str(quote_always(arg));
        }
    } else {
        command.push_str(&invocation.shell);
    }
    command.push(b')');
    Some(command)
}

fn recipe_invocation(invocation: &Invocation, custom: bool) -> Result<Option<Vec<u8>>, GenError> {
    let mut recipe = Vec::new();
    if custom
        && let Some((program, arguments)) = invocation.argv.split_first()
        && program == b"$(MAKE)"
    {
        make_recipe_text(&mut recipe, &shell_command_prefix(invocation))?;
        recipe.push_str(b"$(MAKE)");
        for argument in arguments {
            recipe.push(b' ');
            make_recipe_text(&mut recipe, &rcmake_base::shell::quote(argument))?;
        }
        recipe.push(b')');
    } else if let Some(command) = shell_invocation(invocation) {
        make_recipe_text(&mut recipe, &command)?;
    }
    Ok((!recipe.is_empty()).then_some(recipe))
}

fn shell_command_prefix(invocation: &Invocation) -> Vec<u8> {
    let mut command = b"(".to_vec();
    if let Some(directory) = &invocation.working_dir {
        command.push_str(b"CDPATH= cd ");
        let directory = if directory.starts_with(b"-") {
            quote_always(&[b"./", directory.as_slice()].concat())
        } else {
            quote_always(directory)
        };
        command.push_str(directory);
        command.push_str(b" && ");
    }
    command
}

pub(super) fn make_recipe_text(out: &mut Vec<u8>, text: &[u8]) -> Result<(), GenError> {
    for &byte in text {
        match byte {
            b'\n' | b'\r' | b'\0' => return Err(invalid_graph("invalid byte in recipe")),
            b'$' => out.push_str(b"$$"),
            _ => out.push(byte),
        }
    }
    Ok(())
}
