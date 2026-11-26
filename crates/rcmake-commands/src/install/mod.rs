//! Configure-time install rules and export requests.

#![forbid(unsafe_code)]

use std::iter::Peekable;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_model::{
    ExportSet, InstallKind, InstallOptions, InstallRule, TargetBinding, TargetId, TargetKind,
};

mod android_mk;
mod apple;
mod components;
mod destinations;
mod export;
pub(crate) use components::registered;
mod export_options;
mod mapped_package;
mod metadata;
mod package_dependencies;
mod paths;
mod runtime;
mod targets;

type Result<T> = std::result::Result<T, CmdError>;
type Args<'a> = Peekable<std::iter::Copied<std::slice::Iter<'a, &'a [u8]>>>;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: false };
    registry.add("install", install, flags);
    registry.add("export", export::command, flags);
}

fn fail(message: impl Into<BString>) -> CmdError {
    CmdError::Status(message.into())
}

fn value<'a>(args: &mut Args<'a>, option: &[u8]) -> Result<&'a [u8]> {
    args.next_if(|token| !keyword(token)).ok_or_else(|| {
        fail(format!(
            "option {} requires a value.",
            String::from_utf8_lossy(option)
        ))
    })
}

fn artifact(token: &[u8]) -> bool {
    matches!(
        token,
        b"ARCHIVE"
            | b"LIBRARY"
            | b"RUNTIME"
            | b"OBJECTS"
            | b"FRAMEWORK"
            | b"BUNDLE"
            | b"PRIVATE_HEADER"
            | b"PUBLIC_HEADER"
            | b"RESOURCE"
            | b"FILE_SET"
            | b"CXX_MODULES_BMI"
    )
}

fn keyword(token: &[u8]) -> bool {
    artifact(token)
        || matches!(
            token,
            b"EXPORT"
                | b"DESTINATION"
                | b"TYPE"
                | b"PERMISSIONS"
                | b"FILE_PERMISSIONS"
                | b"DIRECTORY_PERMISSIONS"
                | b"CONFIGURATIONS"
                | b"COMPONENT"
                | b"NAMELINK_COMPONENT"
                | b"OPTIONAL"
                | b"EXCLUDE_FROM_ALL"
                | b"NAMELINK_ONLY"
                | b"NAMELINK_SKIP"
                | b"INCLUDES"
                | b"RENAME"
                | b"USE_SOURCE_PERMISSIONS"
                | b"MESSAGE_NEVER"
                | b"FILES_MATCHING"
                | b"PATTERN"
                | b"REGEX"
                | b"EXCLUDE"
                | b"ALL_COMPONENTS"
                | b"NAMESPACE"
                | b"FILE"
                | b"EXPORT_LINK_INTERFACE_LIBRARIES"
                | b"EXPORT_PACKAGE_DEPENDENCIES"
                | b"CXX_MODULES_DIRECTORY"
                | b"APPEND"
                | b"RUNTIME_DEPENDENCY_SET"
                | b"RUNTIME_DEPENDENCIES"
                | b"RUNTIME_DEPENDENCY_TARGETS"
                | b"BUILD_DEPENDS"
        )
        || runtime::filter_keyword(token)
}

fn values<'a>(args: &mut Args<'a>) -> Vec<&'a [u8]> {
    std::iter::from_fn(|| args.next_if(|token| !keyword(token))).collect()
}

fn permissions(args: &mut Args<'_>) -> Result<Vec<BString>> {
    values(args)
        .into_iter()
        .map(|token| {
            crate::file_path::permission(token)
                .map(|_| token.into())
                .ok_or_else(|| fail([b"given invalid permission \"", token, b"\"."].concat()))
        })
        .collect()
}

