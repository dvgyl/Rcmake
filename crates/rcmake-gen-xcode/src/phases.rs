//! Xcode build phases and per-configuration shell scripts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::Path,
};

use rcmake_gen_common::{GenError, ide::UtilityKind};
use rcmake_model::{CustomCommandKind, ProjectModel, Target, TargetEvent, TargetKind};
use rcmake_resolve::{BuildGraph, BuildStep, ResolvedTarget, ide::IdeCustomPhase};

use crate::{
    object::{self, Isa, Objects, Reference, Value, attrs, list},
    rerun, text,
};

pub(crate) fn quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub(crate) fn files(
    objects: &mut Objects,
    isa: Isa,
    key: &str,
    name: &str,
    files: Vec<Reference>,
) -> Result<Reference, GenError> {
    objects.add(
        isa,
        key,
        name,
        attrs([
            ("buildActionMask", "2147483647".into()),
            ("files", list(files)),
            ("runOnlyForDeploymentPostprocessing", "0".into()),
        ]),
    )
}

pub(crate) fn build_file(
    objects: &mut Objects,
    key: &str,
    group: &str,
    file: &Reference,
    settings: object::Attrs,
) -> Result<Reference, GenError> {
    let mut content = attrs([("fileRef", file.clone().into())]);
    if !settings.is_empty() {
        content.insert("settings".into(), Value::Dict(settings));
    }
    objects.add(
        Isa::PBXBuildFile,
        key,
        &format!("{} in {group}", file.comment),
        content,
    )
}

#[derive(Default)]
pub(crate) struct Script {
    lines: BTreeMap<String, Vec<String>>,
    inputs: BTreeSet<String>,
    outputs: BTreeSet<String>,
    always: bool,
}

impl Script {
    fn push(&mut self, config: &str, cwd: &str, commands: impl IntoIterator<Item = String>) {
        let commands = commands.into_iter().collect::<Vec<_>>();
        if commands.is_empty() {
            return;
        }
        let lines = self.lines.entry(config.into()).or_default();
        lines.push(format!("cd {}", quote(cwd)));
        lines.extend(commands);
    }

    pub(crate) fn version_links(
        &mut self,
        config: &str,
        product: &crate::settings::Product,
    ) -> Result<(), GenError> {
        if let Some((actual, aliases)) = product.version_links() {
            crate::file_name(&actual)?;
            let mut commands = Vec::new();
            for alias in aliases {
                crate::file_name(&alias)?;
                commands.push(format!(
                    "/bin/ln -sfn {} {}",
                    quote(&actual),
                    quote(&format!("./{alias}"))
                ));
            }
            self.push(config, &product.dir, commands);
            self.always = true;
        }
        Ok(())
    }

    pub(crate) fn shell(
        self,
        objects: &mut Objects,
        key: &str,
        name: &str,
        configs: &[String],
    ) -> Result<Option<Reference>, GenError> {
        if self.lines.is_empty() {
            return Ok(None);
        }
        let mut script = String::from("set -e\n");
        for config in configs {
            if let Some(lines) = self.lines.get(config) {
                let _ = writeln!(
                    script,
                    "if test \"$CONFIGURATION\" = {}; then :",
                    quote(config)
                );
                for line in lines {
                    let _ = writeln!(script, "  {line}");
                }
                script.push_str("fi\n");
            }
        }
        let mut fields = attrs([
            ("buildActionMask", "2147483647".into()),
            ("files", list(Vec::<Reference>::new())),
            ("inputPaths", list(self.inputs)),
            ("name", name.into()),
            ("outputPaths", list(self.outputs)),
            ("runOnlyForDeploymentPostprocessing", "0".into()),
            ("shellPath", "/bin/sh".into()),
            ("shellScript", script.into()),
            ("showEnvVarsInLog", "0".into()),
        ]);
        if self.always {
            fields.insert("alwaysOutOfDate".into(), "1".into());
        }
        objects
            .add(Isa::PBXShellScriptBuildPhase, key, name, fields)
            .map(Some)
    }
}

#[derive(Default)]
pub(crate) struct Custom {
    pub(crate) rules: Script,
    pub(crate) pre: Script,
    pub(crate) link: Script,
    pub(crate) post: Script,
}

