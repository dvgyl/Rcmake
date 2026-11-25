//! Built-in commands owned by the evaluator: `break`, `continue`, `return`, `include`,
//! `include_guard`, and `message`.

use rcmake_base::BString;
use rcmake_base::list::{Empty, split_into};
use rcmake_diag::{
    DiagAction, DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus,
};
use rcmake_expr::ExpandedArg;
use rcmake_host::Stream;

use crate::util::cat;
use crate::util::collapse_full_path;
use crate::{CmdError, CmdFlags, CmdResult, Cx, Flow, IncludeOptions, Interp, LogLevel, Registry};

const SCRIPTABLE: CmdFlags = CmdFlags { scriptable: true };

/// Register the commands that this crate owns.
pub(crate) fn register_core(reg: &mut Registry) {
    reg.add("break", cmd_break, SCRIPTABLE);
    reg.add("continue", cmd_continue, SCRIPTABLE);
    reg.add("return", cmd_return, SCRIPTABLE);
    reg.add("include", cmd_include, SCRIPTABLE);
    reg.add("include_guard", cmd_include_guard, SCRIPTABLE);
    reg.add("message", cmd_message, SCRIPTABLE);
}

fn in_loop(interp: &Interp) -> bool {
    interp.loop_blocks.last().is_some_and(|&n| n > 0)
}

fn cmd_break(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if !in_loop(cx.interp) {
        cx.interp.error_here(
            "A BREAK command was found outside of a proper FOREACH or WHILE loop scope.".into(),
        );
        return Err(CmdError::Reported);
    }
    if !args.is_empty() {
        cx.interp
            .error_here("The BREAK command does not accept any arguments.".into());
        return Err(CmdError::Reported);
    }
    Ok(Flow::Break)
}

fn cmd_continue(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if !in_loop(cx.interp) {
        cx.interp.fatal_here(
            "A CONTINUE command was found outside of a proper FOREACH or WHILE loop scope.".into(),
        );
        return Err(CmdError::Reported);
    }
    if !args.is_empty() {
        cx.interp
            .fatal_here("The CONTINUE command does not accept any arguments.".into());
        return Err(CmdError::Reported);
    }
    Ok(Flow::Continue)
}

fn cmd_return(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some(first) = args.first() else {
        return Ok(Flow::Return { propagate: None });
    };
    match cx.policy(PolicyId::new(140)) {
        PolicyStatus::Warn => {
            cx.issue_policy_warning(
                PolicyId::new(140),
                b"",
                b"return() checks its arguments when the policy is set to NEW. Since the policy is not set the OLD behavior will be used so the arguments will be ignored.",
            );
            return Ok(Flow::Normal);
        }
        PolicyStatus::Old => return Ok(Flow::Normal),
        PolicyStatus::New => {}
    }
    if first.value.as_slice() != b"PROPAGATE" {
        cx.set_fatal();
        return Err(CmdError::Status(cat(&[
            b"called with unsupported argument \"",
            &first.value,
            b"\"",
        ])));
    }
    let names = args.iter().skip(1).map(|a| a.value.clone()).collect();
    Ok(Flow::Return {
        propagate: Some(names),
    })
}

/// Modules whose use is governed by a deprecation policy (`include()`).
const DEPRECATED_MODULES: [(&[u8], u16); 12] = [
    (b"CMakeDetermineVSServicePack", 196),
    (b"CMakeFindFrameworks", 173),
    (b"Dart", 145),
    (b"Documentation", 106),
    (b"FindBoost", 167),
    (b"FindCABLE", 191),
    (b"FindCUDA", 146),
    (b"FindDart", 145),
    (b"FindGCCXML", 188),
    (b"FindPythonInterp", 148),
    (b"FindPythonLibs", 148),
    (b"WriteCompilerDetectionHeader", 120),
];

fn is_full_path(p: &[u8]) -> bool {
    matches!(p.first(), Some(b'/' | b'\\')) || (p.len() >= 2 && p.get(1) == Some(&b':'))
}