fn common(
    cx: &Cx<'_>,
    signature: &[u8],
    options: &mut InstallOptions,
    key: &[u8],
    args: &mut Args<'_>,
) -> Result<bool> {
    match key {
        b"DESTINATION" => {
            let raw = value(args, key)?;
            let path = if signature == b"DIRECTORY" {
                normalized_destination(cx, raw)
            } else {
                destination(cx, raw)
            };
            options.destination = (signature != b"TARGETS" || !path.is_empty()).then_some(path);
        }
        b"COMPONENT" => options.component = Some(value(args, key)?.into()),
        b"PERMISSIONS" if signature != b"DIRECTORY" => options.permissions = permissions(args)?,
        b"FILE_PERMISSIONS" if signature == b"DIRECTORY" => {
            options.permissions = permissions(args)?
        }
        b"CONFIGURATIONS" => {
            options.configurations = values(args).into_iter().map(BString::from).collect()
        }
        b"OPTIONAL" if signature != b"EXPORT" => options.optional = true,
        b"EXCLUDE_FROM_ALL" => options.exclude_from_all = true,
        b"BUILD_DEPENDS" if matches!(signature, b"FILES" | b"PROGRAMS" | b"DIRECTORY") => {
            options
                .build_depends
                .extend(values(args).into_iter().map(BString::from));
        }
        b"RENAME" if matches!(signature, b"FILES" | b"PROGRAMS") => {
            options.rename = Some(value(args, key)?.into())
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Parse an argument-group `DESTINATION` (`cmInstallCommandArguments::Finalize`).
fn destination(cx: &Cx<'_>, path: &[u8]) -> BString {
    convert_to_unix_slashes(&normalized_destination(cx, path))
}

/// `cmSystemTools::ConvertToUnixSlashes` without home expansion: use `/` separators,
/// merge doubled separators after a leading `//`, and remove one trailing `/`.
fn convert_to_unix_slashes(path: &[u8]) -> BString {
    let mut out = BString::default();
    for (index, &byte) in path.iter().enumerate() {
        let byte = if byte == b'\\' { b'/' } else { byte };
        if byte == b'/' && index > 1 && out.last() == Some(&b'/') {
            continue;
        }
        out.push(byte);
    }
    let drive_root = out.len() == 3 && out.get(1) == Some(&b':');
    if out.len() > 1 && out.last() == Some(&b'/') && !drive_root {
        out.pop();
    }
    out
}

fn normalized_destination(cx: &Cx<'_>, path: &[u8]) -> BString {
    if path.is_empty() || cx.policy(PolicyId::new(177)) != PolicyStatus::New {
        path.into()
    } else if path.windows(2).any(|window| window == b"$<") {
        [b"$<PATH:CMAKE_PATH,NORMALIZE,", path, b">"]
            .concat()
            .into()
    } else {
        Lexical::new(path).normalize()
    }
}

fn type_destination(cx: &Cx<'_>, kind: &[u8]) -> Result<BString> {
    let (var, default): (&[u8], &[u8]) = match kind {
        b"BIN" => (b"BINDIR", b"bin"),
        b"SBIN" => (b"SBINDIR", b"sbin"),
        b"LIB" => (b"LIBDIR", b"lib"),
        b"INCLUDE" => (b"INCLUDEDIR", b"include"),
        b"SYSCONF" => (b"SYSCONFDIR", b"etc"),
        b"SHAREDSTATE" => (b"SHAREDSTATEDIR", b"com"),
        b"LOCALSTATE" => (b"LOCALSTATEDIR", b"var"),
        b"RUNSTATE" => (b"RUNSTATEDIR", b"var/run"),
        b"DATA" => (b"DATADIR", b""),
        b"INFO" => (b"INFODIR", b"info"),
        b"LOCALE" => (b"LOCALEDIR", b"locale"),
        b"MAN" => (b"MANDIR", b"man"),
        b"DOC" => (b"DOCDIR", b"doc"),
        b"LIBEXEC" => (b"LIBEXECDIR", b"libexec"),
        _ => return Err(fail([b"given invalid TYPE \"", kind, b"\"."].concat())),
    };
    let configured = cx.var_or_empty(&[b"CMAKE_INSTALL_", var].concat());
    if !configured.is_empty() {
        return Ok(configured);
    }
    if kind == b"RUNSTATE" {
        let mut local = type_destination(cx, b"LOCALSTATE")?;
        local.extend_from_slice(b"/run");
        return Ok(local);
    }
    let mut root = cx.var_or_empty(b"CMAKE_INSTALL_DATAROOTDIR");
    if root.is_empty() {
        root = b"share".as_slice().into();
    }
    Ok(match kind {
        b"DATA" => root,
        b"DOC" | b"INFO" | b"LOCALE" | b"MAN" => [root.as_slice(), b"/", default].concat().into(),
        _ => default.into(),
    })
}

fn install(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    let input: Vec<_> = input.iter().map(|arg| arg.value.as_slice()).collect();
    let [signature, rest @ ..] = input.as_slice() else {
        return Ok(Flow::Normal);
    };
    if *signature == b"IMPORTED_RUNTIME_ARTIFACTS" {
        return runtime::imported(cx, rest);
    }
    if *signature == b"RUNTIME_DEPENDENCY_SET" {
        return runtime::set(cx, rest);
    }
    if matches!(*signature, b"CODE" | b"SCRIPT") {
        return scripts(cx, signature, rest);
    }
    destinations::warn_unnormalized(cx, rest);
    let mut args = rest.iter().copied().peekable();
    let mut options = InstallOptions::default();
    let mut extra_destinations = Vec::new();
    let mut library_namelink_only = false;
    let mut target_components = None;
    let kind = match *signature {
        b"TARGETS" => {
            let (kind, namelink_only, components) =
                targets::parse(cx, &mut args, &mut options, &mut extra_destinations)?;
            library_namelink_only = namelink_only;
            target_components = Some(components);
            kind
        }
        b"FILES" | b"PROGRAMS" | b"DIRECTORY" => {
            paths::parse(cx, signature, &mut args, &mut options)?
        }
        b"EXPORT" => install_export(cx, &mut args, &mut options)?,
        b"EXPORT_ANDROID_MK" => android_mk::parse(cx, &mut args, &mut options)?,
        b"PACKAGE_INFO" | b"SBOM" => metadata::install(cx, signature, rest, &mut options)?,
        _ => {
            return Err(fail(
                [b"does not recognize sub-command ", *signature].concat(),
            ));
        }
    };
    if matches!(&kind, InstallKind::Targets { targets, .. } if targets.is_empty())
        || matches!(&kind, InstallKind::Files(files) | InstallKind::Programs(files) if files.is_empty())
        || matches!(&kind, InstallKind::Directory(dirs) if dirs.is_empty() && options.destination.is_none())
    {
        return Ok(Flow::Normal);
    }
    if options
        .component
        .as_ref()
        .is_none_or(|name| name.is_empty())
    {
        options.component = Some(default_component(cx));
    }
    let destinations = extra_destinations
        .iter()
        .filter_map(|(_, path, bmi)| (!bmi).then_some(path));
    let absolute_destinations = options
        .destination
        .iter()
        .chain(destinations)
        .filter(|_| *signature != b"EXPORT_ANDROID_MK")
        .filter(|path| Lexical::new(path).absolute())
        .cloned()
        .collect::<Vec<_>>();
    for path in absolute_destinations {
        warn_absolute_destination(cx, &path);
    }
    let mapped = if let InstallKind::Export { set, .. } = &kind {
        mapped_package::rules(cx, set, &options)?
    } else {
        Vec::new()
    };
    let members = match &kind {
        InstallKind::Targets {
            targets,
            export: Some(set),
        } => {
            let model = cx.project();
            let targets = targets
                .iter()
                .copied()
                .filter(|id| {
                    !library_namelink_only
                        || model.target(*id).is_some_and(|target| {
                            target.kind != TargetKind::SharedLibrary
                                || target
                                    .props
                                    .get(b"FRAMEWORK")
                                    .is_some_and(|value| rcmake_base::truth::is_on(value))
                        })
                })
                .collect::<Vec<_>>();
            Some((set.clone(), targets))
        }
        _ => None,
    };
    let snapshots = if let InstallKind::Targets { targets, .. } = &kind {
        let model = cx.project();
        targets
            .iter()
            .filter_map(|id| {
                model.target(*id).map(|target| {
                    let entries = [
                        b"PRIVATE_HEADER".as_slice(),
                        b"PUBLIC_HEADER",
                        b"RESOURCE",
                        b"FRAMEWORK",
                    ]
                    .map(|key| (key, target.props.get(key).cloned().unwrap_or_default()));
                    (id, entries)
                })
            })
            .map(|(id, entries)| (*id, entries))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let snapshots = destinations::drop_headers_without_destination(
        cx,
        rest,
        options.destination.as_ref(),
        snapshots,
    );
    for rule in mapped {
        cx.project().add_install(rule);
    }
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    let index = cx.project().add_install(InstallRule {
        dir,
        backtrace,
        kind,
        options,
    });
    let section = format!("_RCMAKE_INSTALL_{index}");
    record_args(cx, &section, &input);
    if let Some(components) = target_components {
        cx.global().props.set(
            format!("{section}_COMPONENTS").into(),
            rcmake_base::list::join(components),
        );
    }
    for (id, entries) in snapshots {
        for (key, value) in entries {
            let key = String::from_utf8_lossy(key);
            cx.global()
                .props
                .set(format!("{section}_{key}_{}", id.index()).into(), value);
        }
    }
    let absolute_action = cx
        .diag_state()
        .get(DiagCategory::InstallAbsoluteDestination);
    cx.global().props.set(
        format!("{section}_ABSOLUTE_ACTION").into(),
        absolute_action.as_str().into(),
    );
    let author_action = cx.diag_state().get(DiagCategory::Author);
    cx.global().props.set(
        format!("{section}_AUTHOR_ACTION").into(),
        author_action.as_str().into(),
    );
    let message = cx.var_or_empty(b"CMAKE_INSTALL_MESSAGE");
    if matches!(message.as_slice(), b"ALWAYS" | b"LAZY" | b"NEVER") {
        cx.global()
            .props
            .set(format!("{section}_MESSAGE").into(), message);
    }
    for (argument, path, _) in extra_destinations {
        cx.global()
            .props
            .set(format!("{section}_ARG{argument}").into(), path);
    }
    if let Some((name, targets)) = members {
        let set = ensure_export_set(cx, &name);
        set.targets.extend(targets);
        set.install_rules.push(index);
    }
    Ok(Flow::Normal)
}

fn ensure_export_set<'a>(cx: &'a mut Cx<'_>, name: &[u8]) -> &'a mut ExportSet {
    cx.project()
        .exports
        .entry(name.into())
        .or_insert_with(|| ExportSet {
            name: name.into(),
            ..ExportSet::default()
        })
}

fn warn_absolute_destination(cx: &mut Cx<'_>, path: &[u8]) {
    cx.issue(
        MessageKind::Warning,
        Effect::None,
        Some(DiagCategory::InstallAbsoluteDestination),
        [
            b"INSTALL command given absolute DESTINATION path:\n  ",
            path,
        ]
        .concat()
        .into(),
    );
}

pub(crate) fn default_component(cx: &mut Cx<'_>) -> BString {
    let name = cx.var(b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME");
    let name = if name.as_ref().is_some_and(|name| name == b"<PROJECT_NAME>") {
        cx.var(b"PROJECT_NAME")
    } else {
        name
    };
    name.filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Unspecified".into())
}

fn scripts(cx: &mut Cx<'_>, signature: &[u8], rest: &[&[u8]]) -> CmdResult {
    let mut args = rest.iter().copied().peekable();
    let mut options = InstallOptions::default();
    let mut blocks = Vec::new();
    let mut shared = Vec::new();
    let mut key = signature;
    let payload = |token: &[u8]| {
        !matches!(
            token,
            b"SCRIPT"
                | b"CODE"
                | b"COMPONENT"
                | b"EXCLUDE_FROM_ALL"
                | b"BUILD_DEPENDS"
                | b"ALL_COMPONENTS"
        )
    };
    loop {
        match key {
            b"SCRIPT" | b"CODE" => {
                let input = args.next_if(|token| payload(token)).ok_or_else(|| {
                    let key = BString::from(key);
                    fail(format!(
                        "{key} given invalid argument:\n  {key}: missing required value"
                    ))
                })?;
                let kind = if key == b"SCRIPT" {
                    InstallKind::Script(script_path(cx, input)?)
                } else {
                    InstallKind::Code(input.into())
                };
                blocks.push((key, input, kind));
            }
            b"ALL_COMPONENTS" => {
                options.all_components = true;
                shared.push(key);
            }
            b"COMPONENT" => {
                if let Some(name) = args.next_if(|token| payload(token)) {
                    options.component = Some(name.into());
                    shared.extend([key, name]);
                }
            }
            b"EXCLUDE_FROM_ALL" => {
                options.exclude_from_all = true;
                shared.push(key);
            }
            b"BUILD_DEPENDS" => {
                shared.push(key);
                while let Some(path) = args.next_if(|token| payload(token)) {
                    shared.push(path);
                    options.build_depends.push(path.into());
                }
            }
            _ => {
                return Err(fail(format!(
                    "{} given unknown argument: \"{}\".",
                    BString::from(signature),
                    BString::from(key),
                )));
            }
        }
        let Some(next) = args.next() else { break };
        key = next;
    }
    if options.all_components && options.component.is_some() {
        return Err(fail("ALL_COMPONENTS and COMPONENT are mutually exclusive"));
    }
    options
        .component
        .get_or_insert_with(|| default_component(cx));
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    for (signature, input, kind) in blocks {
        let index = cx.project().add_install(InstallRule {
            dir,
            backtrace: backtrace.clone(),
            kind,
            options: options.clone(),
        });
        let mut recorded = vec![signature, input];
        recorded.extend_from_slice(&shared);
        record_args(cx, &format!("_RCMAKE_INSTALL_{index}"), &recorded);
    }
    Ok(Flow::Normal)
}

fn script_path(cx: &Cx<'_>, input: &[u8]) -> Result<BString> {
    if input.starts_with(b"$<INSTALL_PREFIX>") {
        return Ok(input.into());
    }
    let file = cx.current_source_dir().join(to_native(input));
    if cx
        .host()
        .metadata(&file, rcmake_host::Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == rcmake_host::FileKind::Dir)
    {
        return Err(fail("given a directory as value of SCRIPT argument."));
    }
    Ok(from_native_cmake(&file))
}

fn install_export(
    cx: &mut Cx<'_>,
    args: &mut Args<'_>,
    options: &mut InstallOptions,
) -> Result<InstallKind> {
    let packages = package_dependencies::enabled(cx);
    let set = value(args, b"EXPORT")?;
    let mut namespace = BString::default();
    let mut file = None;
    let mut cxx_modules_directory = None;
    while let Some(key) = args.next() {
        match key {
            b"NAMESPACE" => {
                namespace = args
                    .next_if(|token| !keyword(token))
                    .unwrap_or_default()
                    .into()
            }
            b"FILE" => {
                let name = value(args, key)?;
                check_file(name)?;
                if name.contains(&b'/') || name.contains(&b'\\') {
                    return Err(fail("EXPORT given a FILE option containing a path."));
                }
                file = Some(name.into());
            }
            b"CXX_MODULES_DIRECTORY" => cxx_modules_directory = Some(value(args, key)?.into()),
            b"EXPORT_PACKAGE_DEPENDENCIES" if packages => {
                options.export_package_dependencies = true
            }
            b"EXPORT_LINK_INTERFACE_LIBRARIES" => {}
            _ if common(cx, b"EXPORT", options, key, args)? => {}
            _ => {
                return Err(fail(
                    [b"EXPORT given unknown argument \"", key, b"\"."].concat(),
                ));
            }
        }
    }
    if options
        .destination
        .as_ref()
        .is_none_or(|path| path.is_empty())
    {
        return Err(fail("EXPORT given no DESTINATION!"));
    }
    ensure_export_set(cx, set);
    Ok(InstallKind::Export {
        set: set.into(),
        namespace,
        file,
        cxx_modules_directory,
    })
}

fn target_ids(cx: &mut Cx<'_>, names: Vec<&[u8]>, install: bool) -> Result<Vec<TargetId>> {
    let dir = cx.current_dir_id();
    let project = cx.project();
    names
        .into_iter()
        .map(|name| {
            // CMake 4.5 installs the target that an ALIAS target references.
            let binding = project.lookup_target(dir, name);
            if let Some((id, target)) = binding
                .map(TargetBinding::id)
                .and_then(|id| project.target(id).map(|target| (id, target)))
                .filter(|(_, target)| !target.is_imported())
            {
                if !matches!(target.kind, TargetKind::Utility | TargetKind::GlobalTarget) {
                    return Ok(id);
                }
                let name = BString::from(name);
                return Err(fail(if install {
                    format!("TARGETS given target \"{name}\" which is not built by this project.")
                } else {
                    format!("given custom target \"{name}\" which may not be exported.")
                }));
            }
            let name = BString::from(name);
            Err(fail(if install {
                format!("TARGETS given target \"{name}\" which does not exist.")
            } else {
                format!("given target \"{name}\" which is not built by this project.")
            }))
        })
        .collect()
}

fn check_file(file: &[u8]) -> Result<()> {
    if file.ends_with(b".cmake") {
        Ok(())
    } else {
        Err(fail(
            [
                b"FILE option given filename \"",
                file,
                b"\" which does not have an extension of \".cmake\".",
            ]
            .concat(),
        ))
    }
}

fn unknown(option: &[u8]) -> CmdError {
    fail([b"given unknown argument \"", option, b"\"."].concat())
}

/// Store arguments separately to preserve empty values, semicolons, and binary content.
pub(crate) fn record_args(cx: &mut Cx<'_>, section: &str, args: &[&[u8]]) {
    let directory = cx.current_dir_id().index();
    let props = &mut cx.global().props;
    props.set(
        format!("{section}_DIRECTORY").into(),
        directory.to_string().into(),
    );
    props.set(
        format!("{section}_ARGC").into(),
        args.len().to_string().into(),
    );
    for (index, arg) in args.iter().enumerate() {
        props.set(format!("{section}_ARG{index}").into(), (*arg).into());
    }
}

pub(crate) fn record_request(cx: &mut Cx<'_>, prefix: &str, args: &[&[u8]]) -> usize {
    let key = format!("{prefix}_COUNT");
    let index = cx
        .global()
        .props
        .get(key.as_bytes())
        .and_then(|count| std::str::from_utf8(count).ok())
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or_default();
    record_args(cx, &format!("{prefix}_{index}"), args);
    cx.global()
        .props
        .set(key.into(), index.saturating_add(1).to_string().into());
    index
}

#[cfg(test)]
mod tests;
