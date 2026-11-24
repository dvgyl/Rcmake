use rcmake_base::list;
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{BT, ModelError, TargetKind, UsageField, UsageScope};

use super::{absolute, bytes, model_error, scope, source_dir, usage, usage_target};

pub(super) fn add(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let [name, first, rest @ ..] = args else {
        return Err(CmdError::status(
            "called with incorrect number of arguments",
        ));
    };
    let reuse = match (bytes(first), rest) {
        (b"REUSE_FROM", [provider]) => Some(provider),
        (b"REUSE_FROM", _) => {
            return Err(CmdError::status(
                "called with incorrect number of arguments",
            ));
        }
        _ => None,
    };
    let id = usage_target(cx, &name.value, "precompile headers")?;
    let dir = source_dir(cx);
    let bt = cx.backtrace();
    let target = cx
        .project()
        .target_mut(id)
        .ok_or_else(|| model_error(ModelError::UnknownObject))?;
    if matches!(target.kind, TargetKind::Utility | TargetKind::GlobalTarget) {
        return Err(CmdError::status("called with non-compilable target type"));
    }
    if let Some(provider) = reuse {
        target.props.set(
            "PRECOMPILE_HEADERS_REUSE_FROM".into(),
            provider.value.clone(),
        );
        return Ok(Flow::Normal);
    }
    let mut keyword = first;
    let mut rest = rest;
    loop {
        let scope = scope(bytes(keyword))
            .ok_or_else(|| CmdError::status("called with invalid arguments"))?;
        usage::validate_scope(target, scope, false)?;
        let end = rest
            .iter()
            .position(|arg| super::scope(bytes(arg)).is_some())
            .unwrap_or(rest.len());
        let (headers, next) = rest.split_at(end);
        if !headers.is_empty() {
            let value = list::join(headers.iter().map(|header| {
                let header = bytes(header);
                if header.starts_with(b"<") || header.starts_with(b"\"") {
                    header.into()
                } else {
                    absolute(header, &dir)
                }
            }));
            if scope != UsageScope::Interface {
                target.usage.push(
                    UsageField::PrecompileHeaders,
                    BT::new(value.clone(), bt.clone()),
                );
            }
            if scope != UsageScope::Private {
                target.usage.push(
                    UsageField::InterfacePrecompileHeaders,
                    BT::new(value, bt.clone()),
                );
            }
        }
        let Some((first, tail)) = next.split_first() else {
            break;
        };
        keyword = first;
        rest = tail;
    }
    Ok(Flow::Normal)
}
