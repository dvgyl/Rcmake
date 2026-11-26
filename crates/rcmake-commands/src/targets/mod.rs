//! Configure-time target creation, properties, sources, and usage requirements.

#![forbid(unsafe_code)]

use std::sync::Arc;

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{
    Backtrace, DiagAction, DiagCategory, Diagnostic, Effect, Frame, FrameKind, Location,
    MessageKind, PolicyId, PolicyStatus, RenderOptions,
};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_model::{DirId, TargetBinding, TargetId, TargetKind, UsageScope};
use rcmake_syntax::{ArgKind, Argument};

use crate::util::{bytes, model_error};

mod create;
mod init;
mod link;
mod precompile;
mod properties;
mod sources;
mod usage;

pub(crate) use create::schedule_generated_name_validation;
pub(crate) use init::initialize;
pub(crate) use properties::target_location;
pub(crate) use sources::{find_file_set, replace_file_set_paths, require_sycl, scope_name};

#[cfg(test)]
mod tests;

pub(crate) fn register(registry: &mut Registry) {
    let flags = CmdFlags { scriptable: false };
    registry.add("add_executable", add_executable, flags);
    registry.add("add_library", add_library, flags);
    registry.add("add_dependencies", add_dependencies, flags);
    registry.add("set_target_properties", set_target_properties, flags);
    registry.add("get_target_property", properties::get, flags);
    registry.add("target_sources", sources::add, flags);
    registry.add(
        "target_include_directories",
        usage::include_directories,
        flags,
    );
    registry.add(
        "target_compile_definitions",
        usage::compile_definitions,
        flags,
    );
    registry.add("target_compile_options", usage::compile_options, flags);
    registry.add("target_compile_features", usage::compile_features, flags);
    crate::compat::project_command!(registry, "target_link_options", usage::link_options);
    crate::compat::project_command!(registry, "target_link_directories", usage::link_directories);
    registry.add("target_link_libraries", link::add, flags);
    crate::compat::project_command!(registry, "target_precompile_headers", precompile::add);
}

fn reported(cx: &mut Cx<'_>, message: impl Into<BString>) -> CmdError {
    cx.issue(MessageKind::Error, Effect::SetError, None, message.into());
    CmdError::Reported
}

fn scope(value: &[u8]) -> Option<UsageScope> {
    match value {
        b"PRIVATE" => Some(UsageScope::Private),
        b"PUBLIC" => Some(UsageScope::Public),
        b"INTERFACE" => Some(UsageScope::Interface),
        _ => None,
    }
}

fn source_dir(cx: &mut Cx<'_>) -> BString {
    let dir = cx.current_dir_id();
    cx.project().dir(dir).source_dir.clone()
}

fn join_source_dir(path: &[u8], dir: &[u8]) -> BString {
    if path.starts_with(b"$<") || !Lexical::new(path).root_directory().is_empty() {
        return path.into();
    }
    let separator = if dir.ends_with(b"/") {
        b"".as_slice()
    } else {
        b"/"
    };
    [dir, separator, path].concat().into()
}

fn usage_target(cx: &mut Cx<'_>, name: &BString, what: &str) -> Result<TargetId, CmdError> {
    let dir = cx.current_dir_id();
    cx.project().find_target(dir, name).ok_or_else(|| {
        reported(
            cx,
            format!(
                "Cannot specify {what} for target \"{name}\" which is not built by this project."
            ),
        )
    })
}

