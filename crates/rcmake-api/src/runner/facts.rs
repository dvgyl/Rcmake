use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, Limits};
use rcmake_diag::{Backtrace, Diagnostic, FrameKind, SourceId, SourceMap, source::SourceKind};
use rcmake_host::path;
use rcmake_model::{DirId, ProjectModel};
use rcmake_syntax::{CommandInvocation, parse_list_file};

use crate::{
    EvaluationFacts, ExecutedSite, ExecutedSymbol, FileEdge, FileEdgeKind, PreviousValue,
    SourceLocation, SourceRange, VarLedgerSnapshot, VariableDefinition, VariableWriteDetails,
};

const MAX_SOURCE_READS: usize = 4096;
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
pub(super) struct SourceReads {
    files: BTreeMap<PathBuf, Option<Arc<[u8]>>>,
    bytes: usize,
}

impl SourceReads {
    pub(super) fn record(&mut self, path: &Path, bytes: &[u8]) {
        if let Some(previous) = self.files.get_mut(path) {
            if previous
                .as_deref()
                .is_some_and(|previous| previous != bytes)
            {
                *previous = None;
            }
        } else if self.files.len() < MAX_SOURCE_READS
            && self.bytes.saturating_add(bytes.len()) <= MAX_SOURCE_BYTES
        {
            self.files.insert(path.to_owned(), Some(Arc::from(bytes)));
            self.bytes += bytes.len();
        }
    }
}

struct ParsedSource {
    id: SourceId,
    commands: Arc<[CommandInvocation]>,
}

pub(super) fn evaluation_facts(
    model: &ProjectModel,
    ledger: Option<&VarLedgerSnapshot>,
    reads: &SourceReads,
    limits: &Limits,
    mut sources: SourceMap,
    collect_index: bool,
) -> EvaluationFacts {
    let mut facts = EvaluationFacts::default();
    let mut parsed = BTreeMap::new();
    for file in model.dirs().flat_map(|(_, directory)| &directory.listfiles) {
        if parsed.contains_key(file) {
            continue;
        }
        let Some(Some(bytes)) = reads.files.get(&path::to_native(file)) else {
            continue;
        };
        let Some(id) = sources.add(file.clone(), SourceKind::File, Arc::clone(bytes)) else {
            continue;
        };
        let commands = parse_list_file(bytes, id, SourceKind::File, limits)
            .map_or_else(|_| Arc::from([]), |list| list.commands);
        parsed.insert(file.clone(), ParsedSource { id, commands });
    }
    facts.source_map = Some(Arc::new(sources));

    for (dir, alias, global, trace) in model.alias_declarations() {
        if let Some(location) = declaration_site(&parsed, trace, alias) {
            facts.aliases.push(crate::AliasSite {
                dir,
                global,
                name: alias.clone(),
                location,
            });
        }
        include_edges(model, &parsed, dir, trace, &mut facts.file_edges);
    }

    for (id, target) in model.targets() {
        if let Some(mut site) = SourceLocation::from_backtrace(&target.backtrace) {
            let name = target
                .backtrace
                .top()
                .and_then(|frame| frame.name.as_deref());
            let (source, command) = command_at(&parsed, &site, name);
            site.source_id = source;
            site.range = command.and_then(|command| {
                command
                    .args
                    .first()
                    .filter(|arg| arg.text == target.name)
                    .and_then(|arg| source_range(&arg.span))
            });
            facts.target_sites.insert(id, site);
        }
        include_edges(
            model,
            &parsed,
            target.dir,
            &target.backtrace,
            &mut facts.file_edges,
        );
    }
    for (_, dir) in model.dirs() {
        let Some((parent, site)) = dir
            .parent
            .zip(SourceLocation::from_backtrace(&dir.backtrace))
        else {
            continue;
        };
        let destination =
            path::from_native_cmake(&path::to_native(&dir.source_dir).join("CMakeLists.txt"));
        if parsed.contains_key(&destination) {
            add_file_edge(
                &parsed,
                &mut facts.file_edges,
                FileEdgeKind::AddSubdirectory,
                parent,
                site,
                b"add_subdirectory",
                destination,
            );
        }
        include_edges(
            model,
            &parsed,
            parent,
            &dir.backtrace,
            &mut facts.file_edges,
        );
    }
    if let Some(ledger) = ledger {
        let mut executed = Vec::new();
        for name in ledger.names() {
            for event in ledger.history(name) {
                let dir = write_directory(model, &event.backtrace);
                if let Some(dir) = dir {
                    include_edges(model, &parsed, dir, &event.backtrace, &mut facts.file_edges);
                }
                if let Some(location) = declaration_site(&parsed, &event.backtrace, name) {
                    observe_binding(model, &parsed, &mut facts, name, event, &location);
                    if collect_index
                        && let Some(dir) = dir
                        && let Some(site) = executed_binding(name, event, dir, location)
                    {
                        executed.push((event.seq, site));
                    }
                }
                facts.variable_writes.insert(
                    event.seq,
                    VariableWriteDetails {
                        dir,
                        binding_dir: event.binding_dir,
                        command: event
                            .backtrace
                            .top()
                            .and_then(|frame| frame.name.as_deref())
                            .map(BString::from),
                        previous: event
                            .old_value
                            .clone()
                            .map_or(PreviousValue::Unset, PreviousValue::Value),
                    },
                );
            }
        }
        executed.sort_unstable_by_key(|(seq, _)| *seq);
        let sites = Arc::make_mut(&mut facts.executed_sites);
        for (_, site) in executed {
            sites.record(site);
        }
    }
    facts
}

