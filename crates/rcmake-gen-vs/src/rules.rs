//! MSBuild custom commands, build events, and regeneration rules.

use rcmake_resolve::ide::IdeCustomStep;

use crate::{Project, Settings, join, msbuild, string, windows, xml::Writer};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rule {
    pub(crate) commands: Vec<Command>,
    pub(crate) inputs: Vec<String>,
    pub(crate) outputs: Vec<String>,
    pub(crate) message: String,
    pub(crate) always: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    pub(crate) argv: Vec<String>,
    pub(crate) dir: String,
}

impl From<&IdeCustomStep> for Rule {
    fn from(step: &IdeCustomStep) -> Self {
        Self {
            commands: step
                .commands
                .iter()
                .filter(|line| !line.is_empty())
                .map(|line| Command {
                    argv: line.iter().map(|arg| string(arg)).collect(),
                    dir: string(&step.working_dir),
                })
                .collect(),
            inputs: step.depends.iter().map(|path| string(path)).collect(),
            outputs: step
                .outputs
                .iter()
                .chain(&step.byproducts)
                .map(|path| string(path))
                .collect(),
            message: step
                .comment
                .as_ref()
                .map_or_else(String::new, |value| string(value)),
            always: step.always_run || step.outputs.is_empty(),
        }
    }
}

// Quote one argument for the Windows C run-time parser.
pub(crate) fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && !arg.chars().any(|ch| {
            ch.is_whitespace() || matches!(ch, '"' | '&' | '|' | '<' | '>' | '^' | '(' | ')')
        })
    {
        return arg.into();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => slashes += 1,
            _ => {
                let count = if ch == '"' { slashes * 2 + 1 } else { slashes };
                out.extend(std::iter::repeat_n('\\', count));
                out.push(ch);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

fn batch_arg(arg: &str) -> String {
    quote(&arg.replace('%', "%%"))
}

pub(crate) fn script(commands: &[Command]) -> String {
    let mut lines = vec!["setlocal DisableDelayedExpansion".to_owned()];
    for command in commands {
        let Some((program, args)) = command.argv.split_first() else {
            continue;
        };
        if !command.dir.is_empty() {
            lines.push(format!("cd /D {}", batch_arg(&windows(&command.dir))));
            lines.push("if errorlevel 1 exit /b %errorlevel%".into());
        }
        let program = batch_arg(&windows(program));
        let prefix = if program
            .trim_end_matches('"')
            .to_ascii_lowercase()
            .ends_with(".bat")
            || program
                .trim_end_matches('"')
                .to_ascii_lowercase()
                .ends_with(".cmd")
        {
            "call "
        } else {
            ""
        };
        let args = args.iter().map(|arg| batch_arg(arg)).collect::<Vec<_>>();
        lines.push(
            format!("{prefix}{program} {}", args.join(" "))
                .trim_end()
                .into(),
        );
        lines.push("if errorlevel 1 exit /b %errorlevel%".into());
    }
    lines.push("endlocal".into());
    msbuild(&lines.join("\r\n")).replace("%24%28Configuration%29", "$(Configuration)")
}

pub(crate) fn metadata(xml: &mut Writer, rule: &Rule, condition: &str) {
    let attrs = [("Condition", condition)];
    xml.text("Command", &attrs, &script(&rule.commands));
    if !rule.message.is_empty() {
        xml.text("Message", &attrs, &msbuild(&rule.message));
    }
    if !rule.inputs.is_empty() {
        let inputs = path_list(&rule.inputs);
        xml.text(
            "AdditionalInputs",
            &attrs,
            &format!("{inputs};%(AdditionalInputs)"),
        );
    }
    if !rule.outputs.is_empty() {
        xml.text("Outputs", &attrs, &path_list(&rule.outputs));
    }
    if rule.always {
        xml.text("BuildAlways", &attrs, "true");
        xml.text("VerifyInputsAndOutputsExist", &attrs, "false");
    }
    xml.text("LinkObjects", &attrs, "false");
}

fn path_list(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| msbuild(&windows(path)))
        .collect::<Vec<_>>()
        .join(";")
}

pub(crate) fn verify_globs(
    settings: &Settings,
    project: &Project,
    inputs: &[String],
) -> Option<Rule> {
    inputs
        .contains(&join(&settings.bin, rcmake_gen_common::glob::STAMP))
        .then(|| Rule {
            commands: vec![Command {
                argv: vec![
                    settings.cmake.clone(),
                    "-P".into(),
                    join(&settings.bin, rcmake_gen_common::glob::SCRIPT),
                ],
                dir: project.dir.clone(),
            }],
            message: "Check glob results".into(),
            ..Rule::default()
        })
}

pub(crate) fn regeneration(settings: &Settings, project: &Project, inputs: &[String]) -> Rule {
    let stamp = join(&settings.bin, "CMakeFiles/generate.stamp");
    Rule {
        commands: vec![
            Command {
                argv: vec![
                    settings.cmake.clone(),
                    "-S".into(),
                    settings.source.clone(),
                    "-B".into(),
                    settings.bin.clone(),
                    "--regenerate-during-build".into(),
                ],
                dir: project.dir.clone(),
            },
            Command {
                argv: vec![
                    settings.cmake.clone(),
                    "-E".into(),
                    "touch".into(),
                    stamp.clone(),
                ],
                dir: project.dir.clone(),
            },
        ],
        inputs: inputs.to_vec(),
        outputs: vec![stamp],
        message: "Check the build system".into(),
        always: false,
    }
}
