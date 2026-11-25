use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use rcmake_api::{CompletionKind, ExecutedSymbol, SourceLocation, WorkspaceIndex};
use rcmake_base::BString;
use rcmake_diag::{SourceId, source::SourceKind};
use rcmake_host::{
    Host,
    path::{from_native_cmake, to_native},
};
use rcmake_json::Value;
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::{
    Position, PositionEncoding, SemanticContext, SemanticDefinition, SemanticLocation,
    SemanticResult,
    analysis::Document,
    expression::{self, Kind as ExprKind},
    json::{object, uint},
    lexical,
    position::Lines,
    roles::{Arguments, Role, Variable},
    uri,
};

use super::{
    Buffers, EvalSemanticProvider, Snapshot, SnapshotReply, buffer_path,
    presentation::{Binding, ValueAt},
    read_file,
};

const MAX_DEFINITIONS: usize = 32;
const MAX_COMPLETIONS: usize = 256;
const MAX_HOVER_BYTES: usize = 8192;

type HoverText = (String, Option<i64>);

impl EvalSemanticProvider {
    /// Get a Markdown hover and the version of the buffer that supplied it.
    ///
    /// A provisional result may come from a partial configure or an earlier edit.
    #[must_use]
    pub fn hover_markdown(&self, context: &SemanticContext<'_>) -> Option<(String, Option<i64>)> {
        self.render_hover(context, true)
    }

    pub(super) fn render_hover(
        &self,
        context: &SemanticContext<'_>,
        markdown: bool,
    ) -> Option<(String, Option<i64>)> {
        self.hover_reply(context, markdown)?
            .current(&self.snapshots)?
    }

    /// Compute a hover that can be delivered on another thread.
    #[must_use]
    pub fn hover_reply(
        &self,
        context: &SemanticContext<'_>,
        markdown: bool,
    ) -> Option<SnapshotReply<Option<HoverText>>> {
        self.query_hover(context, markdown, |_, version, text| (text, version))
    }

    pub(super) fn query_hover<T>(
        &self,
        context: &SemanticContext<'_>,
        markdown: bool,
        format: impl FnOnce(&[u8], Option<i64>, String) -> T,
    ) -> Option<SnapshotReply<Option<T>>> {
        self.snapshots.query(|snapshot, buffers| {
            let query = Query::new(self.host(), snapshot, buffers, context)?;
            let (kind, name) = query.subject()?;
            let label = if !query.fresh || query.index.is_provisional() {
                if markdown {
                    "*(provisional snapshot)*\n\n"
                } else {
                    "Provisional snapshot.\n\n"
                }
            } else {
                ""
            };
            let separator = if markdown { "\n\n---\n\n" } else { "\n\n" };
            let mut sections = Vec::new();
            let mut used = label.len();
            for binding in query.bindings(kind, name) {
                let section = binding.hover(&snapshot.result, markdown);
                if sections.contains(&section) {
                    continue;
                }
                let gap = if sections.is_empty() {
                    0
                } else {
                    separator.len()
                };
                let next = used + gap + section.len();
                if next > MAX_HOVER_BYTES {
                    break;
                }
                sections.push(section);
                used = next;
                if sections.len() == 8 {
                    break;
                }
            }
            if sections.is_empty() {
                return None;
            }
            let version = snapshot
                .inputs
                .by_uri
                .get(context.uri)
                .map(|buffer| buffer.document.version);
            Some(format(
                name,
                version,
                format!("{label}{}", sections.join(separator)),
            ))
        })
    }

    /// Get evaluated definitions or file destinations in execution order.
    ///
    /// When a file executes in several scopes, each distinct location appears
    /// once. An empty array lets the server use its lexical resolver.
    #[must_use]
    pub fn definitions(&self, context: &SemanticContext<'_>) -> Vec<SemanticLocation> {
        self.definitions_reply(context)
            .and_then(|reply| reply.current(&self.snapshots))
            .unwrap_or_default()
    }

    /// Compute definitions that can be delivered on another thread.
    #[must_use]
    pub fn definitions_reply(
        &self,
        context: &SemanticContext<'_>,
    ) -> Option<SnapshotReply<Vec<SemanticLocation>>> {
        self.snapshots.query(|snapshot, buffers| {
            let Some(query) =
                Query::new(self.host(), snapshot, buffers, context).filter(|query| query.fresh)
            else {
                return Vec::new();
            };
            Self::query_definitions(&query)
        })
    }