fn declaration_site(
    parsed: &BTreeMap<BString, ParsedSource>,
    trace: &Backtrace,
    symbol: &[u8],
) -> Option<SourceLocation> {
    let mut site = SourceLocation::from_backtrace(trace)?;
    let name = trace.top()?.name.as_deref();
    let (source, command) = command_at(parsed, &site, name);
    site.source_id = source;
    site.range = command.and_then(|command| {
        let mut matches = command.args.iter().filter(|arg| arg.text == symbol);
        let arg = matches.next().filter(|_| matches.next().is_none())?;
        source_range(&arg.span)
    });
    Some(site)
}

fn executed_binding(
    name: &BString,
    event: &rcmake_eval::VarEvent,
    dir: DirId,
    location: SourceLocation,
) -> Option<ExecutedSite> {
    location.range?;
    let cached = matches!(
        event.op,
        rcmake_eval::VarOp::CacheSet
            | rcmake_eval::VarOp::CacheDeclare
            | rcmake_eval::VarOp::CacheUnset
    );
    let definition = match &event.new_value {
        Some(value) if value.len == value.prefix.len() => Some(VariableDefinition {
            value: value.prefix.clone(),
            location: Some(location.clone()),
            cached,
            provisional: false,
        }),
        Some(_) => return None,
        None => None,
    };
    Some(ExecutedSite {
        dir,
        site: location,
        symbol: ExecutedSymbol::Variable {
            name: name.clone(),
            definition,
        },
    })
}

fn observe_binding(
    model: &ProjectModel,
    parsed: &BTreeMap<BString, ParsedSource>,
    facts: &mut EvaluationFacts,
    name: &BString,
    event: &rcmake_eval::VarEvent,
    location: &SourceLocation,
) {
    use rcmake_eval::VarOp;
    let matches = |value: &BString| {
        event
            .new_value
            .as_ref()
            .is_some_and(|preview| preview.matches(value))
    };
    match event.op {
        VarOp::CacheUnset => {
            facts.cache_force.remove(name);
            facts.cache_sites.remove(name);
        }
        VarOp::CacheSet
            if model
                .cache()
                .get(name)
                .is_some_and(|entry| matches(&entry.value)) =>
        {
            facts.cache_sites.insert(name.clone(), location.clone());
            let (_, command) = command_at(parsed, location, Some(b"set"));
            if let Some(force) = command.and_then(cache_force) {
                facts.cache_force.insert(name.clone(), force);
            } else {
                facts.cache_force.remove(name);
            }
        }
        VarOp::CacheDeclare
            if model
                .cache()
                .get(name)
                .is_some_and(|entry| matches(&entry.value)) =>
        {
            facts
                .cache_sites
                .entry(name.clone())
                .or_insert_with(|| location.clone());
            let (_, command) = command_at(parsed, location, Some(b"set"));
            let force = command.and_then(cache_force).unwrap_or(false);
            facts.cache_force.entry(name.clone()).or_insert(force);
        }
        VarOp::Set | VarOp::ParentScope | VarOp::Unset => {
            let Some(dir) = event.binding_dir else { return };
            let sites = facts.variable_sites.entry(dir).or_default();
            if model.dir(dir).vars.get(name).is_some_and(matches) {
                sites.insert(name.clone(), location.clone());
            } else {
                sites.remove(name);
            }
        }
        _ => {}
    }
}

fn cache_force(command: &CommandInvocation) -> Option<bool> {
    let first = command.args.first()?;
    if first.text.starts_with(b"CACHE{") {
        return Some(
            command
                .args
                .iter()
                .skip(1)
                .take_while(|arg| arg.text != b"VALUE")
                .any(|arg| arg.kind == rcmake_syntax::ArgKind::Unquoted && arg.text == b"FORCE"),
        );
    }
    let last = command.args.last()?;
    let start = command.args.len().checked_sub(4)?;
    if last.kind == rcmake_syntax::ArgKind::Unquoted
        && last.text == b"FORCE"
        && command
            .args
            .get(start)
            .is_some_and(|arg| arg.text == b"CACHE")
    {
        return Some(true);
    }
    let start = command.args.len().checked_sub(3)?;
    command
        .args
        .get(start)
        .filter(|arg| arg.text == b"CACHE")
        .map(|_| false)
}

