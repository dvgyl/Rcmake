//! Framework targets that a link feature names by bundle name.

use std::collections::BTreeSet;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target};

use super::super::artifacts::{build_relative, is_darwin};
use super::super::join;
use super::super::shell::simple_args;
use crate::{LinkFragmentRole, LinkStep, ide, usage::UsageResolver};

/// Return the files of the linked framework targets that a link feature
/// names only by bundle name, for example `-framework Foo`.
///
/// The link step depends on these files, and their directories go into the
/// build RPATH, as for a framework that the link line names by path.
pub(super) fn named_framework_files(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    libraries: &[BString],
) -> Vec<BString> {
    if !is_darwin(model, target.dir) {
        return Vec::new();
    }
    let words: BTreeSet<BString> = step
        .fragments
        .iter()
        .filter(|fragment| fragment.role == LinkFragmentRole::Libraries)
        .filter(|fragment| fragment.text.value.windows(9).any(|w| w == b"framework"))
        .flat_map(|fragment| simple_args(&fragment.text.value).into_iter().flatten())
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Vec::new();
    };
    let mut usage = UsageResolver::new(model);
    let ctx = EvalContext {
        config: config.clone(),
        link_language: Some(step.language.clone()),
        ..EvalContext::default()
    };
    let Ok(dependencies) = usage.link_targets(id, &ctx) else {
        return Vec::new();
    };
    let build = &model.dir(rcmake_model::DirId::ROOT).binary_dir;
    let absolute = |path: &[u8]| Lexical::new(&join(build, path)).normalize();
    let known: BTreeSet<_> = libraries.iter().map(|path| absolute(path)).collect();
    let mut files = Vec::new();
    for dependency in dependencies {
        let Some(producer) = model.target(dependency) else {
            continue;
        };
        if producer.is_imported()
            || !matches!(
                ide::bundle_kind(producer),
                ide::BundleKind::Framework { .. }
            )
        {
            continue;
        }
        let Some(file) = usage.artifact(dependency, ArtifactKind::File, ArtifactPart::Full, config)
        else {
            continue;
        };
        let named = bundle_name(&file).is_some_and(|name| words.contains(name));
        if named && !known.contains(&absolute(&file)) {
            files.push(build_relative(model, &file));
        }
    }
    files
}

/// Return the bundle name of a framework file path, for example `Foo` for
/// `/build/Foo.framework/Versions/A/Foo`. A leading shell quote is ignored.
pub(crate) fn bundle_name(path: &[u8]) -> Option<&[u8]> {
    let end = path
        .windows(b".framework/".len())
        .position(|window| window == b".framework/")?;
    let root = path.get(..end)?;
    let start = root
        .iter()
        .rposition(|&byte| byte == b'/')
        .map_or(0, |index| index + 1);
    let name = root.get(start..)?;
    let name = name.strip_prefix(b"'").unwrap_or(name);
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::bundle_name;

    #[test]
    fn bundle_name_reads_the_framework_root() {
        let path = if cfg!(windows) {
            b"C:/work/Debug/Foo.framework/Versions/A/Foo".as_slice()
        } else {
            b"/work/Debug/Foo.framework/Versions/A/Foo".as_slice()
        };
        assert_eq!(bundle_name(path), Some(b"Foo".as_slice()));
        assert_eq!(bundle_name(b"Foo.framework/Foo"), Some(b"Foo".as_slice()));
        assert_eq!(bundle_name(b"'Foo.framework/Foo'"), Some(b"Foo".as_slice()));
        assert_eq!(bundle_name(b"libfoo.dylib"), None);
    }
}