    pub(super) fn versioned_definition(
        &self,
        context: &SemanticContext<'_>,
    ) -> Option<SemanticResult<SemanticDefinition>> {
        self.snapshots
            .query(|snapshot, buffers| {
                let query = Query::new(self.host(), snapshot, buffers, context)?;
                if !query.fresh {
                    return None;
                }
                let location = Self::query_definitions(&query).into_iter().next()?;
                let target_version = buffers
                    .by_uri
                    .get(&location.uri)
                    .map(|buffer| buffer.document.version);
                Some(SemanticResult {
                    uri: context.uri.to_owned(),
                    version: context.version,
                    value: SemanticDefinition {
                        location,
                        target_version,
                    },
                })
            })?
            .current(&self.snapshots)?
    }

    fn query_definitions(query: &Query<'_>) -> Vec<SemanticLocation> {
        let mut locations = Vec::new();
        if query.on_path_argument() {
            for path in &query.spellings {
                let Some(site) = query.site(path) else {
                    continue;
                };
                for edge in query.index.resolve_file_edges(None, &site) {
                    if !query.source_matches(&edge.site) {
                        continue;
                    }
                    if let Some(uri) = query.file_uri(&edge.destination) {
                        push_location(
                            &mut locations,
                            SemanticLocation {
                                uri,
                                start: Position::default(),
                                end: Position::default(),
                            },
                        );
                        if locations.len() == MAX_DEFINITIONS {
                            return locations;
                        }
                    }
                }
            }
            if !locations.is_empty() {
                return locations;
            }
        }
        if let Some((kind, name)) = query.subject() {
            let mut origins = BTreeSet::new();
            for binding in query.bindings(kind, name) {
                if let Some(site) = binding.origin()
                    && origins.insert(site)
                    && let Some(location) = query.location(site)
                {
                    push_location(&mut locations, location);
                    if locations.len() == MAX_DEFINITIONS {
                        break;
                    }
                }
            }
        }
        locations
    }

    /// Get evaluated LSP completion items for the cursor's syntactic context.
    ///
    /// Each item includes its kind, a detail preview, and the replacement range.
    /// An empty array lets the server use its lexical completion items.
    #[must_use]
    pub fn completions(&self, context: &SemanticContext<'_>) -> Vec<Value> {
        self.completions_reply(context)
            .and_then(|reply| reply.current(&self.snapshots))
            .unwrap_or_default()
    }

    /// Compute completions that can be delivered on another thread.
    #[must_use]
    pub fn completions_reply(
        &self,
        context: &SemanticContext<'_>,
    ) -> Option<SnapshotReply<Vec<Value>>> {
        self.snapshots.query(|snapshot, buffers| {
            let Some(query) =
                Query::new(self.host(), snapshot, buffers, context).filter(|query| query.fresh)
            else {
                return Vec::new();
            };
            Self::query_completions(&query)
        })
    }

    fn query_completions(query: &Query<'_>) -> Vec<Value> {
        let Some((kind, span)) = query.completion_site() else {
            return Vec::new();
        };
        let Some(prefix) = query
            .doc
            .text
            .get(span.start..query.offset)
            .filter(|prefix| prefix.len() <= 4096)
        else {
            return Vec::new();
        };
        let edit = query.doc.span(span, query.encoding);
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        for dir in query
            .spellings
            .iter()
            .flat_map(|path| query.index.scopes_for_file(path))
        {
            for item in query.index.completions_for_scope(*dir, prefix.as_bytes()) {
                let name = item.name.as_slice();
                let binding = match (kind, item.kind) {
                    (Kind::Target, CompletionKind::Target) => {
                        query.index.lookup_target(*dir, name).map(Binding::Target)
                    }
                    (Kind::Callable, CompletionKind::Callable) => {
                        query.index.lookup_callable(name).map(Binding::Callable)
                    }
                    (Kind::Variable(Variable::Normal), CompletionKind::Variable) => {
                        query.index.lookup_variable(*dir, name).map(|var| {
                            Binding::snapshot_variable(
                                name,
                                var,
                                &query.snapshot.result.model.dir(*dir).source_dir,
                            )
                        })
                    }
                    (Kind::Variable(Variable::Cache), CompletionKind::Variable) => {
                        query.cache(name)
                    }
                    _ => None,
                };
                if let Some(binding) = binding
                    && let Ok(label) = std::str::from_utf8(name)
                    && seen.insert(label)
                {
                    let mut detail = binding.detail();
                    if query.index.is_provisional() {
                        detail.push_str(" (provisional snapshot)");
                    }
                    items.push(object([
                        ("label", Value::string(label)),
                        ("kind", uint(binding.completion_kind())),
                        ("detail", Value::string(detail)),
                        (
                            "textEdit",
                            object([("range", edit.clone()), ("newText", Value::string(label))]),
                        ),
                    ]));
                    if items.len() == MAX_COMPLETIONS {
                        return items;
                    }
                }
            }
        }
        items
    }

