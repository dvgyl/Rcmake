//! Source paths and their build-time producers.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_genex::ConfigId;
use rcmake_host::{FileKind, Follow, Host, path::to_native};
use rcmake_model::{Directory, ProjectModel, TargetId, TargetKind};

use crate::{ResolveError, absolute_path, usage::UsageResolver};

#[derive(Default)]
pub(crate) struct TargetInputs {
    pub(crate) sources: IndexSet<BString>,
    pub(crate) objects: IndexSet<BString>,
    pub(crate) standard_requirements: IndexMap<BString, &'static [u8]>,
}

pub(crate) struct SourcePaths<'a> {
    host: &'a dyn Host,
    generated: IndexSet<BString>,
    objects: IndexMap<BString, TargetId>,
}

impl<'a> SourcePaths<'a> {
    pub(crate) fn new(host: &'a dyn Host) -> Self {
        Self {
            host,
            generated: IndexSet::new(),
            objects: IndexMap::new(),
        }
    }

    pub(crate) fn index_generated(
        &mut self,
        model: &ProjectModel,
        config: &ConfigId,
    ) -> Result<(), ResolveError> {
        self.generated.clear();
        let mut usage = UsageResolver::new(model);
        for (_, command) in model.commands() {
            let (outputs, byproducts) =
                crate::lower::command_outputs(model, command, config, &mut usage)?;
            self.generated.extend(outputs);
            self.generated.extend(byproducts);
        }
        Ok(())
    }

    pub(crate) fn index_objects(&mut self, model: &ProjectModel, config: &ConfigId) {
        self.objects.clear();
        let usage = UsageResolver::with_source_paths(model, self);
        self.objects = model
            .targets()
            .filter(|(_, target)| target.kind == TargetKind::ObjectLibrary)
            .filter_map(|(id, _)| Some((id, usage.target_objects(id, config, None)?)))
            .flat_map(|(id, objects)| {
                list::split(&objects, list::Empty::Drop)
                    .into_iter()
                    .map(move |path| (Lexical::new(&path).normalize(), id))
            })
            .collect();
    }

    pub(crate) fn object_owner(&self, path: &[u8]) -> Option<TargetId> {
        self.objects.get(&Lexical::new(path).normalize()).copied()
    }

    pub(crate) fn find(&self, dir: &Directory, source: &[u8]) -> Option<BString> {
        let paths = [
            absolute_path(&dir.source_dir, source),
            absolute_path(&dir.binary_dir, source),
        ];
        if let Some(path) = paths.iter().find(|path| {
            self.generated.contains(*path)
                || self.objects.contains_key(*path)
                || source_flag(dir, path, b"GENERATED")
        }) {
            return Some(path.clone());
        }
        paths.into_iter().find(|path| {
            self.host
                .metadata(&to_native(path), Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
        })
    }

    pub(crate) fn resolve(&self, dir: &Directory, source: &[u8]) -> Result<BString, ResolveError> {
        self.find(dir, source).ok_or_else(|| {
            ResolveError::Message([b"Cannot find source file:\n\n  ", source].concat().into())
        })
    }
}

pub(crate) fn external_object(dir: &Directory, path: &BString) -> bool {
    source_flag(dir, path, b"EXTERNAL_OBJECT")
        || matches!(Lexical::new(path).stem_extension(true).1, b".o" | b".obj")
}

fn source_flag(dir: &Directory, source: &BString, property: &[u8]) -> bool {
    dir.source_props
        .get(source)
        .and_then(|props| props.get(property))
        .is_some_and(|value| truth::is_on(value))
}
