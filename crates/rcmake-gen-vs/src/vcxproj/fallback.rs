//! Project settings when the resolver has no IDE data.

use rcmake_model::{CustomCommand, CustomCommandKind, TargetEvent};
use rcmake_resolve::Invocation;

use super::*;

impl Context<'_> {
    pub(super) fn fallback(
        &self,
        plan: &mut Plan,
        config: &mut Config,
        fact: Option<&ResolvedTarget>,
        steps: &[&BuildStep],
        link: Option<&LinkStep>,
        name: &str,
    ) {
        let Some(target) = self.project.target.and_then(|id| self.model.target(id)) else {
            return;
        };
        let mut language = "CXX".to_owned();
        if let Some(fact) = fact.filter(|fact| !fact.compile_groups.is_empty()) {
            let group_settings = |group: &rcmake_resolve::CompileGroup| {
                let language = string(&group.language);
                let key = format!("CMAKE_{language}_FLAGS");
                let specific = format!("{key}_{}", name.to_ascii_uppercase());
                let flags = [key, specific]
                    .into_iter()
                    .filter_map(|key| self.model.definition(target.dir, key.as_bytes()))
                    .flat_map(|value| flags_table::split(&string(value)))
                    .chain(
                        group
                            .flags
                            .iter()
                            .flat_map(|flag| ide_flags(&string(&flag.value))),
                    );
                let mut settings = flags_table::compile(flags);
                for define in &group.defines {
                    settings.list(
                        "PreprocessorDefinitions",
                        msbuild_value(&string(&define.value)),
                    );
                }
                for include in &group.includes {
                    settings.list(
                        "AdditionalIncludeDirectories",
                        msbuild_value(&windows(&string(&include.path.value))),
                    );
                }
                settings
            };
            if let Some(group) = fact.compile_groups.first() {
                language = string(&group.language);
                if fact.compile_groups.len() == 1 {
                    config.compile = group_settings(group);
                }
            }
            for unit in &fact.compile_units {
                let path = self.source_path(&string(&unit.source.value));
                let kind = source_kind(&path, Kind::Compile);
                let item = plan.source(path.clone(), kind, "", name);
                if kind != Kind::Compile {
                    continue;
                }
                item.object = self.object_path(&path, name, Some(fact));
                if let Some(group) = fact.compile_groups.get(unit.group) {
                    item.language = string(&group.language);
                    if fact.compile_groups.len() > 1 {
                        item.compile = Some(group_settings(group));
                    }
                }
            }
        } else {
            let mut per_source_settings = false;
            for step in steps {
                if let BuildStep::Compile {
                    object,
                    source,
                    invocation,
                    ..
                } = step
                {
                    let path = self.source_path(&string(source));
                    let kind = source_kind(&path, Kind::Compile);
                    let item = plan.source(path.clone(), kind, "", name);
                    if kind != Kind::Compile {
                        continue;
                    }
                    let object = string(object);
                    item.object = if object.to_ascii_lowercase().ends_with(".obj") {
                        absolute(&self.settings.bin, &object)
                    } else {
                        self.object_path(&path, name, fact)
                    };
                    item.language = source_language(&path).into();
                    item.compile = invocation_settings(invocation, source, object.as_bytes())
                        .or_else(|| {
                            Some(self.compile_settings(Some(target), name, &item.language, None))
                        });
                    per_source_settings = true;
                    language.clone_from(&item.language);
                }
            }
            if let Some(link) = link {
                language = string(&link.language);
            }
            if !per_source_settings {
                config.compile = self.compile_settings(Some(target), name, &language, None);
            }
        }
        let has_compile_facts = fact.is_some_and(|fact| !fact.compile_units.is_empty())
            || steps
                .iter()
                .any(|step| matches!(step, BuildStep::Compile { .. }));
        let dir = self.model.dir(target.dir);
        for entry in &target.sources {
            for source in list::split(&entry.value, list::Empty::Drop) {
                if source.windows(2).any(|part| part == b"$<") {
                    continue;
                }
                let path = self.source_path(&string(&source));
                let props = dir.source_props.get(path.as_bytes());
                let kind = if target.kind == TargetKind::InterfaceLibrary {
                    Kind::Other
                } else if props
                    .and_then(|props| props.get(b"HEADER_FILE_ONLY"))
                    .is_some_and(|value| truth::is_on(value))
                {
                    Kind::Header
                } else {
                    source_kind(&path, Kind::Other)
                };
                if kind != Kind::Compile || !has_compile_facts {
                    let item = plan.source(path.clone(), kind, "", name);
                    if kind == Kind::Compile {
                        item.object = self.object_path(&path, name, fact);
                        item.language = source_language(&path).into();
                        if item.language != language {
                            item.compile = Some(self.compile_settings(
                                Some(target),
                                name,
                                &item.language,
                                None,
                            ));
                        }
                    }
                }
            }
        }
        for set in target
            .file_sets
            .iter()
            .filter_map(|&id| self.model.file_set(id))
        {
            if set.kind == rcmake_model::FileSetKind::Headers {
                for source in &set.files {
                    for path in list::split(&source.value, list::Empty::Drop) {
                        if !path.windows(2).any(|part| part == b"$<") {
                            let kind = if target.kind == TargetKind::InterfaceLibrary {
                                Kind::Other
                            } else {
                                Kind::Header
                            };
                            plan.source(self.source_path(&string(&path)), kind, "", name);
                        }
                    }
                }
            }
        }

        config.link = self.fallback_link(target, link, name);
        let mut produced = Vec::new();
        for (index, step) in steps.iter().enumerate() {
            if let BuildStep::Custom(step) = step {
                produced.extend(
                    step.outputs
                        .iter()
                        .map(|path| absolute(&self.settings.bin, &string(path))),
                );
                let mut rule = invocation_rule(step);
                for output in &mut rule.outputs {
                    *output = absolute(&self.settings.bin, output);
                }
                self.custom(plan, name, index, None, rule);
            }
        }
        for (index, command) in self.model.commands() {
            match command.kind {
                CustomCommandKind::TargetEvent { target: id, event }
                    if Some(id) == self.project.target =>
                {
                    let rule = self.model_rule(command, name);
                    let event = match event {
                        TargetEvent::PreBuild => 0,
                        TargetEvent::PreLink => 1,
                        TargetEvent::PostBuild => 2,
                    };
                    if !rule.commands.is_empty()
                        && let Some(events) = config.events.get_mut(event)
                    {
                        events.push(rule);
                    }
                }
                CustomCommandKind::Utility(id)
                    if Some(id) == self.project.target
                        && produced.is_empty()
                        && !steps
                            .iter()
                            .any(|step| matches!(step, BuildStep::Custom(_))) =>
                {
                    self.custom(
                        plan,
                        name,
                        steps.len() + index.index(),
                        None,
                        self.model_rule(command, name),
                    );
                }
                CustomCommandKind::Output if command.dir == target.dir => {
                    let rule = self.model_rule(command, name);
                    if rule.outputs.iter().any(|path| {
                        plan.items
                            .values()
                            .any(|item| item.path == *path && item.configs.contains_key(name))
                    }) && !rule.outputs.iter().any(|path| produced.contains(path))
                    {
                        let main = command
                            .main_dependency
                            .as_ref()
                            .map(|path| self.source_path(&string(path)));
                        let main = main.filter(|path| {
                            !plan.items.contains_key(&(Kind::Compile, path.clone()))
                        });
                        self.custom(plan, name, steps.len() + index.index(), main, rule);
                    }
                }
                _ => {}
            }
        }
    }

    fn fallback_link(
        &self,
        target: &Target,
        step: Option<&LinkStep>,
        config: &str,
    ) -> flags_table::Settings {
        let archive = target.kind == TargetKind::StaticLibrary;
        let class = match target.kind {
            TargetKind::Executable => "EXE",
            TargetKind::SharedLibrary => "SHARED",
            TargetKind::ModuleLibrary => "MODULE",
            TargetKind::StaticLibrary => "STATIC",
            _ => return flags_table::Settings::default(),
        };
        let key = format!("CMAKE_{class}_LINKER_FLAGS");
        let config_key = format!("{key}_{}", config.to_ascii_uppercase());
        let target_key = if archive {
            "STATIC_LIBRARY_FLAGS"
        } else {
            "LINK_FLAGS"
        };
        let target_config_key = format!("{target_key}_{}", config.to_ascii_uppercase());
        let mut flags: Vec<_> = [key, config_key]
            .iter()
            .filter_map(|key| self.model.definition(target.dir, key.as_bytes()))
            .chain(
                [target_key, &target_config_key]
                    .iter()
                    .filter_map(|key| target.props.get(key.as_bytes())),
            )
            .flat_map(|value| flags_table::split(&string(value)))
            .collect();
        let mut libraries = Vec::new();
        let mut directories = Vec::new();
        if let Some(step) = step {
            for fragment in &step.fragments {
                let words = flags_table::split(&string(&fragment.text.value));
                match fragment.role {
                    LinkFragmentRole::Flags => flags.extend(words),
                    LinkFragmentRole::Libraries => {
                        for word in words {
                            if link_option(&word) {
                                flags.push(word);
                            } else if !archive || source_kind(&word, Kind::Other) == Kind::Object {
                                libraries.push(library(&word));
                            }
                        }
                    }
                    LinkFragmentRole::LibraryPath => {
                        directories.extend(words.into_iter().map(|word| {
                            windows(
                                word.strip_prefix("-L")
                                    .or_else(|| word.strip_prefix("/LIBPATH:"))
                                    .unwrap_or(&word),
                            )
                        }))
                    }
                    LinkFragmentRole::FrameworkPath => {}
                }
            }
        } else {
            flags.extend(
                target
                    .usage
                    .entries(UsageField::LinkOptions)
                    .iter()
                    .flat_map(|entry| ide_flags(&string(&entry.value))),
            );
            directories.extend(
                target
                    .usage
                    .entries(UsageField::LinkDirectories)
                    .iter()
                    .map(|entry| windows(&self.source_path(&string(&entry.value)))),
            );
        }
        for item in target.usage.link_libraries.iter().flatten() {
            if let Some(id) = self.model.find_target(item.lookup_dir, &item.text.value) {
                libraries.extend(self.target_objects(id, config));
                if step.is_none() && !archive {
                    libraries.extend(self.target_library(id, config));
                }
            } else if step.is_none() && !item.text.value.windows(2).any(|part| part == b"$<") {
                let value = string(&item.text.value);
                if link_option(&value) {
                    flags.push(value);
                } else if !archive || source_kind(&value, Kind::Other) == Kind::Object {
                    libraries.push(library(&value));
                }
            }
        }
        let mut out = flags_table::link(flags, archive);
        for library in libraries {
            out.list("AdditionalDependencies", msbuild(&library));
        }
        for directory in directories {
            out.list("AdditionalLibraryDirectories", msbuild_value(&directory));
        }
        out
    }

    fn model_rule(&self, command: &CustomCommand, config: &str) -> Rule {
        let dir = self.model.dir(command.dir);
        let binary = absolute(&self.settings.bin, &string(&dir.binary_dir));
        let config_value = |value: &[u8]| string(value).replace("$<CONFIG>", config);
        let dep_path = |value: &[u8]| {
            let value = config_value(value);
            let source = absolute(&self.project.source, &value);
            if self.inputs.contains(&source)
                || self
                    .model
                    .dir(command.dir)
                    .source_props
                    .contains_key(source.as_bytes())
            {
                source
            } else {
                absolute(&binary, &value)
            }
        };
        Rule {
            commands: command
                .command_lines
                .iter()
                .filter(|line| !line.is_empty())
                .map(|line| Command {
                    argv: line.iter().map(|arg| config_value(arg)).collect(),
                    dir: command.working_directory.as_ref().map_or_else(
                        || binary.clone(),
                        |dir| absolute(&binary, &config_value(dir)),
                    ),
                })
                .collect(),
            inputs: command
                .main_dependency
                .iter()
                .map(|value| self.source_path(&config_value(value)))
                .chain(
                    command
                        .depends
                        .iter()
                        .filter(|entry| self.model.find_target(command.dir, &entry.value).is_none())
                        .map(|entry| dep_path(&entry.value)),
                )
                .collect(),
            outputs: command
                .outputs
                .iter()
                .chain(&command.byproducts)
                .map(|entry| absolute(&binary, &config_value(&entry.value)))
                .collect(),
            message: command
                .comment
                .as_ref()
                .map_or_else(String::new, |value| config_value(value)),
            always: matches!(command.kind, CustomCommandKind::Utility(_))
                || command.outputs.is_empty(),
        }
    }
}

