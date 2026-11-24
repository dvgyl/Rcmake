//! Shared paths for file sets and generated files.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;

use crate::{
    BuildGraph, BuildStep, ResolveError, ResolvedTarget,
    sources::{SourcePaths, properties},
    usage::{UsageResolver, handle},
};
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{Backtrace, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::{FileKind, Follow, Host, path::to_native};
use rcmake_model::{
    BT, DirId, FileSet, FileSetKind, InstallOptions, ProjectModel, Target, UsageScope,
};

/// Collect the configuration and language contexts for `file(GENERATE)`.
#[must_use]
pub fn file_generate_contexts(model: &ProjectModel, configs: &[ConfigId]) -> Vec<EvalContext> {
    let languages = model
        .global
        .props
        .get(b"ENABLED_LANGUAGES")
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    let no_language = languages.is_empty().then_some(None);
    languages
        .into_iter()
        .map(Some)
        .chain(no_language)
        .flat_map(|language| {
            configs.iter().map(move |config| EvalContext {
                config: config.clone(),
                language: language.clone(),
                ..EvalContext::default()
            })
        })
        .collect()
}

/// Read CMP0070 for a `file(GENERATE)` request.
#[must_use]
pub fn file_generate_policy(model: &ProjectModel, dir: DirId, request: usize) -> PolicyStatus {
    let key = format!("_RCMAKE_FILE_GENERATE_{request}_CMP0070");
    match model
        .global
        .props
        .get(key.as_bytes())
        .map(|value| value.as_slice())
    {
        Some(b"New") => PolicyStatus::New,
        Some(b"Old") => PolicyStatus::Old,
        Some(b"Warn") => PolicyStatus::Warn,
        // Older records only have the directory's final policy map.
        _ => model
            .dir(dir)
            .final_policies
            .as_ref()
            .map_or(PolicyStatus::Warn, |policies| {
                policies.get(PolicyId::new(70))
            }),
    }
}

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
    let context = EvalContext {
        config: config.clone(),
        head: Some(handle(set.target)),
        ..EvalContext::default()
    };
    map_set_files(model, usage, target, set, &context, |_, body| {
        ResolveError::Message(body)
    })
}

pub(crate) fn validate_membership(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    configs: &[ConfigId],
) -> Result<(), ResolveError> {
    let mut usage = UsageResolver::new(model).with_ide(paths.ide);
    let has_interface_sources = model
        .targets()
        .flat_map(|(_, target)| &target.file_sets)
        .filter_map(|&id| model.file_set(id))
        .any(|set| set.kind == FileSetKind::Sources && set.scope != UsageScope::Private);
    let report = |backtrace: &Backtrace, body: BString| {
        paths.emit(
            model,
            MessageKind::Error,
            Location::Backtrace(backtrace.clone()),
            body.clone(),
        );
        ResolveError::Reported(body)
    };
    for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        for config in configs {
            let context = EvalContext {
                config: config.clone(),
                head: Some(handle(id)),
                ..EvalContext::default()
            };
            for set in target
                .file_sets
                .iter()
                .filter_map(|&id| model.file_set(id))
                .filter(|set| set.scope != UsageScope::Interface)
            {
                map_set_files(model, &mut usage, target, set, &context, report)?;
            }
            if has_interface_sources && target.kind.compiles_objects() {
                for (_, set) in usage.source_file_sets(id, &context)? {
                    if set.target != id {
                        map_set_files(model, &mut usage, target, set, &context, report)?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_source_includes(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    resolved: &ResolvedTarget,
) -> Result<(), ResolveError> {
    let dir = model.dir(target.dir);
    for unit in &resolved.compile_units {
        let Some(group) = resolved.compile_groups.get(unit.group) else {
            continue;
        };
        let Some(raw) = properties(dir, &unit.source.value).get(b"INCLUDE_DIRECTORIES") else {
            continue;
        };
        let context = EvalContext {
            config: resolved.config.clone(),
            language: Some(group.language.clone()),
            head: Some(handle(resolved.id)),
            ..EvalContext::default()
        };
        let includes = usage.file_set_paths(
            resolved.id,
            b"INCLUDE_DIRECTORIES",
            &[BT::new(raw.clone(), unit.source.backtrace.clone())],
            &context,
        )?;
        if let Some(relative) = includes
            .iter()
            .find(|path| !Lexical::new(&path.value).absolute())
        {
            let root = Lexical::new(&model.dir(DirId::ROOT).source_dir);
            let source = Lexical::new(&dir.source_dir);
            let file = Lexical::new(&source.append(b"CMakeLists.txt"))
                .relative(root)
                .unwrap_or_else(|| source.append(b"CMakeLists.txt"));
            let filename = Lexical::new(&unit.source.value).filename();
            return Err(ResolveError::Message(format!(
                "CMake Error in {file}:\n  Found relative path while evaluating include directories of \"{}\":\n\n    \"{}\"\n",
                BString::from(filename), relative.value
            ).into()));
        }
    }
    Ok(())
}

fn map_set_files(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    target: &Target,
    set: &FileSet,
    context: &EvalContext,
    error: impl Fn(&Backtrace, BString) -> ResolveError,
) -> Result<BTreeMap<BString, Vec<(BString, BString)>>, ResolveError> {
    let source = Lexical::new(&model.dir(target.dir).source_dir);
    let mut bases = IndexMap::<BString, BString>::new();
    if set.base_dirs.is_empty() {
        let dir = &model.dir(target.dir).source_dir;
        bases.insert(Lexical::new(dir).normalize(), dir.clone());
    }
    for base in usage.file_set_paths(set.target, b"BASE_DIRS", &set.base_dirs, context)? {
        let path = source.append(&base.value);
        let normalized = Lexical::new(&path).normalize();
        for (prior, spelling) in &bases {
            if relative_member(prior, &normalized).is_some()
                || relative_member(&normalized, prior).is_some()
            {
                return Err(error(
                    &base.backtrace,
                    format!("Base directories in file set cannot be subdirectories of each other:\n\n  {spelling}\n  {path}").into(),
                ));
            }
        }
        bases.entry(normalized).or_insert(path);
    }
    let mut roots: BTreeMap<_, Vec<_>> = bases
        .keys()
        .cloned()
        .map(|base| (base, Vec::new()))
        .collect();
    for file in usage.file_set_paths(set.target, b"SOURCES", &set.files, context)? {
        let path = source.append(&file.value);
        let normalized = Lexical::new(&path).normalize();
        let (base, relative) = bases
            .keys()
            .find_map(|base| relative_member(base, &normalized).map(|relative| (base, relative)))
            .ok_or_else(|| {
                let mut body = BString::from(format!(
                    "File:\n\n  {path}\n\nmust be in one of the file set's base directories:\n"
                ));
                for base in bases.values() {
                    body.extend_from_slice(b"\n  ");
                    body.extend_from_slice(base);
                }
                error(&file.backtrace, body)
            })?;
        let files = roots.entry(base.clone()).or_default();
        if !files.iter().any(|(known, _)| known == &normalized) {
            files.push((normalized, relative));
        }
    }
    Ok(roots)
}

fn relative_member(base: &[u8], path: &[u8]) -> Option<BString> {
    let relative = Lexical::new(path).relative(Lexical::new(base))?;
    (!matches!(relative.as_slice(), b"." | b"..") && !relative.starts_with(b"../"))
        .then_some(relative)
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
