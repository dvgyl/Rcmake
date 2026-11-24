//! Custom commands, utility targets, and custom rule templates.

use std::collections::BTreeSet;

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_host::path::from_native;
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, ModelError, PropOrigin, PropScope, Target, TargetBinding,
    TargetEvent, TargetKind, TargetOrigin,
};

mod rule;
#[cfg(test)]
mod tests;

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
}

impl Section {
    fn pending_value(self, body: &CustomCommand) -> Option<&'static str> {
        match self {
            Self::Target => Some("TARGET"),
            Self::MainDependency => Some("MAIN_DEPENDENCY"),
            Self::WorkingDirectory => Some("WORKING_DIRECTORY"),
            Self::Comment if body.comment.is_none() => Some("COMMENT"),
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
    sources: Vec<BT<BString>>,
    events: Vec<TargetEvent>,
    keywords: BTreeSet<&'static str>,
    append: bool,
    explicit_only: bool,
    implicit_language: Option<BString>,
    missing_values: BTreeSet<&'static str>,
    extra_comment: bool,
}

impl Options {
    fn parse(&mut self, body: &mut CustomCommand, args: &[ExpandedArg], utility: bool) {
        for arg in args {
            let previous = self.section;
            if let Some(keyword) = self.keyword(body, &arg.value, utility) {
                if keyword != "ARGS" {
                    if let Some(previous) = previous.pending_value(body) {
                        self.missing_values.insert(previous);
                    }
                    self.implicit_language = None;
                }
                if !self.keywords.insert(keyword) && keyword == "COMMENT" {
                    self.extra_comment = true;
                }
            } else {
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
            b"IMPLICIT_DEPENDS" if !utility => (Section::ImplicitDepends, "IMPLICIT_DEPENDS"),
            b"WORKING_DIRECTORY" => (Section::WorkingDirectory, "WORKING_DIRECTORY"),
            b"COMMENT" => (Section::Comment, "COMMENT"),
            b"DEPFILE" if !utility => (Section::Depfile, "DEPFILE"),
            b"JOB_POOL" => (Section::JobPool, "JOB_POOL"),
            b"JOB_SERVER_AWARE" => (Section::JobServerAware, "JOB_SERVER_AWARE"),
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
                        self.events.push(event);
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
            Section::None => {}
            Section::Command => {
                if let Some(line) = body.command_lines.last_mut() {
                    line.push(value);
                }
            }
            Section::Output => body.outputs.push(tagged(value)),
            Section::Depends => body.depends.push(tagged(value)),
            Section::Byproducts => body.byproducts.push(tagged(value)),
            Section::Sources => self.sources.push(tagged(value)),
            Section::Comment => self.extra_comment |= body.comment.replace(value).is_some(),
            Section::ImplicitDepends => {
                if let Some(language) = self.implicit_language.take() {
                    body.depends.push(tagged(value.clone()));
                    body.implicit_depends.push((language, value));
                } else {
                    self.implicit_language = Some(value);
                }
            }
            section => {
                match section {
                    Section::Target => self.target = Some(value),
                    Section::MainDependency => body.main_dependency = Some(value),
                    Section::WorkingDirectory => body.working_directory = Some(value),
                    Section::Depfile => body.depfile = Some(value),
                    Section::JobPool => body.job_pool = Some(value),
                    Section::JobServerAware => body.job_server_aware = truth::is_on(&value),
                    _ => {}
                }
                self.section = Section::None;
            }
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
    if args.len() < 4 {
        return Err(CmdError::status("called with wrong number of arguments."));
    }
    let mut body = new_command(cx);
    let mut options = Options::default();
    options.parse(&mut body, args, false);

    for keyword in &options.missing_values {
        check_policy(
            cx,
            format!("Keyword {keyword} requires a value, but none was given."),
        )?;
    }
    if options.extra_comment {
        check_policy(
            cx,
            "COMMENT requires exactly one argument, but multiple values or COMMENT keywords have been given.",
        )?;
    }

    match (&options.target, body.outputs.is_empty()) {
        (Some(target), true) => return add_event(cx, body, &options, target),
        (None, false) => {}
        _ => {
            return Err(CmdError::status(
                "Wrong syntax. A TARGET or OUTPUT must be specified.",
            ));
        }
    }

    if options.append {
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
        )?;
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
        )?;
    }
    if body.depfile.is_some() && !body.implicit_depends.is_empty() {
        return Err(CmdError::status(
            "IMPLICIT_DEPENDS and DEPFILE can not both be specified.",
        ));
    }
    resolve_paths(cx, &mut body)?;
    if options.append {
        return append(cx, body);
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

fn check_keywords(
    cx: &mut Cx<'_>,
    options: &Options,
    allowed: &[&str],
    context: &str,
) -> Result<(), CmdError> {
    let unsupported: Vec<_> = options
        .keywords
        .iter()
        .copied()
        .filter(|keyword| !allowed.contains(keyword))
        .collect();
    if !unsupported.is_empty() {
        check_policy(
            cx,
            format!(
                "The following keywords are not supported when {context}: {}",
                unsupported.join(", ")
            ),
        )?;
    }
    Ok(())
}

fn check_policy(cx: &mut Cx<'_>, message: impl Into<BString>) -> Result<(), CmdError> {
    let policy = PolicyId::new(175);
    match cx.policy(policy) {
        PolicyStatus::New => Err(crate::command_error(cx, message)),
        PolicyStatus::Warn => {
            let message = message.into();
            cx.issue_policy_warning(policy, b"", &message);
            Ok(())
        }
        PolicyStatus::Old => Ok(()),
    }
}

fn append(cx: &mut Cx<'_>, body: CustomCommand) -> CmdResult {
    let Some(output) = body.outputs.first() else {
        return Err(CmdError::status("APPEND requires an OUTPUT."));
    };
    let project = cx.project();
    let existing = project.commands().find_map(|(id, command)| {
        (command.dir == body.dir
            && command.kind == CustomCommandKind::Output
            && command.outputs.contains(output))
        .then_some(id)
    });
    let Some(existing) = existing.and_then(|id| project.command_mut(id)) else {
        return Err(crate::command_error(
            cx,
            format!(
                "Attempt to APPEND to custom command with output\n\n  {}\n\nwhich is not already a custom command output.",
                output.value
            ),
        ));
    };
    existing.command_lines.extend(body.command_lines);
    existing.depends.extend(body.depends);
    existing.implicit_depends.extend(body.implicit_depends);
    Ok(Flow::Normal)
}

fn add_event(
    cx: &mut Cx<'_>,
    mut body: CustomCommand,
    options: &Options,
    name: &[u8],
) -> CmdResult {
    check_keywords(
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
            "VERBATIM",
            "COMMAND_EXPAND_LISTS",
            "USES_TERMINAL",
        ],
        "using add_custom_command(TARGET)",
    )?;
    if options.events.len() != 1 {
        check_policy(
            cx,
            "Exactly one of PRE_BUILD, PRE_LINK, or POST_BUILD must be given.",
        )?;
    }
    if body.command_lines.is_empty() {
        check_policy(cx, "At least one COMMAND must be given.")?;
    }
    let event = options
        .events
        .last()
        .copied()
        .unwrap_or(TargetEvent::PostBuild);
    let project = cx.project();
    let name_text = BString::from(name);
    let target = match project.lookup_target(body.dir, name) {
        Some(TargetBinding::Target(id) | TargetBinding::Alias(id)) => id,
        None => {
            return Err(crate::command_error(
                cx,
                format!("No TARGET '{name_text}' has been created in this directory."),
            ));
        }
    };
    let entry = project
        .target(target)
        .ok_or_else(|| CmdError::status("Unknown target."))?;
    let problem = if entry.dir != body.dir {
        Some(format!(
            "TARGET '{name_text}' was not created in this directory."
        ))
    } else if entry.is_imported() {
        Some(format!(
            "TARGET '{name_text}' is IMPORTED and does not build here."
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
        return Err(crate::command_error(cx, problem));
    }
    body.kind = CustomCommandKind::TargetEvent { target, event };
    resolve_paths(cx, &mut body)?;
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
    if name.is_empty()
        || !name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte))
    {
        return Err(CmdError::status(format!("Invalid target name \"{name}\".")));
    }

    if matches!(
        name.as_slice(),
        b"all" | b"clean" | b"help" | b"install" | b"edit_cache" | b"rebuild_cache"
    ) {
        return Err(crate::command_error(
            cx,
            format!(
                "The target name \"{name}\" is reserved or not valid for certain CMake features, such as generator expressions, and may result in undefined behavior."
            ),
        ));
    }
    if name == b"test" && cx.var_is_on(b"CMAKE_TESTING_ENABLED") {
        return Err(crate::command_error(
            cx,
            "The target name \"test\" is reserved when CTest testing is enabled.",
        ));
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
    resolve_paths(cx, &mut body)?;

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
    let id = cx.project().add_target(target).map_err(model_error)?;
    body.kind = CustomCommandKind::Utility(id);
    record(cx, body)?;
    crate::dirs::source_groups::apply_to_target(cx, id)?;
    Ok(Flow::Normal)
}

fn resolve_paths(cx: &mut Cx<'_>, body: &mut CustomCommand) -> Result<(), CmdError> {
    if body.uses_terminal && body.job_pool.is_some() {
        return Err(CmdError::status("JOB_POOL is shadowed by USES_TERMINAL."));
    }
    let base = from_native(&cx.current_binary_dir());
    for entry in body.outputs.iter_mut().chain(&mut body.byproducts) {
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
    let backtrace = body.backtrace.clone();
    let generated: Vec<_> = body
        .outputs
        .iter()
        .chain(&body.byproducts)
        .map(|entry| entry.value.clone())
        .collect();
    let project = cx.project();
    let id = project.add_command(body).map_err(model_error)?;
    for path in generated {
        if !path.windows(2).any(|pair| pair == b"$<") {
            project
                .set_prop(
                    &PropScope::Source { dir, path },
                    b"GENERATED",
                    Some("1".into()),
                    PropOrigin {
                        dir,
                        backtrace: &backtrace,
                    },
                )
                .map_err(model_error)?;
        }
    }
    Ok(id)
}

fn model_error(error: ModelError) -> CmdError {
    CmdError::status(error.to_string())
}
