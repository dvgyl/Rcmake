use super::*;
use rcmake_resolve::{Invocation, lower::custom_shell_operator};

pub(super) fn write_step(
    dialect: MakeDialect,
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
        check_make_path_for(dialect, path)?;
    }
    if step.always {
        phony_for(
            dialect,
            out,
            &step.goal,
            step.inputs.iter().map(|value| value.as_slice()),
        );
    } else {
        rule_for(
            dialect,
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
        let mut command = if dialect.is_nmake() {
            b"$(CMAKE_COMMAND) -E make_directory ".to_vec()
        } else {
            b"mkdir -p -- ".to_vec()
        };
        make_recipe_text_for(
            dialect,
            &mut command,
            &quote_arg(dialect, &path::from_native_cmake(&directory)),
        )?;
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
            return Err(GenError::Unimplemented(if dialect.is_nmake() {
                "device linking for NMake Makefiles"
            } else {
                "device linking for Unix Makefiles"
            }));
        }
        // Step collection rejects module steps before recipes are written.
        BuildStep::Scan { .. } | BuildStep::Collate { .. } => (Vec::new(), [].as_slice()),
    };
    let invocations = step.revised_invocations.as_deref().unwrap_or(invocations);
    if !message.is_empty() {
        progress::report(dialect, out, &message, step.progress)?;
    }
    let custom = matches!(step.action, BuildStep::Custom(_));
    if step.jobserver_aware && !dialect.is_nmake() {
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
                write_custom_arguments(dialect, out, invocation)?;
                out.push(b'\n');
            } else if let Some(command) = recipe_invocation(dialect, invocation, custom)? {
                out.push_str(b"\t+$(SILENT)");
                out.push_str(&command);
                out.push(b'\n');
            }
        }
    } else {
        for invocation in invocations {
            commands.extend(recipe_invocation(dialect, invocation, custom)?);
        }
        let separate = dialect.is_nmake()
            || matches!(step.action, BuildStep::Link { .. })
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
            phony_for(dialect, out, output, [step.goal.as_slice()]);
            if !dialect.is_nmake() {
                out.push_str(b"\t\n");
            }
            continue;
        }
        if !step.always {
            if dialect.is_nmake() {
                out.push_str(b"!IF !EXIST(\"");
                make_recipe_text(out, output)?;
                out.push_str(b"\")\n");
                rule_for(dialect, out, &step.goal, [FORCE]);
                out.push_str(b"!ENDIF\n");
            } else {
                // Older GNU Make needs one recipe to own all outputs.
                out.push_str(b"ifeq ($(wildcard ");
                make_word_for(dialect, out, output, Word::Wildcard);
                out.push_str(b"),)\n");
                rule_for(dialect, out, &step.goal, [FORCE]);
                out.push_str(b"endif\n");
            }
        }
        rule_for(dialect, out, output, [step.goal.as_slice()]);
        if dialect.is_nmake() {
            out.push_str(b"\t$(SILENT)if not exist ");
            make_recipe_text_for(dialect, out, &quote_arg(dialect, output))?;
            out.push_str(b" exit /b 1\n");
        } else {
            // A recipe makes Make read the new file timestamp after the producer runs.
            out.push_str(b"\t$(SILENT)test -e ");
            let output = quote_always(output);
            make_recipe_text_for(dialect, out, &output)?;
            out.push_str(b" || test -L ");
            make_recipe_text_for(dialect, out, &output)?;
            out.push(b'\n');
        }
    }
    if let Some(depfile) = &step.depfile {
        if dialect.is_nmake() {
            out.push_str(b"!IF EXIST(\"");
            make_recipe_text(out, depfile)?;
            out.push_str(b"\")\n!INCLUDE \"");
            make_recipe_text(out, depfile)?;
            out.push_str(b"\"\n!ENDIF\n");
        } else {
            out.push_str(b"-include ");
            make_word_for(dialect, out, depfile, Word::Input);
            out.push(b'\n');
        }
    }
    Ok(())
}