fn add_library(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let options = validate_creation(cx, args, true)?;
    let Some(name) = args.first() else {
        return create::library(cx, args);
    };
    let replaced;
    let default_kind: &[u8] = if cx.var_is_on(b"BUILD_SHARED_LIBS") {
        b"SHARED"
    } else {
        b"STATIC"
    };
    let kind = options.kind.map_or(default_kind, |(_, kind)| kind);
    let args = if !options.alias
        && !options.imported
        && matches!(kind, b"SHARED" | b"MODULE")
        && cx
            .global()
            .props
            .get(b"TARGET_SUPPORTS_SHARED_LIBS")
            .is_some_and(|value| truth::is_off(value))
    {
        let kind = BString::from(kind);
        let body = format!(
            "ADD_LIBRARY called with {kind} option but the target platform does not support dynamic linking."
        );
        match cx.policy(PolicyId::new(164)) {
            PolicyStatus::New => return Err(crate::command_error(cx, body)),
            PolicyStatus::Warn => crate::author_warning(
                cx,
                format!("{body} Building a STATIC library instead. This may lead to problems."),
            ),
            PolicyStatus::Old => {}
        }
        let mut options_args = args.to_vec();
        if let Some((index, _)) = options.kind {
            let option = options_args
                .get_mut(index)
                .ok_or_else(|| CmdError::status("could not locate the library type argument"))?;
            option.value = "STATIC".into();
        } else {
            options_args.insert(
                1,
                ExpandedArg {
                    value: "STATIC".into(),
                    kind: name.kind,
                    line: name.line,
                },
            );
        }
        replaced = options_args;
        replaced.as_slice()
    } else {
        args
    };
    let flow = create::library(cx, args)?;
    if !options.alias {
        initialize_target_properties(cx, &name.value);
    }
    Ok(flow)
}

fn add_executable(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let options = validate_creation(cx, args, false)?;
    let flow = create::executable(cx, args)?;
    if !options.alias
        && let Some(name) = args.first()
    {
        initialize_target_properties(cx, &name.value);
    }
    Ok(flow)
}

#[derive(Default)]
struct CreationOptions<'a> {
    kind: Option<(usize, &'a [u8])>,
    imported: bool,
    alias: bool,
    exclude: bool,
}

fn validate_creation<'a>(
    cx: &mut Cx<'_>,
    args: &'a [ExpandedArg],
    library: bool,
) -> Result<CreationOptions<'a>, CmdError> {
    let mut options = CreationOptions::default();
    let [name, rest @ ..] = args else {
        return Ok(options);
    };
    for (index, option) in rest.iter().map(bytes).enumerate() {
        let interface = options.kind.is_some_and(|(_, kind)| kind == b"INTERFACE");
        match option {
            b"EXCLUDE_FROM_ALL" => options.exclude = true,
            b"IMPORTED" => options.imported = true,
            b"GLOBAL" if options.imported => {}
            b"GLOBAL" if interface => {
                return Err(CmdError::status(
                    "GLOBAL option may only be used with IMPORTED libraries.",
                ));
            }
            b"ALIAS" if interface => {
                return Err(CmdError::status(
                    "INTERFACE library specified with conflicting ALIAS type.",
                ));
            }
            b"ALIAS" => {
                options.alias = true;
                if !library {
                    break;
                }
            }
            b"WIN32" | b"MACOSX_BUNDLE" if !library => {}
            b"SYMBOLIC" if library => {}
            b"INTERFACE" if library && options.kind.is_some() => {
                return Err(CmdError::status(
                    "INTERFACE library specified with conflicting/multiple types.",
                ));
            }
            b"INTERFACE" if library && options.alias => {
                return Err(CmdError::status(
                    "INTERFACE library specified with conflicting ALIAS type.",
                ));
            }
            b"STATIC" | b"SHARED" | b"MODULE" | b"OBJECT" | b"UNKNOWN" if interface => {
                return Err(CmdError::status(format!(
                    "INTERFACE library specified with conflicting {} type.",
                    BString::from(option)
                )));
            }
            b"INTERFACE" | b"STATIC" | b"SHARED" | b"MODULE" | b"OBJECT" | b"UNKNOWN"
                if library =>
            {
                options.kind = Some((index + 1, option));
            }
            _ => break,
        }
    }
    let dir = cx.current_dir_id();
    if options.alias {
        if options.exclude
            || !library
                && rest
                    .iter()
                    .skip_while(|arg| bytes(arg) != b"ALIAS")
                    .nth(1)
                    .is_some_and(|arg| bytes(arg) == b"EXCLUDE_FROM_ALL")
        {
            return Err(CmdError::status(
                "EXCLUDE_FROM_ALL with ALIAS makes no sense.",
            ));
        }
        if options.imported {
            return Err(CmdError::status("IMPORTED with ALIAS is not allowed."));
        }
    } else if !options.imported
        && matches!(
            cx.project().lookup_target(dir, bytes(name)),
            Some(TargetBinding::Alias(_))
        )
    {
        return Err(CmdError::status(format!(
            "cannot create target \"{}\" because an alias with the same name already exists.",
            name.value
        )));
    }
    Ok(options)
}

