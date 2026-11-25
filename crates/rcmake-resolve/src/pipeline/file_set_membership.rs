//! Policy `CMP0211`: a file belongs to at most one non-`HEADERS` file set.

use indexmap::IndexMap;
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{Diagnostic, Effect, MessageKind, PolicyId, PolicyStatus, RenderOptions, render};
use rcmake_host::{Host, Stream};
use rcmake_model::{BT, DirId, FileSetKind, ProjectModel, Target, UsageScope};

use crate::{ResolveError, cxx_modules::generation_location, sources::absolute_path};

/// Report each file that two non-`HEADERS` file sets of a target contain, or
/// that a target lists both as a source and in such a file set.
///
/// With `NEW`, each conflict is an error. Without a setting, it is a policy
/// warning. With `OLD`, the target is not checked.
pub(super) fn validate(model: &ProjectModel, host: &dyn Host) -> Result<(), ResolveError> {
    let policy = PolicyId::new(211);
    let mut failure = None;
    for (_, target) in model.targets() {
        let status = target.policies.policies.get(policy);
        if target.is_imported() || status == PolicyStatus::Old {
            continue;
        }
        for message in conflicts(model, target) {
            if status == PolicyStatus::Warn {
                crate::pipeline::policy_warning(model, host, policy, target, &message);
                continue;
            }
            let diagnostic = Diagnostic {
                annotations: None,
                kind: MessageKind::Error,
                effect: Effect::None,
                category: None,
                policy: None,
                location: generation_location(model, target),
                body: message.clone().into(),
            };
            let options = RenderOptions {
                relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                ..RenderOptions::default()
            };
            host.write_out(Stream::Stderr, &render(&diagnostic, &options));
            failure.get_or_insert(ResolveError::Reported(message.into()));
        }
    }
    failure.map_or(Ok(()), Err)
}

fn conflicts(model: &ProjectModel, target: &Target) -> Vec<String> {
    let dir = &model.dir(target.dir).source_dir;
    let paths = |entries: &[BT<BString>]| {
        entries
            .iter()
            .filter(|entry| !entry.value.windows(2).any(|pair| pair == b"$<"))
            .flat_map(|entry| list::split(&entry.value, list::Empty::Drop))
            .map(|path| Lexical::new(&absolute_path(dir, &path)).normalize())
            .collect::<Vec<_>>()
    };
    let mut owners = IndexMap::new();
    let mut messages = Vec::new();
    let mut report = |file: &BString, owner: &BString| {
        messages.push(format!(
            "In target \"{}\" the file\n\n  {file}\n\nalready belongs to file set \"{owner}\".",
            target.name
        ));
    };
    for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
        if set.kind == FileSetKind::Headers || set.scope == UsageScope::Interface {
            continue;
        }
        for file in paths(&set.files) {
            match owners.get(&file) {
                Some(owner) if owner != &set.name => report(&file, owner),
                Some(_) => {}
                None => {
                    owners.insert(file, set.name.clone());
                }
            }
        }
    }
    for file in paths(&target.sources) {
        if let Some(owner) = owners.get(&file) {
            report(&file, owner);
        }
    }
    messages
}
