//! Commands that configure directories and tests.

#![forbid(unsafe_code)]

use std::path::Path;

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry, SubdirOptions};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, path};
use rcmake_model::{BT, DirId, UsageField, UsageProps};

mod discover;
pub(crate) mod source_groups;
mod tests_commands;

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: false };
    registry.add("add_subdirectory", add_subdirectory, flags);
    crate::compat::project_command!(registry, "subdirs", subdirs);
    registry.add("include_directories", include_directories, flags);
    registry.add("add_definitions", add_definitions, flags);
    registry.add("remove_definitions", remove_definitions, flags);
    crate::compat::project_command!(registry, "add_compile_definitions", add_compile_definitions);
    registry.add("add_compile_options", add_compile_options, flags);
    crate::compat::project_command!(registry, "add_link_options", add_link_options);
    registry.add("link_directories", link_directories, flags);
    registry.add("link_libraries", link_libraries, flags);
    registry.add("source_group", source_groups::command, flags);
    tests_commands::register(registry, flags);
    crate::compat::project_command!(registry, "discover_tests", discover::discover_tests);
}

fn values(args: &[ExpandedArg]) -> impl Iterator<Item = &[u8]> {
    args.iter().map(|arg| arg.value.as_slice())
}

fn add_subdirectory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [source, rest @ ..] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let mut options = SubdirOptions::default();
    if rest.len() > 3 {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    let mut binary = None;
    for arg in values(rest) {
        match arg {
            b"EXCLUDE_FROM_ALL" if !options.exclude_from_all => options.exclude_from_all = true,
            b"SYSTEM" if !options.system => options.system = true,
            b"EXCLUDE_FROM_ALL" | b"SYSTEM" => {
                return Err(CmdError::status(
                    "called with incorrect number of arguments",
                ));
            }
            _ if binary.is_none() => binary = Some(arg),
            _ => {
                return Err(CmdError::status(
                    "called with incorrect number of arguments",
                ));
            }
        }
    }
    run_subdirectory(cx, &source.value, binary, options)
}

fn subdirs(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if args.is_empty() {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    }
    cx.issue_in_directory(
        MessageKind::DeprecationWarning,
        Effect::None,
        Some(DiagCategory::Deprecated),
        "The 'subdirs' command has been superseded.  Use the 'add_subdirectory' command instead."
            .into(),
    );
    let mut options = SubdirOptions::default();
    for name in values(args) {
        match name {
            b"EXCLUDE_FROM_ALL" => options.exclude_from_all = true,
            b"PREORDER" => {}
            _ => {
                run_subdirectory(cx, name, None, options)?;
            }
        }
    }
    Ok(Flow::Normal)
}

fn run_subdirectory(
    cx: &mut Cx<'_>,
    source: &[u8],
    binary: Option<&[u8]>,
    options: SubdirOptions,
) -> CmdResult {
    let src = normalized_path(&cx.resolve_path(source));
    if !cx
        .host()
        .metadata(&src, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return Err(CmdError::status(
            [
                b"given source \"".as_slice(),
                source,
                b"\" which is not an existing directory.",
            ]
            .concat(),
        ));
    }
    let base = cx.current_binary_dir();
    let bin = match binary.filter(|bin| !bin.is_empty()) {
        Some(bin) => normalized_path(&base.join(path::to_native(bin))),
        None => {
            let current = normalized_path(&cx.current_source_dir());
            let relative = src.strip_prefix(&current).ok().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| {
                CmdError::status(
                    [
                        b"not given a binary directory but the given source directory \"".as_slice(),
                        &path::from_native(&src),
                        b"\" is not a subdirectory of \"",
                        &path::from_native(&current),
                        b"\".  When specifying an out-of-tree source a binary directory must be explicitly specified.",
                    ]
                    .concat(),
                )
            })?;
            base.join(relative)
        }
    };
    let bin_bytes = path::from_native(&bin);
    let source_bytes = path::from_native(&src);
    if cx
        .project()
        .dirs()
        .any(|(_, entry)| entry.binary_dir == bin_bytes)
    {
        return Err(crate::command_error(
            cx,
            [
                b"The binary directory\n  ".as_slice(),
                &bin_bytes,
                b"\nis already used to build a source directory.  It cannot be used to build source directory\n  ",
                &source_bytes,
                b"\nSpecify a unique binary directory name.",
            ]
            .concat(),
        ));
    }
    cx.run_subdirectory(&src, &bin, options)?;
    Ok(Flow::Normal)
}

