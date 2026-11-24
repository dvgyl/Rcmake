use std::{collections::BTreeMap, ops::Range};

use rcmake_diag::policy::PolicyId;
use rcmake_json::Value;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::{
    analysis::Document,
    catalog::{COMMANDS, MODULE_COMMANDS, VARIABLES, signature},
    expression::{self, Kind as ExpressionKind},
    json::{object, uint},
    lexical,
    position::{Position, PositionEncoding, range},
    roles::{self, Arguments, Role, Variable},
    workspace::Workspace,
};

const DOCS: &str = "https://cmake.org/cmake/help/latest";
const MAX_COMPLETIONS: usize = 256;
const MAX_DEFINITIONS: usize = 32;
pub(crate) const TOKEN_TYPES: [&str; 7] = [
    "function", "keyword", "variable", "string", "comment", "class", "operator",
];
const COMMAND: usize = 0;
const KEYWORD: usize = 1;
const VARIABLE: usize = 2;
const STRING: usize = 3;
const COMMENT: usize = 4;
const TARGET: usize = 5;
const GENERATOR: usize = 6;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Project,
    Target,
    Function,
    Directory,
    Variable(Variable),
}

impl Kind {
    fn symbol(self) -> usize {
        match self {
            Self::Project => 2,
            Self::Target => 5,
            Self::Function => 12,
            Self::Directory => 19,
            Self::Variable(_) => 13,
        }
    }

    fn completion(self) -> usize {
        match self {
            Self::Target => 7,
            Self::Function => 3,
            _ => 6,
        }
    }
}

struct Declaration<'a> {
    kind: Kind,
    name: &'a str,
    selection: Range<usize>,
    command: &'a CommandInvocation,
    scope: Option<usize>,
    conditional: bool,
}

fn scoped_commands(
    document: &Document,
) -> impl Iterator<Item = (&CommandInvocation, Option<usize>, bool)> {
    let mut blocks = Vec::new();
    let mut scope = None;
    let mut conditional = false;
    document.commands.iter().map(move |command| {
        let name = command.lower.as_slice();
        let before = (command, scope, conditional);
        if matches!(
            name,
            b"function" | b"macro" | b"block" | b"if" | b"foreach" | b"while"
        ) {
            blocks.push((name, scope, conditional));
            if matches!(name, b"function" | b"macro" | b"block") {
                scope = Some(command.span.start);
            }
            conditional |= matches!(name, b"if" | b"foreach" | b"while");
        }
        if let Some(&(opening, parent_scope, parent_conditional)) = blocks.last()
            && name.strip_prefix(b"end") == Some(opening)
        {
            blocks.pop();
            scope = parent_scope;
            conditional = parent_conditional;
        }
        before
    })
}

fn declarations(document: &Document) -> impl Iterator<Item = Declaration<'_>> {
    scoped_commands(document).filter_map(|(command, scope, conditional)| {
        let kind = match command.lower.as_slice() {
            b"project" => Kind::Project,
            b"add_executable" | b"add_library" | b"add_custom_target" => Kind::Target,
            b"function" | b"macro" => Kind::Function,
            b"add_subdirectory" => Kind::Directory,
            b"option" | b"set" => Kind::Variable(roles::declaration_variable(command)),
            _ => return None,
        };
        let arg = command.args.first()?;
        let raw_name = std::str::from_utf8(&arg.text).ok()?;
        let (name, skip) = variable_spelling(kind, raw_name);
        if name.is_empty() || name.len() > 4096 || name.contains(['$', '\\', '\0']) {
            return None;
        }
        let selection = payload(document, arg)?;
        let selection = selection
            .start
            .checked_add(skip)
            .filter(|&start| document.text.get(start..start + name.len()) == Some(name))
            .map_or(selection, |start| start..start + name.len());
        Some(Declaration {
            kind,
            name,
            selection,
            command,
            scope,
            conditional,
        })
    })
}

fn variable_spelling(kind: Kind, name: &str) -> (&str, usize) {
    let prefix = match kind {
        Kind::Variable(Variable::Environment) => "ENV{",
        Kind::Variable(Variable::Cache) => "CACHE{",
        _ => return (name, 0),
    };
    name.strip_prefix(prefix)
        .and_then(|tail| tail.strip_suffix('}'))
        .map_or((name, 0), |name| (name, prefix.len()))
}

