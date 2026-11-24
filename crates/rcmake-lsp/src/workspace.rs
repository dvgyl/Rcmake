use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use rcmake_host::{FileKind, Follow, Host, ReadRange};
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::{analysis::Document, uri};

const MAX_DEPTH: usize = 32;
const MAX_VISITS: usize = 256;
const MAX_EDGE_COMMANDS: usize = 200_000;
const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_DISK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MODULE_PATHS: usize = 64;
const MAX_ALTERNATIVES: usize = 16;

#[derive(Default)]
pub(crate) struct DiskCache {
    files: BTreeMap<String, Arc<Document>>,
    bytes: u64,
}

impl DiskCache {
    fn parse(&mut self, uri: &str, text: Vec<u8>) -> Option<Arc<Document>> {
        if let Some(doc) = self
            .files
            .get(uri)
            .filter(|doc| doc.text.as_bytes() == text.as_slice())
        {
            return Some(Arc::clone(doc));
        }
        let document = Arc::new(Document::new(String::from_utf8(text).ok()?, 0));
        if let Some(prior) = self.files.remove(uri) {
            self.bytes -= prior.text.len() as u64;
        }
        if self.files.len() >= MAX_FILES || self.bytes + document.text.len() as u64 > MAX_DISK_BYTES
        {
            self.files.clear();
            self.bytes = 0;
        }
        self.bytes += document.text.len() as u64;
        self.files.insert(uri.into(), Arc::clone(&document));
        Some(document)
    }
}

pub(crate) struct Workspace<'a> {
    open: BTreeMap<String, (&'a str, Option<&'a Document>)>,
    disk: BTreeMap<String, Arc<Document>>,
    sources: BTreeMap<String, BTreeSet<PathBuf>>,
    edges: BTreeMap<(String, usize), BTreeSet<String>>,
    current: String,
    root: Option<PathBuf>,
    bundled_modules: Option<PathBuf>,
    readable_roots: Vec<PathBuf>,
}

impl<'a> Workspace<'a> {
    pub(crate) fn new(
        host: &dyn Host,
        current_uri: &'a str,
        roots: &[PathBuf],
        bundled_modules: Option<&Path>,
        documents: impl Iterator<Item = (&'a str, Option<&'a Document>)>,
        cache: &mut DiskCache,
    ) -> Self {
        let mut workspace = Self {
            open: documents
                .map(|(key, document)| (key_for_uri(key), (key, document)))
                .collect(),
            disk: BTreeMap::new(),
            sources: BTreeMap::new(),
            edges: BTreeMap::new(),
            current: key_for_uri(current_uri),
            root: None,
            bundled_modules: bundled_modules.map(Path::to_owned),
            readable_roots: Vec::new(),
        };
        let Some(path) = uri::path(current_uri) else {
            return workspace;
        };
        let parent = path.parent().unwrap_or(&path);
        workspace.root = roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
            .or_else(|| {
                let cwd = host.initial_cwd();
                if path.starts_with(&cwd) && workspace.exists(host, &cwd.join("CMakeLists.txt")) {
                    return Some(cwd);
                }
                parent
                    .ancestors()
                    .take(16)
                    .filter(|dir| workspace.exists(host, &dir.join("CMakeLists.txt")))
                    .last()
                    .map(Path::to_owned)
            });
        workspace.readable_roots = workspace
            .root
            .iter()
            .chain(&workspace.bundled_modules)
            .map(|root| host.canonicalize(root).unwrap_or_else(|_| root.clone()))
            .collect();
        let mut visit = Visit {
            host,
            cache,
            remaining_bytes: MAX_DISK_BYTES,
            remaining_visits: MAX_VISITS,
            remaining_commands: MAX_EDGE_COMMANDS,
            active: BTreeSet::new(),
        };
        let mut modules = Modules::default();
        if let Some(root) = workspace.root.clone() {
            let entry = root.join("CMakeLists.txt");
            workspace.walk(&mut visit, &entry, Some(&root), &mut modules, false);
        }
        if !workspace.sources.contains_key(&workspace.current) {
            let source = (path
                .file_name()
                .is_some_and(|name| name == "CMakeLists.txt"))
            .then_some(parent);
            workspace.walk(&mut visit, &path, source, &mut Modules::default(), false);
        }
        workspace
    }

    pub(crate) fn documents(&self) -> impl Iterator<Item = (&str, &Document, bool)> {
        let connected = self
            .sources
            .keys()
            .filter(|key| *key != &self.current)
            .filter_map(|key| {
                self.document(key)
                    .map(|doc| (self.display_uri(key), doc, true))
            });
        let other_open = self.open.iter().filter_map(|(key, (uri, doc))| {
            let doc = (*doc)?;
            (key != &self.current
                && !self.sources.contains_key(key)
                && self.root.as_ref().is_some_and(|root| {
                    crate::uri::path(key).is_some_and(|path| path.starts_with(root))
                }))
            .then_some((*uri, doc, false))
        });
        connected.chain(other_open)
    }

