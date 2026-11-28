use rcmake_base::{list, path_lex::Lexical};
use rcmake_model::{TargetId, TargetKind};
use rcmake_resolve::{
    BuildStep, ResolvedTarget,
    ide::{IdeFacts, IdeLinkItem, IdeSourceKind},
};

use super::{
    Command, Config, Context, Kind, Plan, Rule, absolute, build_compile_units, flags_table,
    ide_flags, join, msbuild, msbuild_value, object_path, source_kind, string, windows,
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
        self.link_defaults(config);
        // As in CMake, a target has no import library when the platform does
        // not use them (`CMAKE_IMPORT_LIBRARY_SUFFIX` is empty) or when it has
        // managed code only.
        let import = fact
            .and_then(|fact| fact.artifacts.import_file.as_ref())
            .filter(|_| self.has_import_library());
        if !self.has_import_library() {
            config.link.take("ImportLibrary");
        }
        for (property, file) in [
            ("ImportLibrary", import),
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

    /// Add the link settings that CMake writes for every linked target.
    ///
    /// The flags of the project come first, so a setting is added only when
    /// the flags do not set it. Empty values stop MSBuild from using its own
    /// defaults.
    fn link_defaults(&self, config: &mut Config) {
        let link = &mut config.link;
        if !link.has("SubSystem") {
            link.set("SubSystem", if config.gui { "Windows" } else { "Console" });
        }
        if !link.has("GenerateDebugInformation") {
            link.set("GenerateDebugInformation", "false");
        }
        if !link.has("Version")
            && let Some(version) = self
                .project
                .target
                .and_then(|id| self.model.target(id))
                .and_then(|target| target.props.get(b"VERSION"))
                .and_then(|value| image_version(&string(value)))
        {
            link.set("Version", version);
        }
        link.inherit("IgnoreSpecificDefaultLibraries");
        for key in [
            "DataExecutionPrevention",
            "ImageHasSafeExceptionHandlers",
            "LinkErrorReporting",
            "RandomizedBaseAddress",
        ] {
            if !link.has(key) {
                link.set(key, "");
            }
        }
    }

    /// Tell if the target has an import library, as CMake decides.
    fn has_import_library(&self) -> bool {
        let Some(target) = self.project.target.and_then(|id| self.model.target(id)) else {
            return false;
        };
        // CMake uses import libraries only when the platform sets a suffix
        // for them. Windows always sets `.lib`, so only an empty value turns
        // them off.
        let dll_platform = self
            .model
            .definition(target.dir, b"CMAKE_IMPORT_LIBRARY_SUFFIX")
            .is_none_or(|suffix| !suffix.is_empty());
        let managed = matches!(
            target.kind,
            TargetKind::Executable | TargetKind::SharedLibrary
        ) && target
            .props
            .get(b"COMMON_LANGUAGE_RUNTIME")
            .is_some_and(|value| !matches!(value.as_slice(), b"" | b"netcore"));
        dll_platform && !managed
    }

    fn definition_command(&self, fact: &ResolvedTarget) -> Option<&rcmake_resolve::Invocation> {
        let cfg = fact.config.0.as_slice();
        let graph = self.indices.graph_by_config.get(cfg).copied()?;
        let target = self
            .indices
            .graph_target
            .get(&(cfg, fact.metadata.name.as_slice()))
            .copied()?;
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
        let mut flags = self.linker_variable_flags(config);
        flags.extend(
            ide.link_options
                .iter()
                .flat_map(|flag| ide_flags(&string(&flag.value))),
        );
        let mut libraries = Vec::new();
        for item in &ide.link_items {
            match item {
                IdeLinkItem::Target(id) => {
                    if !archive && let Some(file) = self.target_library(*id, config) {
                        libraries.push(file);
                    }
                    libraries.extend(self.target_objects(*id, config));
                }
                IdeLinkItem::File(file) if is_targets_file(&string(file)) => {}
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
                                .replace("<LINK_ITEM>", &library)
                                .replace("<LIB_ITEM>", &library),
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

    /// The linker flags that CMake reads from variables and the older target
    /// properties, before `LINK_OPTIONS`.
    ///
    /// A linked target uses `CMAKE_<TYPE>_LINKER_FLAGS[_<CONFIG>]` and
    /// `LINK_FLAGS[_<CONFIG>]`. A static library uses
    /// `CMAKE_STATIC_LINKER_FLAGS[_<CONFIG>]` and
    /// `STATIC_LIBRARY_FLAGS[_<CONFIG>]`.
    fn linker_variable_flags(&self, config: &str) -> Vec<String> {
        let Some(target) = self.project.target.and_then(|id| self.model.target(id)) else {
            return Vec::new();
        };
        let (variable, property) = match target.kind {
            TargetKind::Executable => ("CMAKE_EXE_LINKER_FLAGS", "LINK_FLAGS"),
            TargetKind::SharedLibrary => ("CMAKE_SHARED_LINKER_FLAGS", "LINK_FLAGS"),
            TargetKind::ModuleLibrary => ("CMAKE_MODULE_LINKER_FLAGS", "LINK_FLAGS"),
            TargetKind::StaticLibrary => ("CMAKE_STATIC_LINKER_FLAGS", "STATIC_LIBRARY_FLAGS"),
            _ => return Vec::new(),
        };
        let upper = config.to_ascii_uppercase();
        let variables = [variable.to_owned(), format!("{variable}_{upper}")];
        let properties = [property.to_owned(), format!("{property}_{upper}")];
        variables
            .iter()
            .filter_map(|key| self.model.definition(target.dir, key.as_bytes()))
            .chain(
                properties
                    .iter()
                    .filter_map(|key| target.props.get(key.as_bytes())),
            )
            .flat_map(|value| flags_table::split(&string(value)))
            .collect()
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
            .indices
            .resolved_by_id
            .get(&(id, config.as_bytes()))
            .copied();
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
            .indices
            .project_by_target
            .get(&id)
            .and_then(|&index| self.projects.get(index))?;
        Some(windows(&join(
            &join(&project.dir, config),
            &format!("{}.lib", project.name),
        )))
    }

    pub(super) fn target_objects(&self, id: TargetId, config: &str) -> Vec<String> {
        let Some(project) = self
            .indices
            .project_by_target
            .get(&id)
            .and_then(|&index| self.projects.get(index))
            .filter(|project| project.kind == TargetKind::ObjectLibrary)
        else {
            return Vec::new();
        };
        let fact = self
            .indices
            .resolved_by_id
            .get(&(id, config.as_bytes()))
            .copied();
        let compile_units = build_compile_units(fact);
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
        } else if let Some(graph) = self.indices.graph_by_config.get(config.as_bytes()).copied()
            && let Some(entry) = self
                .indices
                .graph_target
                .get(&(config.as_bytes(), project.name.as_bytes()))
                .copied()
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
                            object_path(
                                project,
                                self.settings,
                                self.model,
                                &source,
                                config,
                                fact,
                                compile_units.as_ref(),
                            )
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
                    compile_units.as_ref(),
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

/// The image version `<major>.<minor>` of a `VERSION` property, as CMake
/// reads it. Missing or invalid parts are zero. A version `0.0` gives `None`.
fn image_version(value: &str) -> Option<String> {
    let mut parts = value.split('.');
    let mut number = || {
        parts
            .next()
            .map(|part| {
                let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u32>().unwrap_or(0)
            })
            .unwrap_or(0)
    };
    let (major, minor) = (number(), number());
    (major != 0 || minor != 0).then(|| format!("{major}.{minor}"))
}

/// Tell if a linked file is an MSBuild `.targets` file, which the project
/// imports instead of linking.
fn is_targets_file(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("targets"))
}

/// The `.targets` files in the link items, with `\` separators.
pub(super) fn targets_files(ide: &IdeFacts) -> Vec<String> {
    ide.link_items
        .iter()
        .filter_map(|item| match item {
            IdeLinkItem::File(file) => Some(string(file)),
            _ => None,
        })
        .filter(|file| is_targets_file(file))
        .map(|file| windows(&file))
        .collect()
}

#[cfg(test)]
mod image_version_tests {
    use super::image_version;

    #[test]
    fn version_keeps_major_and_minor_only() {
        assert_eq!(image_version("0.1").as_deref(), Some("0.1"));
        assert_eq!(image_version("1.0").as_deref(), Some("1.0"));
        assert_eq!(image_version("65535.65535").as_deref(), Some("65535.65535"));
        assert_eq!(image_version("1.2.3").as_deref(), Some("1.2"));
        assert_eq!(image_version("2").as_deref(), Some("2.0"));
        assert_eq!(image_version(""), None);
        assert_eq!(image_version("0.0"), None);
    }
}