fn initialize_target_properties(cx: &mut Cx<'_>, name: &[u8]) {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let Some(id) = model.find_target(dir, name) else {
        return;
    };
    let Some(target) = model.target(id) else {
        return;
    };
    let kind = target.kind;
    let imported = target.is_imported();
    let mut properties = Vec::new();
    if matches!(kind, TargetKind::SharedLibrary | TargetKind::ModuleLibrary) && imported {
        properties.push(("POSITION_INDEPENDENT_CODE", Some("True".into())));
    }
    if kind == TargetKind::SharedLibrary {
        properties.push((
            "DLL_NAME_WITH_SOVERSION",
            cx.var(b"CMAKE_DLL_NAME_WITH_SOVERSION"),
        ));
    }
    if !imported && kind != TargetKind::InterfaceLibrary {
        for (property, fallback) in [
            ("DISABLE_PRECOMPILE_HEADERS", None),
            ("WATCOM_RUNTIME_LIBRARY", None),
            ("Swift_LANGUAGE_VERSION", None),
            ("SYCL_EXTENSION_MODE", None),
            ("AUTOMOC_EXECUTABLE", None),
            ("AUTOUIC_EXECUTABLE", None),
            ("AUTORCC_EXECUTABLE", None),
            ("IOS_INSTALL_COMBINED", None),
            ("CUDA_RUNTIME_LIBRARY", None),
            ("HIP_RUNTIME_LIBRARY", None),
            ("FRAMEWORK_MULTI_CONFIG_POSTFIX", None),
            ("WIN32_EXECUTABLE", None),
            ("MACOSX_BUNDLE", None),
            ("PCH_WARN_INVALID", Some("ON")),
            ("PCH_INSTANTIATE_TEMPLATES", Some("ON")),
            ("ISPC_HEADER_SUFFIX", Some("_ispc.h")),
            ("UNITY_BUILD_BATCH_SIZE", Some("8")),
            ("UNITY_BUILD_MODE", Some("BATCH")),
        ] {
            let key = format!("CMAKE_{property}");
            let value = cx
                .var(key.as_bytes())
                .or_else(|| fallback.map(BString::from));
            properties.push((property, value));
        }
    }
    let Some(target) = cx.project().target_mut(id) else {
        return;
    };
    for (property, value) in properties {
        if let Some(value) = value
            && target.props.get(property.as_bytes()).is_none()
        {
            target.props.set(property.into(), value);
        }
    }
    if target
        .props
        .get(b"INTERPROCEDURAL_OPTIMIZATION")
        .is_some_and(|value| truth::is_on(value))
    {
        link::schedule_validation(cx);
    }
}

fn defer_validation(cx: &mut Cx<'_>, command: &[u8], args: &[&[u8]]) -> bool {
    if !cx.defer_active() {
        return false;
    }
    let line = cx.current_line();
    let args: Vec<_> = args
        .iter()
        .map(|&text| Argument {
            kind: ArgKind::Bracket,
            text: text.into(),
            line,
            span: 0..0,
        })
        .collect();
    let id = cx.defer_new_id();
    cx.defer_call(DirId::ROOT, id, command, &args)
}

