//! Xcode build phases and per-configuration shell scripts.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::Path,
};

use rcmake_gen_common::{GenError, ide::UtilityKind};
use rcmake_model::{CustomCommandKind, ProjectModel, TargetEvent};
use rcmake_resolve::{BuildStep, ResolvedTarget, ide::IdeCustomPhase};

use crate::{
    object::{self, Isa, Objects, Reference, Value, attrs, list},
    rerun, text,
};

pub(crate) fn quote(value: &str) -> String {
    text(&rcmake_base::shell::quote(value.as_bytes()))
}

fn quote_command(value: &str) -> String {
    let mut remainder = value;
    let mut quoted = String::new();
    while let Some(start) = remainder.find("$(") {
        let (before, placeholder) = remainder.split_at(start);
        if let Some(after) = placeholder.strip_prefix("$(CONFIGURATION)") {
            quoted.push_str(&quote(before));
            quoted.push_str(r#""${CONFIGURATION}""#);
            remainder = after;
        } else if let Some(after) = placeholder.strip_prefix("$(EFFECTIVE_PLATFORM_NAME)") {
            quoted.push_str(&quote(before));
            quoted.push_str(r#""${EFFECTIVE_PLATFORM_NAME}""#);
            remainder = after;
        } else {
            let (literal, after) = remainder.split_at(start + 2);
            quoted.push_str(&quote(literal));
            remainder = after;
        }
    }
    quoted.push_str(&quote(remainder));
    quoted
}

fn quote_legacy_argument(value: &str) -> String {
    value
        .replace("$(CONFIGURATION)", "${CONFIGURATION}")
        .replace("$(EFFECTIVE_PLATFORM_NAME)", "${EFFECTIVE_PLATFORM_NAME}")
        .replace(' ', "\\ ")
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
    inputs: ConfigPaths,
    outputs: ConfigPaths,
    depfiles: ConfigPaths,
    comment: Option<String>,
    always: bool,
}

impl Script {
    fn push(&mut self, config: &str, cwd: &str, commands: impl IntoIterator<Item = String>) {
        let commands = commands.into_iter().collect::<Vec<_>>();
        if commands.is_empty() {
            return;
        }
        let lines = self.lines.entry(config.into()).or_default();
        lines.push(format!("cd {}", quote_command(cwd)));
        lines.extend(commands);
    }

    pub(crate) fn version_links(
        &mut self,
        config: &str,
        product: &crate::settings::Product,
    ) -> Result<(), GenError> {
        let mut commands = Vec::new();
        for (alias, destination) in product.version_links() {
            crate::file_name(&destination)?;
            crate::file_name(&alias)?;
            commands.push(format!(
                "/bin/ln -sfn {} {}",
                quote(&destination),
                quote(&format!("./{alias}"))
            ));
        }
        if !commands.is_empty() {
            self.push(config, &product.dir, commands);
            self.always = true;
        }
        Ok(())
    }

    pub(crate) fn shell(
        self,
        objects: &mut Objects,
        key: &str,
        default_name: &str,
        configs: &[String],
    ) -> Result<Option<Reference>, GenError> {
        if self.lines.is_empty() {
            return Ok(None);
        }
        let name = self.comment.as_deref().unwrap_or(default_name);
        let depfiles = phase_paths(&self.depfiles, configs);
        let merged = (depfiles.len() > 1).then(|| {
            format!(
                "${{TARGET_TEMP_DIR}}/rcmake-{}.d",
                rcmake_base::hash::HashAlgo::Sha256.hex_digest(key.as_bytes())
            )
        });
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
                if let Some(merged) = &merged {
                    let _ = write!(script, "  /bin/cat");
                    for depfile in self.depfiles.get(config).into_iter().flatten() {
                        let _ = write!(script, " {}", quote_command(depfile));
                    }
                    let _ = writeln!(script, " < /dev/null > \"{merged}\"");
                }
                script.push_str("fi\n");
            }
        }
        let outputs = phase_paths(&self.outputs, configs);
        let inputs = phase_paths(&self.inputs, configs);
        let mut fields = attrs([
            ("buildActionMask", "2147483647".into()),
            ("files", list(Vec::<Reference>::new())),
            ("inputPaths", list(inputs.difference(&outputs).cloned())),
            ("name", name.into()),
            ("outputPaths", list(outputs)),
            ("runOnlyForDeploymentPostprocessing", "0".into()),
            ("shellPath", "/bin/sh".into()),
            ("shellScript", script.into()),
            ("showEnvVarsInLog", "0".into()),
        ]);
        if self.always {
            fields.insert("alwaysOutOfDate".into(), "1".into());
        }
        if let Some(depfile) = merged.or_else(|| depfiles.into_iter().next()) {
            fields.insert(
                "dependencyFile".into(),
                depfile
                    .replace("${TARGET_TEMP_DIR}", "$(TARGET_TEMP_DIR)")
                    .into(),
            );
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
    pub(crate) fn collect<'a>(
        &mut self,
        target: &ResolvedTarget,
        model: &ProjectModel,
        bin: &Path,
        steps: impl Iterator<Item = (usize, &'a BuildStep)>,
    ) {
        let config = text(&target.config.0);
        let build_dir = bin.join(text(&target.metadata.binary_dir));
        let build_dir = build_dir.to_string_lossy();
        // The graph starts with the output rules in model order.
        let output_rules: Vec<_> = model
            .commands()
            .filter(|(_, command)| command.kind == CustomCommandKind::Output)
            .map(|(_, command)| command.verbatim)
            .collect();
        let mut legacy_outputs = BTreeSet::new();
        let depfiles: BTreeMap<_, _> = steps
            .filter_map(|(index, step)| {
                let BuildStep::Custom(step) = step else {
                    return None;
                };
                let output = bin.join(text(step.outputs.first()?));
                if output_rules.get(index) == Some(&false) {
                    legacy_outputs.insert(output.clone());
                }
                let depfile = bin.join(text(step.depfile.as_ref()?));
                Some((output, depfile.to_string_lossy().into_owned()))
            })
            .collect();
        let mut legacy_events = model
            .commands()
            .filter_map(|(_, command)| {
                let phase = match command.kind {
                    CustomCommandKind::Utility(id) if id == target.id => IdeCustomPhase::Rule,
                    CustomCommandKind::TargetEvent { target: id, event } if id == target.id => {
                        match event {
                            TargetEvent::PreBuild => IdeCustomPhase::PreBuild,
                            TargetEvent::PreLink => IdeCustomPhase::PreLink,
                            TargetEvent::PostBuild => IdeCustomPhase::PostBuild,
                        }
                    }
                    _ => return None,
                };
                Some((phase, !command.verbatim))
            })
            .collect::<Vec<_>>();
        if let Some(ide) = target.ide.as_deref() {
            for step in &ide.custom {
                let legacy = if step.phase != IdeCustomPhase::Rule || step.always_run {
                    legacy_events
                        .iter()
                        .position(|(phase, _)| *phase == step.phase)
                        .is_some_and(|index| legacy_events.remove(index).1)
                } else {
                    step.outputs
                        .first()
                        .is_some_and(|output| legacy_outputs.contains(&bin.join(text(output))))
                };
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
                if phase.lines.is_empty() {
                    phase.comment = step.comment.as_ref().map(|comment| text(comment));
                }
                phase.push(
                    &config,
                    &cwd,
                    step.commands
                        .iter()
                        .filter(|argv| !argv.is_empty())
                        .map(|argv| {
                            argv.iter()
                                .enumerate()
                                .map(|(index, arg)| {
                                    let arg = text(arg);
                                    if legacy && index != 0 {
                                        quote_legacy_argument(&arg)
                                    } else {
                                        quote_command(&arg)
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        }),
                );
                phase.paths(bin, &config, &step.depends, &step.outputs, &step.byproducts);
                for output in &step.outputs {
                    if let Some(depfile) = depfiles.get(&bin.join(text(output))) {
                        phase
                            .depfiles
                            .entry(config.clone())
                            .or_default()
                            .insert(depfile.clone());
                    }
                }
                phase.always |= step.always_run && !step.commands.is_empty();
            }
        }
    }
}

type ConfigPaths = BTreeMap<String, BTreeSet<String>>;

fn phase_paths(paths: &ConfigPaths, configs: &[String]) -> BTreeSet<String> {
    let mut result: BTreeSet<_> = paths.values().flatten().cloned().collect();
    let [first, _, ..] = configs else {
        return result;
    };
    let Some(first_paths) = paths.get(first) else {
        return result;
    };
    let present =
        |config: &str, path: &str| paths.get(config).is_some_and(|paths| paths.contains(path));
    let common: BTreeSet<_> = first_paths
        .iter()
        .filter(|path| configs.iter().all(|config| present(config, path)))
        .map(String::as_str)
        .collect();
    for path in first_paths
        .iter()
        .filter(|path| !common.contains(path.as_str()))
    {
        let components: Vec<_> = path.split('/').collect();
        for (index, part) in components.iter().enumerate() {
            if part != first {
                continue;
            }
            let Some((before, [_, after @ ..])) = components.split_at_checked(index) else {
                continue;
            };
            let substitute = |config: &str| {
                before
                    .iter()
                    .copied()
                    .chain([config])
                    .chain(after.iter().copied())
                    .collect::<Vec<_>>()
                    .join("/")
            };
            let variants: Vec<_> = configs.iter().map(|config| substitute(config)).collect();
            if configs.iter().zip(&variants).all(|(config, variant)| {
                result.contains(variant)
                    && present(config, variant)
                    && !common.contains(variant.as_str())
            }) {
                for variant in variants {
                    result.remove(&variant);
                }
                result.insert(substitute("$(CONFIGURATION)"));
                break;
            }
        }
    }
    result
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
        let path =
            |value: &rcmake_base::BString| bin.join(text(value)).to_string_lossy().into_owned();
        self.inputs
            .entry(config.into())
            .or_default()
            .extend(inputs.iter().map(path));
        self.outputs
            .entry(config.into())
            .or_default()
            .extend(outputs.iter().chain(byproducts).map(path));
    }
}

pub(crate) fn utility(
    kind: UtilityKind,
    bin: &Path,
    cwd: &Path,
    configs: &[String],
    cmake: &str,
    (ctest, ctest_arguments): (&str, &[rcmake_base::BString]),
    cpack: &str,
) -> Script {
    let command = match kind {
        UtilityKind::AllBuild => "echo 'Build all projects'".into(),
        UtilityKind::ZeroCheck => format!(
            "/bin/sh {}",
            quote(&bin.join(rerun::SCRIPT).to_string_lossy())
        ),
        UtilityKind::Install => format!(
            "{} \"-DBUILD_TYPE=$CONFIGURATION\" \"-DEFFECTIVE_PLATFORM_NAME=$EFFECTIVE_PLATFORM_NAME\" -P cmake_install.cmake",
            quote(cmake)
        ),
        UtilityKind::RunTests => {
            let mut command = quote(ctest);
            for argument in ctest_arguments {
                command.push(' ');
                command.push_str(&quote(&argument.to_string()));
            }
            command.push_str(" -C \"$CONFIGURATION$EFFECTIVE_PLATFORM_NAME\"");
            command
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_arguments_use_the_command_escaping_mode() {
        assert_eq!(quote_legacy_argument(""), "");
        assert_eq!(quote_command(""), "''");
    }

    #[test]
    fn expands_xcode_variables_in_working_directory_and_argument() {
        let path = "/build dir/$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)/a";
        let expected = r#"'/build dir/'"${CONFIGURATION}"''"${EFFECTIVE_PLATFORM_NAME}"/a"#;
        let mut script = Script::default();
        let argument = quote_command(path);
        script.push("Debug", path, [format!("/bin/echo {argument}")]);
        assert_eq!(
            script.lines.get("Debug"),
            Some(&vec![
                format!("cd {expected}"),
                format!("/bin/echo {expected}")
            ])
        );
        assert_eq!(quote_command("$(echo unsafe)"), "'$(''echo unsafe)'");
    }

    #[test]
    fn substitutes_only_the_component_that_varies_by_configuration() {
        let configs = ["Debug", "Release"].map(str::to_owned);
        let paths = configs
            .iter()
            .map(|config| {
                (
                    config.clone(),
                    BTreeSet::from([
                        "/work/Release/app/Debug/gen.py".into(),
                        format!("/work/Debug/app/build/{config}/out.c"),
                    ]),
                )
            })
            .collect();
        assert_eq!(
            phase_paths(&paths, &configs),
            BTreeSet::from([
                "/work/Release/app/Debug/gen.py".into(),
                "/work/Debug/app/build/$(CONFIGURATION)/out.c".into(),
            ])
        );
    }
}
