use std::{collections::BTreeMap, ops::Range, path::PathBuf};

use rcmake_diag::policy::PolicyId;
use rcmake_json::Value;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::{
    analysis::Document,
    catalog::{COMMANDS, KEYWORDS, VARIABLES},
    json::{object, uint},
    position::{Position, PositionEncoding},
};

pub(crate) const TOKEN_TYPES: [&str; 5] = ["function", "keyword", "variable", "string", "comment"];
const COMMAND: usize = 0;
const KEYWORD: usize = 1;
const VARIABLE: usize = 2;
const STRING: usize = 3;
const COMMENT: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Project,
    Target,
    Function,
    Directory,
    Variable,
}

impl Kind {
    fn symbol(self) -> usize {
        match self {
            Self::Project => 2,
            Self::Target => 5,
            Self::Function => 12,
            Self::Directory => 19,
            Self::Variable => 13,
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
    argument: &'a Argument,
    command: &'a CommandInvocation,
}

fn declarations(document: &Document) -> impl Iterator<Item = Declaration<'_>> {
    document.commands.iter().filter_map(|command| {
        let kind = match command.lower.as_slice() {
            b"project" => Kind::Project,
            b"add_executable" | b"add_library" | b"add_custom_target" => Kind::Target,
            b"function" | b"macro" => Kind::Function,
            b"add_subdirectory" => Kind::Directory,
            b"option" | b"set" => Kind::Variable,
            _ => return None,
        };
        let argument = command.args.first()?;
        let name = std::str::from_utf8(&argument.text).ok()?;
        if name.is_empty() || name.len() > 4096 {
            return None;
        }
        Some(Declaration {
            kind,
            name,
            argument,
            command,
        })
    })
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
                    (
                        "selectionRange",
                        doc.span(decl.argument.span.clone(), encoding),
                    ),
                ])
            })
            .collect(),
    )
}

fn word_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || b"_-.+:".contains(&byte);
    let start = text
        .as_bytes()
        .get(..offset)?
        .iter()
        .rev()
        .take_while(|&&byte| word(byte))
        .count();
    let end = text
        .as_bytes()
        .get(offset..)?
        .iter()
        .take_while(|&&byte| word(byte))
        .count();
    (start + end > 0).then_some(offset - start..offset + end)
}

fn command_name(command: &CommandInvocation) -> Range<usize> {
    command.span.start..command.span.start + command.name.len()
}

fn variable_refs(text: &str) -> impl Iterator<Item = Range<usize>> {
    let mut cursor = 0;
    std::iter::from_fn(move || {
        let mut iter = text.as_bytes().get(cursor..)?.iter();
        while let Some(byte) = iter.next() {
            let start = text.len() - iter.len() - 1;
            if *byte == b'\\' {
                iter.next();
                continue;
            }
            if *byte != b'$' {
                continue;
            }
            let tail = text.get(start..)?;
            let Some(name) = ["${", "$ENV{", "$CACHE{"]
                .into_iter()
                .find_map(|prefix| tail.strip_prefix(prefix))
            else {
                continue;
            };
            let Some(end) = name.find(['{', '}', '\n']) else {
                continue;
            };
            if name.as_bytes().get(end) == Some(&b'}') {
                cursor = text.len() - name.len() + end + 1;
                return Some(start..cursor);
            }
        }
        None
    })
}

fn variable_name(text: &str) -> Option<&str> {
    let (_, tail) = text.split_once('{')?;
    tail.strip_suffix('}').filter(|name| !name.is_empty())
}

struct Subject<'a> {
    text: &'a str,
    range: Range<usize>,
    kind: Option<Kind>,
    scoped: bool,
}

fn subject(doc: &Document, offset: usize) -> Option<Subject<'_>> {
    if let Some(command) = doc.command_at(offset) {
        let name = command_name(command);
        if offset <= name.end {
            return Some(Subject {
                text: std::str::from_utf8(&command.name).ok()?,
                range: name,
                kind: Some(Kind::Function),
                scoped: true,
            });
        }
        let arg = command.args.iter().find(|arg| arg.span.contains(&offset))?;
        if arg.kind != ArgKind::Bracket {
            let raw = doc.text.get(arg.span.clone())?;
            if let Some(span) =
                variable_refs(raw).find(|span| span.contains(&(offset - arg.span.start)))
            {
                return Some(Subject {
                    text: variable_name(raw.get(span.clone())?)?,
                    range: arg.span.start + span.start..arg.span.start + span.end,
                    kind: Some(Kind::Variable),
                    scoped: true,
                });
            }
        }
        let range = word_at(&doc.text, offset)?;
        let kind = match command.lower.as_slice() {
            b"set" | b"option" | b"unset" if command.args.first() == Some(arg) => {
                Some(Kind::Variable)
            }
            b"add_executable" | b"add_library" | b"add_custom_target"
                if command.args.first() == Some(arg) =>
            {
                Some(Kind::Target)
            }
            b"function" | b"macro" if command.args.first() == Some(arg) => Some(Kind::Function),
            cmd if cmd.starts_with(b"target_") || cmd == b"add_dependencies" => Some(Kind::Target),
            _ => None,
        };
        return Some(Subject {
            text: doc.text.get(range.clone())?,
            range,
            kind,
            scoped: kind.is_some(),
        });
    }
    None
}

fn matching_declaration<'a>(doc: &'a Document, subject: &Subject<'_>) -> Option<Declaration<'a>> {
    let matches = |decl: &Declaration<'_>| {
        subject.kind.is_none_or(|kind| kind == decl.kind)
            && match decl.kind {
                Kind::Function => decl.name.eq_ignore_ascii_case(subject.text),
                _ => decl.name == subject.text,
            }
    };
    let mut first_after = None;
    let mut last_before = None;
    for decl in declarations(doc).filter(matches) {
        if decl.argument.span.start <= subject.range.start {
            last_before = Some(decl);
        } else {
            first_after = Some(decl);
            break;
        }
    }
    last_before.or(first_after)
}