fn generation_diagnostic(
    cx: &Cx<'_>,
    root: &BString,
    location: Location,
    mut kind: MessageKind,
    category: Option<DiagCategory>,
    body: BString,
) -> bool {
    if let Some(category) = category {
        match cx.diag_state().get(category) {
            DiagAction::Ignore => return false,
            DiagAction::Warn => {}
            DiagAction::SendError | DiagAction::FatalError => kind = MessageKind::AuthorError,
        }
    }
    let failed = matches!(kind, MessageKind::Error | MessageKind::AuthorError);
    // A generation call stack has no file frames, as CMake shows it.
    let location = match location {
        Location::Backtrace(backtrace) => {
            Location::Backtrace(rcmake_resolve::lower::listed_backtrace(&backtrace))
        }
        location => location,
    };
    let diagnostic = Diagnostic {
        annotations: None,
        kind,
        effect: if failed {
            Effect::SetError
        } else {
            Effect::None
        },
        location,
        category,
        policy: None,
        body,
    };
    let render = RenderOptions {
        relative_base: Some(root.clone()),
        ..RenderOptions::default()
    };
    cx.host().write_out(
        rcmake_host::Stream::Stderr,
        &rcmake_diag::render(&diagnostic, &render),
    );
    failed
}

fn generation_failed(cx: &mut Cx<'_>) {
    cx.host().write_out(
        rcmake_host::Stream::Stderr,
        b"CMake Generate step failed.  Build files cannot be regenerated correctly.\n",
    );
    cx.set_fatal();
}

const CHECK_TARGETS: &[u8] = b"\0rcmake-check-targets";

fn schedule_target_validation(cx: &mut Cx<'_>) {
    if cx.global().props.get(CHECK_TARGETS).is_none()
        && defer_validation(cx, b"add_dependencies", &[CHECK_TARGETS])
    {
        cx.global().props.set(CHECK_TARGETS.into(), "1".into());
    }
}

fn schedule_alias_validation(cx: &mut Cx<'_>, name: &[u8]) {
    let trace = cx.backtrace();
    let frames: Vec<_> = trace
        .frames()
        .map(|frame| {
            let kind = match frame.kind {
                FrameKind::Command => b"command".as_slice(),
                FrameKind::Deferred => b"deferred",
                FrameKind::ModuleInclude => b"include",
            };
            (frame, frame.line.to_string(), kind)
        })
        .collect();
    let args: Vec<_> = [CHECK_TARGETS, name]
        .into_iter()
        .chain(frames.iter().flat_map(|(frame, line, kind)| {
            [
                frame.name.as_deref().unwrap_or_default(),
                frame.file.as_ref(),
                line.as_bytes(),
                *kind,
            ]
        }))
        .collect();
    defer_validation(cx, b"add_dependencies", &args);
}

fn has_pending_deferred_commands(cx: &Cx<'_>) -> bool {
    cx.defer_get_call_ids(DirId::ROOT).is_some_and(|ids| {
        ids.iter().any(|id| {
            cx.defer_get_call(DirId::ROOT, id).is_some_and(|call| {
                !matches!(call.as_slice(), [command, marker, ..]
                    if (command == b"add_dependencies" && marker == CHECK_TARGETS)
                    || (command == b"target_link_libraries" && marker == link::CHECK_LINKS))
            })
        })
    })
}

fn add_dependencies(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if let [marker, check @ ..] = args
        && bytes(marker) == CHECK_TARGETS
        && cx.running_deferred()
    {
        return run_target_validation(cx, args, check);
    }

    properties::add_dependencies(cx, args)?;
    if let [name, deps @ ..] = args
        && !deps.is_empty()
    {
        let dir = cx.current_dir_id();
        let model = cx.project();
        let target = model
            .find_target(dir, bytes(name))
            .and_then(|id| model.target(id));
        if target.is_some_and(|target| {
            !target.is_imported()
                && deps
                    .iter()
                    .any(|dep| model.find_target(target.dir, bytes(dep)).is_none())
        }) {
            // A later command can define the dependency. Move this check to an
            // internal post-directory hook after W11B.
            schedule_target_validation(cx);
        }
    }
    Ok(Flow::Normal)
}

