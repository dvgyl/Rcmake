//! Check evaluated file-set memberships under policy `CMP0211`.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{FileSetId, FileSetKind, ProjectModel, Target};

use crate::{
    ResolveError, cxx_modules::generation_location, lower, sources::SourcePaths,
    usage::UsageResolver,
};

#[derive(Default)]
pub(crate) struct Membership {
    first: IndexMap<BString, (FileSetId, BString)>,
    members: IndexSet<(BString, FileSetId)>,
    sources: IndexSet<BString>,
    conflicts: Vec<(BString, BString)>,
}

impl Membership {
    pub(crate) fn is_empty(&self) -> bool {
        self.first.is_empty()
    }

    pub(crate) fn member(&mut self, path: &[u8], id: FileSetId, name: &BString) {
        let file = Lexical::new(path).normalize();
        if self.members.insert((file.clone(), id)) {
            let (first, owner) = self
                .first
                .entry(file.clone())
                .or_insert_with(|| (id, name.clone()));
            if *first != id {
                self.conflicts.push((file, owner.clone()));
            }
        }
    }

    pub(crate) fn source(&mut self, path: &[u8]) {
        let file = Lexical::new(path).normalize();
        if let Some((_, owner)) = self.first.get(&file)
            && self.sources.insert(file.clone())
        {
            self.conflicts.push((file, owner.clone()));
        }
    }

    pub(crate) fn validate(
        self,
        model: &ProjectModel,
        paths: &SourcePaths<'_>,
        target: &Target,
    ) -> Result<(), ResolveError> {
        let policy = PolicyId::new(211);
        let status = target.policies.policies.get(policy);
        let mut failure = None;
        let mut warnings = IndexMap::<BString, usize>::new();
        for (file, owner) in self.conflicts {
            let body: BString = format!(
                "In target \"{}\" the file\n  {file}\nalready belongs to file set \"{owner}\".",
                target.name
            )
            .into();
            match status {
                PolicyStatus::Old => break,
                PolicyStatus::Warn => {
                    let occurrence = warnings.entry(body.clone()).or_default();
                    if paths.first_file_set_warning(target, &body, *occurrence) {
                        crate::pipeline::policy_warning(
                            model,
                            paths.host,
                            policy,
                            target,
                            &body.to_string(),
                        );
                    }
                    *occurrence += 1;
                }
                PolicyStatus::New => {
                    paths.emit(
                        model,
                        MessageKind::Error,
                        generation_location(model, target),
                        body.clone(),
                    );
                    failure.get_or_insert(ResolveError::Reported(body));
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(super) fn validate_file_sets(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    config: &ConfigId,
) -> Result<(), ResolveError> {
    let mut usage = UsageResolver::new(model).with_ide(paths.ide);
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
            if set.kind != FileSetKind::Headers {
                continue;
            }
            if target
                .props
                .get(b"FRAMEWORK")
                .is_some_and(|value| truth::is_on(value))
            {
                let body = format!(
                    "The file set \"{}\", of type \"HEADERS\", is incompatible with the \"FRAMEWORK\" target \"{}\".",
                    set.name, target.name,
                );
                paths.emit(
                    model,
                    MessageKind::Error,
                    generation_location(model, target),
                    body.clone().into(),
                );
                return Err(ResolveError::Message(body.into()));
            }
            if lower::definition(model, target.dir, b"CMAKE_GENERATOR")
                .is_none_or(|value| !value.starts_with(b"Ninja"))
                || !set
                    .props
                    .get(b"INDEPENDENT_FILES")
                    .is_some_and(|value| truth::is_on(value))
                || lower::definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
                    .is_some_and(|value| truth::is_on(value))
            {
                continue;
            }
            let dir = model.dir(target.dir);
            if usage
                .file_set_paths(id, b"SOURCES", &set.files, &ctx)?
                .iter()
                .any(|file| paths.is_generated(dir, &file.value))
            {
                paths.emit(
                    model, MessageKind::AuthorWarning, generation_location(model, target),
                    format!(
                        "the \"INDEPENDENT_FILES\" property of the file set \"{}\" of the target \"{}\" will be ignored because it is incompatible with the file set type \"HEADERS\".",
                        set.name, target.name,
                    ).into(),
                );
            }
        }
    }
    Ok(())
}
