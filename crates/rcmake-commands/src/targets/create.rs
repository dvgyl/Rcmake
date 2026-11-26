use rcmake_base::{BString, list};
use rcmake_diag::{Backtrace, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow};
use rcmake_model::{
    BT, DirId, ImportedVisibility, ModelError, Target, TargetBinding, TargetKind, TargetOrigin,
};

use super::{
    bytes, generation_diagnostic, init, model_error, reported, schedule_alias_validation,
    schedule_target_validation,
};

pub(super) fn executable(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(cx, args, true)
}

pub(super) fn library(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    add(cx, args, false)
}

#[derive(Default)]
struct Options {
    kind: Option<TargetKind>,
    imported: bool,
    global: bool,
    exclude: bool,
    symbolic: bool,
    win32: bool,
    bundle: bool,
}

impl Options {
    fn parse(&mut self, arg: &[u8], executable: bool) -> bool {
        match arg {
            b"EXCLUDE_FROM_ALL" => self.exclude = true,
            b"IMPORTED" => self.imported = true,
            b"GLOBAL" if self.imported => self.global = true,
            b"WIN32" if executable => self.win32 = true,
            b"MACOSX_BUNDLE" if executable => self.bundle = true,
            _ if executable => return false,
            b"SYMBOLIC" => self.symbolic = true,
            _ => {
                self.kind = match arg {
                    b"STATIC" => Some(TargetKind::StaticLibrary),
                    b"SHARED" => Some(TargetKind::SharedLibrary),
                    b"MODULE" => Some(TargetKind::ModuleLibrary),
                    b"OBJECT" => Some(TargetKind::ObjectLibrary),
                    b"INTERFACE" => Some(TargetKind::InterfaceLibrary),
                    b"UNKNOWN" => Some(TargetKind::UnknownLibrary),
                    _ => return false,
                };
            }
        }
        true
    }

    fn target_kind(&self, cx: &mut Cx<'_>, executable: bool) -> Result<TargetKind, CmdError> {
        if executable {
            return Ok(TargetKind::Executable);
        }
        match self.kind {
            Some(TargetKind::UnknownLibrary) if !self.imported => Err(crate::command_error(
                cx,
                "The UNKNOWN library type may be used only for IMPORTED libraries.",
            )),
            Some(kind) => Ok(kind),
            None if self.imported => Err(CmdError::status(
                "called with IMPORTED argument but no library type.",
            )),
            None if cx.var_is_on(b"BUILD_SHARED_LIBS") => Ok(TargetKind::SharedLibrary),
            None => Ok(TargetKind::StaticLibrary),
        }
    }

    fn origin(&self) -> TargetOrigin {
        if !self.imported {
            TargetOrigin::Normal
        } else if self.global {
            TargetOrigin::Imported(ImportedVisibility::Global)
        } else {
            TargetOrigin::Imported(ImportedVisibility::Local)
        }
    }
}