pub(crate) fn normalized_path(input: &Path) -> std::path::PathBuf {
    let raw = path::from_native(input);
    let normalized = Lexical::new(&raw).normalize();
    path::to_native(
        normalized
            .strip_suffix(b"/")
            .filter(|p| !p.is_empty() && !p.ends_with(b":"))
            .unwrap_or(&normalized),
    )
}

pub(crate) fn find_directory(cx: &mut Cx<'_>, name: &[u8]) -> Option<DirId> {
    if name.is_empty() {
        return Some(cx.current_dir_id());
    }
    let requested = normalized_path(&cx.resolve_path(name));
    cx.project()
        .dirs()
        .find(|(_, dir)| {
            requested == path::to_native(&dir.source_dir)
                || requested == path::to_native(&dir.binary_dir)
        })
        .map(|(id, _)| id)
}

pub(crate) fn scoped_test_directory(cx: &mut Cx<'_>, name: &[u8]) -> Result<DirId, CmdError> {
    find_directory(cx, name).ok_or_else(|| {
        CmdError::status([b"given non-existent DIRECTORY ".as_slice(), name].concat())
    })
}

fn non_target(cx: &mut Cx<'_>, alternative: &str) {
    cx.issue(
        MessageKind::Warning,
        Effect::None,
        Some(DiagCategory::NonTargetDirective),
        format!(
            "Use of non-target directives is not recommended.  Consider {alternative} instead."
        )
        .into(),
    );
}

fn definition(arg: &[u8]) -> Option<&[u8]> {
    let value = arg
        .strip_prefix(b"-D")
        .or_else(|| arg.strip_prefix(b"/D"))?;
    let name = value.split(|&b| b == b'=').next()?;
    let (&first, rest) = name.split_first()?;
    ((first.is_ascii_alphabetic() || first == b'_')
        && rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_'))
    .then_some(value)
}

fn definition_usage(arg: &[u8]) -> (UsageField, &[u8]) {
    match definition(arg) {
        Some(def) => (UsageField::CompileDefinitions, def),
        // Keep legacy flags separate from the COMPILE_OPTIONS property.
        None => (UsageField::InterfaceCompileOptions, arg),
    }
}

fn add_definitions(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(
        cx,
        "target_compile_definitions, target_compile_options, or target_include_directories",
    );
    let bt = cx.backtrace();
    let dir = cx.current_dir_id();
    let usage = &mut cx.project().dir_mut(dir).usage;
    for arg in values(args) {
        let (field, value) = definition_usage(arg);
        usage.push(field, BT::new(value.into(), bt.clone()));
    }
    Ok(Flow::Normal)
}

fn remove_definitions(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let dir = cx.current_dir_id();
    let usage = &mut cx.project().dir_mut(dir).usage;
    for arg in values(args) {
        let (field, value) = definition_usage(arg);
        if let Some(entries) = usage.fields.get_mut(&field) {
            if field == UsageField::CompileDefinitions {
                for entry in entries
                    .iter_mut()
                    .filter(|entry| entry.value.contains(&b';'))
                {
                    let mut definitions = list::split(&entry.value, list::Empty::Keep);
                    let count = definitions.len();
                    definitions.retain(|definition| definition != value);
                    if definitions.len() != count {
                        entry.value = list::join(definitions);
                    }
                }
            }
            entries.retain(|entry| !entry.value.is_empty() && entry.value != value);
        }
    }
    Ok(Flow::Normal)
}

fn add_compile_definitions(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(cx, "target_compile_definitions");
    add_usage(cx, UsageField::CompileDefinitions, values(args));
    Ok(Flow::Normal)
}

fn add_compile_options(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(cx, "target_compile_options");
    add_usage(cx, UsageField::CompileOptions, values(args));
    Ok(Flow::Normal)
}

fn add_link_options(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(cx, "target_link_options");
    add_usage(cx, UsageField::LinkOptions, values(args));
    Ok(Flow::Normal)
}

fn add_usage<'a>(cx: &mut Cx<'_>, field: UsageField, values: impl Iterator<Item = &'a [u8]>) {
    let bt = cx.backtrace();
    let dir = cx.current_dir_id();
    let usage = &mut cx.project().dir_mut(dir).usage;
    for value in values.filter(|value| !value.is_empty()) {
        usage.push(field, BT::new(value.into(), bt.clone()));
    }
}

