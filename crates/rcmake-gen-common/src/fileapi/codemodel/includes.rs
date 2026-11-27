use std::collections::BTreeSet;

use indexmap::IndexMap;
use rcmake_base::{BString, list, path_lex::cmake_style, truth::is_on};
use rcmake_genex::{EvalContext, EvalSite, TargetHandle};
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
            genex(usage, target.dir, &context, value, None).map(|value| {
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|path| without_trailing_slashes(cmake_style(&path)))
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

fn without_trailing_slashes(path: BString) -> BString {
    let mut bytes = path.as_slice();
    while bytes.len() > 1
        && bytes.last() == Some(&b'/')
        && !(bytes.len() == 3 && bytes.get(1) == Some(&b':'))
    {
        bytes = bytes.strip_suffix(b"/").unwrap_or(bytes);
    }
    BString::from(bytes)
}

#[cfg(test)]
mod tests {
    use rcmake_diag::{Backtrace, PolicyMap};
    use rcmake_genex::ConfigId;
    use rcmake_host::PureHost;
    use rcmake_model::{BT, DirId, ProjectModel, Target, TargetKind, TargetOrigin, UsageField};
    use rcmake_resolve::usage::UsageResolver;

    use super::*;

    #[test]
    fn preserves_build_local_interface_and_drops_install_interface() {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/build".into());
        model
            .global
            .props
            .set("ENABLED_LANGUAGES".into(), "CXX".into());
        let mut target = Target::new(
            "iface".into(),
            TargetKind::InterfaceLibrary,
            TargetOrigin::Normal,
            DirId::ROOT,
            Backtrace::default(),
            PolicyMap::default(),
        );
        target.usage.push(
            UsageField::InterfaceIncludeDirectories,
            BT::new(
                "$<BUILD_INTERFACE:/src/build_inc>;$<BUILD_LOCAL_INTERFACE:/src/local_inc>;$<INSTALL_INTERFACE:include>".into(),
                Backtrace::default(),
            ),
        );
        let id = model.add_target(target).expect("add target");
        let host = PureHost::default();
        let resolved_all =
            rcmake_resolve::resolve(&model, &host, &[ConfigId::default()]).expect("resolve");
        let resolved = resolved_all
            .targets
            .iter()
            .find(|t| t.id == id)
            .expect("resolved target");
        let raw = model.target(id).expect("raw target");
        let mut usage = UsageResolver::new(&model);
        let entries = interface_includes(&model, &mut usage, resolved, raw).expect("includes");
        let paths: Vec<String> = entries
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(map) => match map.get(b"path".as_slice()) {
                    Some(Value::String(s)) => Some(s.to_string()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec!["/src/build_inc", "/src/local_inc"]);
    }
}
