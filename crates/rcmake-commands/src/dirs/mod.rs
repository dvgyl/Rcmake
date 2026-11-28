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

const LEGACY_SUBDIRS_PROP: &[u8] = b"\0rcmake-legacy-subdirs";

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
                queue_legacy_subdirectory(cx, name, options)?;
            }
        }
    }
    Ok(Flow::Normal)
}

fn queue_legacy_subdirectory(
    cx: &mut Cx<'_>,
    source: &[u8],
    options: SubdirOptions,
) -> Result<(), CmdError> {
    if cx.running_deferred() {
        cx.issue(
            MessageKind::Error,
            Effect::SetError,
            None,
            "Subdirectories may not be created during deferred execution.".into(),
        );
        return Err(CmdError::Reported);
    }
    let (src, bin) = resolve_subdirectory_paths(cx, source, None)?;
    let src_bytes = path::from_native_cmake(&src);
    let bin_bytes = path::from_native_cmake(&bin);
    check_unique_binary_dir(cx, &src_bytes, &bin_bytes)?;
    let dir_id = cx.current_dir_id();
    let install_position = cx.project().installs.len();
    let mut entry = Vec::with_capacity(src_bytes.len() + bin_bytes.len() + 24);
    entry.extend_from_slice(&src_bytes);
    entry.push(0);
    entry.extend_from_slice(&bin_bytes);
    entry.push(0);
    entry.push(if options.exclude_from_all { b'1' } else { b'0' });
    entry.push(0);
    entry.extend_from_slice(install_position.to_string().as_bytes());
    entry.push(0);
    cx.project()
        .dir_mut(dir_id)
        .props
        .append_string(LEGACY_SUBDIRS_PROP.into(), &entry);
    Ok(())
}

fn resolve_subdirectory_paths(
    cx: &mut Cx<'_>,
    source: &[u8],
    binary: Option<&[u8]>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), CmdError> {
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
                        &path::from_native_cmake(&src),
                        b"\" is not a subdirectory of \"",
                        &path::from_native_cmake(&current),
                        b"\".  When specifying an out-of-tree source a binary directory must be explicitly specified.",
                    ]
                    .concat(),
                )
            })?;
            normalized_path(&base.join(relative))
        }
    };
    Ok((src, bin))
}

fn check_unique_binary_dir(
    cx: &mut Cx<'_>,
    source_bytes: &[u8],
    bin_bytes: &[u8],
) -> Result<(), CmdError> {
    let already_used = cx.project().dirs().any(|(_, entry)| {
        entry.binary_dir == bin_bytes
            || entry
                .props
                .get(LEGACY_SUBDIRS_PROP)
                .is_some_and(|raw| legacy_subdirs_reserves(raw.as_slice(), bin_bytes))
    });
    if already_used {
        return Err(crate::command_error(
            cx,
            [
                b"The binary directory\n  ".as_slice(),
                bin_bytes,
                b"\nis already used to build a source directory.  It cannot be used to build source directory\n  ",
                source_bytes,
                b"\nSpecify a unique binary directory name.",
            ]
            .concat(),
        ));
    }
    Ok(())
}

fn legacy_subdirs_reserves(raw: &[u8], bin_bytes: &[u8]) -> bool {
    let mut fields = raw.split(|&byte| byte == 0);
    while let (Some(src), Some(bin), Some(_exclude), Some(_install_pos)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    {
        if !src.is_empty() && bin == bin_bytes {
            return true;
        }
    }
    false
}

fn run_subdirectory(
    cx: &mut Cx<'_>,
    source: &[u8],
    binary: Option<&[u8]>,
    options: SubdirOptions,
) -> CmdResult {
    let (src, bin) = resolve_subdirectory_paths(cx, source, binary)?;
    let bin_bytes = path::from_native_cmake(&bin);
    let source_bytes = path::from_native_cmake(&src);
    check_unique_binary_dir(cx, &source_bytes, &bin_bytes)?;
    cx.run_subdirectory(&src, &bin, options)?;
    Ok(Flow::Normal)
}

pub(crate) fn normalized_path(input: &Path) -> std::path::PathBuf {
    let raw = path::from_native_cmake(input);
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

fn add_definitions(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    non_target(
        cx,
        "target_compile_definitions, target_compile_options, or target_include_directories",
    );
    let bt = cx.backtrace();
    let dir = cx.current_dir_id();
    let directory = cx.project().dir_mut(dir);
    for arg in values(args).filter(|arg| !arg.is_empty()) {
        match definition(arg) {
            Some(value) => directory.usage.push(
                UsageField::CompileDefinitions,
                BT::new(value.into(), bt.clone()),
            ),
            None => add_define_flag(&mut directory.define_flags, arg),
        }
    }
    Ok(Flow::Normal)
}

fn remove_definitions(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let dir = cx.current_dir_id();
    let directory = cx.project().dir_mut(dir);
    for arg in values(args).filter(|arg| !arg.is_empty()) {
        let Some(value) = definition(arg) else {
            remove_define_flag(&mut directory.define_flags, arg);
            continue;
        };
        if let Some(entries) = directory
            .usage
            .fields
            .get_mut(&UsageField::CompileDefinitions)
        {
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
            entries.retain(|entry| !entry.value.is_empty() && entry.value != value);
        }
    }
    Ok(Flow::Normal)
}

/// Append a flag that is not a definition (`s_AddDefineFlag`): one leading space, and
/// `\n` and `\r` become spaces.
fn add_define_flag(flags: &mut BString, flag: &[u8]) {
    flags.push(b' ');
    flags.extend(flag.iter().map(|&byte| {
        if matches!(byte, b'\n' | b'\r') {
            b' '
        } else {
            byte
        }
    }));
}

/// Remove each instance of `flag` that has whitespace or a string end on both sides
/// (`s_RemoveDefineFlag`).
fn remove_define_flag(flags: &mut BString, flag: &[u8]) {
    let mut position = 0;
    while let Some(offset) = flags
        .get(position..)
        .and_then(|rest| rest.windows(flag.len()).position(|window| window == flag))
    {
        let start = position + offset;
        let end = start + flag.len();
        let left = start == 0 || flags.get(start - 1).is_some_and(u8::is_ascii_whitespace);
        let right = flags.get(end).is_none_or(u8::is_ascii_whitespace);
        if left && right {
            flags.drain(start..end);
            position = start;
        } else {
            position = start + 1;
        }
    }
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
            let entry = BT::new(usage_path(cx, value, true), bt.clone());
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
        .map(|arg| BT::new(usage_path(cx, arg, false), bt.clone()))
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

fn usage_path(cx: &Cx<'_>, value: &[u8], preserve_false: bool) -> BString {
    if value.starts_with(b"$<") || (preserve_false && rcmake_base::truth::is_off(value)) {
        return value.into();
    }
    let native = path::to_native(value);
    if native.is_absolute() {
        path::from_native_cmake(&native)
    } else {
        path::from_native_cmake(&cx.resolve_path(value))
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
                opens_lookup_scope: false,
                closes_lookup_scope: false,
                scope: None,
                text: BT::new(lib.into(), bt.clone()),
                lookup_dir: dir,
            }));
    }
    Ok(Flow::Normal)
}

