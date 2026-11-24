use std::{collections::BTreeMap, ops::Range};

use rcmake_json::Value;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::{
    analysis::Document,
    catalog::{
        self, DOCS, Entry, MODULE_COMMANDS, Topic, code_block, code_span, signature_override,
    },
    expression::{self, Kind as ExpressionKind},
    json::{object, uint},
    lexical,
    position::{Position, PositionEncoding, range},
    roles::{self, Arguments, Role, Variable},
    workspace::Workspace,
};

pub(crate) const MAX_COMPLETIONS: usize = 256;
const MAX_DEFINITIONS: usize = 32;

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

pub(crate) fn payload(document: &Document, arg: &Argument) -> Option<Range<usize>> {
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

pub(crate) fn command_name(command: &CommandInvocation) -> Range<usize> {
    command.span.start..command.span.start + command.name.len()
}

#[derive(Clone, Copy)]
enum SubjectKind {
    Name(Kind),
    Keyword(roles::Keyword, &'static str),
    Catalog(Topic),
    Package,
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
    let kind = if let Some(kind) = catalog_argument(command, index) {
        kind
    } else {
        match arguments.role(index) {
            Role::Target | Role::TargetDeclaration if expression::target_name(raw) => {
                SubjectKind::Name(Kind::Target)
            }
            Role::CommandDeclaration => SubjectKind::Name(Kind::Function),
            Role::Variable(namespace) => SubjectKind::Name(Kind::Variable(namespace)),
            _ => return None,
        }
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

fn catalog_argument(command: &CommandInvocation, index: usize) -> Option<SubjectKind> {
    let arg = |index| {
        command
            .args
            .get(index)
            .map(|arg: &Argument| arg.text.as_slice())
    };
    let after = |marker: &[u8]| {
        command
            .args
            .iter()
            .position(|arg| arg.text.as_slice() == marker)
            .and_then(|marker| index.checked_sub(marker + 1))
    };
    let topic = match (command.lower.as_slice(), index) {
        (b"include", 0) => Topic::Module,
        (b"find_package", 0) => return Some(SubjectKind::Package),
        (b"cmake_policy", 1) if Arguments::new(command).role(index) == Role::Policy => {
            Topic::Policy
        }
        (b"get_target_property", 2) => Topic::TargetProperty,
        (b"set_target_properties", _) if after(b"PROPERTIES").is_some_and(|n| n % 2 == 0) => {
            Topic::TargetProperty
        }
        (b"set_property", _) if arg(0) == Some(b"TARGET") && after(b"PROPERTY") == Some(0) => {
            Topic::TargetProperty
        }
        (b"get_property", _) if arg(1) == Some(b"TARGET") && after(b"PROPERTY") == Some(0) => {
            Topic::TargetProperty
        }
        _ => return None,
    };
    Some(SubjectKind::Catalog(topic))
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
    workspace: &'a Workspace,
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
    catalog::command(lookup)
        .map(|entry| CommandInfo {
            name: entry.0,
            summary: entry.1,
            signature: signature_override(entry.0).or(Some(entry.2)),
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
    let mut text = if markdown {
        format!("{}\n\n{}", code_span(info.name), info.summary)
    } else {
        format!("{}\n\n{}", info.name, info.summary.replace("``", ""))
    };
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

fn catalog_hover(topic: Topic, entry: &Entry, markdown: bool) -> String {
    let mut text = if markdown {
        format!("{}\n\n{}", code_span(entry.0), entry.1)
    } else {
        format!("{}\n\n{}", entry.0, entry.1.replace("``", ""))
    };
    if !entry.2.is_empty() {
        text.push_str("\n\n");
        if markdown {
            text.push_str(&code_block(entry.2));
        } else {
            text.push_str(entry.2);
        }
    }
    if markdown {
        text.push_str(&format!("\n\n[CMake documentation]({})", topic.link(entry)));
    }
    text
}

fn display_path(uri: &str, markdown: bool) -> String {
    let path = crate::uri::path(uri).map_or_else(|| uri.into(), |path| path.display().to_string());
    if markdown { code_span(&path) } else { path }
}

pub(crate) fn hover<'a>(
    doc: &'a Document,
    uri: &'a str,
    workspace: impl FnOnce() -> Workspace,
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
            let path = display_path(destination, markdown);
            format!("Defined in {path} on line {}.", decl.command.line)
        };
        let snippet = if markdown {
            code_block(snippet)
        } else {
            snippet.to_owned()
        };
        Some(format!("{place}\n\n{snippet}"))
    } else if !matches.is_empty() {
        let mut text = format!("{} possible definitions.", matches.len());
        for (destination, _, decl) in matches.iter().take(5) {
            let path = display_path(destination, false);
            let site = format!("{path}:{}", decl.command.line);
            let site = if markdown { code_span(&site) } else { site };
            text.push_str(&format!("\n\n- {site}"));
        }
        Some(text)
    } else {
        match subject.kind {
            SubjectKind::Keyword((name, text), command) => Some(if markdown {
                format!("`{name}`\n\n{text}\n\n[CMake documentation]({DOCS}/command/{command}.html)")
            } else {
                format!("{name}\n\n{text}")
            }),
            SubjectKind::Catalog(topic) => topic.lookup(subject.text)
                .map(|entry| catalog_hover(topic, entry, markdown)),
            SubjectKind::Package => Topic::Module.lookup(&format!("Find{}", subject.text))
                .map(|entry| catalog_hover(Topic::Module, entry, markdown)),
            SubjectKind::Name(Kind::Variable(Variable::Normal | Variable::Cache)) => Topic::Variable
                .lookup(subject.text)
                .map(|entry| catalog_hover(Topic::Variable, entry, markdown)),
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
    workspace: impl FnOnce() -> Workspace,
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
    workspace: impl FnOnce() -> Workspace,
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
    let mut topic = None;
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
                topic = catalog_argument(command, index);
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
        for entry in Topic::Command.entries() {
            add(entry.0, 3, signature_override(entry.0).or(Some(entry.2)));
        }
        for (name, _, signature, _) in MODULE_COMMANDS {
            add(name, 3, Some(signature));
        }
    }
    if kinds
        .iter()
        .any(|kind| matches!(kind, Kind::Variable(Variable::Normal | Variable::Cache)))
    {
        topic = Some(SubjectKind::Catalog(Topic::Variable));
    }
    match topic {
        Some(SubjectKind::Catalog(topic)) => {
            for entry in topic.entries() {
                add(entry.0, 6, Some(entry.1));
            }
        }
        Some(SubjectKind::Package) => {
            for entry in Topic::Module.entries() {
                if let Some(package) = entry.0.strip_prefix("Find") {
                    add(package, 9, Some(entry.1));
                }
            }
        }
        _ => {}
    }
    completion_items(entries.into_values().collect())
}

pub(crate) fn completion_items(mut items: Vec<Value>) -> Value {
    if items.len() < MAX_COMPLETIONS {
        return Value::Array(items);
    }
    items.truncate(MAX_COMPLETIONS);
    object([
        ("isIncomplete", Value::Bool(true)),
        ("items", Value::Array(items)),
    ])
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