fn payload(document: &Document, arg: &Argument) -> Option<Range<usize>> {
    let bytes = document.text.as_bytes().get(arg.span.clone())?;
    match arg.kind {
        ArgKind::Unquoted => Some(arg.span.clone()),
        ArgKind::Quoted => {
            let (end, closed) = lexical::quoted_end(bytes, 0);
            Some(arg.span.start + 1..arg.span.start + end - usize::from(closed))
        }
        ArgKind::Bracket => lexical::bracket(bytes, 0)
            .map(|(_, content)| arg.span.start + content.start..arg.span.start + content.end),
    }
}

pub(crate) fn symbols(doc: &Document, encoding: PositionEncoding) -> Value {
    Value::Array(
        declarations(doc)
            .take(20_000)
            .map(|decl| {
                object([
                    ("name", Value::string(decl.name)),
                    ("kind", uint(decl.kind.symbol())),
                    ("range", doc.span(decl.command.span.clone(), encoding)),
                    ("selectionRange", doc.span(decl.selection, encoding)),
                ])
            })
            .collect(),
    )
}

fn word_at(text: &str, offset: usize) -> Range<usize> {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || b"_-.+:".contains(&byte);
    let start = text
        .as_bytes()
        .get(..offset)
        .unwrap_or_default()
        .iter()
        .rev()
        .take_while(|&&byte| word(byte))
        .count();
    let end = text
        .as_bytes()
        .get(offset..)
        .unwrap_or_default()
        .iter()
        .take_while(|&&byte| word(byte))
        .count();
    offset - start..offset + end
}

fn command_name(command: &CommandInvocation) -> Range<usize> {
    command.span.start..command.span.start + command.name.len()
}

#[derive(Clone, Copy)]
enum SubjectKind {
    Name(Kind),
    Keyword(roles::Keyword, &'static str),
    Policy,
    Generator,
}

struct Subject<'a> {
    text: &'a str,
    span: Range<usize>,
    kind: SubjectKind,
}

fn command_subject(command: &CommandInvocation) -> Option<Subject<'_>> {
    Some(Subject {
        text: std::str::from_utf8(&command.name).ok()?,
        span: command_name(command),
        kind: SubjectKind::Name(Kind::Function),
    })
}

fn argument_subject<'a>(
    doc: &'a Document,
    command: &'a CommandInvocation,
    arg: &'a Argument,
    index: usize,
    offset: usize,
) -> Option<Subject<'a>> {
    if arg.kind == ArgKind::Bracket {
        return None;
    }
    let payload = payload(doc, arg)?;
    let raw = doc.text.get(payload.clone())?;
    if let Some(expr) = expression::expressions(raw).into_iter().find(|expr| {
        expr.span.contains(&offset.saturating_sub(payload.start)) && offset >= payload.start
    }) {
        let kind = match expr.kind {
            ExpressionKind::Reference(namespace) => SubjectKind::Name(Kind::Variable(namespace)),
            ExpressionKind::Target => SubjectKind::Name(Kind::Target),
            ExpressionKind::Generator => SubjectKind::Generator,
        };
        return Some(Subject {
            text: raw.get(expr.name)?,
            span: payload.start + expr.span.start..payload.start + expr.span.end,
            kind,
        });
    }
    if !payload.contains(&offset) {
        return None;
    }
    let arguments = Arguments::new(command);
    if let Some(keyword) = arguments.keyword(index, arg)
        && let Some(info) = command_info(std::str::from_utf8(&command.lower).ok()?)
    {
        return Some(Subject {
            text: raw,
            span: payload,
            kind: SubjectKind::Keyword(keyword, info.name),
        });
    }
    let kind = match arguments.role(index) {
        Role::Target | Role::TargetDeclaration if expression::target_name(raw) => {
            SubjectKind::Name(Kind::Target)
        }
        Role::CommandDeclaration => SubjectKind::Name(Kind::Function),
        Role::Variable(namespace) => SubjectKind::Name(Kind::Variable(namespace)),
        Role::Policy => SubjectKind::Policy,
        _ => return None,
    };
    let (name, skip) = match kind {
        SubjectKind::Name(kind) => variable_spelling(kind, raw),
        _ => (raw, 0),
    };
    Some(Subject {
        text: name,
        span: payload.start + skip..payload.start + skip + name.len(),
        kind,
    })
}

