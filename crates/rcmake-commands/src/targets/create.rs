use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{
    BT, ImportedVisibility, ModelError, Target, TargetBinding, TargetKind, TargetOrigin,
};

use super::{bytes, init, model_error};

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
            target
                .sources
                .push(BT::new(sources, target.backtrace.clone()));
        }
    }
    let id = cx
        .project()
        .add_target(target)
        .map_err(|err| duplicate(cx, name, options.imported, err))?;
    crate::dirs::source_groups::apply_to_target(cx, id)?;
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
            | b"clean"
            | b"help"
            | b"install"
            | b"package"
            | b"package_source"
            | b"edit_cache"
            | b"rebuild_cache"
    );
    if name == b"test" && cx.var_is_on(b"CMAKE_TESTING_ENABLED") {
        return Err(crate::command_error(
            cx,
            "The target name \"test\" is reserved when CTest testing is enabled.",
        ));
    }
    if valid && (special || !reserved) {
        Ok(())
    } else {
        Err(crate::command_error(
            cx,
            format!(
                "The target name \"{name}\" is reserved or not valid for certain CMake features, such as generator expressions, and may result in undefined behavior."
            ),
        ))
    }
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
            "ALIAS target may not have other arguments.",
        ));
    }
    let referent = &referent.value;
    let dir = cx.current_dir_id();
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
    model.add_alias(dir, name.clone(), id).map_err(|err| {
        if matches!(err, ModelError::DuplicateTarget(_)) {
            CmdError::status(format!("cannot create ALIAS target \"{name}\" because another target with the same name already exists."))
        } else {
            model_error(err)
        }
    })?;
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