impl Interp {
    pub(crate) fn warn_deprecated_variable(&mut self, name: &[u8]) {
        let id = PolicyId::new(218);
        if matches!(name, b"CMAKE_WARN_DEPRECATED" | b"CMAKE_ERROR_DEPRECATED")
            && self.policy_status(id) == PolicyStatus::Warn
        {
            self.issue_policy_warning(id, b"", b"");
        }
    }

    pub(crate) fn legacy_deprecation_action(&self, message: bool) -> DiagAction {
        let is_on = rcmake_base::truth::is_on;
        let error = self
            .lookup_var(b"CMAKE_ERROR_DEPRECATED")
            .is_some_and(|v| is_on(v));
        let silent = self
            .lookup_var(b"CMAKE_WARN_DEPRECATED")
            .is_some_and(|value| {
                let v = value.as_slice();
                !v.is_empty() && v != b"NOTFOUND" && !v.ends_with(b"-NOTFOUND") && !is_on(v)
            });
        match (message, error, silent) {
            (true, true, _) => DiagAction::FatalError,
            (_, _, true) => DiagAction::Ignore,
            (false, true, false) => DiagAction::SendError,
            _ => DiagAction::Warn,
        }
    }

    /// `cmMakefile::GetModulesFile`: returns `(path, system)`; `path` is empty when the
    /// module is not found.
    pub(crate) fn modules_file(&self, file: &[u8]) -> (BString, bool) {
        let cwd = rcmake_host::path::from_native_cmake(&self.config.cwd);
        let mut in_module_path = None;
        if let Some(paths) = self.lookup_var(b"CMAKE_MODULE_PATH") {
            let mut dirs = Vec::new();
            split_into(paths, Empty::Drop, &mut dirs);
            for dir in dirs {
                let mut candidate = crate::util::replace_all(&dir, b"\\", b"/");
                candidate.push(b'/');
                candidate.extend_from_slice(file);
                let abs = collapse_full_path(&candidate, &cwd);
                if self.exists(&abs) {
                    in_module_path = Some(BString::from(candidate));
                    break;
                }
            }
        }
        let root = rcmake_host::path::from_native_cmake(&self.config.cmake_root);
        let mods = cat(&[&root, b"/Modules/"]);
        let in_root = Some(cat(&[&mods, file])).filter(|p| self.exists(p));
        match (in_module_path, in_root) {
            (Some(user), Some(system)) => {
                let current = self
                    .lookup_var(b"CMAKE_CURRENT_LIST_FILE")
                    .cloned()
                    .unwrap_or_default();
                if current.starts_with(&mods) {
                    (system, true)
                } else {
                    (user, false)
                }
            }
            (Some(user), None) => (user, false),
            (None, Some(system)) => (system, true),
            (None, None) => (BString::default(), true),
        }
    }
}