pub(super) fn shell_invocation_for(
    dialect: MakeDialect,
    invocation: &Invocation,
) -> Option<Vec<u8>> {
    if invocation.shell.is_empty() && invocation.argv.is_empty() {
        return None;
    }
    if dialect.is_nmake() {
        let mut command = Vec::new();
        if let Some(directory) = &invocation.working_dir {
            command.push_str(b"cd /d ");
            command.push_str(quote_cmd(directory));
            command.push_str(b" && ");
        }
        if invocation.shell.is_empty() {
            for (index, arg) in invocation.argv.iter().enumerate() {
                if index != 0 {
                    command.push(b' ');
                }
                command.push_str(quote_cmd(arg));
            }
        } else {
            command.push_str(&invocation.shell);
        }
        return Some(command);
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

fn recipe_invocation(
    dialect: MakeDialect,
    invocation: &Invocation,
    custom: bool,
) -> Result<Option<Vec<u8>>, GenError> {
    let mut recipe = Vec::new();
    if custom
        && invocation
            .argv
            .first()
            .is_some_and(|program| program == b"$(MAKE)")
    {
        if dialect.is_nmake() {
            if let Some(directory) = &invocation.working_dir {
                recipe.push_str(b"cd /d ");
                make_recipe_text_for(dialect, &mut recipe, &quote_cmd(directory))?;
                recipe.push_str(b" && ");
            }
            recipe.push_str(b"$(MAKE)");
            write_custom_arguments(dialect, &mut recipe, invocation)?;
        } else {
            make_recipe_text_for(dialect, &mut recipe, &shell_command_prefix(invocation))?;
            recipe.push_str(b"$(MAKE)");
            write_custom_arguments(dialect, &mut recipe, invocation)?;
            recipe.push(b')');
        }
    } else if let Some(command) = shell_invocation_for(dialect, invocation) {
        make_recipe_text_for(dialect, &mut recipe, &command)?;
    }
    Ok((!recipe.is_empty()).then_some(recipe))
}

fn write_custom_arguments(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    invocation: &Invocation,
) -> Result<(), GenError> {
    if let Some((program, arguments)) = invocation.argv.split_first() {
        if !dialect.is_nmake() {
            let word = rcmake_base::shell::quote(program);
            if let Some(shell) = invocation.shell.strip_prefix(word.as_slice()) {
                return make_recipe_text_for(dialect, out, shell);
            }
        }
        for argument in arguments {
            out.push(b' ');
            if custom_shell_operator(argument) {
                make_recipe_text_for(dialect, out, argument)?;
            } else if dialect.is_nmake() {
                make_recipe_text_for(dialect, out, &quote_cmd(argument))?;
            } else {
                make_recipe_text_for(dialect, out, &rcmake_base::shell::quote(argument))?;
            }
        }
    }
    Ok(())
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
    make_recipe_text_for(MakeDialect::Unix, out, text)
}

pub(super) fn make_recipe_text_for(
    dialect: MakeDialect,
    out: &mut Vec<u8>,
    text: &[u8],
) -> Result<(), GenError> {
    for &byte in text {
        match byte {
            b'\n' | b'\r' | b'\0' => return Err(invalid_graph("invalid byte in recipe")),
            b'$' => out.push_str(b"$$"),
            b'%' if dialect.is_nmake() => out.push_str(b"%%"),
            _ => out.push(byte),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_recipes_keep_shell_operators_and_quote_other_arguments() -> Result<(), GenError> {
        let invocation = Invocation {
            argv: [
                "$(MAKE)",
                "value with spaces",
                "a; b",
                ">",
                "out",
                ">>",
                "log",
                "|",
                "cat",
                "&&",
                "echo",
                "2>&1",
                "$HOME",
            ]
            .map(BString::from)
            .to_vec(),
            shell: "echo '>'".into(),
            working_dir: None,
            link_response: None,
        };
        assert_eq!(
            recipe_invocation(MakeDialect::Unix, &invocation, true)?,
            Some(
                b"($(MAKE) 'value with spaces' 'a; b' > out >> log | cat && echo 2>&1 '$$HOME')"
                    .to_vec()
            ),
        );
        assert_eq!(
            recipe_invocation(MakeDialect::NMake, &invocation, true)?,
            Some(
                b"$(MAKE) \"value with spaces\" \"a; b\" > \"out\" >> \"log\" | \"cat\" && \"echo\" 2>&1 \"$$HOME\""
                    .to_vec()
            ),
        );
        Ok(())
    }

    #[test]
    fn nmake_recipes_escape_percent_signs_without_changing_unix() -> Result<(), GenError> {
        let mut unix = Vec::new();
        make_recipe_text_for(MakeDialect::Unix, &mut unix, b"echo 100% $PATH")?;
        assert_eq!(unix, b"echo 100% $$PATH");

        let mut nmake = Vec::new();
        make_recipe_text_for(MakeDialect::NMake, &mut nmake, b"echo 100% $PATH")?;
        assert_eq!(nmake, b"echo 100%% $$PATH");
        Ok(())
    }
}
