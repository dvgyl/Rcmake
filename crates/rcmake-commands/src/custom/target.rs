//! `add_custom_target()`.

use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{CustomCommandKind, Target, TargetKind, TargetOrigin};

use super::{Options, Section, new_command, record, resolve_paths};
use crate::util::model_error;

pub(super) fn add_target(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some((name, args)) = args.split_first() else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let name = &name.value;
    if name.iter().any(|byte| matches!(byte, b'/' | b'\\')) {
        return Err(CmdError::status(format!(
            "called with invalid target name \"{name}\".  Target names may not contain a slash.  Use ADD_CUSTOM_COMMAND to generate files."
        )));
    }
    if name.is_empty()
        || !name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte))
        || matches!(
            name.as_slice(),
            b"all" | b"clean" | b"help" | b"install" | b"edit_cache" | b"rebuild_cache"
        )
    {
        cx.issue(MessageKind::Error, Effect::SetError, None,
            format!(
                "The target name \"{name}\" is reserved or not valid for certain CMake features, such as generator expressions, and may result in undefined behavior."
            ).into(),
        );
        return Ok(Flow::Normal);
    }
    let all = args.first().is_some_and(|arg| arg.value == b"ALL");
    let args = if all {
        args.get(1..).unwrap_or_default()
    } else {
        args
    };
    let mut body = new_command(cx);
    body.command_lines.push(Vec::new());
    let mut options = Options {
        section: Section::Command,
        ..Options::default()
    };
    options.parse(&mut body, args, true);
    if options.unknown_signature {
        return Err(CmdError::status("Wrong syntax.  Unknown type of argument."));
    }
    if body.command_lines.is_empty() {
        let required = if !body.byproducts.is_empty() {
            Some("BYPRODUCTS")
        } else if body.uses_terminal {
            Some("USES_TERMINAL")
        } else {
            body.command_expand_lists.then_some("COMMAND_EXPAND_LISTS")
        };
        if let Some(keyword) = required {
            return Err(crate::command_error(
                cx,
                format!("{keyword} may not be specified without any COMMAND"),
            ));
        }
    }
    resolve_paths(cx, &mut body, true)?;

    let mut target = Target::new(
        name.clone(),
        TargetKind::Utility,
        TargetOrigin::Normal,
        body.dir,
        body.backtrace.clone(),
        body.policies,
    );
    crate::targets::initialize(cx, &mut target);
    if !all {
        target.props.set("EXCLUDE_FROM_ALL".into(), "TRUE".into());
    }
    target.sources = options.sources;
    if target.name == b"codegen" {
        let action = cx.diag_state().get(DiagCategory::Policy);
        target.props.set(
            "_RCMAKE_CODEGEN_NAME_DIAGNOSTIC".into(),
            action.as_str().into(),
        );
    }
    let id = cx.project().add_target(target).map_err(model_error)?;
    body.kind = CustomCommandKind::Utility(id);
    record(cx, body)?;
    crate::dirs::source_groups::apply_to_target(cx, id)?;
    crate::targets::schedule_generated_name_validation(cx, name);
    Ok(Flow::Normal)
}