    pub(super) fn host(&self) -> &dyn Host {
        self.snapshots.request.host.as_ref()
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Target,
    Callable,
    Variable(Variable),
}

impl Kind {
    fn expression(kind: ExprKind) -> Option<Self> {
        match kind {
            ExprKind::Target => Some(Self::Target),
            ExprKind::Reference(Variable::Environment) | ExprKind::Generator => None,
            ExprKind::Reference(namespace) => Some(Self::Variable(namespace)),
        }
    }
}

struct Query<'a> {
    host: &'a dyn Host,
    snapshot: &'a Snapshot,
    index: &'a WorkspaceIndex,
    buffers: &'a Buffers,
    spellings: Vec<BString>,
    doc: &'a Document,
    offset: usize,
    encoding: PositionEncoding,
    fresh: bool,
    same_buffer: bool,
    checked_sources: RefCell<BTreeMap<SourceId, bool>>,
}

impl<'a> Query<'a> {
    fn new(
        host: &'a dyn Host,
        snapshot: &'a Snapshot,
        buffers: &'a Buffers,
        context: &SemanticContext<'_>,
    ) -> Option<Self> {
        let uri_path = uri::path(context.uri)?;
        let buffer = buffers.by_uri.get(context.uri)?;
        let path = &buffer.path;
        let doc = &buffer.document;
        if buffer_path(host, &uri_path) != *path
            || doc.version != context.version
            || doc.text != context.text
            || !doc.text.is_char_boundary(context.offset)
        {
            return None;
        }
        let spellings = snapshot.paths.get(path).map_or_else(
            || vec![from_native_cmake(&uri_path)],
            |paths| paths.iter().cloned().collect(),
        );
        let same_buffer = snapshot
            .inputs
            .selected
            .get(path)
            .is_some_and(|buffer| buffer.document.text == doc.text);
        Some(Self {
            host,
            snapshot,
            index: snapshot.result.index.as_ref()?,
            buffers,
            same_buffer,
            spellings,
            doc,
            offset: context.offset,
            encoding: context.encoding,
            fresh: same_buffer && snapshot.fresh(buffers),
            checked_sources: RefCell::default(),
        })
    }

    fn site(&self, path: &BString) -> Option<SourceLocation> {
        self.same_buffer.then_some(())?;
        Some(SourceLocation {
            path: path.clone(),
            line: u32::try_from(self.doc.lines.line_at(self.offset) + 1).ok()?,
            byte_offset: Some(u32::try_from(self.offset).ok()?),
            source_id: None,
            range: None,
        })
    }

    fn source_matches(&self, site: &SourceLocation) -> bool {
        // Byte offsets refer to raw file bytes, including CRLF line endings.
        let text = self.doc.text.as_bytes();
        if let Some(id) = site.source_id {
            return *self
                .checked_sources
                .borrow_mut()
                .entry(id)
                .or_insert_with(|| {
                    self.index.source(id).is_some_and(|source| {
                        source.path == site.path
                            && source.kind == SourceKind::File
                            && source.text.as_ref() == text
                    })
                })
                && self
                    .index
                    .source(id)
                    .is_some_and(|source| source.path == site.path);
        }
        self.snapshot.matches(&to_native(&site.path), text)
    }

    fn on_path_argument(&self) -> bool {
        self.doc.command_at(self.offset).is_some_and(|command| {
            Arguments::new(command).role(0) == Role::Path
                && command
                    .args
                    .first()
                    .is_some_and(|arg| arg.span.contains(&self.offset))
        })
    }