fn include_directories(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(cx, "target_include_directories");
    let (before, args) = placement(args, cx.var_is_on(b"CMAKE_INCLUDE_DIRECTORIES_BEFORE"));
    let mut system = false;
    let mut includes = Vec::new();
    let mut system_includes = Vec::new();
    let bt = cx.backtrace();
    for arg in values(args) {
        if arg == b"SYSTEM" {
            system = true;
            continue;
        }
        for value in arg
            .split(|&b| b == b'\n')
            .map(<[u8]>::trim_ascii)
            .filter(|v| !v.is_empty())
        {
            let entry = BT::new(usage_path(cx, value), bt.clone());
            if system {
                system_includes.push(entry.clone());
            }
            includes.push(entry);
        }
    }
    if before {
        includes.reverse();
        system_includes.reverse();
    }
    record_include_directories(cx, &includes, &system_includes, before);
    Ok(Flow::Normal)
}

pub(crate) fn record_include_directories(
    cx: &mut Cx<'_>,
    includes: &[BT<BString>],
    system_includes: &[BT<BString>],
    before: bool,
) {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let directory = model.dir_mut(dir);
    let targets = directory.targets.clone();
    insert_paths(
        &mut directory.usage,
        UsageField::IncludeDirectories,
        includes,
        before,
    );
    insert_paths(
        &mut directory.usage,
        UsageField::InterfaceSystemIncludeDirectories,
        system_includes,
        before,
    );
    for id in targets {
        if let Some(target) = model.target_mut(id).filter(|target| !target.is_imported()) {
            insert_paths(
                &mut target.usage,
                UsageField::IncludeDirectories,
                includes,
                before,
            );
        }
    }
}

fn link_directories(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(cx, "target_link_directories");
    let (before, args) = placement(args, cx.var_is_on(b"CMAKE_LINK_DIRECTORIES_BEFORE"));
    let bt = cx.backtrace();
    let entries: Vec<_> = values(args)
        .filter(|arg| !arg.is_empty())
        .map(|arg| BT::new(usage_path(cx, arg), bt.clone()))
        .collect();
    let dir = cx.current_dir_id();
    insert_paths(
        &mut cx.project().dir_mut(dir).usage,
        UsageField::LinkDirectories,
        &entries,
        before,
    );
    Ok(Flow::Normal)
}

fn placement(args: &[ExpandedArg], default: bool) -> (bool, &[ExpandedArg]) {
    match args {
        [arg, rest @ ..] if arg.value == b"BEFORE" => (true, rest),
        [arg, rest @ ..] if arg.value == b"AFTER" => (false, rest),
        _ => (default, args),
    }
}

fn usage_path(cx: &Cx<'_>, value: &[u8]) -> BString {
    if value.starts_with(b"$<") || path::to_native(value).is_absolute() {
        value.into()
    } else {
        path::from_native(&cx.resolve_path(value))
    }
}

fn insert_paths(usage: &mut UsageProps, field: UsageField, entries: &[BT<BString>], before: bool) {
    if !entries.is_empty() {
        let paths = usage.fields.entry(field).or_default();
        if before {
            paths.splice(..0, entries.iter().cloned());
        } else {
            paths.extend_from_slice(entries);
        }
    }
}

fn link_libraries(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(cx, "target_link_libraries");
    let mut libs = values(args);
    while let Some(arg) = libs.next() {
        if matches!(arg, b"debug" | b"optimized" | b"general") && libs.next().is_none() {
            return Err(CmdError::status(
                [
                    b"The \"".as_slice(),
                    arg,
                    b"\" argument must be followed by a library",
                ]
                .concat(),
            ));
        }
    }
    if !args.is_empty() {
        let dir = cx.current_dir_id();
        let value = list::join(values(args));
        let bt = cx.backtrace();
        let directory = cx.project().dir_mut(dir);
        directory.props.append("LINK_LIBRARIES".into(), &value);
        directory
            .usage
            .link_libraries
            .get_or_insert_default()
            .extend(values(args).map(|lib| rcmake_model::LinkItem {
                text: BT::new(lib.into(), bt.clone()),
                lookup_dir: dir,
            }));
    }
    Ok(Flow::Normal)
}