    pub(crate) fn variable_sources(&self, candidate: &str) -> bool {
        let key = key_for_uri(candidate);
        self.sources
            .get(&self.current)
            .zip(self.sources.get(&key))
            .is_some_and(|(current, candidate)| {
                current
                    .iter()
                    .any(|path| candidate.iter().any(|source| path.starts_with(source)))
            })
    }

    pub(crate) fn file_locations(&self, command: &CommandInvocation) -> impl Iterator<Item = &str> {
        self.edges
            .get(&(self.current.clone(), command.span.start))
            .into_iter()
            .flatten()
            .map(|key| self.display_uri(key))
    }

    fn document(&self, key: &str) -> Option<&Document> {
        if let Some((_, doc)) = self.open.get(key) {
            *doc
        } else {
            self.disk.get(key).map(Arc::as_ref)
        }
    }

    fn display_uri<'b>(&'b self, key: &'b str) -> &'b str {
        self.open.get(key).map_or(key, |(uri, _)| *uri)
    }

    fn exists(&self, host: &dyn Host, path: &Path) -> bool {
        self.open
            .get(&uri::file(path))
            .is_some_and(|(_, doc)| doc.is_some())
            || host
                .metadata(path, Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
    }

    fn load(
        &mut self,
        visit: &mut Visit<'_>,
        path: &Path,
        key: &str,
    ) -> Option<Arc<[CommandInvocation]>> {
        if let Some(doc) = self.document(key) {
            return Some(Arc::clone(&doc.commands));
        }
        if self.open.contains_key(key) || self.disk.len() >= MAX_FILES {
            return None;
        }
        let canonical = visit.host.canonicalize(path).ok()?;
        if !self
            .readable_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            return None;
        }
        let limit = MAX_FILE_BYTES.min(visit.remaining_bytes);
        let metadata = visit.host.metadata(&canonical, Follow::Yes).ok()?;
        if limit == 0 || metadata.kind != FileKind::File || metadata.len > limit {
            return None;
        }
        let data = visit.host.read(&canonical, ReadRange::All { limit }).ok()?;
        visit.remaining_bytes = visit.remaining_bytes.saturating_sub(data.len() as u64);
        let document = visit.cache.parse(key, data)?;
        let commands = Arc::clone(&document.commands);
        self.disk.insert(key.into(), document);
        Some(commands)
    }

    fn walk(
        &mut self,
        visit: &mut Visit<'_>,
        path: &Path,
        source: Option<&Path>,
        modules: &mut Modules,
        conditional: bool,
    ) {
        let key = uri::file(path);
        if visit.active.len() >= MAX_DEPTH
            || visit.remaining_visits == 0
            || visit.active.contains(&key)
        {
            return;
        }
        let Some(commands) = self.load(visit, path, &key) else {
            return;
        };
        visit.remaining_visits -= 1;
        visit.active.insert(key.clone());
        let sources = self.sources.entry(key.clone()).or_default();
        if let Some(source) = source {
            sources.insert(source.to_owned());
        }
        let mut blocks = Vec::new();
        let mut callable_depth = 0;
        for command in commands.iter() {
            if visit.remaining_commands == 0 {
                break;
            }
            visit.remaining_commands -= 1;
            if let Some(doc) = self.document(&key)
                && doc.is_unfinished(command)
            {
                continue;
            }
            let name = command.lower.as_slice();
            match name {
                b"if" | b"foreach" | b"while" | b"function" | b"macro" | b"block" => {
                    blocks.push(name);
                    callable_depth += usize::from(matches!(name, b"function" | b"macro"));
                }
                b"endif" | b"endforeach" | b"endwhile" | b"endfunction" | b"endmacro"
                | b"endblock" => {
                    if blocks
                        .last()
                        .is_some_and(|opening| name.get(3..) == Some(*opening))
                    {
                        blocks.pop();
                        callable_depth -= usize::from(matches!(name, b"endfunction" | b"endmacro"));
                    }
                }
                _ => {
                    let conditional = conditional || !blocks.is_empty();
                    let paths = self.paths(visit.host, path, source, modules, command);
                    let alternatives = paths.len() > 1;
                    for destination in paths {
                        self.edges
                            .entry((key.clone(), command.span.start))
                            .or_default()
                            .insert(uri::file(&destination));
                        if name == b"add_subdirectory" {
                            self.walk(
                                visit,
                                &destination,
                                destination.parent(),
                                &mut modules.clone(),
                                conditional || alternatives,
                            );
                        } else {
                            self.walk(
                                visit,
                                &destination,
                                source,
                                modules,
                                conditional || alternatives,
                            );
                        }
                    }
                    if callable_depth == 0 {
                        modules.update(path, source, command, conditional);
                    }
                }
            }
        }
        visit.active.remove(&key);
    }