fn subject(doc: &Document, offset: usize) -> Option<Subject<'_>> {
    let command = doc.command_at(offset)?;
    if offset <= command_name(command).end {
        return command_subject(command);
    }
    let (index, arg) = command
        .args
        .iter()
        .enumerate()
        .find(|(_, arg)| arg.span.contains(&offset))?;
    argument_subject(doc, command, arg, index, offset)
}

fn scope_at(doc: &Document, offset: usize) -> Option<usize> {
    scoped_commands(doc)
        .take_while(|(command, _, _)| command.span.start <= offset)
        .last()
        .and_then(|(_, scope, _)| scope)
}

fn matching_declarations<'a>(
    doc: &'a Document,
    doc_uri: &'a str,
    workspace: &'a Workspace<'_>,
    subject: &Subject<'_>,
) -> Vec<(&'a str, &'a Document, Declaration<'a>)> {
    let SubjectKind::Name(kind) = subject.kind else {
        return Vec::new();
    };
    let scope = if matches!(kind, Kind::Variable(_)) {
        scope_at(doc, subject.span.start)
    } else {
        None
    };
    let matches = |decl: &Declaration<'_>, candidate_kind| {
        decl.kind == candidate_kind
            && if kind == Kind::Function {
                decl.name.eq_ignore_ascii_case(subject.text)
            } else {
                decl.name == subject.text
            }
    };
    for candidate_kind in [
        Some(kind),
        (kind == Kind::Variable(Variable::Normal)).then_some(Kind::Variable(Variable::Cache)),
    ]
    .into_iter()
    .flatten()
    {
        let mut nearby = declarations(doc)
            .filter(|decl| matches(decl, candidate_kind))
            .filter(|decl| {
                !matches!(candidate_kind, Kind::Variable(_))
                    || decl.scope.is_none()
                    || decl.scope == scope
            })
            .collect::<Vec<_>>();
        if !nearby.is_empty() {
            if nearby.len() == 1 || nearby.iter().all(|decl| !decl.conditional) {
                let index = nearby
                    .partition_point(|decl| decl.selection.start <= subject.span.start)
                    .saturating_sub(1);
                let decl = nearby.swap_remove(index);
                return vec![(doc_uri, doc, decl)];
            }
            return nearby
                .into_iter()
                .take(MAX_DEFINITIONS)
                .map(|decl| (doc_uri, doc, decl))
                .collect();
        }
        let mut distant = Vec::new();
        let mut has_connected_match = false;
        for (uri, document, connected) in workspace.documents() {
            if !connected && has_connected_match {
                break;
            }
            if candidate_kind == Kind::Variable(Variable::Normal)
                && !workspace.variable_sources(uri)
            {
                continue;
            }
            distant.extend(
                declarations(document)
                    .filter(|decl| matches(decl, candidate_kind))
                    .filter(|decl| {
                        !matches!(candidate_kind, Kind::Variable(_)) || decl.scope.is_none()
                    })
                    .take(MAX_DEFINITIONS - distant.len())
                    .map(|decl| (uri, document, decl)),
            );
            has_connected_match |= connected && !distant.is_empty();
            if distant.len() == MAX_DEFINITIONS {
                break;
            }
        }
        if !distant.is_empty() {
            return distant;
        }
    }
    Vec::new()
}

struct CommandInfo {
    name: &'static str,
    summary: &'static str,
    signature: Option<&'static str>,
    module: Option<&'static str>,
}

impl CommandInfo {
    fn link(&self) -> String {
        if let Some(module) = self.module {
            format!("{DOCS}/module/{module}.html")
        } else {
            format!("{DOCS}/command/{}.html", self.name)
        }
    }
}

fn command_info(lookup: &str) -> Option<CommandInfo> {
    COMMANDS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(lookup))
        .map(|(name, summary)| CommandInfo {
            name,
            summary,
            signature: signature(name),
            module: None,
        })
        .or_else(|| {
            MODULE_COMMANDS
                .iter()
                .find(|(name, _, _, _)| name.eq_ignore_ascii_case(lookup))
                .map(|(name, module, signature, summary)| CommandInfo {
                    name,
                    summary,
                    signature: Some(signature),
                    module: Some(module),
                })
        })
}