fn cmd_include(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if cx.interp.unwinding_package {
        return Err(CmdError::status("called while already in an UNWIND state"));
    }
    if args.is_empty() || args.len() > 4 {
        return Err(CmdError::Status(
            "called with wrong number of arguments.  include() only takes one file.".into(),
        ));
    }
    let mut optional = false;
    let mut options = IncludeOptions::default();
    let mut result_var: Option<BString> = None;
    let mut fname = args.first().map(|a| a.value.clone()).unwrap_or_default();
    let mut i = 1;
    while let Some(arg) = args.get(i) {
        match arg.value.as_slice() {
            b"OPTIONAL" => {
                if optional {
                    return Err(CmdError::Status(
                        "called with invalid arguments: OPTIONAL used twice".into(),
                    ));
                }
                optional = true;
            }
            b"RESULT_VARIABLE" => {
                if result_var.is_some() {
                    return Err(CmdError::Status(
                        "called with invalid arguments: only one result variable allowed".into(),
                    ));
                }
                i += 1;
                match args.get(i) {
                    Some(v) => result_var = Some(v.value.clone()),
                    None => {
                        return Err(CmdError::Status(
                            "called with no value for RESULT_VARIABLE.".into(),
                        ));
                    }
                }
            }
            b"NO_POLICY_SCOPE" => options.no_policy_scope = true,
            b"NO_DIAGNOSTIC_SCOPE" => options.no_diagnostic_scope = true,
            other => {
                if i > 1 {
                    return Err(CmdError::Status(cat(&[
                        b"called with invalid argument: ",
                        other,
                    ])));
                }
            }
        }
        i += 1;
    }
    let result_var = result_var.filter(|v| !v.is_empty());

    if fname.is_empty() {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            "include() given empty file name (ignored).".into(),
        );
        return Ok(Flow::Normal);
    }

    if !is_full_path(&fname) {
        let module = cat(&[&fname, b".cmake"]);
        let (mut mfile, system) = cx.interp.modules_file(&module);
        if system
            && let Some(&(_, num)) = DEPRECATED_MODULES
                .iter()
                .find(|(m, _)| *m == fname.as_slice())
        {
            let id = PolicyId::new(num);
            match cx.policy(id) {
                PolicyStatus::Warn => {
                    let interp = &mut *cx.interp;
                    interp.issue_at(
                        MessageKind::AuthorWarning,
                        Effect::None,
                        Some(DiagCategory::Policy),
                        Some(id),
                        Location::Backtrace(interp.local_command_backtrace()),
                        Interp::policy_warning_text(id),
                        true,
                    );
                }
                PolicyStatus::Old => {}
                PolicyStatus::New => mfile = BString::default(),
            }
        }
        if !mfile.is_empty() {
            fname = mfile;
        }
    }

    let base = rcmake_host::path::from_native(&cx.current_source_dir());
    let abs = collapse_full_path(&fname, &base);
    if is_target_export(cx, &abs) {
        cx.interp.fatal_here(cat(&[
            b"The file\n  ", &abs,
            b"\nwas generated by the export() command.  It may not be used as the argument to the include() command.  Use ALIAS targets instead to refer to targets by alternative names.",
        ]));
        return Err(CmdError::Reported);
    }
    let missing = !cx.interp.exists(&abs);
    let is_dir = cx.interp.is_dir(&abs);
    if missing || is_dir {
        if let Some(var) = &result_var {
            cx.set_var(var, "NOTFOUND".into());
        }
        if optional {
            return Ok(Flow::Normal);
        }
        let what: &[u8] = if missing {
            b"could not find requested file:\n  "
        } else {
            b"requested file is a directory:\n  "
        };
        return Err(CmdError::Status(cat(&[what, &fname])));
    }

    let read = cx.interp.read_dependent_file(&abs, options);
    if let Some(var) = &result_var {
        let value = if read { abs.clone() } else { "NOTFOUND".into() };
        cx.set_var(var, value);
    }
    if !optional && !read && !cx.fatal_occurred() {
        return Err(CmdError::Status(cat(&[
            b"could not load requested file:\n  ",
            &fname,
        ])));
    }
    if read && cx.interp.unwinding_package {
        return Ok(Flow::Return { propagate: None });
    }
    Ok(Flow::Normal)
}

fn is_target_export(cx: &mut Cx<'_>, path: &[u8]) -> bool {
    let key = cat(&[b"_RCMAKE_TARGET_EXPORT_OUTPUT_", path]);
    cx.global().props.get(&key).is_some()
}

fn cmd_include_guard(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if args.len() > 1 {
        return Err(CmdError::Status(
            "given an invalid number of arguments. The command takes at most 1 argument.".into(),
        ));
    }
    #[derive(PartialEq)]
    enum Scope {
        Variable,
        Directory,
        Global,
    }
    let scope = match args.first().map(|a| a.value.as_slice()) {
        None => Scope::Variable,
        Some(b"DIRECTORY") => Scope::Directory,
        Some(b"GLOBAL") => Scope::Global,
        Some(other) => {
            return Err(CmdError::Status(cat(&[b"given an invalid scope: ", other])));
        }
    };
    let file = cx.current_list_file();
    let var = BString::from(format!(
        "__INCGUARD_{}__",
        rcmake_base::hash::HashAlgo::Md5.hex_digest(&file)
    ));
    let done = Ok(Flow::Return { propagate: None });
    match scope {
        Scope::Variable => {
            if cx.var_defined(&var) {
                return done;
            }
            cx.set_var(&var, "ON".into());
        }
        Scope::Directory => {
            let dir = cx.current_dir_id();
            let project = cx.project();
            if project
                .ancestors(dir)
                .any(|id| project.dir(id).props.get(&var).is_some())
            {
                return done;
            }
            cx.directory_props().set(var, "TRUE".into());
        }
        Scope::Global => {
            if cx.global().props.get(&var).is_some() {
                return done;
            }
            cx.global().props.set(var, "TRUE".into());
        }
    }
    Ok(Flow::Normal)
}

