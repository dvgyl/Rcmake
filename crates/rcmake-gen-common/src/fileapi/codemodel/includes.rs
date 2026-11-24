use std::collections::BTreeSet;

use indexmap::IndexMap;
use rcmake_base::{BString, list, path_lex::cmake_style, truth::is_on};
use rcmake_genex::{EvalContext, EvalSite, PreprocessMode, TargetHandle};
use rcmake_model::{ProjectModel, Target, UsageField};
use rcmake_resolve::{ResolvedTarget, usage::UsageResolver};

use super::super::{Bool, Object, Value, members, strings};
use crate::{GenError, genex};

pub(super) fn interface_includes(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    resolved: &ResolvedTarget,
    target: &Target,
) -> Result<Vec<Value>, GenError> {
    let property = |field: UsageField| {
        target
            .usage
            .fields
            .get(&field)
            .map(|entries| list::join(entries.iter().map(|entry| &entry.value)))
            .or_else(|| target.props.get(field.name().as_bytes()).cloned())
    };
    let Some(includes) = property(UsageField::InterfaceIncludeDirectories) else {
        return Ok(Vec::new());
    };
    let system_includes = property(UsageField::InterfaceSystemIncludeDirectories);
    let all_system = target
        .props
        .get(b"SYSTEM")
        .map_or(target.is_imported(), |value| is_on(value))
        && !(target.is_imported()
            && target
                .props
                .get(b"IMPORTED_NO_SYSTEM")
                .is_some_and(|value| is_on(value)));
    let mut languages = model
        .global
        .props
        .get(b"ENABLED_LANGUAGES")
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    languages.retain(|language| language != "NONE");
    languages.sort_unstable();
    languages.dedup();
    let handle = TargetHandle(resolved.id.index() as u32);
    let mut context = EvalContext {
        head: Some(handle),
        current: Some(handle),
        config: resolved.config.clone(),
        site: EvalSite {
            property: Some("INCLUDE_DIRECTORIES".into()),
            backtrace: target.backtrace.clone(),
        },
        ..Default::default()
    };
    let mut paths = IndexMap::<(BString, bool), Vec<BString>>::new();
    for language in languages
        .iter()
        .map(Some)
        .chain(languages.is_empty().then_some(None))
    {
        context.language = language.cloned();
        let mut evaluate = |value: &[u8]| {
            genex(
                usage,
                target.dir,
                &context,
                value,
                Some(PreprocessMode::Build),
            )
            .map(|value| {
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|path| cmake_style(&path))
            })
        };
        let system = system_includes
            .as_deref()
            .map(|value| evaluate(value))
            .transpose()?
            .into_iter()
            .flatten()
            .collect::<BTreeSet<_>>();
        for path in evaluate(&includes)? {
            let system = all_system || system.contains(&path);
            let entry = paths.entry((path, system)).or_default();
            if let Some(language) = language
                && entry.last() != Some(language)
            {
                entry.push(language.clone());
            }
        }
    }
    Ok(paths
        .into_iter()
        .map(|((path, system), supported)| {
            let mut entry = members([("path", Value::string(path))]);
            if system {
                entry.insert("isSystem".into(), Bool(true));
            }
            if supported.len() != languages.len() {
                entry.insert("languages".into(), strings(&supported));
            }
            Object(entry)
        })
        .collect())
}