    fn subject(&self) -> Option<(Kind, &'a [u8])> {
        let command = self.doc.command_at(self.offset)?;
        if self.offset <= name_end(command) {
            return Some((Kind::Callable, command.name.as_slice()));
        }
        let (index, arg) = command
            .args
            .iter()
            .enumerate()
            .find(|(_, arg)| arg.span.contains(&self.offset))?;
        let span = argument_payload(&self.doc.text, arg)?;
        let raw = self.doc.text.get(span.clone())?;
        let relative = self.offset.checked_sub(span.start)?;
        if let Some(expr) = expression::expressions(raw)
            .into_iter()
            .find(|expr| expr.span.contains(&relative))
        {
            return Some((Kind::expression(expr.kind)?, raw.get(expr.name)?.as_bytes()));
        }
        if !span.contains(&self.offset) {
            return None;
        }
        let arguments = Arguments::new(command);
        if arguments.keyword(index, arg).is_some() {
            return None;
        }
        match arguments.role(index) {
            Role::Target | Role::TargetDeclaration if expression::target_name(raw) => {
                Some((Kind::Target, raw.as_bytes()))
            }
            Role::CommandDeclaration => Some((Kind::Callable, raw.as_bytes())),
            Role::Variable(Variable::Normal) => {
                Some((Kind::Variable(Variable::Normal), raw.as_bytes()))
            }
            Role::Variable(Variable::Cache) => {
                let name = raw
                    .strip_prefix("CACHE{")
                    .and_then(|name| name.strip_suffix('}'))
                    .unwrap_or(raw);
                Some((Kind::Variable(Variable::Cache), name.as_bytes()))
            }
            _ => None,
        }
    }