fn run_target_validation(
    cx: &mut Cx<'_>,
    args: &[ExpandedArg],
    check: &[ExpandedArg],
) -> CmdResult {
    // A deferred command can create targets, enable testing, or queue more commands.
    if has_pending_deferred_commands(cx) {
        let values: Vec<_> = args.iter().map(bytes).collect();
        defer_validation(cx, b"add_dependencies", &values);
        return Ok(Flow::Normal);
    }
    match check {
        [] => {
            cx.global().props.unset(CHECK_TARGETS);
            if create::validate_generated_names(cx) {
                cx.set_fatal();
            } else if validate_dependencies(cx) {
                generation_failed(cx);
            }
        }
        [name, frames @ ..] => {
            let trace = deferred_alias_backtrace(frames)?;
            if create::validate_generated_name(cx, &name.value, trace) {
                cx.set_fatal();
            }
        }
    }
    Ok(Flow::Normal)
}

fn deferred_alias_backtrace(args: &[ExpandedArg]) -> Result<Backtrace, CmdError> {
    let invalid = || CmdError::status("Invalid deferred alias location.");
    let (frames, rest) = args.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(invalid());
    }
    let mut trace = Backtrace::default();
    for [command, file, line, kind] in frames.iter().rev() {
        let line = std::str::from_utf8(bytes(line))
            .ok()
            .and_then(|line| line.parse().ok())
            .ok_or_else(invalid)?;
        let kind = match bytes(kind) {
            b"command" => FrameKind::Command,
            b"deferred" => FrameKind::Deferred,
            b"include" => FrameKind::ModuleInclude,
            _ => return Err(invalid()),
        };
        let command = bytes(command);
        trace = trace.push(Frame {
            file: Arc::from(bytes(file)),
            line,
            name: (!command.is_empty()).then(|| Arc::from(command)),
            kind,
        });
    }
    Ok(trace)
}

fn validate_dependencies(cx: &mut Cx<'_>) -> bool {
    let model = cx.project();
    let root = model.dir(DirId::ROOT).source_dir.clone();
    let mut missing = Vec::new();
    for (_, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        let mut deps: Vec<_> = target
            .utility_deps
            .iter()
            .filter(|dep| model.find_target(target.dir, &dep.value).is_none())
            .collect();
        deps.sort_unstable_by(|left, right| left.value.cmp(&right.value));
        missing.extend(deps.into_iter().map(|dep| {
            (
                Location::Backtrace(dep.backtrace.clone()),
                format!(
                    "The dependency target \"{}\" of target \"{}\" does not exist.",
                    dep.value, target.name
                ),
            )
        }));
    }
    let failed = !missing.is_empty();
    for (location, message) in missing {
        generation_diagnostic(
            cx,
            &root,
            location,
            MessageKind::Error,
            None,
            message.into(),
        );
    }
    failed
}

fn set_target_properties(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some(marker) = args.iter().position(|arg| bytes(arg) == b"PROPERTIES") else {
        return properties::set(cx, args);
    };
    let (Some(names_and_keyword), Some((names, [keyword, pairs @ ..]))) =
        (args.get(..=marker), args.split_at_checked(marker))
    else {
        return properties::set(cx, args);
    };
    let (pairs, trailing) = pairs.as_chunks::<2>();
    if !trailing.is_empty() || names.is_empty() {
        return properties::set(cx, args);
    }
    properties::set(cx, names_and_keyword)?;
    let dir = cx.current_dir_id();
    for name in names {
        let id = usage_target(cx, &name.value, "target properties")?;
        for [property, value] in pairs {
            if matches!(bytes(property), b"ALIASED_TARGET" | b"ALIAS_GLOBAL")
                && matches!(
                    cx.project().lookup_target(dir, bytes(name)),
                    Some(TargetBinding::Alias(_))
                )
            {
                return properties::set(
                    cx,
                    &[
                        name.clone(),
                        keyword.clone(),
                        property.clone(),
                        value.clone(),
                    ],
                );
            }
            if check_target_property(cx, id, bytes(property))
                || !validate_target_property_value(cx, id, bytes(property), bytes(value), false)
            {
                continue;
            }
            if matches!(bytes(property), b"BINARY_DIR" | b"IMPORTED" | b"SOURCE_DIR") {
                if let Some(target) = cx.project().target_mut(id) {
                    target
                        .props
                        .set(property.value.clone(), value.value.clone());
                }
                continue;
            }
            properties::set(
                cx,
                &[
                    name.clone(),
                    keyword.clone(),
                    property.clone(),
                    value.clone(),
                ],
            )?;
            schedule_target_property_validation(cx, bytes(property));
        }
    }
    Ok(Flow::Normal)
}

