//! Shared paths for file sets and generated files.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    BuildGraph, BuildStep, ResolveError,
    usage::{UsageResolver, handle},
};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_genex::{ConfigId, EvalContext, PreprocessMode, evaluate, parse, strip};
use rcmake_host::{FileKind, Follow, Host, path::to_native};
use rcmake_model::{FileSet, InstallOptions, ProjectModel, Target};

/// Get the install options for a named file set in one install rule.
pub fn options_for_set(model: &ProjectModel, index: usize, name: &[u8]) -> Option<InstallOptions> {
    let prefix = format!("_RCMAKE_INSTALL_{index}_ARG");
    let mut args = (0..)
        .map_while(|i| model.global.props.get(format!("{prefix}{i}").as_bytes()))
        .map(|value| value.as_slice())
        .peekable();
    loop {
        args.find(|arg| *arg == b"FILE_SET")?;
        if args.next()? == name {
            break;
        }
    }
    let mut options = model.installs.get(index)?.options.clone();
    while let Some(key) = args.next() {
        match key {
            b"DESTINATION" => options.destination = Some(args.next()?.into()),
            b"COMPONENT" => options.component = Some(args.next()?.into()),
            b"OPTIONAL" => options.optional = true,
            b"EXCLUDE_FROM_ALL" => options.exclude_from_all = true,
            b"PERMISSIONS" => {
                options.permissions = std::iter::from_fn(|| args.next_if(|arg| !keyword(arg)))
                    .map(BString::from)
                    .collect();
            }
            b"CONFIGURATIONS" => {
                options.configurations = std::iter::from_fn(|| args.next_if(|arg| !keyword(arg)))
                    .map(BString::from)
                    .collect();
            }
            _ => break,
        }
    }
    Some(options)
}

fn keyword(value: &[u8]) -> bool {
    matches!(
        value,
        b"ARCHIVE"
            | b"LIBRARY"
            | b"RUNTIME"
            | b"FRAMEWORK"
            | b"BUNDLE"
            | b"OBJECTS"
            | b"PUBLIC_HEADER"
            | b"PRIVATE_HEADER"
            | b"RESOURCE"
            | b"FILE_SET"
            | b"CXX_MODULES_BMI"
            | b"INCLUDES"
            | b"DESTINATION"
            | b"COMPONENT"
            | b"PERMISSIONS"
            | b"OPTIONAL"
            | b"EXCLUDE_FROM_ALL"
            | b"CONFIGURATIONS"
    )
}

/// Group file set members by base directory and retain each relative path.
pub fn set_files(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    set: &FileSet,
    config: &ConfigId,
) -> Result<BTreeMap<BString, Vec<(BString, BString)>>, ResolveError> {
    let handle = handle(set.target);
    let context = EvalContext {
        config: config.clone(),
        head: Some(handle),
        current: Some(handle),
        ..EvalContext::default()
    };
    let source = Lexical::new(&model.dir(target.dir).source_dir);
    let mut expand = |items: &[rcmake_model::BT<BString>]| {
        let mut paths = Vec::new();
        for item in items {
            let ast = strip(&parse(&item.value)?, PreprocessMode::Build);
            let (value, _) = evaluate(&ast, &context, &mut usage.host(target.dir))?;
            paths.extend(
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|path| Lexical::new(&source.append(&path)).normalize()),
            );
        }
        Ok::<_, ResolveError>(paths)
    };
    let mut bases = expand(&set.base_dirs)?;
    if set.base_dirs.is_empty() {
        bases.push(model.dir(target.dir).source_dir.clone());
    }
    let mut roots: BTreeMap<_, Vec<_>> = bases
        .iter()
        .cloned()
        .map(|base| (base, Vec::new()))
        .collect();
    for file in expand(&set.files)? {
        let (base, relative) = bases
            .iter()
            .find_map(|base| {
                let relative = Lexical::new(&file).relative(Lexical::new(base))?;
                (!matches!(relative.as_slice(), b"." | b"..") && !relative.starts_with(b"../"))
                    .then_some((base, relative))
            })
            .ok_or_else(|| {
                ResolveError::Message(
                    format!(
                        "File \"{file}\" is outside the base directories of file set \"{}\".",
                        set.name
                    )
                    .into(),
                )
            })?;
        let files = roots.entry(base.clone()).or_default();
        if !files.iter().any(|(known, _)| known == &file) {
            files.push((file, relative));
        }
    }
    Ok(roots)
}

fn clang_tidy_fixes_directory(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
) -> Option<BString> {
    let key = [language, b"_CLANG_TIDY_EXPORT_FIXES_DIR"].concat();
    let directory = target.props.get(&key).filter(|value| !value.is_empty())?;
    let path = Lexical::new(&model.dir(target.dir).binary_dir).append(directory);
    Some(Lexical::new(&path).normalize())
}

pub(crate) fn clang_tidy_fixes_file(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    object: &[u8],
) -> Option<BString> {
    let dir = clang_tidy_fixes_directory(model, target, language)?;
    let object = crate::lower::build_relative(model, object);
    let object = Lexical::new(&object)
        .components()
        .filter(|part| !matches!(*part, b"." | b".."))
        .collect::<Vec<_>>()
        .join(&b'/');
    let filename = [object.as_slice(), b".yaml"].concat();
    Some(Lexical::new(&dir).append(&filename))
}

pub(crate) fn clean_lint_fixes(model: &ProjectModel, host: &dyn Host, graphs: &[BuildGraph]) {
    let files: BTreeSet<_> = graphs
        .iter()
        .flat_map(|graph| &graph.steps)
        .filter_map(|step| match step {
            BuildStep::Compile { invocation, .. } => Some(&invocation.argv),
            _ => None,
        })
        .flat_map(|args| {
            args.iter()
                .skip_while(|arg| *arg != b"__run_co_compile")
                .take_while(|arg| *arg != b"--")
                .filter_map(|arg| arg.strip_prefix(b"--tidy="))
        })
        .flat_map(|command| list::iter(command, list::Empty::Drop))
        .filter_map(|arg| arg.strip_prefix(b"--export-fixes=").map(to_native))
        .collect();
    let dirs: BTreeSet<_> = model
        .targets()
        .filter(|(_, target)| !target.is_imported())
        .flat_map(|(_, target)| {
            [b"C".as_slice(), b"CXX", b"OBJC", b"OBJCXX"]
                .into_iter()
                .filter_map(|language| clang_tidy_fixes_directory(model, target, language))
        })
        .map(|dir| to_native(&dir))
        .filter(|dir| files.iter().any(|file| file.starts_with(dir)))
        .collect();
    for dir in dirs {
        let mut pending = vec![(dir, 0)];
        while let Some((dir, depth)) = pending.pop() {
            if depth >= 64
                || !host
                    .metadata(&dir, Follow::No)
                    .is_ok_and(|entry| entry.kind == FileKind::Dir)
            {
                continue;
            }
            let Ok(entries) = host.read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = dir.join(to_native(&entry.name));
                if entry.kind == FileKind::Dir {
                    pending.push((path, depth + 1));
                } else if entry.kind == FileKind::File
                    && entry.name.ends_with(b".yaml")
                    && !files.contains(&path)
                {
                    let _ = host.remove_file(&path);
                }
            }
        }
    }
}