    fn bindings(&self, kind: Kind, name: &'a [u8]) -> Vec<Binding<'a>> {
        let mut observed = false;
        let mut bindings = Vec::new();
        for path in &self.spellings {
            let Some(site) = self.site(path) else {
                continue;
            };
            for record in self.index.executed_at(None, &site) {
                let binding = match (kind, &record.symbol) {
                    (
                        Kind::Callable,
                        ExecutedSymbol::Callable {
                            name: actual,
                            definition,
                        },
                    ) if actual.eq_ignore_ascii_case(name) => {
                        Some(definition.as_ref().map(Binding::Callable))
                    }
                    (
                        Kind::Target,
                        ExecutedSymbol::Target {
                            name: actual,
                            definition,
                        },
                    ) if actual.as_slice() == name => {
                        Some(definition.as_ref().map(Binding::Target))
                    }
                    (
                        Kind::Variable(_),
                        ExecutedSymbol::Variable {
                            name: actual,
                            definition,
                        },
                    ) if actual.as_slice() == name => {
                        // The verified cursor syntax supplies the namespace.
                        // An undefined reference still counts as an observation.
                        Some(
                            definition
                                .as_ref()
                                .map(|value| Binding::variable(name, value, ValueAt::WriteSite)),
                        )
                    }
                    _ => None,
                };
                if let Some(binding) = binding
                    && self.source_matches(&record.site)
                {
                    observed = true;
                    bindings.extend(binding);
                }
            }
        }
        // An executed but undefined name also takes precedence over the final state.
        if observed {
            return bindings;
        }
        let mut dirs = self
            .spellings
            .iter()
            .flat_map(|path| self.index.scopes_for_file(path).iter().copied())
            .peekable();
        if dirs.peek().is_none() {
            return Vec::new();
        }
        match kind {
            Kind::Callable => self
                .index
                .lookup_callable(name)
                .map(Binding::Callable)
                .into_iter()
                .collect(),
            Kind::Target => dirs
                .filter_map(|dir| self.index.lookup_target(dir, name).map(Binding::Target))
                .collect(),
            Kind::Variable(Variable::Cache) => self.cache(name).into_iter().collect(),
            Kind::Variable(Variable::Normal) => dirs
                .filter_map(|dir| {
                    self.index.lookup_variable(dir, name).map(|var| {
                        Binding::snapshot_variable(
                            name,
                            var,
                            &self.snapshot.result.model.dir(dir).source_dir,
                        )
                    })
                })
                .collect(),
            Kind::Variable(Variable::Environment) => Vec::new(),
        }
    }

    fn cache(&self, name: &'a [u8]) -> Option<Binding<'a>> {
        let result = &self.snapshot.result;
        let entry = result.model.cache().get(name)?;
        Some(Binding::Variable {
            name,
            value: &entry.value,
            origin: result.facts.cache_sites.get(name),
            cached: true,
            value_at: ValueAt::FinalCache,
        })
    }

    fn file_uri(&self, path: &[u8]) -> Option<String> {
        let path = to_native(path);
        if !path.is_absolute() {
            return None;
        }
        let buffer = self.buffers.at(self.host, &path);
        let disk;
        let text = if let Some(buffer) = buffer {
            buffer.document.text.as_bytes()
        } else {
            disk = read_file(self.host, &path).ok()?;
            &disk
        };
        if self.snapshot.reads.files.contains_key(&path) && !self.snapshot.matches(&path, text) {
            return None;
        }
        Some(buffer.map_or_else(|| uri::file(&path), |buffer| buffer.uri.clone()))
    }

    fn location(&self, site: &SourceLocation) -> Option<SemanticLocation> {
        let path = to_native(&site.path);
        if !path.is_absolute() {
            return None;
        }
        let buffer = self.buffers.at(self.host, &path);
        let owned;
        let disk_lines;
        let (text, lines) = if let Some(buffer) = buffer {
            (buffer.document.text.as_str(), &buffer.document.lines)
        } else {
            owned = read_file(self.host, &path).ok()?;
            let text = std::str::from_utf8(&owned).ok()?;
            disk_lines = Lines::new(text);
            (text, &disk_lines)
        };
        if let Some(id) = site.source_id {
            let source = self.index.source(id)?;
            if source.kind != SourceKind::File
                || source.path != site.path
                || source.text.as_ref() != text.as_bytes()
            {
                return None;
            }
        } else if !self.snapshot.matches(&path, text.as_bytes()) {
            return None;
        }
        let offsets = site
            .range
            .map(|span| (span.start, span.end))
            .or_else(|| site.byte_offset.map(|offset| (offset, offset)));
        let (start, end) = if let Some((start, end)) = offsets {
            let (start, end) = (usize::try_from(start).ok()?, usize::try_from(end).ok()?);
            text.get(start..end)?;
            (
                lines.position(text, start, self.encoding),
                lines.position(text, end, self.encoding),
            )
        } else {
            let line = usize::try_from(site.line.saturating_sub(1)).ok()?;
            lines.start(line)?;
            let position = Position { line, character: 0 };
            (position, position)
        };
        Some(SemanticLocation {
            uri: buffer.map_or_else(|| uri::file(&path), |buffer| buffer.uri.clone()),
            start,
            end,
        })
    }

    fn completion_site(&self) -> Option<(Kind, Range<usize>)> {
        if self
            .doc
            .comments()
            .iter()
            .any(|comment| comment.start <= self.offset && self.offset <= comment.end)
        {
            return None;
        }
        let word = word_at(&self.doc.text, self.offset);
        let Some(command) = self.doc.command_at(self.offset) else {
            let start = self.doc.lines.start(self.doc.lines.line_at(self.offset))?;
            return self
                .doc
                .text
                .get(start..word.start)?
                .trim_start_matches('\u{feff}')
                .trim_ascii()
                .is_empty()
                .then_some((Kind::Callable, word));
        };
        if self.offset <= name_end(command) {
            return Some((Kind::Callable, command.span.start..name_end(command)));
        }
        if !self
            .doc
            .text
            .get(name_end(command)..self.offset)?
            .contains('(')
        {
            return None;
        }
        let index = command
            .args
            .partition_point(|arg| arg.span.end < self.offset);
        let arguments = Arguments::new(command);
        if let Some(arg) = command
            .args
            .get(index)
            .filter(|arg| arg.span.start <= self.offset)
        {
            let span = argument_payload(&self.doc.text, arg)?;
            let relative = self.offset.checked_sub(span.start)?;
            let raw = self.doc.text.get(span.clone())?;
            if let Some(expr) = expression::expressions(raw)
                .into_iter()
                .find(|expr| expr.name.start <= relative && relative <= expr.name.end)
            {
                return Some((
                    Kind::expression(expr.kind)?,
                    span.start + expr.name.start..span.start + expr.name.end,
                ));
            }
            if arguments.keyword(index, arg).is_some() {
                return None;
            }
        }
        (arguments.role(index) == Role::Target).then_some((Kind::Target, word))
    }
}

fn name_end(command: &CommandInvocation) -> usize {
    command.span.start + command.name.len()
}

fn argument_payload(text: &str, arg: &Argument) -> Option<Range<usize>> {
    match arg.kind {
        ArgKind::Unquoted => Some(arg.span.clone()),
        ArgKind::Quoted => {
            let raw = text.as_bytes().get(arg.span.clone())?;
            let (end, closed) = lexical::quoted_end(raw, 0);
            Some(arg.span.start + 1..arg.span.start + end - usize::from(closed))
        }
        ArgKind::Bracket => None,
    }
}

fn word_at(text: &str, offset: usize) -> Range<usize> {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || b"_-.+:".contains(&byte);
    let (before, after) = text.as_bytes().split_at(offset);
    let left = before.iter().rev().take_while(|&&byte| word(byte)).count();
    let right = after.iter().take_while(|&&byte| word(byte)).count();
    offset - left..offset + right
}

fn push_location(locations: &mut Vec<SemanticLocation>, candidate: SemanticLocation) {
    if !locations.iter().any(|prior| {
        prior.uri == candidate.uri && prior.start == candidate.start && prior.end == candidate.end
    }) {
        locations.push(candidate);
    }
}
