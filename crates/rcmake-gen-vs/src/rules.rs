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
    /// MSBuild can run this rule in parallel with others (`BuildInParallel`).
    pub(crate) parallel: bool,
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
            parallel: false,
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
    let mut out = Vec::new();
    rcmake_base::shell::windows_argument(arg.as_bytes(), &mut out, true);
    String::from_utf8_lossy(&out).into_owned()
}

fn batch_arg(arg: &str) -> String {
    quote(&arg.replace('%', "%%"))
}

/// Write the batch script of an MSBuild custom build step or build event.
///
/// `run_path` is the `CMAKE_MSVCIDE_RUN_PATH` value of the directory that
/// owns the commands. As in CMake, the script adds it to the front of `PATH`
/// inside `setlocal`, after the first change of the working directory. See
/// [`set_path`] for the batch quoting.
pub(crate) fn script(commands: &[Command], run_path: Option<&str>) -> String {
    let mut lines = vec!["setlocal DisableDelayedExpansion".to_owned()];
    let mut run_path = run_path;
    for command in commands {
        let Some((program, args)) = command.argv.split_first() else {
            continue;
        };
        if !command.dir.is_empty() {
            lines.push(format!("cd /D {}", batch_arg(&windows(&command.dir))));
            lines.push("if errorlevel 1 exit /b %errorlevel%".into());
        }
        if let Some(path) = run_path.take() {
            lines.push(set_path(path));
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

/// Write the batch line that adds `run_path` to the front of `PATH`.
///
/// CMake writes the value without change, so a `&` in a path such as
/// `C:\R&D\tools` splits the line into two commands. This line quotes the
/// whole assignment, so `&`, `|`, `<`, `>`, and `^` stay text. It doubles
/// each `%`, so `%TEMP%` stays text and is not expanded. A `"` cannot be part
/// of a Windows path, so it is removed. For example, `C:\R&D;%TEMP%` gives
/// `set "PATH=C:\R&D;%%TEMP%%;%PATH%"`.
pub(crate) fn set_path(run_path: &str) -> String {
    let path = run_path.replace('"', "").replace('%', "%%");
    format!("set \"PATH={path};%PATH%\"")
}

pub(crate) fn metadata(xml: &mut Writer, rule: &Rule, condition: &str, run_path: Option<&str>) {
    let attrs = [("Condition", condition)];
    if rule.parallel {
        xml.text("BuildInParallel", &attrs, "true");
    }
    xml.text("Command", &attrs, &script(&rule.commands, run_path));
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

/// The rule of `ZERO_CHECK`, which checks the build system before each
/// build, as CMake writes it.
///
/// The rule runs `cmake -S<source> -B<binary> --check-stamp-list
/// CMakeFiles/generate.stamp.list --vs-solution-file <solution>` in the top
/// binary directory, followed by `extra` arguments. Its inputs are all
/// listfiles, and its outputs are the generation stamps of all directories.
pub(crate) fn regeneration(
    settings: &Settings,
    project: &Project,
    inputs: &[String],
    (stamps, solution, extra): (&[String], &str, &[String]),
) -> Rule {
    let mut argv = vec![
        settings.cmake.clone(),
        format!("-S{}", settings.source),
        format!("-B{}", settings.bin),
        "--check-stamp-list".into(),
        "CMakeFiles/generate.stamp.list".into(),
        "--vs-solution-file".into(),
        solution.to_owned(),
    ];
    argv.extend(extra.iter().cloned());
    Rule {
        commands: vec![Command {
            argv,
            dir: project.dir.clone(),
        }],
        inputs: inputs.to_vec(),
        outputs: stamps.to_vec(),
        message: "Checking Build System".into(),
        always: false,
        parallel: false,
    }
}