/// Check target properties whose meaning depends on the generated build.
pub(crate) fn schedule_target_property_validation(cx: &mut Cx<'_>, property: &[u8]) {
    if property.starts_with(b"INTERPROCEDURAL_OPTIMIZATION") || property == b"LINK_LIBRARIES" {
        link::schedule_validation(cx);
    }
}

/// Check the name that an imported interface library passes to the linker.
pub(crate) fn validate_target_property_value(
    cx: &mut Cx<'_>,
    id: TargetId,
    property: &[u8],
    value: &[u8],
    append: bool,
) -> bool {
    if property != b"IMPORTED_LIBNAME" && !property.starts_with(b"IMPORTED_LIBNAME_") {
        return true;
    }
    let name = BString::from(property);
    let valid_target = cx
        .project()
        .target(id)
        .is_some_and(|target| target.is_imported() && target.kind == TargetKind::InterfaceLibrary);
    let error = if append {
        format!("{name} property may not be APPENDed.")
    } else if !valid_target {
        format!("{name} property may be set only on imported INTERFACE library targets.")
    } else if value.starts_with(b"-") {
        format!(
            "{name} property value\n  {}\nmay not start with '-'.",
            BString::from(value)
        )
    } else if let Some(&byte) = b":;/\\".iter().find(|byte| value.contains(byte)) {
        format!(
            "{name} property value\n  {}\nmay not contain '{}'.",
            BString::from(value),
            char::from(byte)
        )
    } else {
        return true;
    };
    cx.issue(MessageKind::Error, Effect::SetError, None, error.into());
    false
}

/// Report a failed write to a read-only target property.
pub(crate) fn check_target_property(cx: &mut Cx<'_>, id: TargetId, property: &[u8]) -> bool {
    let Some(target) = cx.project().target(id) else {
        return false;
    };
    let condition = match property {
        b"EXPORT_NAME" | b"SOURCES" if target.is_imported() => "can't be set on imported targets",
        b"IMPORTED_GLOBAL" if !target.is_imported() => "can't be set on non-imported targets",
        b"ALIASED_TARGET"
        | b"FILE_SET_TYPES"
        | b"HEADER_SETS"
        | b"INTERFACE_HEADER_SETS"
        | b"INTERFACE_SOURCE_SETS"
        | b"MANUALLY_ADDED_DEPENDENCIES"
        | b"NAME"
        | b"SOURCE_SETS"
        | b"SYMBOLIC"
        | b"TYPE" => "is read-only for target",
        b"ALIAS_GLOBAL"
        | b"BINARY_DIR"
        | b"CXX_MODULE_SETS"
        | b"IMPORTED"
        | b"INTERFACE_CXX_MODULE_SETS"
        | b"LOCATION"
        | b"LOCATION_CONFIG"
        | b"SOURCE_DIR" => {
            let policy = PolicyId::new(160);
            let name = format!(
                "{} property is read-only for target(\"{}\")\n",
                BString::from(property),
                target.name
            );
            return match target.policies.policies.get(policy) {
                PolicyStatus::New => {
                    cx.issue(MessageKind::Error, Effect::SetError, None, name.into());
                    true
                }
                PolicyStatus::Warn => {
                    cx.issue_policy_warning(policy, b"", name.as_bytes());
                    false
                }
                PolicyStatus::Old => false,
            };
        }
        _ => return false,
    };
    let message = format!(
        "{} property {condition}(\"{}\")\n",
        BString::from(property),
        target.name
    );
    cx.issue(MessageKind::Error, Effect::SetError, None, message.into());
    true
}
