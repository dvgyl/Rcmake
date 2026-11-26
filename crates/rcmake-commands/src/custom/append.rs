//! `add_custom_command(APPEND)`.

use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, find_genex, parse};
use rcmake_model::{
    BT, CommandId, CustomCommand, CustomCommandKind, DirId, ModelError, ProjectModel,
};
use rcmake_resolve::usage::UsageResolver;
use rcmake_syntax::{ArgKind, Argument};

use super::DEFERRED_APPEND;
use crate::util::model_error;

pub(super) fn active_configs(cx: &mut Cx<'_>) -> Vec<ConfigId> {
    let generator = cx.project().generator;
    let mut configs = if let Some(generator) = generator.filter(|kind| kind.multi_config()) {
        cx.var(b"CMAKE_CONFIGURATION_TYPES").map_or_else(
            || {
                generator
                    .default_configuration_types()
                    .iter()
                    .map(|name| ConfigId((*name).into()))
                    .collect()
            },
            |value| {
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(ConfigId)
                    .collect()
            },
        )
    } else {
        vec![ConfigId(cx.var_or_empty(b"CMAKE_BUILD_TYPE"))]
    };
    if configs.is_empty() {
        configs.push(ConfigId::default());
    }
    configs
}

pub(super) fn run(
    cx: &mut Cx<'_>,
    body: CustomCommand,
    count: usize,
    args: Option<&[ExpandedArg]>,
) -> CmdResult {
    let Some(output) = body.outputs.first() else {
        return Err(CmdError::status("APPEND requires an OUTPUT."));
    };
    let configs = active_configs(cx);
    let matched = find_append_command(cx.project(), body.dir, output, count, &configs);
    if let Ok(Some(id)) = matched {
        let existing = cx
            .project()
            .command_mut(id)
            .ok_or_else(|| model_error(ModelError::UnknownObject))?;
        existing.command_lines.extend(body.command_lines);
        existing.depends.extend(body.depends);
        existing.implicit_depends.extend(body.implicit_depends);
        return Ok(Flow::Normal);
    }
    if let Some(args) = args
        && cx.defer_active()
        && (matched.is_err() || append_uses_genex(cx.project(), body.dir, output, count))
    {
        // Retry after configuration. Only commands that preceded APPEND can match.
        let line = cx.current_line();
        let deferred: Vec<_> = [DEFERRED_APPEND.into(), count.to_string().into()]
            .into_iter()
            .chain(args.iter().map(|arg| arg.value.clone()))
            .map(|text| Argument {
                kind: ArgKind::Bracket,
                text,
                line,
                span: 0..0,
            })
            .collect();
        let id = cx.defer_new_id();
        if cx.defer_call(body.dir, id, b"add_custom_command", &deferred) {
            return Ok(Flow::Normal);
        }
    }
    let _ = matched?;
    Err(crate::command_error(
        cx,
        format!(
            "Attempt to APPEND to custom command with output\n  {}\nwhich is not already a custom command output.",
            output.value
        ),
    ))
}

/// Return whether the match for an APPEND output can change later.
///
/// A generator expression in the output, or in the output of a command that
/// can match, can depend on targets that the project creates later. Plain
/// outputs give the final result immediately.
fn append_uses_genex(
    project: &ProjectModel,
    dir: DirId,
    output: &BT<BString>,
    count: usize,
) -> bool {
    find_genex(&output.value).is_some()
        || project
            .commands()
            .take(count)
            .filter(|(_, command)| command.dir == dir && command.kind == CustomCommandKind::Output)
            .any(|(_, command)| {
                command
                    .outputs
                    .iter()
                    .any(|output| find_genex(&output.value).is_some())
            })
}

fn find_append_command(
    project: &ProjectModel,
    dir: DirId,
    output: &BT<BString>,
    count: usize,
    configs: &[ConfigId],
) -> Result<Option<CommandId>, CmdError> {
    let binary = &project.dir(dir).binary_dir;
    let mut usage = None;
    let mut expand = |output: &BT<BString>| {
        if find_genex(&output.value).is_none() {
            return Ok(vec![output.value.clone()]);
        }
        let ast = parse(&output.value)?;
        let mut host = usage
            .get_or_insert_with(|| UsageResolver::new(project))
            .host(dir);
        let mut paths = Vec::new();
        for config in configs {
            let context = EvalContext {
                config: config.clone(),
                site: EvalSite {
                    backtrace: output.backtrace.clone(),
                    property: None,
                },
                ..EvalContext::default()
            };
            let (value, _) = evaluate(&ast, &context, &mut host)?;
            paths.extend(
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|path| Lexical::new(&Lexical::new(binary).append(&path)).normalize()),
            );
        }
        Ok::<_, rcmake_genex::GenexError>(paths)
    };
    let requested = expand(output).map_err(|err| CmdError::status(err.to_string()))?;
    let mut best = None;
    for (id, command) in project.commands().take(count) {
        if command.dir != dir || command.kind != CustomCommandKind::Output {
            continue;
        }
        for output in &command.outputs {
            // Generation reports errors in the original output expression.
            let Ok(paths) = expand(output) else { continue };
            if let Some(index) = requested.iter().position(|path| paths.contains(path))
                && best.is_none_or(|(previous, _)| index < previous)
            {
                best = Some((index, id));
            }
        }
        if best.is_some_and(|(index, _)| index == 0) {
            break;
        }
    }
    Ok(best.map(|(_, id)| id))
}
