use rcmake_base::{list, path_lex::Lexical};
use rcmake_model::{TargetId, TargetKind};
use rcmake_resolve::{
    BuildStep, ResolvedTarget,
    ide::{IdeFacts, IdeLinkItem, IdeSourceKind},
};

use super::{
    Command, Config, Context, Kind, Plan, Rule, absolute, flags_table, ide_flags, join, msbuild,
    msbuild_value, object_path, source_kind, string, windows,
};

impl Context<'_> {
    pub(super) fn windows_link(
        &self,
        plan: &Plan,
        config: &mut Config,
        fact: Option<&ResolvedTarget>,
        name: &str,
    ) {
        if !matches!(
            self.project.kind,
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ) {
            return;
        }
        for (property, file) in [
            (
                "ImportLibrary",
                fact.and_then(|fact| fact.artifacts.import_file.as_ref()),
            ),
            (
                "ProgramDatabaseFile",
                fact.and_then(|fact| fact.artifacts.pdb_file.as_ref()),
            ),
        ] {
            let file = config.link.take(property).or_else(|| {
                let file = absolute(&self.settings.bin, &string(file?));
                Some(crate::cuda::OptionText::WithProperties(&windows(&file)).escaped())
            });
            if let Some(file) = file {
                let path = file.replace('\\', "/");
                let path = Lexical::new(path.as_bytes());
                let dir = if string(path.filename()).contains("$(") {
                    format!("$([System.IO.Path]::GetDirectoryName('{file}'))")
                } else {
                    windows(&string(path.parent()))
                };
                if !dir.is_empty() && !config.link_output_dirs.contains(&dir) {
                    config.link_output_dirs.push(dir);
                }
                config.link.set(property, file);
            }
        }
        if config.link.has("ModuleDefinitionFile") {
            return;
        }
        if let Some(fact) = fact
            && let Some(command) = self.definition_command(fact)
            && let Some(output) = command.argv.get(3)
        {
            let output = absolute(&self.settings.bin, &string(output));
            config
                .link
                .set("ModuleDefinitionFile", msbuild(&windows(&output)));
            config.events.get_mut(1).into_iter().for_each(|rules| {
                rules.push(Rule {
                    commands: vec![Command {
                        argv: command.argv.iter().map(|arg| string(arg)).collect(),
                        dir: self.settings.bin.clone(),
                    }],
                    message: "Generate the module definition file".into(),
                    ..Rule::default()
                });
            });
            return;
        }
        let mut definitions = plan.items.values().filter(|item| {
            item.kind == Kind::Other
                && item
                    .path
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("def"))
                && item
                    .configs
                    .get(name)
                    .is_some_and(|source| !source.excluded)
        });
        if let Some(file) = definitions.next().filter(|_| definitions.next().is_none()) {
            config
                .link
                .set("ModuleDefinitionFile", msbuild(&windows(&file.path)));
        }
    }

    fn definition_command(&self, fact: &ResolvedTarget) -> Option<&rcmake_resolve::Invocation> {
        let graph = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config == fact.config)?;
        let target = graph
            .targets
            .iter()
            .find(|target| target.name == fact.metadata.name)?;
        target
            .steps
            .iter()
            .filter_map(|index| graph.steps.get(*index))
            .find_map(|step| {
                let BuildStep::Link { invocations, .. } = step else {
                    return None;
                };
                invocations.iter().find(|command| {
                    command.argv.get(1).is_some_and(|arg| arg == b"-E")
                        && command
                            .argv
                            .get(2)
                            .is_some_and(|arg| arg == b"__create_def")
                })
            })
    }

    pub(super) fn ide_link(&self, ide: &IdeFacts, config: &str) -> flags_table::Settings {
        let archive = self.project.kind == TargetKind::StaticLibrary;
        let mut flags: Vec<_> = ide
            .link_options
            .iter()
            .flat_map(|flag| ide_flags(&string(&flag.value)))
            .collect();
        let mut libraries = Vec::new();
        for item in &ide.link_items {
            match item {
                IdeLinkItem::Target(id) => {
                    if !archive && let Some(file) = self.target_library(*id, config) {
                        libraries.push(file);
                    }
                    libraries.extend(self.target_objects(*id, config));
                }
                IdeLinkItem::File(file)
                    if !archive || source_kind(&string(file), Kind::Other) == Kind::Object =>
                {
                    libraries.push(windows(&string(file)));
                }
                IdeLinkItem::Library(name) if !archive => libraries.push(library(&string(name))),
                IdeLinkItem::Flag(flag) => flags.push(string(flag)),
                IdeLinkItem::VisualStudioFeature { item, template } if !archive => {
                    let library = match item.as_ref() {
                        IdeLinkItem::Target(id) => self.target_library(*id, config),
                        IdeLinkItem::File(file) => Some(windows(&string(file))),
                        _ => None,
                    };
                    if let Some(library) = library {
                        let template = string(template);
                        let template = template.strip_prefix("LINKER:").unwrap_or(&template);
                        libraries.push(
                            template
                                .replace("<LIBRARY>", &library)
                                .replace("<LINK_ITEM>", &library),
                        );
                    }
                }
                _ => {}
            }
        }
        let mut out = flags_table::link(flags, archive);
        for library in libraries {
            if ide.pch.iter().any(|pch| {
                pch.companion
                    .as_ref()
                    .is_some_and(|path| windows(&string(path)) == library)
            }) {
                out.list_unique("AdditionalDependencies", msbuild(&library));
            } else {
                out.list("AdditionalDependencies", msbuild(&library));
            }
        }
        for dir in &ide.link_directories {
            out.list(
                "AdditionalLibraryDirectories",
                msbuild_value(&windows(&string(dir))),
            );
        }
        out
    }

    pub(super) fn target_library(&self, id: TargetId, config: &str) -> Option<String> {
        let target = self.model.target(id)?;
        if !matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::UnknownLibrary
        ) {
            return None;
        }
        let resolved = self
            .result
            .targets
            .iter()
            .find(|fact| fact.id == id && fact.config.0.as_slice() == config.as_bytes());
        if let Some(file) = resolved.and_then(|fact| {
            fact.artifacts.import_file.as_ref().or_else(|| {
                if target.kind == TargetKind::SharedLibrary {
                    fact.artifacts
                        .linker_file
                        .as_ref()
                        .filter(|file| Some(*file) != fact.artifacts.file.as_ref())
                } else {
                    fact.artifacts
                        .linker_file
                        .as_ref()
                        .or(fact.artifacts.file.as_ref())
                }
            })
        }) {
            return Some(windows(&absolute(&self.settings.bin, &string(file))));
        }
        if target.is_imported() {
            let config = config.to_ascii_uppercase();
            let keys = [
                format!("IMPORTED_IMPLIB_{config}"),
                "IMPORTED_IMPLIB".into(),
                format!("IMPORTED_LOCATION_{config}"),
                "IMPORTED_LOCATION".into(),
            ];
            let keys = if target.kind == TargetKind::SharedLibrary {
                &keys[..2]
            } else {
                keys.as_slice()
            };
            return keys
                .iter()
                .find_map(|key| target.props.get(key.as_bytes()))
                .map(|file| windows(&string(file)));
        }
        let project = self
            .projects
            .iter()
            .find(|project| project.target == Some(id))?;
        Some(windows(&join(
            &join(&project.dir, config),
            &format!("{}.lib", project.name),
        )))
    }

    pub(super) fn target_objects(&self, id: TargetId, config: &str) -> Vec<String> {
        let Some(project) = self.projects.iter().find(|project| {
            project.target == Some(id) && project.kind == TargetKind::ObjectLibrary
        }) else {
            return Vec::new();
        };
        let fact = self
            .result
            .targets
            .iter()
            .find(|fact| fact.id == id && fact.config.0.as_slice() == config.as_bytes());
        let ide = fact.and_then(|fact| fact.ide.as_deref());
        let sources: Vec<_> = if let Some(ide) = ide {
            ide.sources
                .iter()
                .filter(|source| {
                    matches!(source.kind, IdeSourceKind::Compile { .. })
                        && !ide.pch.iter().any(|pch| pch.source == source.path)
                        && !source
                            .excluded_configs
                            .iter()
                            .any(|value| value.as_slice() == config.as_bytes())
                })
                .map(|source| string(&source.path))
                .collect()
        } else if let Some(fact) = fact.filter(|fact| !fact.compile_units.is_empty()) {
            fact.compile_units
                .iter()
                .map(|unit| string(&unit.source.value))
                .collect()
        } else if let Some(graph) = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config.0.as_slice() == config.as_bytes())
            && let Some(entry) = graph
                .targets
                .iter()
                .find(|entry| entry.name.as_slice() == project.name.as_bytes())
        {
            return entry
                .steps
                .iter()
                .filter_map(|&index| match graph.steps.get(index) {
                    Some(BuildStep::Compile { object, source, .. }) => {
                        let source = absolute(&project.source, &string(source));
                        if !source_kind(&source, Kind::Compile).compiles() {
                            return None;
                        }
                        let object = string(object);
                        let path = if object.to_ascii_lowercase().ends_with(".obj") {
                            absolute(&self.settings.bin, &object)
                        } else {
                            object_path(project, self.settings, self.model, &source, config, fact)
                        };
                        Some(windows(&path))
                    }
                    _ => None,
                })
                .collect();
        } else {
            self.model
                .target(id)
                .into_iter()
                .flat_map(|target| &target.sources)
                .flat_map(|entry| list::split(&entry.value, list::Empty::Drop))
                .map(|path| string(&path))
                .filter(|path| source_kind(path, Kind::Other).compiles())
                .collect()
        };
        sources
            .into_iter()
            .map(|source| {
                let source = absolute(&project.source, &source);
                windows(&object_path(
                    project,
                    self.settings,
                    self.model,
                    &source,
                    config,
                    fact,
                ))
            })
            .chain(
                ide.into_iter()
                    .flat_map(|ide| &ide.pch)
                    .filter(|pch| {
                        pch.reuse_from.is_none_or(|owner| {
                            self.model
                                .target(owner)
                                .is_some_and(|target| target.kind == TargetKind::ObjectLibrary)
                        })
                    })
                    .filter_map(|pch| pch.companion.as_ref().map(|path| windows(&string(path)))),
            )
            .collect()
    }
}

pub(super) fn library(value: &str) -> String {
    let value = value.strip_prefix("-l").unwrap_or(value);
    if value.contains(['/', '\\', '.']) {
        windows(value)
    } else {
        format!("{value}.lib")
    }
}