pub(crate) fn hover(doc: &Document, offset: usize, encoding: PositionEncoding) -> Value {
    let Some(subject) = subject(doc, offset) else {
        return Value::Null;
    };
    let summary = if let Some(decl) = matching_declaration(doc, &subject) {
        let end = decl.command.span.end.min(decl.command.span.start + 512);
        let end = doc.text.floor_char_boundary(end);
        let snippet = doc
            .text
            .get(decl.command.span.start..end)
            .unwrap_or_default();
        Some(format!(
            "Defined on line {}.\n\n{snippet}",
            decl.command.line
        ))
    } else if let Some(policy) = PolicyId::parse(subject.text.as_bytes()).and_then(PolicyId::info) {
        Some(policy.summary.to_owned())
    } else {
        let lookup = if subject.scoped && subject.kind == Some(Kind::Function) {
            COMMANDS
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(subject.text))
        } else {
            VARIABLES.iter().find(|(name, _)| *name == subject.text)
        };
        lookup.map(|(name, summary)| format!("{name}\n\n{summary}"))
    };
    summary.map_or(Value::Null, |summary| {
        object([
            (
                "contents",
                object([
                    ("kind", Value::string("plaintext")),
                    ("value", Value::string(summary)),
                ]),
            ),
            ("range", doc.span(subject.range, encoding)),
        ])
    })
}

pub(crate) fn definition(
    doc: &Document,
    uri: &str,
    offset: usize,
    encoding: PositionEncoding,
) -> Value {
    let Some(subject) = subject(doc, offset) else {
        return Value::Null;
    };
    matching_declaration(doc, &subject).map_or(Value::Null, |decl| {
        location(uri, doc.span(decl.argument.span.clone(), encoding))
    })
}

pub(crate) fn location(uri: &str, range: Value) -> Value {
    object([("uri", Value::string(uri)), ("range", range)])
}

pub(crate) fn file_reference(doc: &Document, offset: usize, dir: PathBuf) -> Vec<PathBuf> {
    let Some(command) = doc.command_at(offset) else {
        return Vec::new();
    };
    let Some(arg) = command
        .args
        .first()
        .filter(|arg| arg.span.contains(&offset))
    else {
        return Vec::new();
    };
    let Ok(name) = std::str::from_utf8(&arg.text) else {
        return Vec::new();
    };
    if name.is_empty() || name.contains(['$', '\0']) {
        return Vec::new();
    }
    match command.lower.as_slice() {
        b"add_subdirectory" => vec![dir.join(name).join("CMakeLists.txt")],
        b"include" => {
            let path = dir.join(name);
            if name.contains('.') || name.contains(['/', '\\']) {
                vec![path]
            } else {
                vec![path.with_extension("cmake"), path]
            }
        }
        _ => Vec::new(),
    }
}

pub(crate) fn module_name(doc: &Document, offset: usize) -> Option<&str> {
    let command = doc.command_at(offset)?;
    let arg = command.args.first()?;
    let name = std::str::from_utf8(&arg.text).ok()?;
    (command.lower == "include"
        && arg.span.contains(&offset)
        && !name.is_empty()
        && !name.contains(['.', '/', '\\', '$', '\0']))
    .then_some(name)
}

pub(crate) fn completion(doc: &Document, offset: usize) -> Value {
    let prefix = word_at(&doc.text, offset)
        .and_then(|span| doc.text.get(span.start..offset))
        .unwrap_or_default();
    let prefix = prefix.to_ascii_lowercase();
    let mut entries: BTreeMap<&str, usize> = BTreeMap::new();
    for (name, kind) in COMMANDS
        .iter()
        .map(|(name, _)| (*name, 3))
        .chain(KEYWORDS.iter().map(|name| (*name, 14)))
        .chain(VARIABLES.iter().map(|(name, _)| (*name, 6)))
        .chain(declarations(doc).map(|decl| (decl.name, decl.kind.completion())))
    {
        if entries.len() < 2048 && name.to_ascii_lowercase().starts_with(&prefix) {
            entries.insert(name, kind);
        }
    }
    Value::Array(
        entries
            .into_iter()
            .map(|(name, kind)| object([("label", Value::string(name)), ("kind", uint(kind))]))
            .collect(),
    )
}

pub(crate) fn semantic_tokens(doc: &Document, encoding: PositionEncoding) -> Value {
    let comments = doc.comments();
    let mut comments = comments.into_iter().peekable();
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
        for arg in &command.args {
            tokens.comments_before(&mut comments, arg.span.start);
            let Some(text) = doc.text.get(arg.span.clone()) else {
                continue;
            };
            if arg.kind == ArgKind::Bracket {
                tokens.push(arg.span.clone(), STRING);
            } else {
                let kind = if arg.kind == ArgKind::Quoted {
                    Some(STRING)
                } else if KEYWORDS.contains(&text) {
                    Some(KEYWORD)
                } else if matches!(command.lower.as_slice(), b"set" | b"option" | b"unset")
                    && command.args.first() == Some(arg)
                {
                    Some(VARIABLE)
                } else {
                    None
                };
                let mut cursor = arg.span.start;
                for var in variable_refs(text) {
                    let var = arg.span.start + var.start..arg.span.start + var.end;
                    if let Some(kind) = kind {
                        tokens.push(cursor..var.start, kind);
                    }
                    cursor = var.end;
                    tokens.push(var, VARIABLE);
                }
                if let Some(kind) = kind {
                    tokens.push(cursor..arg.span.end, kind);
                }
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