pub(super) fn source_language(path: &str) -> &'static str {
    if path.to_ascii_lowercase().ends_with(".c") {
        "C"
    } else {
        "CXX"
    }
}

fn link_option(word: &str) -> bool {
    if word.starts_with('-') {
        return !word.starts_with("-l");
    }
    word.strip_prefix('/').is_some_and(|flag| {
        let name = flag.split(':').next().unwrap_or_default();
        !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn invocation_settings(
    invocation: &Invocation,
    source: &[u8],
    object: &[u8],
) -> Option<flags_table::Settings> {
    let (compiler, args) = invocation.argv.split_first()?;
    let compiler = string(compiler);
    let name = compiler.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    if !matches!(name.as_str(), "cl" | "cl.exe" | "clang-cl" | "clang-cl.exe") {
        return None;
    }
    let mut flags = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg.as_slice() == source
            || arg.as_slice() == object
            || matches!(arg.as_slice(), b"/c" | b"-c")
        {
            continue;
        }
        let arg = string(arg);
        let option = arg.strip_prefix('/').or_else(|| arg.strip_prefix('-'));
        if matches!(option, Some("Fo" | "Fd" | "o")) {
            args.next();
        } else if !option.is_some_and(|option| option.starts_with("Fo") || option.starts_with("Fd"))
        {
            flags.push(arg);
        }
    }
    Some(flags_table::compile(flags))
}
