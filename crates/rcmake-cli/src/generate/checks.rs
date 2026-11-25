use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use rcmake_base::{BString, truth::is_on};
use rcmake_host::{Host, Stream};
use rcmake_model::{DirId, GeneratorKind, InstallKind, ProjectModel, TargetId};
use rcmake_resolve::{GenerationResult, LinkKind, ResolvedTarget};

pub(super) fn report(
    model: &ProjectModel,
    result: &GenerationResult,
    kind: GeneratorKind,
    host: &dyn Host,
) -> bool {
    let mut targets = BTreeMap::<_, BTreeMap<TargetId, Vec<_>>>::new();
    if !kind.is_ide() {
        for target in &result.targets {
            if let Some(entry) = model.target(target.id) {
                targets
                    .entry(entry.dir)
                    .or_default()
                    .entry(target.id)
                    .or_default()
                    .push(target);
            }
        }
    }
    let mut installs = BTreeMap::<_, Vec<_>>::new();
    for (index, rule) in model.installs.iter().enumerate() {
        installs
            .entry(rule.dir)
            .or_default()
            .push((index, &rule.kind));
    }
    let mut errors = String::new();
    for (dir, _) in model.dirs() {
        if let Some(targets) = targets.get(&dir) {
            for configs in targets.values() {
                for target in configs {
                    check_scoped_rules(model, dir, target, kind, &mut errors);
                }
            }
        }
        if !model
            .definition(dir, b"CMAKE_SKIP_INSTALL_RULES")
            .is_some_and(|value| is_on(value))
            && let Some(rules) = installs.get(&dir)
        {
            for &(index, rule) in rules {
                check_install_export_set(model, index, rule, &mut errors);
            }
        }
    }
    if errors.is_empty() {
        return false;
    }
    host.write_out(Stream::Stderr, errors.as_bytes());
    true
}

fn check_install_export_set(
    model: &ProjectModel,
    index: usize,
    kind: &InstallKind,
    errors: &mut String,
) {
    let (signature, set) = match kind {
        InstallKind::Export { set, .. } => ("EXPORT", set.as_slice()),
        InstallKind::PackageInfo(spec) => ("PACKAGE_INFO", spec.export.as_slice()),
        InstallKind::Code(code) if code.is_empty() => {
            let args = &model.global.props;
            let key = format!("_RCMAKE_INSTALL_{index}_ARG0");
            if !args
                .get(key.as_bytes())
                .is_some_and(|name| name == "EXPORT_ANDROID_MK")
            {
                return;
            }
            let key = format!("_RCMAKE_INSTALL_{index}_ARG1");
            (
                "EXPORT_ANDROID_MK",
                args.get(key.as_bytes())
                    .map_or(b"".as_slice(), |name| name.as_slice()),
            )
        }
        _ => return,
    };
    if model
        .exports
        .get(set)
        .is_none_or(|exports| exports.install_rules.is_empty() && exports.targets.is_empty())
    {
        let set = BString::from(set);
        let _ = writeln!(
            errors,
            "CMake Error: INSTALL({signature}) given unknown export \"{set}\""
        );
    }
}

fn check_scoped_rules(
    model: &ProjectModel,
    dir: DirId,
    target: &ResolvedTarget,
    kind: GeneratorKind,
    errors: &mut String,
) {
    let check = |language: &BString,
                 suffix: &str,
                 alternative: Option<&str>,
                 errors: &mut String| {
        let key = format!("CMAKE_{language}_{suffix}");
        let alternative = alternative.map(|name| format!("CMAKE_{language}_{name}"));
        let defined = |scope| {
            model.definition(scope, key.as_bytes()).is_some()
                || alternative
                    .as_ref()
                    .is_some_and(|name| model.definition(scope, name.as_bytes()).is_some())
        };
        // CMP0220 OLD leaves language rules in the directory that enabled the language.
        if !defined(dir)
            && model.languages().enabled.contains(language)
            && model.dirs().any(|(dir, _)| defined(dir))
        {
            let _ = writeln!(
                errors,
                "CMake Error: Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\n{key}"
            );
        }
    };
    let makefiles = kind == GeneratorKind::UnixMakefiles;
    let languages = target.compile_units.iter().filter_map(|unit| {
        target
            .compile_groups
            .get(unit.group)
            .map(|group| &group.language)
    });
    if makefiles {
        for language in languages {
            check(language, "COMPILE_OBJECT", None, errors);
        }
    } else {
        for language in languages.collect::<BTreeSet<_>>() {
            check(language, "COMPILE_OBJECT", None, errors);
        }
    }
    if let Some(link) = &target.link {
        if makefiles {
            let suffix = match link.kind {
                LinkKind::Executable => "LINK_EXECUTABLE",
                LinkKind::SharedLibrary => "CREATE_SHARED_LIBRARY",
                LinkKind::ModuleLibrary => "CREATE_SHARED_MODULE",
                LinkKind::StaticArchive => "CREATE_STATIC_LIBRARY",
            };
            let alternative = (link.kind == LinkKind::StaticArchive).then_some("ARCHIVE_CREATE");
            check(&link.language, suffix, alternative, errors);
        } else if link.kind == LinkKind::StaticArchive
            && model
                .definition(
                    dir,
                    format!("CMAKE_{}_CREATE_STATIC_LIBRARY", link.language).as_bytes(),
                )
                .is_none()
        {
            check(&link.language, "ARCHIVE_CREATE", None, errors);
            check(&link.language, "ARCHIVE_FINISH", None, errors);
        }
    }
}