fn command_hover(
    info: &CommandInfo,
    command: Option<&CommandInvocation>,
    markdown: bool,
) -> String {
    let mut text = format!("{}\n\n{}", info.name, info.summary);
    if let Some(signature) = info.signature {
        text.push_str("\n\n");
        if markdown {
            text.push_str(&code_block(signature));
        } else {
            text.push_str(signature);
        }
    }
    if info.name.starts_with("target_")
        && let Some(command) = command
    {
        for (keyword, meaning) in Arguments::new(command).keywords(1) {
            if !matches!(*keyword, "PRIVATE" | "PUBLIC" | "INTERFACE") {
                continue;
            }
            if markdown {
                text.push_str(&format!("\n\n- `{keyword}`. {meaning}"));
            } else {
                text.push_str(&format!("\n\n{keyword}. {meaning}"));
            }
        }
    }
    if markdown {
        text.push_str(&format!("\n\n[CMake documentation]({})", info.link()));
    }
    text
}

fn code_block(text: &str) -> String {
    let backticks = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat((backticks + 1).max(3));
    format!("{fence}cmake\n{text}\n{fence}")
}

pub(crate) fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\`*_[]{}<>()#+-.!|&~".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub(crate) fn hover<'a>(
    doc: &'a Document,
    uri: &'a str,
    workspace: impl FnOnce() -> Workspace<'a>,
    offset: usize,
    encoding: PositionEncoding,
    markdown: bool,
) -> Value {
    let Some(subject) = subject(doc, offset) else {
        return Value::Null;
    };
    let workspace = matches!(subject.kind, SubjectKind::Name(_)).then(workspace);
    let matches = workspace.as_ref().map_or_else(Vec::new, |workspace| {
        matching_declarations(doc, uri, workspace, &subject)
    });
    let info = match subject.kind {
        SubjectKind::Name(Kind::Function) => command_info(subject.text),
        _ => None,
    };
    let summary = if let Some(info) =
        info.filter(|info| matches.is_empty() || info.module.is_some())
    {
        Some(command_hover(&info, doc.command_at(offset), markdown))
    } else if let [(destination, document, decl)] = matches.as_slice() {
        let end = document
            .text
            .floor_char_boundary(decl.command.span.end.min(decl.command.span.start + 512));
        let snippet = document
            .text
            .get(decl.command.span.start..end)
            .unwrap_or_default();
        let place = if *destination == uri {
            format!("Defined on line {}.", decl.command.line)
        } else {
            let path = crate::uri::path(destination)
                .map_or_else(|| (*destination).into(), |path| path.display().to_string());
            format!("Defined in {path} on line {}.", decl.command.line)
        };
        let place = if markdown {
            escape_markdown(&place)
        } else {
            place
        };
        let snippet = if markdown {
            code_block(snippet)
        } else {
            snippet.to_owned()
        };
        Some(format!("{place}\n\n{snippet}"))
    } else if !matches.is_empty() {
        Some(format!("{} possible definitions.", matches.len()))
    } else {
        match subject.kind {
            SubjectKind::Keyword((name, text), command) => Some(if markdown {
                format!("`{name}`\n\n{text}\n\n[CMake documentation]({DOCS}/command/{command}.html)")
            } else {
                format!("{name}\n\n{text}")
            }),
            SubjectKind::Policy => PolicyId::parse(subject.text.as_bytes())
                .and_then(PolicyId::info)
                .map(|policy| if markdown {
                    format!("`{}`\n\n{}\n\n[CMake documentation]({DOCS}/policy/{}.html)", subject.text, policy.summary, subject.text)
                } else {
                    policy.summary.to_owned()
                }),
            SubjectKind::Name(Kind::Variable(Variable::Normal)) => VARIABLES
                .iter()
                .find(|(name, _)| *name == subject.text)
                .map(|(name, summary)| if markdown {
                    format!("`{name}`\n\n{summary}\n\n[CMake documentation]({DOCS}/variable/{name}.html)")
                } else {
                    format!("{name}\n\n{summary}")
                }),
            SubjectKind::Generator => expression::description(subject.text).map(|summary| {
                if markdown {
                    format!("`$<{}>`\n\n{summary}\n\n[Generator expressions]({DOCS}/manual/cmake-generator-expressions.7.html)", subject.text)
                } else {
                    format!("$<{}>\n\n{summary}", subject.text)
                }
            }),
            _ => None,
        }
    };
    summary.map_or(Value::Null, |summary| {
        object([
            (
                "contents",
                object([
                    (
                        "kind",
                        Value::string(if markdown { "markdown" } else { "plaintext" }),
                    ),
                    ("value", Value::string(summary)),
                ]),
            ),
            ("range", doc.span(subject.span, encoding)),
        ])
    })
}

pub(crate) fn definition<'a>(
    doc: &'a Document,
    uri: &'a str,
    workspace: impl FnOnce() -> Workspace<'a>,
    offset: usize,
    encoding: PositionEncoding,
) -> Value {
    let path = doc.command_at(offset).filter(|command| {
        matches!(command.lower.as_slice(), b"include" | b"add_subdirectory")
            && command
                .args
                .first()
                .is_some_and(|arg| arg.span.contains(&offset))
    });
    let subject =
        subject(doc, offset).filter(|subject| matches!(subject.kind, SubjectKind::Name(_)));
    if path.is_none() && subject.is_none() {
        return Value::Null;
    }
    let workspace = workspace();
    if let Some(command) = path {
        let files = workspace
            .file_locations(command)
            .map(|uri| location(uri, range(Position::default(), Position::default())))
            .collect();
        let result = locations(files);
        if result != Value::Null {
            return result;
        }
    }
    let Some(subject) = subject else {
        return Value::Null;
    };
    locations(
        matching_declarations(doc, uri, &workspace, &subject)
            .into_iter()
            .map(|(uri, document, decl)| location(uri, document.span(decl.selection, encoding)))
            .collect(),
    )
}

fn locations(mut values: Vec<Value>) -> Value {
    match values.len() {
        0 => Value::Null,
        1 => values.remove(0),
        _ => Value::Array(values),
    }
}

pub(crate) fn location(uri: &str, range: Value) -> Value {
    object([("uri", Value::string(uri)), ("range", range)])
}

pub(crate) fn completion<'a>(
    doc: &'a Document,
    workspace: impl FnOnce() -> Workspace<'a>,
    offset: usize,
    encoding: PositionEncoding,
) -> Value {
    let mut span = word_at(&doc.text, offset);
    let comments = doc.comments();
    if comments.iter().any(|comment| {
        comment.contains(&offset)
            || (comment.end == offset
                && lexical::bracket(
                    doc.text.as_bytes().get(..offset).unwrap_or_default(),
                    comment.start + 1,
                )
                .is_none_or(|(end, content)| end == content.end))
    }) {
        return Value::Array(Vec::new());
    }
    let mut kinds = Vec::new();
    let mut keywords: &[roles::Keyword] = &[];
    let mut prefer_targets = false;
    if let Some(command) = doc.command_at(offset) {
        let name = command_name(command);
        if offset <= name.end {
            kinds.push(Kind::Function);
            span = name;
        } else if doc
            .text
            .get(name.end..offset)
            .is_some_and(|tail| tail.contains('('))
        {
            let index = command.args.partition_point(|arg| arg.span.end < offset);
            let argument = command
                .args
                .get(index)
                .filter(|arg| arg.span.start <= offset);
            if argument.is_some_and(|arg| arg.kind == ArgKind::Bracket) {
                return Value::Array(Vec::new());
            }
            if let Some(payload) = argument.and_then(|arg| payload(doc, arg))
                && payload.start <= offset
                && offset <= payload.end
                && let Some(raw) = doc.text.get(payload.clone())
                && let Some(expr) = expression::expressions(raw).into_iter().find(|expr| {
                    expr.name.start <= offset - payload.start
                        && offset - payload.start <= expr.name.end
                })
            {
                match expr.kind {
                    ExpressionKind::Reference(namespace) => kinds.push(Kind::Variable(namespace)),
                    ExpressionKind::Target => kinds.push(Kind::Target),
                    ExpressionKind::Generator => {}
                }
                span = payload.start + expr.name.start..payload.start + expr.name.end;
            } else {
                let arguments = Arguments::new(command);
                keywords = arguments.keywords(index);
                if arguments.role(index) == Role::Target {
                    kinds.push(Kind::Target);
                    prefer_targets = index != 1;
                }
            }
        }
    } else {
        let line = doc.lines.line_at(offset);
        let start = doc.lines.start(line).unwrap_or_default();
        if doc.text.get(start..span.start).is_some_and(|prefix| {
            prefix
                .trim_start_matches('\u{feff}')
                .trim_ascii()
                .is_empty()
        }) {
            kinds.push(Kind::Function);
        }
    }
    let prefix = doc.text.get(span.start..offset).unwrap_or_default();
    if prefix.len() > 4096 {
        return Value::Array(Vec::new());
    }
    let lower = prefix.to_ascii_lowercase();
    let mut entries = BTreeMap::new();
    let mut add = |name: &'_ str, kind: usize, detail: Option<&str>| {
        if entries.len() < MAX_COMPLETIONS
            && (if kind == 3 || kind == 14 {
                name.get(..lower.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(&lower))
            } else {
                name.starts_with(prefix)
            })
        {
            let key = if kind == 3 {
                name.to_ascii_lowercase()
            } else {
                name.to_owned()
            };
            entries.entry(key).or_insert_with(|| {
                let priority = usize::from(match kind {
                    7 => !prefer_targets,
                    14 => prefer_targets,
                    _ => false,
                });
                let mut fields = vec![
                    ("label", Value::string(name)),
                    ("kind", uint(kind)),
                    ("sortText", Value::string(format!("{priority}-{name}"))),
                    (
                        "textEdit",
                        object([
                            ("range", doc.span(span.clone(), encoding)),
                            ("newText", Value::string(name)),
                        ]),
                    ),
                ];
                if let Some(detail) = detail {
                    fields.push(("detail", Value::string(detail)));
                }
                object(fields)
            });
        }
    };
    for (name, description) in keywords {
        add(name, 14, Some(description));
    }
    let scope = if kinds.iter().any(|kind| matches!(kind, Kind::Variable(_))) {
        scope_at(doc, offset)
    } else {
        None
    };
    if !kinds.is_empty() {
        for decl in declarations(doc) {
            if completion_kind(&kinds, decl.kind)
                && (decl.scope.is_none()
                    || decl.scope == scope
                    || !matches!(decl.kind, Kind::Variable(_)))
            {
                add(
                    decl.name,
                    decl.kind.completion(),
                    declaration_detail(doc, &decl),
                );
            }
        }
        let workspace = workspace();
        for (uri, candidate, _) in workspace.documents() {
            let variables_visible = workspace.variable_sources(uri);
            for decl in declarations(candidate) {
                if completion_kind(&kinds, decl.kind)
                    && (!matches!(decl.kind, Kind::Variable(_)) || decl.scope.is_none())
                    && (decl.kind != Kind::Variable(Variable::Normal) || variables_visible)
                {
                    add(
                        decl.name,
                        decl.kind.completion(),
                        declaration_detail(candidate, &decl),
                    );
                }
            }
        }
    }
    if kinds.contains(&Kind::Function) {
        for (name, summary) in COMMANDS {
            add(name, 3, signature(name).or(Some(summary)));
        }
        for (name, _, signature, _) in MODULE_COMMANDS {
            add(name, 3, Some(signature));
        }
    }
    if kinds.contains(&Kind::Variable(Variable::Normal)) {
        for (name, summary) in VARIABLES {
            add(name, 6, Some(summary));
        }
    }
    Value::Array(entries.into_values().collect())
}

fn completion_kind(kinds: &[Kind], kind: Kind) -> bool {
    kinds.contains(&kind)
        || (kind == Kind::Variable(Variable::Cache)
            && kinds.contains(&Kind::Variable(Variable::Normal)))
}

fn declaration_detail<'a>(doc: &'a Document, decl: &Declaration<'a>) -> Option<&'a str> {
    let span = decl.command.span.clone();
    if decl.kind == Kind::Function && span.len() <= 512 {
        doc.text.get(span)
    } else {
        None
    }
}

pub(crate) fn semantic_tokens(doc: &Document, encoding: PositionEncoding) -> Value {
    let mut comments = doc.comments().into_iter().peekable();
    let mut tokens = Tokens {
        doc,
        encoding,
        data: Vec::new(),
        last: Position::default(),
        last_offset: 0,
    };
    for command in doc.commands.iter() {
        let name = command_name(command);
        tokens.comments_before(&mut comments, name.start);
        tokens.push(name, COMMAND);
        let arguments = Arguments::new(command);
        for (index, arg) in command.args.iter().enumerate() {
            tokens.comments_before(&mut comments, arg.span.start);
            if arg.kind == ArgKind::Bracket {
                tokens.push(arg.span.clone(), STRING);
                continue;
            }
            let Some(payload) = payload(doc, arg) else {
                continue;
            };
            let Some(text) = doc.text.get(payload.clone()) else {
                continue;
            };
            let style = if arguments.keyword(index, arg).is_some() {
                Some(KEYWORD)
            } else {
                match arguments.role(index) {
                    Role::Variable(_) => Some(VARIABLE),
                    Role::CommandDeclaration => Some(COMMAND),
                    Role::Target | Role::TargetDeclaration if expression::target_name(text) => {
                        Some(TARGET)
                    }
                    _ => None,
                }
            };
            let background = (arg.kind == ArgKind::Quoted).then_some(STRING);
            let styled_payload = style.is_some() && background.is_some();
            if styled_payload {
                tokens.push(arg.span.start..payload.start, STRING);
            }
            let body = if styled_payload {
                payload.clone()
            } else {
                arg.span.clone()
            };
            let mut start = body.start;
            for expr in expression::expressions(text) {
                let span = payload.start + expr.span.start..payload.start + expr.span.end;
                if let Some(style) = style.or(background) {
                    tokens.push(start..span.start, style);
                }
                start = span.end;
                tokens.push(
                    span,
                    match expr.kind {
                        ExpressionKind::Reference(_) => VARIABLE,
                        ExpressionKind::Generator => GENERATOR,
                        ExpressionKind::Target => TARGET,
                    },
                );
            }
            if let Some(style) = style.or(background) {
                tokens.push(start..body.end, style);
            }
            if styled_payload {
                tokens.push(payload.end..arg.span.end, STRING);
            }
        }
    }
    tokens.comments_before(&mut comments, usize::MAX);
    object([("data", Value::Array(tokens.data))])
}

struct Tokens<'a> {
    doc: &'a Document,
    encoding: PositionEncoding,
    data: Vec<Value>,
    last: Position,
    last_offset: usize,
}

impl Tokens<'_> {
    fn comments_before(
        &mut self,
        comments: &mut std::iter::Peekable<impl Iterator<Item = Range<usize>>>,
        offset: usize,
    ) {
        while let Some(comment) = comments.next_if(|comment| comment.start < offset) {
            self.push(comment, COMMENT);
        }
    }

    fn push(&mut self, span: Range<usize>, kind: usize) {
        let mut cursor = span.start;
        while cursor < span.end && self.data.len() < 500_000 {
            let line = self.doc.lines.line_at(cursor);
            let start = self.doc.lines.start(line).unwrap_or(0);
            let line_end = start
                + self
                    .doc
                    .lines
                    .text(&self.doc.text, line)
                    .map_or(0, str::len);
            let end = span.end.min(line_end);
            if let Some(text) = self
                .doc
                .text
                .get(cursor..end)
                .filter(|text| !text.is_empty())
            {
                let (prior_offset, prior_character) = if self.last.line == line {
                    (self.last_offset, self.last.character)
                } else {
                    (start, 0)
                };
                let character = self
                    .doc
                    .text
                    .get(prior_offset..cursor)
                    .map_or(prior_character, |prefix| {
                        prior_character + self.encoding.width(prefix)
                    });
                let delta_line = line - self.last.line;
                let delta_character = if delta_line == 0 {
                    character.saturating_sub(self.last.character)
                } else {
                    character
                };
                self.data.extend(
                    [
                        delta_line,
                        delta_character,
                        self.encoding.width(text),
                        kind,
                        0,
                    ]
                    .map(uint),
                );
                self.last = Position { line, character };
                self.last_offset = cursor;
            }
            let Some(next) = self.doc.lines.start(line + 1) else {
                break;
            };
            cursor = next;
        }
    }
}
