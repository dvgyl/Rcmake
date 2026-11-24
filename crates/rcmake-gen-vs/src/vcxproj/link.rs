use rcmake_base::list;
use rcmake_model::{TargetId, TargetKind};
use rcmake_resolve::{
    BuildStep,
    ide::{IdeFacts, IdeLinkItem, IdeSourceKind},
};

use super::{
    Context, Kind, absolute, flags_table, ide_flags, join, msbuild, msbuild_value, object_path,
    source_kind, string, windows,
};

impl Context<'_> {
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
                _ => {}
            }
        }
        let mut out = flags_table::link(flags, archive);
        for library in libraries {
            out.list("AdditionalDependencies", msbuild(&library));
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
            fact.artifacts
                .linker_file
                .as_ref()
                .or(fact.artifacts.import_file.as_ref())
                .or(fact.artifacts.file.as_ref())
        }) {
            return Some(windows(&absolute(&self.settings.bin, &string(file))));
        }
        if target.is_imported() {
            let config = config.to_ascii_uppercase();
            return [
                format!("IMPORTED_IMPLIB_{config}"),
                "IMPORTED_IMPLIB".into(),
                format!("IMPORTED_LOCATION_{config}"),
                "IMPORTED_LOCATION".into(),
            ]
            .into_iter()
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
        let sources: Vec<_> = if let Some(ide) = fact.and_then(|fact| fact.ide.as_deref()) {
            ide.sources
                .iter()
                .filter(|source| {
                    matches!(source.kind, IdeSourceKind::Compile { .. })
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
                        if source_kind(&source, Kind::Compile) != Kind::Compile {
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
                .filter(|path| source_kind(path, Kind::Other) == Kind::Compile)
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