fn write_directory(model: &ProjectModel, trace: &Backtrace) -> Option<DirId> {
    let mut frames = trace.frames().peekable();
    while let Some(frame) = frames.next() {
        if frame.kind != FrameKind::ModuleInclude
            || frames.peek().is_some_and(|caller| {
                !caller
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(b"add_subdirectory"))
            })
        {
            continue;
        }
        let src = path::to_native(&frame.file);
        if src.file_name().is_none_or(|name| name != "CMakeLists.txt") {
            continue;
        }
        let dir = path::from_native_cmake(src.parent()?);
        let mut matching = model
            .dirs()
            .filter(|(_, candidate)| candidate.source_dir == dir);
        if let Some((id, _)) = matching.next() {
            return matching.next().is_none().then_some(id);
        }
    }
    None
}

fn include_edges(
    model: &ProjectModel,
    parsed: &BTreeMap<BString, ParsedSource>,
    dir: DirId,
    trace: &Backtrace,
    edges: &mut Vec<FileEdge>,
) {
    let mut frames = trace.frames().peekable();
    while let Some(frame) = frames.next() {
        if frame.kind != FrameKind::ModuleInclude {
            continue;
        }
        let Some(caller) = frames.peek() else {
            break;
        };
        if !caller
            .name
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(b"include"))
            || !model
                .dir(dir)
                .listfiles
                .iter()
                .any(|file| file.as_slice() == frame.file.as_ref())
        {
            continue;
        }
        add_file_edge(
            parsed,
            edges,
            FileEdgeKind::Include,
            dir,
            SourceLocation {
                path: caller.file.as_ref().into(),
                line: caller.line,
                byte_offset: None,
                source_id: None,
                range: None,
            },
            b"include",
            frame.file.as_ref().into(),
        );
    }
}

fn add_file_edge(
    parsed: &BTreeMap<BString, ParsedSource>,
    edges: &mut Vec<FileEdge>,
    kind: FileEdgeKind,
    dir: DirId,
    mut site: SourceLocation,
    name: &[u8],
    destination: BString,
) {
    let (source, command) = command_at(parsed, &site, Some(name));
    let Some(argument_range) = command
        .and_then(|command| command.args.first())
        .and_then(|arg| source_range(&arg.span))
    else {
        return;
    };
    site.source_id = source;
    let edge = FileEdge {
        kind,
        dir,
        site,
        argument_range: Some(argument_range),
        destination,
    };
    if edges.len() < MAX_SOURCE_READS && !edges.contains(&edge) {
        edges.push(edge);
    }
}

fn source_range(range: &Range<usize>) -> Option<SourceRange> {
    Some(SourceRange {
        start: u32::try_from(range.start).ok()?,
        end: u32::try_from(range.end).ok()?,
    })
}

fn command_at<'a>(
    parsed: &'a BTreeMap<BString, ParsedSource>,
    site: &SourceLocation,
    name: Option<&[u8]>,
) -> (Option<SourceId>, Option<&'a CommandInvocation>) {
    let Some(source) = parsed.get(&site.path) else {
        return (None, None);
    };
    let first = source
        .commands
        .partition_point(|command| command.line < site.line);
    let mut commands = source
        .commands
        .iter()
        .skip(first)
        .take_while(|command| command.line == site.line)
        .filter(|command| name.is_some_and(|name| command.name.eq_ignore_ascii_case(name)));
    let command = commands.next().filter(|_| commands.next().is_none());
    (Some(source.id), command)
}

pub(super) fn merge_diagnostics(
    output: &mut Vec<Diagnostic>,
    sources: &mut SourceMap,
    diagnostics: &[Diagnostic],
    origin: &SourceMap,
) {
    let mut ids = BTreeMap::new();
    let mut translate = |id| {
        if let Some(mapped) = ids.get(&id) {
            return Some(*mapped);
        }
        let source = origin.get(id)?;
        let mapped = sources.add(source.path.clone(), source.kind, source.text.clone())?;
        ids.insert(id, mapped);
        Some(mapped)
    };
    output.extend(diagnostics.iter().cloned().map(|mut diagnostic| {
        if let Some(annotations) = &mut diagnostic.annotations {
            annotations.labels.retain_mut(|label| {
                let Some(id) = translate(label.span.source) else {
                    return false;
                };
                label.span.source = id;
                true
            });
            annotations.fixes.retain_mut(|fix| {
                fix.edits.iter_mut().all(|edit| {
                    let Some(id) = translate(edit.span.source) else {
                        return false;
                    };
                    edit.span.source = id;
                    true
                })
            });
        }
        diagnostic
    }));
}