#[cfg(test)]
mod legacy_subdirs_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{PureHost, Stream};
    use rcmake_model::DirId;

    use crate::test_support::TestResult;

    #[test]
    fn subdirs_evaluates_after_parent_listfile_and_preserves_install_position() -> TestResult {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let source = root.join("src");
        let binary = root.join("build");
        let command = root.join("bin/cmake");
        let host = Arc::new(PureHost::new(source.clone(), command.clone()));
        host.seed_file(
            &source.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 3.19)
project(bullet_like NONE)
subdirs(sub)
set(INCLUDE_INSTALL_DIR include/bullet)
install(FILES root.h DESTINATION ${INCLUDE_INSTALL_DIR})
"#,
        )?;
        host.seed_file(
            &source.join("sub/CMakeLists.txt"),
            br#"
if(NOT INCLUDE_INSTALL_DIR STREQUAL "include/bullet")
  message(FATAL_ERROR "Child did not see late INCLUDE_INSTALL_DIR: '${INCLUDE_INSTALL_DIR}'")
endif()
install(FILES sub.h DESTINATION ${INCLUDE_INSTALL_DIR})
"#,
        )?;
        let mut interp = Interp::new(
            InterpConfig::new(
                host.clone(),
                RunMode::Configure,
                root.join("share/cmake-4.5"),
                command,
                source.clone(),
            ),
            crate::register_all,
        );
        let outcome = interp.run_configure(&source, &binary, &[]);
        assert_eq!(
            outcome.exit_code,
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        let mut cx = interp.cx();
        let model = cx.project();
        let [child_id] = model.dir(DirId::ROOT).children.as_slice() else {
            return Err("Expected one child directory".into());
        };
        assert_eq!(model.dir(*child_id).install_position, 0);
        assert_eq!(model.installs.len(), 2);
        assert_eq!(
            model.installs[1].options.destination,
            Some("include/bullet".into())
        );
        Ok(())
    }

    #[test]
    fn subdirs_reserves_binary_dir_before_subsequent_add_subdirectory() -> TestResult {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let source = root.join("src");
        let binary = root.join("build");
        let command = root.join("bin/cmake");
        let host = Arc::new(PureHost::new(source.clone(), command.clone()));
        host.seed_file(
            &source.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 3.19)
project(reserve_like NONE)
subdirs(child)
add_subdirectory(other child)
"#,
        )?;
        host.seed_file(
            &source.join("child/CMakeLists.txt"),
            b"set(CHILD_RAN TRUE CACHE BOOL \"\")\n",
        )?;
        host.seed_file(
            &source.join("other/CMakeLists.txt"),
            b"set(OTHER_RAN TRUE CACHE BOOL \"\")\n",
        )?;
        let mut interp = Interp::new(
            InterpConfig::new(
                host.clone(),
                RunMode::Configure,
                root.join("share/cmake-4.5"),
                command,
                source.clone(),
            ),
            crate::register_all,
        );
        let outcome = interp.run_configure(&source, &binary, &[]);
        assert_ne!(outcome.exit_code, 0);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(
            stderr.contains("add_subdirectory"),
            "expected duplicate error on add_subdirectory, got:\n{stderr}"
        );
        assert!(
            stderr.contains("is already used to build a source directory"),
            "{stderr}"
        );
        assert!(
            stderr.contains("/src/other"),
            "expected duplicate error to name second source dir, got:\n{stderr}"
        );
        assert!(
            !stderr.contains("/src/child"),
            "deferred subdirs(child) should not fail on its own reservation:\n{stderr}"
        );
        let mut cx = interp.cx();
        assert_eq!(cx.var_or_empty(b"OTHER_RAN"), "");
        assert_eq!(cx.project().dirs().count(), 1);
        Ok(())
    }
}
