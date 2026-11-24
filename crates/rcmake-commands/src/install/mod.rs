//! Configure-time install rules and export requests.

#![forbid(unsafe_code)]

use std::iter::Peekable;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::{from_native, to_native};
use rcmake_model::{
    BT, ExportSet, InstallKind, InstallOptions, InstallRule, TargetBinding, TargetId, TargetKind,
    UsageScope,
};

mod metadata;

type Result<T> = std::result::Result<T, CmdError>;
type Args<'a> = Peekable<std::iter::Copied<std::slice::Iter<'a, &'a [u8]>>>;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: false };
    registry.add("install", install, flags);
    registry.add("export", export, flags);
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
                | b"CXX_MODULES_DIRECTORY"
                | b"APPEND"
        )
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
        b"DESTINATION" => options.destination = Some(destination(cx, value(args, key)?)),
        b"TYPE" if matches!(signature, b"FILES" | b"PROGRAMS" | b"DIRECTORY") => {
            options.destination = Some(type_destination(cx, value(args, key)?)?);
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
        b"RENAME" if matches!(signature, b"FILES" | b"PROGRAMS") => {
            options.rename = Some(value(args, key)?.into())
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn destination(cx: &Cx<'_>, path: &[u8]) -> BString {
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
    let mut root = cx.var_or_empty(b"CMAKE_INSTALL_DATAROOTDIR");
    if root.is_empty() {
        root = b"share".as_slice().into();
    }
    Ok(match kind {
        b"DATA" => root,
        b"DOC" => [root.as_slice(), b"/doc/", &cx.var_or_empty(b"PROJECT_NAME")]
            .concat()
            .into(),
        b"INFO" | b"LOCALE" | b"MAN" => [root.as_slice(), b"/", default].concat().into(),
        _ => default.into(),
    })
}

fn install(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    let input: Vec<_> = input.iter().map(|arg| arg.value.as_slice()).collect();
    let [signature, rest @ ..] = input.as_slice() else {
        return Ok(Flow::Normal);
    };
    let mut args = rest.iter().copied().peekable();
    let mut options = InstallOptions::default();
    let mut extra_destinations = Vec::new();
    let kind = match *signature {
        b"TARGETS" => targets(cx, &mut args, &mut options, &mut extra_destinations)?,
        b"FILES" | b"PROGRAMS" | b"DIRECTORY" => paths(cx, signature, &mut args, &mut options)?,
        b"CODE" | b"SCRIPT" => script(cx, signature, &mut args, &mut options)?,
        b"EXPORT" => install_export(cx, &mut args, &mut options)?,
        b"PACKAGE_INFO" | b"SBOM" => metadata::install(cx, signature, rest, &mut options)?,
        _ => {
            return Err(fail(
                [b"does not recognize sub-command ", *signature].concat(),
            ));
        }
    };
    if matches!(&kind, InstallKind::Targets { targets, .. } if targets.is_empty())
        || matches!(&kind, InstallKind::Files(files) | InstallKind::Programs(files) if files.is_empty())
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
    for path in options
        .destination
        .iter()
        .chain(extra_destinations.iter().map(|(_, path)| path))
    {
        if Lexical::new(path).absolute() {
            cx.issue(
                MessageKind::Warning,
                Effect::None,
                Some(DiagCategory::InstallAbsoluteDestination),
                [
                    b"An absolute installation destination was given: ",
                    path.as_slice(),
                ]
                .concat()
                .into(),
            );
        }
    }
    let members = match &kind {
        InstallKind::Targets {
            targets,
            export: Some(set),
        } => Some((set.clone(), targets.clone())),
        _ => None,
    };
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
    for (argument, path) in extra_destinations {
        cx.global()
            .props
            .set(format!("{section}_ARG{argument}").into(), path);
    }
    if let Some((name, targets)) = members {
        let set = cx
            .project()
            .exports
            .entry(name.clone())
            .or_insert_with(|| ExportSet {
                name,
                ..ExportSet::default()
            });
        set.targets.extend(targets);
        set.install_rules.push(index);
    }
    Ok(Flow::Normal)
}

pub(crate) fn default_component(cx: &mut Cx<'_>) -> BString {
    cx.var(b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Unspecified".into())
}

fn targets(
    cx: &mut Cx<'_>,
    args: &mut Args<'_>,
    options: &mut InstallOptions,
    destinations: &mut Vec<(usize, BString)>,
) -> Result<InstallKind> {
    let argument_count = args.len() + 1;
    let targets = target_ids(cx, values(args), b"TARGETS")?;
    let mut export = None;
    let mut artifact_options = InstallOptions::default();
    let mut destination_argument = None;
    let mut group: Option<&[u8]> = None;
    let mut namelink = None;
    let mut file_sets = Vec::new();
    while let Some(key) = args.next() {
        match key {
            b"EXPORT" if group.is_none() => export = Some(value(args, key)?.into()),
            b"INCLUDES" => {
                if args.next() != Some(b"DESTINATION") {
                    return Err(fail("TARGETS INCLUDES requires DESTINATION."));
                }
                options
                    .includes_destination
                    .extend(values(args).into_iter().map(BString::from));
            }
            b"NAMELINK_COMPONENT" | b"NAMELINK_ONLY" | b"NAMELINK_SKIP" => {
                if !matches!(group, Some(b"LIBRARY" | b"ARCHIVE")) {
                    return Err(fail(
                        [
                            b"TARGETS given ",
                            key,
                            b" option outside LIBRARY or ARCHIVE.",
                        ]
                        .concat(),
                    ));
                }
                if key == b"NAMELINK_COMPONENT" {
                    value(args, key)?;
                } else if namelink
                    .replace(key)
                    .is_some_and(|previous| previous != key)
                {
                    return Err(fail("TARGETS given both NAMELINK_ONLY and NAMELINK_SKIP."));
                }
            }
            key if artifact(key) => {
                destinations.extend(
                    destination_argument
                        .take()
                        .zip(artifact_options.destination.take()),
                );
                artifact_options = InstallOptions::default();
                group = Some(key);
                namelink = None;
                if key == b"FILE_SET" {
                    let name = args
                        .next_if(|token| !token.is_empty() && !keyword(token))
                        .ok_or_else(|| {
                            fail("TARGETS given FILE_SET option without file set name.")
                        })?;
                    file_sets.push(name);
                }
            }
            _ => {
                if group.is_some() && key == b"DESTINATION" {
                    destination_argument = Some(argument_count - args.len());
                }
                let options = if group.is_some() {
                    &mut artifact_options
                } else {
                    &mut *options
                };
                if !common(cx, b"TARGETS", options, key, args)? {
                    return Err(unknown(key));
                }
            }
        }
    }
    destinations.extend(destination_argument.zip(artifact_options.destination));
    if export.is_some() {
        require_exported_file_sets(cx, &targets, &file_sets)?;
    }
    Ok(InstallKind::Targets { targets, export })
}

fn require_exported_file_sets(
    cx: &mut Cx<'_>,
    targets: &[TargetId],
    file_sets: &[&[u8]],
) -> Result<()> {
    let project = cx.project();
    for target in targets.iter().filter_map(|id| project.target(*id)) {
        if target
            .file_sets
            .iter()
            .filter_map(|id| project.file_set(*id))
            .any(|set| {
                set.scope != UsageScope::Private && !file_sets.contains(&set.name.as_slice())
            })
        {
            return Err(fail(
                [
                    b"TARGETS target ",
                    target.name.as_slice(),
                    b" is exported but not all of its interface file sets are installed",
                ]
                .concat(),
            ));
        }
    }
    Ok(())
}

fn paths(
    cx: &mut Cx<'_>,
    signature: &[u8],
    args: &mut Args<'_>,
    options: &mut InstallOptions,
) -> Result<InstallKind> {
    let inputs = values(args);
    let mut destination_key = None;
    let mut rule = false;
    while let Some(key) = args.next() {
        if matches!(key, b"TYPE" | b"DESTINATION")
            && destination_key
                .replace(key)
                .is_some_and(|previous| previous != key)
        {
            return Err(fail("TYPE and DESTINATION are mutually exclusive."));
        }
        match key {
            b"PATTERN" | b"REGEX" if signature == b"DIRECTORY" => {
                let expression = value(args, key)?;
                if key == b"REGEX" {
                    cx.regex(expression).map_err(|err| fail(err.to_string()))?;
                }
                rule = true;
            }
            b"EXCLUDE" if rule => {}
            b"PERMISSIONS" if rule => {
                permissions(args)?;
            }
            _ if rule => return Err(unknown(key)),
            b"DIRECTORY_PERMISSIONS" if signature == b"DIRECTORY" => {
                permissions(args)?;
            }
            b"USE_SOURCE_PERMISSIONS" | b"MESSAGE_NEVER" | b"FILES_MATCHING"
                if signature == b"DIRECTORY" => {}
            _ if common(cx, signature, options, key, args)? => {}
            _ => return Err(unknown(key)),
        }
    }
    if options
        .destination
        .as_ref()
        .is_none_or(|path| path.is_empty())
        && (!inputs.is_empty() || signature == b"DIRECTORY")
    {
        return Err(fail([signature, b" given no DESTINATION!"].concat()));
    }
    if options.rename.is_some() && inputs.len() > 1 {
        return Err(fail(
            [signature, b" given RENAME option with more than one file."].concat(),
        ));
    }
    let backtrace = cx.backtrace();
    let inputs = inputs
        .into_iter()
        .filter(|input| !input.is_empty())
        .map(|input| BT::new(source_path(cx, input), backtrace.clone()))
        .collect();
    Ok(match signature {
        b"FILES" => InstallKind::Files(inputs),
        b"PROGRAMS" => InstallKind::Programs(inputs),
        _ => InstallKind::Directory(inputs),
    })
}

fn source_path(cx: &Cx<'_>, path: &[u8]) -> BString {
    if Lexical::new(path).absolute() || path.starts_with(b"$<") {
        path.into()
    } else {
        from_native(&cx.current_source_dir().join(to_native(path)))
    }
}

fn script(
    cx: &Cx<'_>,
    signature: &[u8],
    args: &mut Args<'_>,
    options: &mut InstallOptions,
) -> Result<InstallKind> {
    let input = args
        .next()
        .ok_or_else(|| fail([signature, b" requires an argument."].concat()))?;
    let mut all = false;
    while let Some(key) = args.next() {
        match key {
            b"ALL_COMPONENTS" => all = true,
            b"COMPONENT" => options.component = Some(value(args, key)?.into()),
            b"EXCLUDE_FROM_ALL" => options.exclude_from_all = true,
            _ => return Err(unknown(key)),
        }
    }
    if all && options.component.is_some() {
        return Err(fail("ALL_COMPONENTS and COMPONENT are mutually exclusive."));
    }
    Ok(if signature == b"SCRIPT" {
        InstallKind::Script(source_path(cx, input))
    } else {
        InstallKind::Code(input.into())
    })
}

fn install_export(
    cx: &Cx<'_>,
    args: &mut Args<'_>,
    options: &mut InstallOptions,
) -> Result<InstallKind> {
    let set = value(args, b"EXPORT")?.into();
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
            b"EXPORT_LINK_INTERFACE_LIBRARIES" => {}
            _ if common(cx, b"EXPORT", options, key, args)? => {}
            _ => return Err(unknown(key)),
        }
    }
    if options
        .destination
        .as_ref()
        .is_none_or(|path| path.is_empty())
    {
        return Err(fail("EXPORT given no DESTINATION!"));
    }
    Ok(InstallKind::Export {
        set,
        namespace,
        file,
        cxx_modules_directory,
    })
}

fn target_ids(cx: &mut Cx<'_>, names: Vec<&[u8]>, signature: &[u8]) -> Result<Vec<TargetId>> {
    let dir = cx.current_dir_id();
    let project = cx.project();
    names
        .into_iter()
        .map(|name| {
            let Some(binding) = project.lookup_target(dir, name) else {
                return Err(fail(
                    [
                        signature,
                        b" given target \"",
                        name,
                        b"\" which does not exist.",
                    ]
                    .concat(),
                ));
            };
            let TargetBinding::Target(id) = binding else {
                return Err(fail(
                    [signature, b" given ALIAS target \"", name, b"\"."].concat(),
                ));
            };
            if project.target(id).is_some_and(|target| {
                target.is_imported()
                    || matches!(target.kind, TargetKind::Utility | TargetKind::GlobalTarget)
            }) {
                return Err(fail(
                    [
                        signature,
                        b" given target \"",
                        name,
                        b"\" which is not built by this project.",
                    ]
                    .concat(),
                ));
            }
            Ok(id)
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

fn export(cx: &mut Cx<'_>, input: &[ExpandedArg]) -> CmdResult {
    let input: Vec<_> = input.iter().map(|arg| arg.value.as_slice()).collect();
    let [signature, rest @ ..] = input.as_slice() else {
        return Err(fail("called with no arguments."));
    };
    if matches!(*signature, b"PACKAGE_INFO" | b"SBOM") {
        metadata::export(cx, signature, rest)?;
        return Ok(Flow::Normal);
    }
    if *signature == b"PACKAGE" {
        let [name] = rest else {
            return Err(fail("PACKAGE requires one package name."));
        };
        if name.is_empty()
            || !name
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
        {
            return Err(fail(
                [b"PACKAGE given invalid package name \"", *name, b"\"."].concat(),
            ));
        }
        let enabled = if cx.policy(PolicyId::new(90)) == PolicyStatus::New {
            cx.var_is_on(b"CMAKE_EXPORT_PACKAGE_REGISTRY")
        } else {
            !cx.var_is_on(b"CMAKE_EXPORT_NO_PACKAGE_REGISTRY")
        };
        if enabled {
            record_request(cx, "_RCMAKE_PACKAGE_EXPORT", &input);
        }
        return Ok(Flow::Normal);
    }
    let mut args = rest.iter().copied().peekable();
    let name = match *signature {
        b"EXPORT" => {
            let name = value(&mut args, b"EXPORT")?;
            if !cx.project().exports.contains_key(name) {
                return Err(fail([b"Export set \"", name, b"\" not found."].concat()));
            }
            Some(name)
        }
        b"TARGETS" => {
            target_ids(cx, values(&mut args), b"TARGETS")?;
            None
        }
        _ => return Err(unknown(signature)),
    };
    let mut file = None;
    let mut append = false;
    while let Some(key) = args.next() {
        match key {
            b"FILE" => {
                let name = value(&mut args, key)?;
                check_file(name)?;
                file = Some(BString::from(name));
            }
            b"NAMESPACE" => {
                if args.next_if(|token| !keyword(token)).is_none() {
                    return Err(fail(
                        [
                            *signature,
                            b" given invalid argument:\n\n  NAMESPACE: missing required value",
                        ]
                        .concat(),
                    ));
                }
            }
            b"CXX_MODULES_DIRECTORY" => {
                value(&mut args, key)?;
            }
            b"APPEND" if name.is_none() => append = true,
            b"EXPORT_LINK_INTERFACE_LIBRARIES" if name.is_none() => {}
            _ => return Err(unknown(key)),
        }
    }
    let file = file.or_else(|| name.map(|name| [name, b".cmake"].concat().into()));
    if let Some(file) = file {
        let output = Lexical::new(&from_native(
            &cx.current_binary_dir().join(to_native(&file)),
        ))
        .normalize();
        let key = [b"_RCMAKE_EXPORT_OUTPUT_".as_slice(), output.as_slice()].concat();
        if !append && cx.global().props.get(&key).is_some() {
            return Err(fail(
                [
                    b"command already specified for the file ",
                    file.as_slice(),
                    b". Did you miss 'APPEND' keyword?",
                ]
                .concat(),
            ));
        }
        cx.global().props.set(key.into(), b"1".as_slice().into());
        record_request(cx, "_RCMAKE_BUILD_EXPORT", &input);
    }
    Ok(Flow::Normal)
}

// Store arguments separately to preserve empty values, semicolons, and binary content.
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

pub(crate) fn record_request(cx: &mut Cx<'_>, prefix: &str, args: &[&[u8]]) {
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
}

#[cfg(test)]
mod tests;