/// What `message()` does with its text.
enum Display {
    /// `cmMakefile::DisplayStatus`: `-- <text>` on stdout.
    Status,
    /// `CHECK_START`.
    CheckStart,
    /// `CHECK_PASS` / `CHECK_FAIL`.
    CheckResult(&'static str),
    /// An entry in the configure log.
    ConfigureLog,
    /// `NOTICE` and messages without a mode: raw text on stderr.
    Notice,
    /// Warnings and errors through the diagnostic engine.
    Diag {
        kind: MessageKind,
        effect: Effect,
        category: Option<(DiagCategory, DiagAction)>,
        fatal: bool,
    },
}

/// `IndentText` in `cmMessageCommand.cxx`.
fn indent_text(interp: &Interp, text: &[u8]) -> Vec<u8> {
    let mut parts = Vec::new();
    if let Some(v) = interp.lookup_var(b"CMAKE_MESSAGE_INDENT") {
        split_into(v, Empty::Drop, &mut parts);
    }
    let mut indent: Vec<u8> = parts.concat();
    let show = interp.show_log_context
        || interp
            .lookup_var(b"CMAKE_MESSAGE_CONTEXT_SHOW")
            .is_some_and(|v| rcmake_base::truth::is_on(v));
    if show {
        let mut ctx = Vec::new();
        if let Some(v) = interp.lookup_var(b"CMAKE_MESSAGE_CONTEXT") {
            split_into(v, Empty::Drop, &mut ctx);
        }
        if !ctx.is_empty() {
            let joined = ctx.join(&b'.');
            let mut prefix = cat(&[b"[", &joined, b"] "]).to_vec();
            prefix.extend_from_slice(&indent);
            indent = prefix;
        }
    }
    if indent.is_empty() {
        return text.to_vec();
    }
    let mut nl = vec![b'\n'];
    nl.extend_from_slice(&indent);
    let mut out = indent;
    out.extend_from_slice(&crate::util::replace_all(text, b"\n", &nl));
    out
}

fn display_status(interp: &Interp, text: &[u8]) {
    let mut out = b"-- ".to_vec();
    out.extend_from_slice(&indent_text(interp, text));
    out.push(b'\n');
    interp.config.host.write_out(Stream::Stdout, &out);
}

/// Display a diagnostic directly through the messenger (`DisplayMessage`), without
/// marking the current command as failed.
fn display_message(
    interp: &mut Interp,
    kind: MessageKind,
    effect: Effect,
    category: Option<(DiagCategory, DiagAction)>,
    body: BString,
) {
    let mut state = *interp.diag_top();
    if let Some((cat, action)) = category {
        state.set(cat, action, false);
    }
    let diag = Diagnostic {
        annotations: None,
        kind,
        effect,
        category: category.map(|(c, _)| c),
        policy: None,
        location: Location::Backtrace(interp.bt.clone()),
        body,
    };
    interp.emit(&diag, &state);
}

/// Map a diagnostic action to `(kind, effect, fatal)` for `AUTHOR_WARNING` and
/// `DEPRECATION`. Returns `None` for `IGNORE`.
fn category_display(
    action: DiagAction,
    warning: MessageKind,
    error: MessageKind,
) -> Option<(MessageKind, Effect, bool)> {
    match action {
        DiagAction::Ignore => None,
        DiagAction::Warn => Some((warning, Effect::None, false)),
        DiagAction::SendError => Some((error, Effect::SetError, false)),
        DiagAction::FatalError => Some((error, Effect::Fatal, true)),
    }
}

fn cmd_message(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    let Some(first) = args.first() else {
        return Err(CmdError::Status(
            "called with incorrect number of arguments".into(),
        ));
    };
    let interp = &mut *cx.interp;
    let mut skip = 1;
    let (level, display) = match first.value.as_slice() {
        b"SEND_ERROR" => (
            LogLevel::Error,
            Display::Diag {
                kind: MessageKind::Error,
                effect: Effect::SetError,
                category: None,
                fatal: false,
            },
        ),
        b"FATAL_ERROR" => (
            LogLevel::Error,
            Display::Diag {
                kind: MessageKind::Error,
                effect: Effect::Fatal,
                category: None,
                fatal: true,
            },
        ),
        b"WARNING" => (
            LogLevel::Warning,
            Display::Diag {
                kind: MessageKind::Warning,
                effect: Effect::None,
                category: None,
                fatal: false,
            },
        ),
        b"AUTHOR_WARNING" => {
            let action = interp.diag_top().get(DiagCategory::Author);
            let Some((kind, effect, fatal)) =
                category_display(action, MessageKind::AuthorWarning, MessageKind::AuthorError)
            else {
                return Ok(Flow::Normal);
            };
            let level = if effect == Effect::None {
                LogLevel::Warning
            } else {
                LogLevel::Error
            };
            (
                level,
                Display::Diag {
                    kind,
                    effect,
                    category: Some((DiagCategory::Author, action)),
                    fatal,
                },
            )
        }
        b"DEPRECATION" => {
            let action = if interp.policy_status(PolicyId::new(218)) == PolicyStatus::New {
                interp.diag_top().get(DiagCategory::Deprecated)
            } else {
                interp.legacy_deprecation_action(true)
            };
            let Some((kind, effect, fatal)) = category_display(
                action,
                MessageKind::DeprecationWarning,
                MessageKind::DeprecationError,
            ) else {
                return Ok(Flow::Normal);
            };
            let level = if effect == Effect::None {
                LogLevel::Warning
            } else {
                LogLevel::Error
            };
            (
                level,
                Display::Diag {
                    kind,
                    effect,
                    category: Some((DiagCategory::Deprecated, action)),
                    fatal,
                },
            )
        }
        b"CHECK_START" => (LogLevel::Status, Display::CheckStart),
        b"CHECK_PASS" => (LogLevel::Status, Display::CheckResult("CHECK_PASS")),
        b"CHECK_FAIL" => (LogLevel::Status, Display::CheckResult("CHECK_FAIL")),
        b"CONFIGURE_LOG" => (LogLevel::Error, Display::ConfigureLog),
        b"STATUS" => (LogLevel::Status, Display::Status),
        b"VERBOSE" => (LogLevel::Verbose, Display::Status),
        b"DEBUG" => (LogLevel::Debug, Display::Status),
        b"TRACE" => (LogLevel::Trace, Display::Status),
        b"NOTICE" => (LogLevel::Notice, Display::Notice),
        _ => {
            skip = 0;
            (LogLevel::Notice, Display::Notice)
        }
    };

    if interp.log_level() < level {
        return Ok(Flow::Normal);
    }
    let text: Vec<u8> = args
        .iter()
        .skip(skip)
        .flat_map(|a| a.value.iter().copied())
        .collect();

    match display {
        Display::ConfigureLog => interp.log_configure_message(&text),
        Display::Status => display_status(interp, &text),
        Display::Notice => {
            let mut out = indent_text(interp, &text);
            out.push(b'\n');
            interp.config.host.write_out(Stream::Stderr, &out);
        }
        Display::CheckStart => {
            display_status(interp, &text);
            interp.checks.push(BString::from(text));
        }
        Display::CheckResult(what) => match interp.checks.pop() {
            Some(top) => {
                let line = cat(&[&top, b" - ", &text]);
                display_status(interp, &line);
            }
            None => display_message(
                interp,
                MessageKind::AuthorWarning,
                Effect::None,
                Some((DiagCategory::Author, DiagAction::Warn)),
                cat(&[b"Ignored ", what.as_bytes(), b" without CHECK_START"]),
            ),
        },
        Display::Diag {
            kind,
            effect,
            category,
            fatal,
        } => {
            display_message(interp, kind, effect, category, BString::from(text));
            if fatal {
                interp.fatal = true;
            }
        }
    }
    Ok(Flow::Normal)
}