    fn paths(
        &self,
        host: &dyn Host,
        current: &Path,
        source: Option<&Path>,
        modules: &Modules,
        command: &CommandInvocation,
    ) -> Vec<PathBuf> {
        let is_include = command.lower == "include";
        if !is_include && command.lower != "add_subdirectory" {
            return Vec::new();
        }
        let Some((arg, name)) = command
            .args
            .first()
            .and_then(|arg| Some((arg, std::str::from_utf8(&arg.text).ok()?)))
        else {
            return Vec::new();
        };
        let Some(name) = literal_path(current, source, name, arg.kind) else {
            return Vec::new();
        };
        if !name.is_absolute() && (source.is_none() || (is_include && modules.unknown)) {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        if is_include && !name.is_absolute() {
            let in_bundled = self
                .bundled_modules
                .as_ref()
                .is_some_and(|root| current.starts_with(root));
            let mut module = name.as_os_str().to_owned();
            module.push(".cmake");
            let bundled = self.bundled_modules.as_ref().map(|root| root.join(&module));
            if in_bundled {
                candidates.extend(bundled.clone());
            }
            if let Some(source) = source {
                candidates.extend(
                    modules
                        .paths
                        .iter()
                        .map(|dir| source.join(dir).join(&module)),
                );
            }
            if !in_bundled {
                candidates.extend(bundled);
            }
        }
        let direct = if name.is_absolute() {
            Some(name)
        } else {
            source.map(|source| source.join(name))
        };
        candidates.extend(direct.map(|path| {
            if is_include {
                path
            } else {
                path.join("CMakeLists.txt")
            }
        }));
        let mut seen = BTreeSet::new();
        candidates
            .into_iter()
            .filter_map(|path| {
                let key = uri::file(&path);
                if !seen.insert(key.clone()) || !self.exists(host, &path) {
                    return None;
                }
                uri::path(&key)
            })
            .take(if modules.uncertain {
                MAX_ALTERNATIVES
            } else {
                1
            })
            .collect()
    }
}

struct Visit<'a> {
    host: &'a dyn Host,
    cache: &'a mut DiskCache,
    remaining_bytes: u64,
    remaining_visits: usize,
    remaining_commands: usize,
    active: BTreeSet<String>,
}

#[derive(Clone, Default)]
struct Modules {
    paths: Vec<PathBuf>,
    uncertain: bool,
    unknown: bool,
}

impl Modules {
    fn update(
        &mut self,
        current: &Path,
        source: Option<&Path>,
        command: &CommandInvocation,
        conditional: bool,
    ) {
        let bytes = |index: usize| {
            command
                .args
                .get(index)
                .map_or(b"".as_slice(), |arg| arg.text.as_slice())
        };
        let (name, skip) = match command.lower.as_slice() {
            b"set" | b"unset" if bytes(0) == b"CMAKE_MODULE_PATH" => (command.lower.as_slice(), 1),
            b"list" if bytes(1) == b"CMAKE_MODULE_PATH" => (bytes(0), 2),
            _ => return,
        };
        if name == b"unset" {
            if conditional {
                self.uncertain = true;
            } else {
                self.paths.clear();
                self.uncertain = false;
                self.unknown = false;
            }
            return;
        }
        if !matches!(name, b"set" | b"APPEND" | b"PREPEND") {
            self.unknown = true;
            return;
        }
        let mut paths = Vec::new();
        let mut unknown = false;
        for arg in command.args.iter().skip(skip) {
            for part in path_list(arg) {
                if let Some(path) = literal_path(current, source, part, arg.kind) {
                    if paths.len() >= MAX_MODULE_PATHS {
                        unknown = true;
                        break;
                    }
                    paths.push(path);
                } else {
                    unknown = true;
                }
            }
        }
        if name == b"set" && !conditional {
            self.paths = paths;
            self.uncertain = false;
            self.unknown = unknown;
        } else {
            self.uncertain |= conditional;
            self.unknown |= unknown;
            if name == b"PREPEND" {
                paths.append(&mut self.paths);
                self.paths = paths;
            } else {
                self.paths.extend(paths);
            }
            self.unknown |= self.paths.len() > MAX_MODULE_PATHS;
            self.paths.truncate(MAX_MODULE_PATHS);
        }
    }
}

fn path_list(arg: &Argument) -> impl Iterator<Item = &str> {
    std::str::from_utf8(&arg.text)
        .unwrap_or_default()
        .split(';')
        .filter(|part| !part.is_empty())
}

fn literal_path(
    current: &Path,
    source: Option<&Path>,
    name: &str,
    kind: ArgKind,
) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['\0', '\\', ';']) {
        return None;
    }
    if kind == ArgKind::Bracket || !name.contains('$') {
        return Some(PathBuf::from(name));
    }
    for (variable, base) in [
        ("${CMAKE_CURRENT_LIST_DIR}", current.parent()),
        ("${CMAKE_CURRENT_SOURCE_DIR}", source),
    ] {
        if let Some(tail) = name.strip_prefix(variable)
            && (tail.is_empty() || tail.starts_with('/'))
            && !tail.contains('$')
        {
            return Some(base?.join(tail.trim_start_matches('/')));
        }
    }
    None
}

fn key_for_uri(key: &str) -> String {
    uri::path(key).map_or_else(|| key.into(), |path| uri::file(&path))
}