impl Custom {
    pub(crate) fn collect(
        &mut self,
        model: &ProjectModel,
        raw: &Target,
        target: &ResolvedTarget,
        graph: Option<&BuildGraph>,
        bin: &Path,
    ) {
        let config = text(&target.config.0);
        let build_dir = bin.join(text(&target.metadata.binary_dir));
        let build_dir = build_dir.to_string_lossy();
        if let Some(ide) = target.ide.as_deref() {
            for step in &ide.custom {
                let phase = match step.phase {
                    IdeCustomPhase::Rule => &mut self.rules,
                    IdeCustomPhase::PreBuild => &mut self.pre,
                    IdeCustomPhase::PreLink => &mut self.link,
                    IdeCustomPhase::PostBuild => &mut self.post,
                };
                let cwd = if step.working_dir.is_empty() {
                    build_dir.clone().into_owned()
                } else {
                    text(&step.working_dir)
                };
                phase.push(
                    &config,
                    &cwd,
                    step.commands
                        .iter()
                        .filter(|argv| !argv.is_empty())
                        .map(|argv| {
                            argv.iter()
                                .map(|arg| quote(&text(arg)))
                                .collect::<Vec<_>>()
                                .join(" ")
                        }),
                );
                phase.paths(bin, &config, &step.depends, &step.outputs, &step.byproducts);
                phase.always |= step.always_run || raw.kind == TargetKind::Utility;
            }
            return;
        }
        if let Some(graph) = graph
            && let Some(graph_target) = graph.targets.iter().find(|item| item.name == raw.name)
        {
            for index in &graph_target.steps {
                if let Some(BuildStep::Custom(step)) = graph.steps.get(*index) {
                    if target
                        .link
                        .as_ref()
                        .is_some_and(|link| step.outputs.contains(&link.output))
                    {
                        continue;
                    }
                    for invocation in &step.invocations {
                        let cwd = invocation
                            .working_dir
                            .as_ref()
                            .map_or_else(|| bin.to_path_buf(), |path| bin.join(text(path)));
                        self.rules
                            .push(&config, &cwd.to_string_lossy(), [text(&invocation.shell)]);
                    }
                    self.rules
                        .paths(bin, &config, &step.depends, &step.outputs, &step.byproducts);
                    self.rules.always |= step.outputs.is_empty() || raw.kind == TargetKind::Utility;
                }
            }
        }
        if raw.kind == TargetKind::Utility {
            return;
        }
        for command in raw
            .event_commands
            .iter()
            .filter_map(|id| model.command(*id))
        {
            let CustomCommandKind::TargetEvent { event, .. } = command.kind else {
                continue;
            };
            let phase = match event {
                TargetEvent::PreBuild => &mut self.pre,
                TargetEvent::PreLink => &mut self.link,
                TargetEvent::PostBuild => &mut self.post,
            };
            let cwd = command.working_directory.as_ref().map_or_else(
                || build_dir.to_string(),
                |dir| {
                    Path::new(build_dir.as_ref())
                        .join(text(dir))
                        .to_string_lossy()
                        .into_owned()
                },
            );
            phase.push(
                &config,
                &cwd,
                command
                    .command_lines
                    .iter()
                    .filter(|line| !line.is_empty())
                    .map(|line| {
                        line.iter()
                            .map(|arg| quote(&text(arg)))
                            .collect::<Vec<_>>()
                            .join(" ")
                    }),
            );
            phase.always = true;
        }
    }
}

impl Script {
    fn paths(
        &mut self,
        bin: &Path,
        config: &str,
        inputs: &[rcmake_base::BString],
        outputs: &[rcmake_base::BString],
        byproducts: &[rcmake_base::BString],
    ) {
        let path = |value: &rcmake_base::BString| {
            let path = bin.join(text(value));
            let mut output = String::new();
            for component in path.components() {
                let name = component.as_os_str().to_string_lossy();
                if !output.is_empty() && !output.ends_with('/') {
                    output.push('/');
                }
                output.push_str(if name == config {
                    "$(CONFIGURATION)"
                } else {
                    &name
                });
            }
            output
        };
        self.inputs.extend(inputs.iter().map(path));
        self.outputs
            .extend(outputs.iter().chain(byproducts).map(path));
    }
}

pub(crate) fn utility(
    kind: UtilityKind,
    bin: &Path,
    cwd: &Path,
    configs: &[String],
    cmake: &str,
    ctest: &str,
    cpack: &str,
) -> Script {
    let command = match kind {
        UtilityKind::AllBuild => "echo 'Build all projects'".into(),
        UtilityKind::ZeroCheck => format!(
            "/bin/sh {}",
            quote(&bin.join(rerun::SCRIPT).to_string_lossy())
        ),
        UtilityKind::Install => format!(
            "{} \"-DBUILD_TYPE=$CONFIGURATION$EFFECTIVE_PLATFORM_NAME\" -P cmake_install.cmake",
            quote(cmake)
        ),
        UtilityKind::RunTests => format!(
            "{} -C \"$CONFIGURATION$EFFECTIVE_PLATFORM_NAME\"",
            quote(ctest)
        ),
        UtilityKind::Package => format!(
            "{} -C \"$CONFIGURATION$EFFECTIVE_PLATFORM_NAME\" --config ./CPackConfig.cmake",
            quote(cpack)
        ),
        UtilityKind::PackageSource => format!(
            "{} -C \"$CONFIGURATION$EFFECTIVE_PLATFORM_NAME\" --config ./CPackSourceConfig.cmake",
            quote(cpack)
        ),
    };
    let mut script = Script {
        always: true,
        ..Script::default()
    };
    for config in configs {
        script.push(config, &cwd.to_string_lossy(), [command.clone()]);
    }
    script
}
