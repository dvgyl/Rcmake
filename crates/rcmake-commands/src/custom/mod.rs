//! Custom commands, utility targets, and custom rule templates.

use std::collections::BTreeSet;

use rcmake_base::{BString, ByteSlice, hash::HashAlgo, list, path_lex::Lexical, truth};
use rcmake_diag::{Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Flow, Registry};
use rcmake_expr::ExpandedArg;
use rcmake_genex::{EvalContext, EvalSite, evaluate, find_genex, parse};
use rcmake_host::path::from_native;
use rcmake_model::{
    BT, CustomCommand, CustomCommandKind, DirId, Directory, GeneratorKind, TargetBinding,
    TargetEvent, TargetKind,
};
use rcmake_resolve::usage::UsageResolver;

use crate::util::model_error;

mod append;
mod rule;
mod target;
#[cfg(test)]
mod tests;

const DEFERRED_APPEND: &[u8] = b"\0RCMAKE_CUSTOM_APPEND";

/// Possible binary path of a relative source property record.
pub(crate) const SOURCE_BINARY_ALTERNATIVE: &[u8] = b"\0rcmake-source-binary-alternative";

/// Directory property recording configure-time source registrations in call order.
pub(crate) const SOURCE_ORDER_PROP: &[u8] = b"\0rcmake-source-order";

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    registry.add("add_custom_command", add_command, project_only);
    registry.add("add_custom_target", target::add_target, project_only);
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
        return append::run(cx, body, count, None);
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
        return append::run(cx, body, count, Some(args));
    }
    let Some(rule_key) = new_output_rule(cx, &body)? else {
        return Ok(Flow::Normal);
    };
    let explicit_only = options.explicit_only
        || truth::is_on(&cx.var_or_empty(b"CMAKE_ADD_CUSTOM_COMMAND_DEPENDS_EXPLICIT_ONLY"));
    let dir = body.dir;
    let id = record(cx, body)?;
    cx.project()
        .dir_mut(dir)
        .props
        .set(rule_key, id.index().to_string().into());
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

fn new_output_rule(cx: &mut Cx<'_>, body: &CustomCommand) -> Result<Option<BString>, CmdError> {
    if !body.command_lines.is_empty()
        && let Some(dependency) = body
            .main_dependency
            .as_ref()
            .filter(|path| !path.is_empty())
    {
        let project = cx.project();
        let dir = project.dir(body.dir);
        let path = Lexical::new(&Lexical::new(&dir.source_dir).append(dependency)).normalize();
        let key: BString = [b"\0rcmake-custom-main/".as_slice(), &path].concat().into();
        let existing = dir
            .props
            .get(&key)
            .and_then(|value| value.to_str().ok()?.parse::<usize>().ok())
            .and_then(|index| project.commands().nth(index).map(|(_, command)| command));
        match existing {
            None => return Ok(Some(key)),
            Some(command) if command.command_lines == body.command_lines => return Ok(None),
            Some(_) => {}
        }
    }

    let output = body
        .outputs
        .first()
        .ok_or_else(|| CmdError::status("No OUTPUT specified."))?;
    let rule_name = custom_rule_identity(cx, body.dir, output);
    let key: BString = [b"\0rcmake-custom-rule/".as_slice(), &rule_name]
        .concat()
        .into();
    let project = cx.project();
    let dir = project.dir(body.dir);
    let existing = dir
        .props
        .get(&key)
        .and_then(|value| value.to_str().ok()?.parse::<usize>().ok())
        .and_then(|index| project.commands().nth(index).map(|(_, command)| command));
    match existing {
        None => Ok(Some(key)),
        Some(command) if command.command_lines == body.command_lines => Ok(None),
        Some(_) => Err(crate::command_error(
            cx,
            format!(
                "Attempt to add a custom rule to output\n  {rule_name}.rule\nwhich already has a custom rule."
            ),
        )),
    }
}