fn add(cx: &mut Cx<'_>, args: &[ExpandedArg], executable: bool) -> CmdResult {
    let Some((name, mut rest)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let name = &name.value;
    let mut options = Options::default();
    while let Some((next, tail)) = rest.split_first() {
        if bytes(next) == b"ALIAS" {
            validate_name(cx, name, true)?;
            return alias(cx, name, tail, executable, &options);
        }
        if !options.parse(bytes(next), executable) {
            break;
        }
        rest = tail;
    }
    validate_name(cx, name, options.imported)?;
    let kind = options.target_kind(cx, executable)?;
    if options.symbolic && kind != TargetKind::InterfaceLibrary {
        return Err(CmdError::status(
            "SYMBOLIC option may only be used with INTERFACE libraries",
        ));
    }
    if options.imported {
        if options.exclude && !executable {
            return Err(CmdError::status(
                "excludeFromAll with IMPORTED target makes no sense.",
            ));
        }
        for (set, flag) in [
            (options.win32, "WIN32"),
            (options.bundle, "MACOSX_BUNDLE"),
            (options.exclude, "EXCLUDE_FROM_ALL"),
        ] {
            if set {
                return Err(CmdError::status(format!(
                    "may not be given {flag} for an IMPORTED target."
                )));
            }
        }
    }
    let dir = cx.current_dir_id();
    let policies = *cx.policies_mut();
    let backtrace = cx.backtrace();
    let mut target = Target::new(
        name.clone(),
        kind,
        options.origin(),
        dir,
        backtrace,
        policies,
    );
    init::initialize(cx, &mut target);
    if options.symbolic {
        target.props.set("SYMBOLIC".into(), "TRUE".into());
    }
    if !options.imported {
        let model = cx.project();
        let project_name = model
            .ancestors(dir)
            .find_map(|id| model.dir(id).props.get(b"_RCMAKE_PROJECT_NAME"));
        if let Some(name) = project_name {
            target
                .props
                .set("_RCMAKE_TARGET_PROJECT_NAME".into(), name.clone());
        }
        for (flag, prop) in [
            (options.win32, "WIN32_EXECUTABLE"),
            (options.bundle, "MACOSX_BUNDLE"),
            (options.exclude, "EXCLUDE_FROM_ALL"),
        ] {
            if flag {
                target.props.set(prop.into(), "TRUE".into());
            }
        }
        let sources = list::join(rest.iter().map(bytes).filter(|value| !value.is_empty()));
        if !sources.is_empty() {
            for item in list::iter(&sources, list::Empty::Drop) {
                if !item.windows(2).any(|w| w == b"$<") {
                    if rcmake_base::path_lex::Lexical::new(&item).absolute() {
                        let normalized = rcmake_base::path_lex::Lexical::new(&item).normalize();
                        let directory = cx.project().dir_mut(dir);
                        directory
                            .source_props
                            .entry(normalized.clone())
                            .or_default();
                        crate::custom::record_source_order(directory, &normalized, false);
                    } else {
                        crate::custom::record_source_order(cx.project().dir_mut(dir), &item, true);
                    }
                }
            }
            target
                .sources
                .push(BT::new(sources, target.backtrace.clone()));
        }
    }
    let global = target.is_global();
    let clear_legacy_dependencies = super::link::has_legacy_link_cache(&target);
    let id = cx
        .project()
        .add_target(target)
        .map_err(|err| duplicate(cx, name, options.imported, err))?;
    crate::dirs::source_groups::apply_to_target(cx, id)?;
    if clear_legacy_dependencies {
        let key = super::link::legacy_link_cache_key(name);
        cx.cache().remove(&key);
    }
    if global {
        schedule_generated_name_validation(cx, name);
    }
    Ok(Flow::Normal)
}

fn validate_name(cx: &mut Cx<'_>, name: &BString, special: bool) -> Result<(), CmdError> {
    let valid = !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-:".contains(byte))
        && (special || !name.contains(&b':'));
    let reserved = matches!(
        name.as_slice(),
        b"all"
            | b"ALL_BUILD"
            | b"help"
            | b"install"
            | b"INSTALL"
            | b"preinstall"
            | b"clean"
            | b"edit_cache"
            | b"rebuild_cache"
            | b"ZERO_CHECK"
    );
    if valid && !reserved {
        Ok(())
    } else {
        Err(reported(
            cx,
            format!(
                "The target name \"{name}\" is reserved or not valid for certain CMake features, such as generator expressions, and may result in undefined behavior."
            ),
        ))
    }
}

/// Schedule the late reservation check for a build target.
pub(crate) fn schedule_generated_name_validation(cx: &mut Cx<'_>, name: &[u8]) {
    if generated_reservation(name).is_some() {
        schedule_target_validation(cx);
    }
}

fn generated_reservation(name: &[u8]) -> Option<(&'static str, Option<&'static str>)> {
    match name {
        b"package" | b"PACKAGE" => Some(("CPack packaging", Some("CPackConfig.cmake"))),
        b"package_source" => Some(("CPack source packaging", Some("CPackSourceConfig.cmake"))),
        b"test" | b"RUN_TESTS" => Some(("CTest testing", None)),
        _ => None,
    }
}

pub(super) fn validate_generated_names(cx: &mut Cx<'_>) -> bool {
    let model = cx.project();
    let candidates: Vec<_> = model
        .targets()
        .filter(|(_, target)| target.is_global() && generated_reservation(&target.name).is_some())
        .map(|(_, target)| (target.name.clone(), target.backtrace.clone()))
        .collect();
    let mut failed = false;
    for (name, trace) in candidates {
        failed |= validate_generated_name(cx, &name, trace);
    }
    failed
}

pub(super) fn validate_generated_name(cx: &mut Cx<'_>, name: &BString, trace: Backtrace) -> bool {
    let Some((feature, file)) = generated_reservation(name) else {
        return false;
    };
    let enabled = match file {
        Some(file) => cx
            .host()
            .metadata(&cx.current_binary_dir().join(file), Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File),
        None => cx.var_is_on(b"CMAKE_TESTING_ENABLED"),
    };
    if !enabled {
        return false;
    }
    let root = cx.project().dir(DirId::ROOT).source_dir.clone();
    generation_diagnostic(
        cx,
        &root,
        Location::Backtrace(trace),
        MessageKind::Error,
        None,
        format!("The target name \"{name}\" is reserved when {feature} is enabled.").into(),
    )
}

fn alias(
    cx: &mut Cx<'_>,
    name: &BString,
    args: &[ExpandedArg],
    executable: bool,
    options: &Options,
) -> CmdResult {
    let [referent] = args else {
        return Err(CmdError::status(
            "ALIAS requires exactly one target argument.",
        ));
    };
    if options.imported
        || options.exclude
        || options.symbolic
        || options.win32
        || options.bundle
        || options.kind.is_some()
    {
        return Err(CmdError::status(
            "ALIAS requires exactly one target argument.",
        ));
    }
    let referent = &referent.value;
    let dir = cx.current_dir_id();
    let reject_collision = cx.policy(PolicyId::new(107)) == PolicyStatus::New;
    let model = cx.project();
    let id = match model.lookup_target(dir, referent) {
        None => {
            return Err(CmdError::status(format!(
                "cannot create ALIAS target \"{name}\" because target \"{referent}\" does not already exist."
            )));
        }
        Some(TargetBinding::Alias(_)) => {
            return Err(CmdError::status(format!(
                "cannot create ALIAS target \"{name}\" because target \"{referent}\" is itself an ALIAS."
            )));
        }
        Some(TargetBinding::Target(id)) => id,
    };
    let target = model
        .target(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    if (executable && target.kind != TargetKind::Executable)
        || (!executable && !target.kind.is_library())
    {
        let kind = if executable {
            "an executable"
        } else {
            "a library"
        };
        return Err(CmdError::status(format!(
            "cannot create ALIAS target \"{name}\" because target \"{referent}\" is not {kind}."
        )));
    }
    let global = target.is_global();
    if !reject_collision && let Some(existing) = model.lookup_target(dir, name) {
        if !global {
            // New subdirectories inherit the local alias when they start.
            model
                .dir_mut(dir)
                .local_targets
                .insert(name.clone(), TargetBinding::Alias(id));
        } else {
            let directories: Vec<_> = model.dirs().map(|(dir, _)| dir).collect();
            for dir in directories {
                // A built target in this directory takes precedence over the alias.
                let aliased = match existing {
                    TargetBinding::Target(current)
                        if model
                            .target(current)
                            .is_some_and(|target| target.dir == dir && !target.is_imported()) =>
                    {
                        current
                    }
                    _ => id,
                };
                model
                    .dir_mut(dir)
                    .local_targets
                    .insert(name.clone(), TargetBinding::Alias(aliased));
            }
        }
    } else {
        model.add_alias(dir, name.clone(), id).map_err(|err| {
            if matches!(err, ModelError::DuplicateTarget(_)) {
                CmdError::status(format!("cannot create ALIAS target \"{name}\" because another target with the same name already exists."))
            } else {
                model_error(err)
            }
        })?;
    }
    let trace = cx.backtrace();
    cx.project()
        .record_alias_declaration(dir, name, global, trace);
    if global && generated_reservation(name).is_some() {
        schedule_alias_validation(cx, name);
    }
    Ok(Flow::Normal)
}

fn duplicate(cx: &mut Cx<'_>, name: &BString, imported: bool, error: ModelError) -> CmdError {
    let ModelError::DuplicateTarget(id) = error else {
        return model_error(error);
    };
    if imported {
        return CmdError::status(format!(
            "cannot create imported target \"{name}\" because another target with the same name already exists."
        ));
    }
    let model = cx.project();
    let Some(target) = model.target(id) else {
        return model_error(error);
    };
    if target.is_imported() {
        return CmdError::status(format!(
            "cannot create target \"{name}\" because an imported target with the same name already exists."
        ));
    }
    let kind = match target.kind {
        TargetKind::Executable => "an executable",
        TargetKind::StaticLibrary => "a static library",
        TargetKind::SharedLibrary => "a shared library",
        TargetKind::ModuleLibrary => "a module library",
        TargetKind::ObjectLibrary => "an object library",
        TargetKind::InterfaceLibrary => "an interface library",
        TargetKind::UnknownLibrary => "an unknown library",
        TargetKind::Utility => "a custom target",
        TargetKind::GlobalTarget => "a global target",
    };
    let dir = &model.dir(target.dir).source_dir;
    CmdError::status(format!(
        "cannot create target \"{name}\" because another target with the same name already exists.  The existing target is {kind} created in source directory\n\"{dir}\".  See documentation for policy CMP0002 for more details."
    ))
}
