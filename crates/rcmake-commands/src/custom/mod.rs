//! Custom commands, utility targets, and custom rule templates.

use std::collections::BTreeSet;

use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_genex::{ConfigId, EvalContext, EvalSite, evaluate, find_genex, parse};
use rcmake_host::path::from_native;
use rcmake_model::{
    BT, CommandId, CustomCommand, CustomCommandKind, DirId, Directory, GeneratorKind, ModelError,
    ProjectModel, Target, TargetBinding, TargetEvent, TargetKind, TargetOrigin,
};
use rcmake_resolve::usage::UsageResolver;
use rcmake_syntax::{ArgKind, Argument};

mod rule;
#[cfg(test)]
mod tests;

const DEFERRED_APPEND: &[u8] = b"\0RCMAKE_CUSTOM_APPEND";

/// Possible binary path of a relative source property record.
pub(crate) const SOURCE_BINARY_ALTERNATIVE: &[u8] = b"\0rcmake-source-binary-alternative";

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    registry.add("add_custom_command", add_command, project_only);
    registry.add("add_custom_target", add_target, project_only);
    crate::compat::project_command!(registry, "add_custom_rule", rule::add);
}

#[derive(Clone, Copy, Default)]
enum Section {
    #[default]
    None,
    Command,
    Output,
    Target,
    Source,
    Depends,
    Byproducts,
    Sources,
    MainDependency,
    ImplicitDepends,
    WorkingDirectory,
    Comment,
    Depfile,
    JobPool,
    JobServerAware,
    Legacy,
}

