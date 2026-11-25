use indexmap::IndexMap;
use rcmake_base::{BString, path_lex::Lexical};
use rcmake_model::{CommandId, CustomCommand, DirId, ProjectModel, TargetId};

use crate::sources::{SourcePaths, absolute_path};

#[derive(Default)]
pub(super) struct Attachments(IndexMap<DirId, IndexMap<BString, (usize, CommandId)>>);

impl Attachments {
    pub(super) fn register(
        &mut self,
        model: &ProjectModel,
        (id, command): (CommandId, &CustomCommand),
        paths: Option<&SourcePaths<'_>>,
        index: usize,
    ) -> bool {
        let Some(main) = command
            .main_dependency
            .as_ref()
            .filter(|_| !command.command_lines.is_empty())
        else {
            return true;
        };
        let dir = model.dir(command.dir);
        let source = paths
            .and_then(|paths| paths.find(dir, main))
            .unwrap_or_else(|| absolute_path(&dir.source_dir, main));
        let input = super::build_relative(model, &Lexical::new(&source).normalize());
        let slot = self.0.entry(command.dir).or_default().entry(input);
        match slot {
            indexmap::map::Entry::Vacant(slot) => {
                slot.insert((index, id));
                true
            }
            indexmap::map::Entry::Occupied(slot) => model
                .command(slot.get().1)
                .is_none_or(|first| first.command_lines != command.command_lines),
        }
    }

    pub(super) fn get(&self, dir: DirId, path: &BString) -> Option<usize> {
        self.0.get(&dir)?.get(path).map(|(index, _)| *index)
    }
}

// Event byproducts belong to their target. They have no independent recipe.
pub(super) fn byproduct_owners(
    model: &ProjectModel,
    config: &rcmake_genex::ConfigId,
    usage: &mut crate::usage::UsageResolver<'_>,
) -> Result<IndexMap<BString, TargetId>, crate::ResolveError> {
    let mut owners = IndexMap::new();
    for (_, command) in model.commands() {
        let Some(owner) = super::command_target(command) else {
            continue;
        };
        if command.byproducts.is_empty() {
            continue;
        }
        let (_, byproducts) = super::command_outputs(model, command, config, usage)?;
        for path in byproducts {
            owners
                .entry(super::build_relative(model, &path))
                .or_insert(owner);
        }
    }
    Ok(owners)
}
