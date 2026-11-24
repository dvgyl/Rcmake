use rcmake_base::{BString, limits::Limits};
use rcmake_diag::{
    Backtrace, Diagnostic, Effect, FrameKind, Location, MessageKind, RenderOptions, render,
};
use rcmake_genex::EvalContext;
use rcmake_host::{Host, Stream};
use rcmake_model::{DirId, ProjectModel, PropScope, PropertyPrint};
use rcmake_regex::{Regex, Scratch};
use rcmake_resolve::{GenerationResult, usage::UsageResolver};

use super::{Filters, Matcher, assignment, missing_target};

/// Display deferred target properties for the active configuration.
pub fn print_deferred_properties(
    model: &ProjectModel,
    result: &GenerationResult,
    host: &dyn Host,
) -> Result<(), BString> {
    if model.property_prints.is_empty() {
        return Ok(());
    }
    let mut usage = UsageResolver::new(model);
    let ctx = EvalContext {
        config: result.configs.first().cloned().unwrap_or_default(),
        ..EvalContext::default()
    };
    for request in &model.property_prints {
        print_request(model, request, host, &mut usage, &ctx)?;
    }
    Ok(())
}

fn print_request(
    model: &ProjectModel,
    request: &PropertyPrint,
    host: &dyn Host,
    usage: &mut UsageResolver<'_>,
    ctx: &EvalContext,
) -> Result<(), BString> {
    let name = request.name_filter.as_ref().map(|value| value.as_slice());
    let value = request.value_filter.as_ref().map(|value| value.as_slice());
    let compile = |pattern: Option<&[u8]>| {
        pattern
            .map(|pattern| Regex::new(pattern).map(std::sync::Arc::new))
            .transpose()
            .map_err(|error| BString::from(error.to_string()))
    };
    let filters = Filters {
        name: compile(name)?,
        value: compile(value)?,
        raw_name: name,
        raw_value: value,
        variable: false,
    };
    let mut matcher = Matcher {
        budget: Limits::default().regex_steps,
        scratch: Scratch::default(),
        insensitive: false,
    };
    let description = filters.description(false);
    let mut lines = BString::default();
    for head in &request.targets {
        let Some(id) = model.find_target(request.dir, head) else {
            missing_target(&mut lines, head);
            continue;
        };
        let mut targets = vec![id];
        if request.follow {
            targets.extend(
                usage
                    .reachable_link_targets(id, ctx)
                    .map_err(|error| BString::from(error.to_string()))?,
            );
        }
        let mut block = BString::default();
        for id in targets {
            let Some(target) = model.target(id) else {
                continue;
            };
            let all;
            let names = if let Some(names) = &request.names {
                names
            } else {
                all = target.props.names();
                &all
            };
            for name in names {
                if !matcher
                    .matches(filters.name.as_deref(), name)
                    .map_err(match_error)?
                {
                    continue;
                }
                let value = model.get_prop(&PropScope::Target(id), name);
                if !matcher
                    .matches(
                        filters.value.as_deref(),
                        value
                            .as_ref()
                            .map_or(b"".as_slice(), |value| value.as_slice()),
                    )
                    .map_err(match_error)?
                {
                    continue;
                }
                let name = [target.name.as_slice(), b".", name].concat();
                assignment(
                    &mut block,
                    &name,
                    value.as_ref().map(|value| value.as_slice()),
                );
            }
        }
        if block.is_empty() && request.names.is_none() {
            let diagnostic = Diagnostic {
                kind: MessageKind::Warning,
                effect: Effect::None,
                category: None,
                policy: None,
                location: Location::Backtrace(
                    request.backtrace.frames()
                        .filter(|frame| frame.kind != FrameKind::ModuleInclude)
                        .collect::<Vec<_>>().into_iter().rev()
                        .fold(Backtrace::default(), |backtrace, frame| backtrace.push(frame.clone()))
                ),
                body: format!("No properties for TARGET {head}{description} in cmake_language(PRINT_PROPERTIES ...).").into(),
            };
            host.write_out(
                Stream::Stderr,
                &render(
                    &diagnostic,
                    &RenderOptions {
                        relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                        ..Default::default()
                    },
                ),
            );
            continue;
        }
        let kind = if request.names.is_some() {
            "Properties"
        } else {
            "All properties"
        };
        let reachable = if request.follow {
            " (and all reachable)"
        } else {
            ""
        };
        lines.extend_from_slice(
            format!(" {kind} for TARGET {head}{reachable}{description}:\n").as_bytes(),
        );
        lines.extend_from_slice(&block);
    }
    if !lines.is_empty() {
        let banner = if request.legacy {
            b"\n".as_slice()
        } else {
            b"Printing properties...\n"
        };
        host.write_out(Stream::Stdout, &[b"-- ", banner, &lines, b"\n"].concat());
    }
    Ok(())
}

fn match_error(error: rcmake_eval::CmdError) -> BString {
    match error {
        rcmake_eval::CmdError::Status(message) => message,
        rcmake_eval::CmdError::Reported => "Could not compare a target property.".into(),
    }
}