/// Derive CMake's custom-command rule file stem for `output` (without `.rule`).
fn custom_rule_identity(cx: &mut Cx<'_>, dir: DirId, output: &BT<BString>) -> BString {
    let normalized = normalize_genex_path(&output.value);
    if find_genex(&normalized).is_none() {
        return normalized;
    }
    let simple: BString = normalized.replace(b"$<CONFIG>", b"(CONFIG)").into();
    if find_genex(&simple).is_none() {
        let path = if Lexical::new(&simple).absolute() {
            simple
        } else {
            let source_dir = &cx.project().dir(dir).source_dir;
            Lexical::new(source_dir).append(&simple)
        };
        return Lexical::new(&path).normalize();
    }
    let configs = append::active_configs(cx);
    let project = cx.project();
    let binary_dir = project.dir(dir).binary_dir.clone();
    if let Ok(ast) = parse(&normalized) {
        let mut usage = UsageResolver::new(project);
        let mut host = usage.host(dir);
        let mut expanded = BTreeSet::new();
        let mut ok = true;
        for config in &configs {
            let context = EvalContext {
                config: config.clone(),
                site: EvalSite {
                    backtrace: output.backtrace.clone(),
                    property: None,
                },
                ..EvalContext::default()
            };
            match evaluate(&ast, &context, &mut host) {
                Ok((value, _)) => {
                    for item in list::split(&value, list::Empty::Drop) {
                        let resolved =
                            Lexical::new(&Lexical::new(&binary_dir).append(&item)).normalize();
                        expanded.insert(resolved);
                    }
                }
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if ok
            && expanded.len() == 1
            && let Some(single) = expanded.into_iter().next()
        {
            return single;
        }
    }
    let digest = HashAlgo::Sha256.hex_digest(&normalized);
    let short = digest.get(..16).unwrap_or(&digest);
    Lexical::new(&Lexical::new(&binary_dir).append(format!("CMakeFiles/{short}").as_bytes()))
        .normalize()
}

/// Normalize path separators and dot segments outside `$<...>` generator expressions.
fn normalize_genex_path(path: &[u8]) -> BString {
    let slashed: Vec<u8> = path
        .iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect();
    if find_genex(&slashed).is_none() {
        return Lexical::new(&slashed).normalize();
    }
    let (prefix, rest) = match slashed.as_slice() {
        [drive, b':', b'/', tail @ ..] if drive.is_ascii_alphabetic() => {
            (slashed.get(..3).unwrap_or_default(), tail)
        }
        [b'/', tail @ ..] => (b"/".as_slice(), tail),
        tail => (b"".as_slice(), tail),
    };
    let absolute = !prefix.is_empty();
    let mut segments: Vec<&[u8]> = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0_usize;
    let mut i = 0_usize;
    while i < rest.len() {
        if rest.get(i..i + 2) == Some(b"$<") {
            depth += 1;
            i += 2;
        } else if rest.get(i) == Some(&b'>') && depth > 0 {
            depth -= 1;
            i += 1;
        } else if rest.get(i) == Some(&b'/') && depth == 0 {
            if let Some(segment) = rest.get(start..i) {
                push_genex_segment(&mut segments, segment, absolute);
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if let Some(segment) = rest.get(start..) {
        push_genex_segment(&mut segments, segment, absolute);
    }
    let mut out = BString::from(prefix);
    for (idx, segment) in segments.into_iter().enumerate() {
        if idx > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(segment);
    }
    out
}

fn push_genex_segment<'a>(segments: &mut Vec<&'a [u8]>, segment: &'a [u8], absolute: bool) {
    if segment.is_empty() || segment == b"." {
        return;
    }
    if segment == b".." {
        if let Some(&last) = segments.last()
            && last != b".."
            && find_genex(last).is_none()
        {
            segments.pop();
        } else if !absolute {
            segments.push(segment);
        }
    } else {
        segments.push(segment);
    }
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
            let bound = directory.source_props.len() > directory.generated_sources.len()
                && if let Some(key) = directory.source_props.iter().find_map(|(key, props)| {
                    let alternate = props.get(SOURCE_BINARY_ALTERNATIVE)?;
                    (key == &path || alternate == &path).then(|| key.clone())
                }) {
                    bind_source(directory, &key, &path);
                    true
                } else {
                    false
                };
            if !bound && !directory.source_props.contains_key(&path) {
                record_source_order(directory, &path, false);
            }
            project.mark_generated(dir, path);
        }
    }
    Ok(id)
}

/// Record a source file registration in configure-time call order for `dir`.
pub(crate) fn record_source_order(dir: &mut Directory, path: &[u8], relative: bool) {
    if path.is_empty() || path.contains(&0) || find_genex(path).is_some() {
        return;
    }
    let slashed: Vec<u8> = path
        .iter()
        .map(|&byte| if byte == b'\\' { b'/' } else { byte })
        .collect();
    let normalized = Lexical::new(&slashed).normalize();
    if normalized.is_empty() {
        return;
    }
    let mut entry = Vec::with_capacity(normalized.len() + 2);
    entry.push(if relative { b'R' } else { b'A' });
    entry.extend_from_slice(&normalized);
    entry.push(0);
    dir.props.append_string(SOURCE_ORDER_PROP.into(), &entry);
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

#[cfg(test)]
#[test]
fn output_rules_keep_the_first_matching_command_details() -> crate::test_support::TestResult {
    use std::sync::Arc;

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{PureHost, Stream, path::to_native};

    let cwd = to_native(if cfg!(windows) { b"C:/work" } else { b"/work" });
    let exe = cwd.join("cmake");
    let src = cwd.join("src");
    let build = cwd.join("build");
    let host = Arc::new(PureHost::new(cwd.clone(), exe.clone()));
    let script = br#"
cmake_minimum_required(VERSION 4.5)
add_custom_command(OUTPUT out one COMMAND tool COMMENT first)
add_custom_command(OUTPUT out two COMMAND tool COMMENT second)
add_custom_command(OUTPUT out APPEND COMMAND later)
add_custom_command(OUTPUT via_main MAIN_DEPENDENCY input COMMAND tool COMMENT main)
add_custom_command(OUTPUT duplicate MAIN_DEPENDENCY ./input COMMAND tool)
add_custom_command(OUTPUT fallback MAIN_DEPENDENCY input COMMAND other)
add_custom_command(OUTPUT out COMMAND different)
"#;
    host.seed_file(&src.join("CMakeLists.txt"), script)?;
    let mut interp = Interp::new(
        InterpConfig::new(host.clone(), RunMode::Configure, cwd.join("data"), exe, cwd),
        crate::register_all,
    );
    assert_eq!(interp.run_configure(&src, &build, &[]).exit_code, 1);
    let diagnostics = host.stream_bytes(Stream::Stderr);
    assert!(
        diagnostics
            .to_string()
            .contains("which already has a custom rule")
    );
    let mut cx = interp.cx();
    let commands = cx
        .project()
        .commands()
        .map(|(_, command)| command)
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].comment, Some("first".into()));
    assert!(commands[0].outputs[1].value.ends_with(b"/one"));
    assert_eq!(commands[0].command_lines.len(), 2);
    assert!(commands[2].outputs[0].value.ends_with(b"/fallback"));
    Ok(())
}

#[cfg(test)]
#[test]
fn output_rules_normalize_equivalent_paths_and_genex_rule_identities()
-> crate::test_support::TestResult {
    use std::sync::Arc;

    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::{PureHost, Stream, path::to_native};

    let run = |script: &[u8]| -> Result<(i32, String), Box<dyn std::error::Error>> {
        let cwd = to_native(if cfg!(windows) { b"C:/work" } else { b"/work" });
        let exe = cwd.join("cmake");
        let src = cwd.join("src");
        let build = cwd.join("build");
        let host = Arc::new(PureHost::new(cwd.clone(), exe.clone()));
        host.seed_file(&src.join("CMakeLists.txt"), script)?;
        let mut interp = Interp::new(
            InterpConfig::new(host.clone(), RunMode::Configure, cwd.join("data"), exe, cwd),
            crate::register_all,
        );
        let code = interp.run_configure(&src, &build, &[]).exit_code;
        Ok((code, host.stream_bytes(Stream::Stderr).to_string()))
    };

    // Equivalent unnormalized plain paths conflict when commands differ.
    let (code, stderr) = run(br#"
cmake_minimum_required(VERSION 4.5)
add_custom_command(OUTPUT dir/../out.txt COMMAND tool_a)
add_custom_command(OUTPUT ./out.txt COMMAND tool_b)
"#)?;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("out.txt.rule"), "{stderr}");
    assert!(
        stderr.contains("which already has a custom rule"),
        "{stderr}"
    );

    // Equivalent unnormalized $<CONFIG> paths map to the same (CONFIG) rule file.
    let (code, stderr) = run(br#"
cmake_minimum_required(VERSION 4.5)
add_custom_command(OUTPUT ${CMAKE_CURRENT_BINARY_DIR}//$<CONFIG>/out.txt COMMAND tool_a)
add_custom_command(OUTPUT sub/../$<CONFIG>/out.txt COMMAND tool_b)
"#)?;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("(CONFIG)/out.txt.rule"), "{stderr}");
    assert!(
        stderr.contains("which already has a custom rule"),
        "{stderr}"
    );

    // A generator expression that expands to a single constant path matches that plain output.
    let (code, stderr) = run(br#"
cmake_minimum_required(VERSION 4.5)
add_custom_command(OUTPUT $<IF:1,out.txt,other.txt> COMMAND tool_a)
add_custom_command(OUTPUT out.txt COMMAND tool_b)
"#)?;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("out.txt.rule"), "{stderr}");
    assert!(
        stderr.contains("which already has a custom rule"),
        "{stderr}"
    );

    // Identical commands on equivalent $<CONFIG> spellings deduplicate cleanly.
    let (code, stderr) = run(br#"
cmake_minimum_required(VERSION 4.5)
add_custom_command(OUTPUT ${CMAKE_CURRENT_BINARY_DIR}//$<CONFIG>/same.txt COMMAND tool_a)
add_custom_command(OUTPUT sub/../$<CONFIG>/same.txt COMMAND tool_a)
"#)?;
    assert_eq!(code, 0, "{stderr}");
    Ok(())
}