impl Section {
    fn pending_value(self) -> Option<&'static str> {
        match self {
            Self::Output => Some("OUTPUT"),
            Self::Target => Some("TARGET"),
            Self::Source => Some("SOURCE"),
            Self::MainDependency => Some("MAIN_DEPENDENCY"),
            Self::WorkingDirectory => Some("WORKING_DIRECTORY"),
            Self::Comment => Some("COMMENT"),
            Self::Depfile => Some("DEPFILE"),
            Self::JobPool => Some("JOB_POOL"),
            Self::JobServerAware => Some("JOB_SERVER_AWARE"),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Options {
    section: Section,
    target: Option<BString>,
    source: Option<BString>,
    sources: Vec<BT<BString>>,
    event: Option<TargetEvent>,
    keywords: BTreeSet<&'static str>,
    append: bool,
    explicit_only: bool,
    implicit_language: Option<BString>,
    missing_values: Vec<&'static str>,
    extra_comment: bool,
    unknown_signature: bool,
}

impl Options {
    fn parse(&mut self, body: &mut CustomCommand, args: &[ExpandedArg], utility: bool) {
        let mut pending = None;
        for arg in args {
            let section = self.section;
            if let Some(keyword) = self.keyword(body, &arg.value, utility) {
                if let Some(previous) = pending.take() {
                    self.missing_values.push(previous);
                }
                if !utility && matches!(self.section, Section::None) {
                    self.section = section;
                } else if keyword != "ARGS" {
                    pending = self.section.pending_value();
                }
                self.keywords.insert(keyword);
            } else {
                self.unknown_signature |= matches!(self.section, Section::None);
                pending = None;
                self.value(body, arg.value.clone());
            }
        }
        body.command_lines.retain(|line| !line.is_empty());
    }

    fn keyword(
        &mut self,
        body: &mut CustomCommand,
        arg: &[u8],
        utility: bool,
    ) -> Option<&'static str> {
        let (section, keyword) = match arg {
            b"COMMAND" => {
                body.command_lines.push(Vec::new());
                (Section::Command, "COMMAND")
            }
            b"ARGS" if !utility => return Some("ARGS"),
            b"OUTPUT" if !utility => (Section::Output, "OUTPUT"),
            b"TARGET" if !utility => (Section::Target, "TARGET"),
            b"DEPENDS" => (Section::Depends, "DEPENDS"),
            b"BYPRODUCTS" => (Section::Byproducts, "BYPRODUCTS"),
            b"SOURCES" if utility => (Section::Sources, "SOURCES"),
            b"MAIN_DEPENDENCY" if !utility => (Section::MainDependency, "MAIN_DEPENDENCY"),
            b"IMPLICIT_DEPENDS" if !utility => {
                self.implicit_language = None;
                (Section::ImplicitDepends, "IMPLICIT_DEPENDS")
            }
            b"WORKING_DIRECTORY" => (Section::WorkingDirectory, "WORKING_DIRECTORY"),
            b"COMMENT" => (Section::Comment, "COMMENT"),
            b"DEPFILE" if !utility => (Section::Depfile, "DEPFILE"),
            b"JOB_POOL" => (Section::JobPool, "JOB_POOL"),
            b"JOB_SERVER_AWARE" => (Section::JobServerAware, "JOB_SERVER_AWARE"),
            b"OUTPUTS" if !utility => (Section::Legacy, "OUTPUTS"),
            b"SOURCE" if !utility => (Section::Source, "SOURCE"),
            _ => {
                let keyword = match arg {
                    b"VERBATIM" => (&mut body.verbatim, "VERBATIM"),
                    b"USES_TERMINAL" => (&mut body.uses_terminal, "USES_TERMINAL"),
                    b"COMMAND_EXPAND_LISTS" => {
                        (&mut body.command_expand_lists, "COMMAND_EXPAND_LISTS")
                    }
                    b"APPEND" if !utility => (&mut self.append, "APPEND"),
                    b"CODEGEN" if !utility => (&mut body.codegen, "CODEGEN"),
                    b"DEPENDS_EXPLICIT_ONLY" if !utility => {
                        (&mut self.explicit_only, "DEPENDS_EXPLICIT_ONLY")
                    }
                    b"PRE_BUILD" | b"PRE_LINK" | b"POST_BUILD" if !utility => {
                        let (event, keyword) = match arg {
                            b"PRE_BUILD" => (TargetEvent::PreBuild, "PRE_BUILD"),
                            b"PRE_LINK" => (TargetEvent::PreLink, "PRE_LINK"),
                            _ => (TargetEvent::PostBuild, "POST_BUILD"),
                        };
                        self.event = Some(event);
                        self.section = Section::None;
                        return Some(keyword);
                    }
                    _ => return None,
                };
                *keyword.0 = true;
                (Section::None, keyword.1)
            }
        };
        self.section = section;
        Some(keyword)
    }

    fn value(&mut self, body: &mut CustomCommand, value: BString) {
        let tagged = |value| BT::new(value, body.backtrace.clone());
        match self.section {
            Section::None | Section::Legacy => {}
            Section::Command => {
                if let Some(line) = body.command_lines.last_mut() {
                    line.push(value);
                }
            }
            Section::Output => body.outputs.push(tagged(value)),
            Section::Depends => body.depends.push(tagged(value)),
            Section::Byproducts => body.byproducts.push(tagged(value)),
            Section::Sources => self.sources.push(tagged(value)),
            Section::Comment => {
                self.extra_comment |= body
                    .comment
                    .replace(value)
                    .is_some_and(|old| !old.is_empty());
            }
            Section::ImplicitDepends => {
                if let Some(language) = self.implicit_language.take() {
                    body.depends.push(tagged(value.clone()));
                    body.implicit_depends.push((language, value));
                } else {
                    self.implicit_language = Some(value);
                }
            }
            section => match section {
                Section::Target => self.target = Some(value),
                Section::Source => self.source = Some(value),
                Section::MainDependency => body.main_dependency = Some(value),
                Section::WorkingDirectory => body.working_directory = Some(value),
                Section::Depfile => body.depfile = Some(value),
                Section::JobPool => body.job_pool = Some(value),
                Section::JobServerAware => body.job_server_aware = truth::is_on(&value),
                _ => {}
            },
        }
    }
}

pub(crate) fn new_command(cx: &mut Cx<'_>) -> CustomCommand {
    CustomCommand::new(
        cx.current_dir_id(),
        CustomCommandKind::Output,
        cx.backtrace(),
        *cx.policies_mut(),
    )
}

fn add_command(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if let [marker, count, rest @ ..] = args
        && marker.value == DEFERRED_APPEND
        && cx.running_deferred()
    {
        let count = std::str::from_utf8(&count.value)
            .ok()
            .and_then(|count| count.parse::<usize>().ok())
            .ok_or_else(|| CmdError::status("Invalid deferred APPEND command count."))?;
        let mut body = new_command(cx);
        let mut options = Options::default();
        options.parse(&mut body, rest, false);
        resolve_paths(cx, &mut body, false)?;
        return append(cx, body, count, None);
    }
    if args.len() < 4 {
        return Err(CmdError::status("called with wrong number of arguments."));
    }
    let mut body = new_command(cx);
    let mut options = Options::default();
    options.parse(&mut body, args, false);
    if options.unknown_signature {
        return Err(crate::command_error(
            cx,
            "add_custom_command Wrong syntax.  Unknown type of argument.",
        ));
    }
    for keyword in &options.missing_values {
        if !check_policy(
            cx,
            format!("Keyword {keyword} requires a value, but none was given."),
            b"",
        ) {
            return Ok(Flow::Normal);
        }
    }
    if options.extra_comment
        && !check_policy(
            cx,
            "COMMENT requires exactly one argument, but multiple values or COMMENT keywords have been given.",
            b"",
        )
    {
        return Ok(Flow::Normal);
    }

    let target = options.target.as_ref().filter(|target| !target.is_empty());
    let legacy_source = options
        .source
        .as_ref()
        .is_some_and(|source| !source.is_empty());
    match (target, body.outputs.is_empty()) {
        (None, false) | (Some(_), true) => {}
        (Some(_), false) if !legacy_source => {
            return Err(crate::command_error(
                cx,
                "add_custom_command Wrong syntax.  A TARGET and OUTPUT can not both be specified.",
            ));
        }
        (None, true) => {
            return Err(CmdError::status(
                "Wrong syntax. A TARGET or OUTPUT must be specified.",
            ));
        }
        (Some(_), false) => {}
    }

    if options.append && body.outputs.is_empty() {
        return Err(crate::command_error(
            cx,
            "add_custom_command given APPEND option with no OUTPUT.",
        ));
    }
    if body
        .depfile
        .as_ref()
        .is_some_and(|depfile| !depfile.is_empty())
        && !body.implicit_depends.is_empty()
        && cx.project().generator != Some(GeneratorKind::Ninja)
    {
        return Err(CmdError::status(
            "IMPLICIT_DEPENDS and DEPFILE can not both be specified.",
        ));
    }

    if body.codegen {
        let error = if body.outputs.is_empty() {
            Some("CODEGEN requires at least 1 OUTPUT.")
        } else if options.append {
            Some("CODEGEN may not be used with APPEND.")
        } else if !body.implicit_depends.is_empty() {
            Some("CODEGEN is not compatible with IMPLICIT_DEPENDS.")
        } else if cx.policy(PolicyId::new(171)) != PolicyStatus::New {
            Some("CODEGEN option requires policy CMP0171 be set to NEW!")
        } else {
            None
        };
        if let Some(error) = error {
            return Err(CmdError::status(error));
        }
    }
    if let Some(target) = target.filter(|_| !options.append) {
        if legacy_source {
            return Err(crate::command_error(
                cx,
                "The SOURCE signatures of add_custom_command are no longer supported.",
            ));
        }
        return add_event(cx, body, &options, target);
    }

    let valid = if options.append {
        check_keywords(
            cx,
            &options,
            &[
                "OUTPUT",
                "APPEND",
                "COMMAND",
                "ARGS",
                "DEPENDS",
                "IMPLICIT_DEPENDS",
                "MAIN_DEPENDENCY",
                "WORKING_DIRECTORY",
                "COMMENT",
            ],
            "using APPEND with add_custom_command(OUTPUT)",
        )
    } else {
        check_keywords(
            cx,
            &options,
            &[
                "OUTPUT",
                "COMMAND",
                "ARGS",
                "DEPENDS",
                "IMPLICIT_DEPENDS",
                "MAIN_DEPENDENCY",
                "WORKING_DIRECTORY",
                "COMMENT",
                "BYPRODUCTS",
                "DEPFILE",
                "JOB_POOL",
                "JOB_SERVER_AWARE",
                "VERBATIM",
                "USES_TERMINAL",
                "COMMAND_EXPAND_LISTS",
                "DEPENDS_EXPLICIT_ONLY",
                "CODEGEN",
            ],
            "using add_custom_command(OUTPUT)",
        )
    };
    if !valid {
        return Ok(Flow::Normal);
    }
    resolve_paths(cx, &mut body, !options.append)?;
    if options.append {
        let count = cx.project().commands().count();
        return append(cx, body, count, Some(args));
    }
    let explicit_only = options.explicit_only
        || truth::is_on(&cx.var_or_empty(b"CMAKE_ADD_CUSTOM_COMMAND_DEPENDS_EXPLICIT_ONLY"));
    let dir = body.dir;
    let id = record(cx, body)?;
    if explicit_only {
        // The command arena has no field for this option.
        cx.project().dir_mut(dir).props.set(
            format!(
                "_RCMAKE_CUSTOM_COMMAND_{}_DEPENDS_EXPLICIT_ONLY",
                id.index()
            )
            .into(),
            "1".into(),
        );
    }
    Ok(Flow::Normal)
}

fn check_keywords(cx: &mut Cx<'_>, options: &Options, allowed: &[&str], context: &str) -> bool {
    let unsupported: Vec<_> = options
        .keywords
        .iter()
        .copied()
        .filter(|keyword| !allowed.contains(keyword))
        .collect();
    unsupported.is_empty()
        || check_policy(
            cx,
            format!(
                "The following keywords are not supported when {context}: {}",
                unsupported.join(", ")
            ),
            b"",
        )
}

fn check_policy(cx: &mut Cx<'_>, message: impl Into<BString>, advice: &[u8]) -> bool {
    let policy = PolicyId::new(175);
    match cx.policy(policy) {
        PolicyStatus::New => {
            cx.issue(MessageKind::Error, Effect::SetError, None, message.into());
            false
        }
        PolicyStatus::Warn => {
            let mut message = message.into();
            if !message.ends_with(b".") {
                message.push(b'.');
            }
            message.extend_from_slice(advice);
            message.push(b'\n');
            cx.issue_policy_warning(policy, &message, b"");
            true
        }
        PolicyStatus::Old => true,
    }
}

fn append(
    cx: &mut Cx<'_>,
    body: CustomCommand,
    count: usize,
    args: Option<&[ExpandedArg]>,
) -> CmdResult {
    let Some(output) = body.outputs.first() else {
        return Err(CmdError::status("APPEND requires an OUTPUT."));
    };
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
        // Move this retry to an internal post-directory hook after W11B.
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
            "Attempt to APPEND to custom command with output\n\n  {}\n\nwhich is not already a custom command output.",
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

fn add_event(
    cx: &mut Cx<'_>,
    mut body: CustomCommand,
    options: &Options,
    name: &[u8],
) -> CmdResult {
    if body.command_lines.is_empty()
        && !check_policy(cx, "At least one COMMAND must be given.", b"")
    {
        return Ok(Flow::Normal);
    }
    if !check_keywords(
        cx,
        options,
        &[
            "TARGET",
            "PRE_BUILD",
            "PRE_LINK",
            "POST_BUILD",
            "COMMAND",
            "ARGS",
            "BYPRODUCTS",
            "WORKING_DIRECTORY",
            "COMMENT",
            "JOB_SERVER_AWARE",
            "VERBATIM",
            "COMMAND_EXPAND_LISTS",
            "USES_TERMINAL",
        ],
        "using add_custom_command(TARGET)",
    ) {
        return Ok(Flow::Normal);
    }
    let event = options.event.unwrap_or(TargetEvent::PostBuild);
    if options
        .keywords
        .iter()
        .filter(|key| matches!(**key, "PRE_BUILD" | "PRE_LINK" | "POST_BUILD"))
        .count()
        != 1
    {
        let name = match event {
            TargetEvent::PreBuild => "PRE_BUILD",
            TargetEvent::PreLink => "PRE_LINK",
            TargetEvent::PostBuild => "POST_BUILD",
        };
        if !check_policy(
            cx,
            "Exactly one of PRE_BUILD, PRE_LINK, or POST_BUILD must be given.",
            format!("  Assuming {name} to preserve backward compatibility.").as_bytes(),
        ) {
            return Ok(Flow::Normal);
        }
    }
    let project = cx.project();
    let name_text = BString::from(name);
    let target = match project.lookup_target(body.dir, name) {
        Some(TargetBinding::Target(id) | TargetBinding::Alias(id)) => id,
        None => {
            cx.issue_in_directory(
                MessageKind::Error,
                Effect::SetError,
                None,
                format!("No TARGET '{name_text}' has been created in this directory.").into(),
            );
            return Ok(Flow::Normal);
        }
    };
    let entry = project
        .target(target)
        .ok_or_else(|| CmdError::status("Unknown target."))?;
    let problem = if entry.is_imported() {
        Some(format!(
            "TARGET '{name_text}' is IMPORTED and does not build here."
        ))
    } else if entry.dir != body.dir {
        Some(format!(
            "TARGET '{name_text}' was not created in this directory."
        ))
    } else {
        match entry.kind {
            TargetKind::ObjectLibrary => Some("OBJECT"),
            TargetKind::InterfaceLibrary => Some("INTERFACE"),
            _ => None,
        }.map(|kind| format!(
            "Target \"{name_text}\" is an {kind} library that may not have PRE_BUILD, PRE_LINK, or POST_BUILD commands."
        ))
    };
    if let Some(problem) = problem {
        cx.issue_in_directory(MessageKind::Error, Effect::SetError, None, problem.into());
        return Ok(Flow::Normal);
    }
    body.kind = CustomCommandKind::TargetEvent { target, event };
    resolve_paths(cx, &mut body, true)?;
    let id = record(cx, body)?;
    if let Some(target) = cx.project().target_mut(target) {
        target.event_commands.push(id);
    }
    Ok(Flow::Normal)
}

fn add_target(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
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

fn resolve_paths(
    cx: &mut Cx<'_>,
    body: &mut CustomCommand,
    validate_outputs: bool,
) -> Result<(), CmdError> {
    if validate_outputs
        && body.uses_terminal
        && body.job_pool.as_ref().is_some_and(|pool| !pool.is_empty())
    {
        return Err(CmdError::status("JOB_POOL is shadowed by USES_TERMINAL."));
    }
    if let Some(quoted) = body
        .command_lines
        .iter()
        .filter_map(|line| line.first())
        .find(|arg| arg.starts_with(b"\""))
    {
        cx.issue(
            MessageKind::Error,
            Effect::SetError,
            None,
            format!("COMMAND may not contain literal quotes:\n  {quoted}\n").into(),
        );
        return Err(CmdError::Reported);
    }
    let base = from_native(&cx.current_binary_dir());
    // The generator checks each path after it evaluates generator expressions.
    for entry in body.outputs.iter_mut().chain(body.byproducts.iter_mut()) {
        entry.value = binary_path(&base, &entry.value);
    }
    if let Some(dir) = &mut body.working_directory
        && !dir.is_empty()
    {
        *dir = binary_path(&base, dir);
    }
    Ok(())
}

fn binary_path(base: &[u8], value: &[u8]) -> BString {
    let value: BString = value
        .iter()
        .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
        .collect::<Vec<_>>()
        .into();
    if value.starts_with(b"$<") {
        return value;
    }
    let absolute = Lexical::new(base).append(&value);
    if value.windows(2).any(|pair| pair == b"$<") {
        // A generator expression can contain path separators within an argument.
        absolute
    } else {
        Lexical::new(&absolute).normalize()
    }
}

pub(crate) fn record(
    cx: &mut Cx<'_>,
    body: CustomCommand,
) -> Result<rcmake_model::CommandId, CmdError> {
    let dir = body.dir;
    let generated: Vec<_> = body
        .outputs
        .iter()
        .chain(&body.byproducts)
        .map(|entry| entry.value.clone())
        .collect();
    let project = cx.project();
    let id = project.add_command(body).map_err(model_error)?;
    for path in generated {
        if find_genex(&path).is_none() {
            let directory = project.dir_mut(dir);
            if directory.source_props.len() > directory.generated_sources.len()
                && let Some(key) = directory.source_props.iter().find_map(|(key, props)| {
                    let alternate = props.get(SOURCE_BINARY_ALTERNATIVE)?;
                    (key == &path || alternate == &path).then(|| key.clone())
                })
            {
                bind_source(directory, &key, &path);
            }
            project.mark_generated(dir, path);
        }
    }
    Ok(id)
}

/// Bind a relative source record to its resolved path. Keep its insertion order.
pub(crate) fn bind_source(directory: &mut Directory, old: &BString, new: &BString) {
    let sources = &mut directory.source_props;
    if old == new {
        if let Some(props) = sources.get_mut(old) {
            props.unset(SOURCE_BINARY_ALTERNATIVE);
        }
    } else if !sources.contains_key(new)
        && let Some((position, _, mut props)) = sources.shift_remove_full(old)
    {
        props.unset(SOURCE_BINARY_ALTERNATIVE);
        sources.shift_insert(position, new.clone(), props);
    }
}

fn model_error(error: ModelError) -> CmdError {
    CmdError::status(error.to_string())
}
