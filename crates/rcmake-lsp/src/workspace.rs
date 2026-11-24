use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use rcmake_host::{FileKind, FileTime, Follow, Host, ReadRange};
use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::{analysis::Document, uri};

const MAX_DEPTH: usize = 32;
const MAX_VISITS: usize = 256;
const MAX_EDGE_COMMANDS: usize = 200_000;
const MAX_FILES: usize = MAX_VISITS;
const MAX_PROBES: usize = 4096;
const MAX_GRAPHS: usize = 4;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_DISK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MODULE_PATHS: usize = 64;
const MAX_ALTERNATIVES: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime: FileTime,
    len: u64,
    dev: u64,
    ino: u64,
}

impl Stamp {
    fn read(host: &dyn Host, path: &Path) -> Option<Self> {
        let meta = host.metadata(path, Follow::Yes).ok()?;
        (meta.kind == FileKind::File).then_some(Self {
            mtime: meta.mtime,
            len: meta.len,
            dev: meta.dev,
            ino: meta.ino,
        })
    }
}

struct CachedFile {
    path: PathBuf,
    stamp: Stamp,
    canonical: PathBuf,
    document: Arc<Document>,
}

#[derive(Default)]
struct DiskCache {
    files: VecDeque<CachedFile>,
    bytes: u64,
}

impl DiskCache {
    fn discard(&mut self, index: usize) -> Option<CachedFile> {
        let file = self.files.remove(index)?;
        self.bytes -= file.stamp.len;
        Some(file)
    }

    fn load(
        &mut self,
        host: &dyn Host,
        path: &Path,
        stamp: Stamp,
        roots: &[PathBuf],
    ) -> Option<Arc<Document>> {
        let readable = |path: &Path| roots.iter().any(|root| path.starts_with(root));
        if let Some(index) = self.files.iter().position(|file| file.path == path)
            && let Some(file) = self.discard(index)
            && file.stamp == stamp
            && readable(&file.canonical)
        {
            let document = Arc::clone(&file.document);
            self.bytes += stamp.len;
            self.files.push_front(file);
            return Some(document);
        }
        let canonical = host.canonicalize(path).ok()?;
        if !readable(&canonical) {
            return None;
        }
        let data = host
            .read(&canonical, ReadRange::All { limit: stamp.len })
            .ok()?;
        if data.len() as u64 != stamp.len || Stamp::read(host, path) != Some(stamp) {
            return None;
        }
        let document = Arc::new(Document::new(String::from_utf8(data).ok()?, 0));
        while self.files.len() >= MAX_FILES || self.bytes + stamp.len > MAX_DISK_BYTES {
            self.bytes -= self.files.pop_back()?.stamp.len;
        }
        self.bytes += stamp.len;
        self.files.push_front(CachedFile {
            path: path.to_owned(),
            stamp,
            canonical,
            document: Arc::clone(&document),
        });
        Some(document)
    }
}

type OpenFiles = BTreeMap<String, (String, Option<Arc<Document>>)>;

#[derive(Default)]
pub(crate) struct WorkspaceCache {
    disk: DiskCache,
    graphs: VecDeque<Arc<WorkspaceGraph>>,
}

impl WorkspaceCache {
    pub(crate) fn invalidate_documents(&mut self) {
        self.graphs.clear();
    }

    pub(crate) fn invalidate_file(&mut self, host: &dyn Host, path: &Path) {
        let key = uri::file(path);
        let path = uri::path(&key).unwrap_or_else(|| path.to_owned());
        let canonical = host.canonicalize(&path).ok();
        let affected = |file: &CachedFile| {
            file.path == path
                || file.canonical == path
                || canonical.as_ref() == Some(&file.canonical)
        };
        let disk = &mut self.disk;
        self.graphs.retain(|graph| {
            !graph.stamps.contains_key(&path)
                && !disk
                    .files
                    .iter()
                    .filter(|file| affected(file))
                    .any(|file| graph.stamps.contains_key(&file.path))
        });
        let bytes = &mut disk.bytes;
        disk.files.retain(|file| {
            if affected(file) {
                *bytes -= file.stamp.len;
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn workspace<'a>(
        &mut self,
        host: &dyn Host,
        current_uri: &str,
        roots: &[uri::Root],
        bundled_modules: Option<&Path>,
        documents: impl Iterator<Item = (&'a str, Option<&'a Arc<Document>>)>,
    ) -> Workspace {
        let current = key_for_uri(current_uri);
        let path = uri::path(&current);
        let root = path.as_deref().and_then(|path| {
            roots
                .iter()
                .map(|root| &root.path)
                .filter(|root| path.starts_with(root))
                .max_by_key(|root| root.components().count())
        });
        let cached = self
            .graphs
            .iter()
            .position(|graph| {
                (graph.entry == current
                    || (graph.connected
                        && graph.sources.contains_key(&current)
                        && root.is_none_or(|root| graph.root.as_ref() == Some(root))))
                    && graph
                        .stamps
                        .iter()
                        .all(|(path, stamp)| Stamp::read(host, path) == *stamp)
            })
            .and_then(|index| self.graphs.remove(index));
        let graph = cached.unwrap_or_else(|| {
            let open = documents
                .map(|(uri, doc)| (key_for_uri(uri), (uri.to_owned(), doc.cloned())))
                .collect();
            let mut visit = Visit {
                host,
                cache: &mut self.disk,
                roots,
                stamps: BTreeMap::new(),
                remaining_bytes: MAX_DISK_BYTES,
                remaining_visits: MAX_VISITS,
                remaining_commands: MAX_EDGE_COMMANDS,
                active: BTreeSet::new(),
            };
            let graph = WorkspaceGraph::build(
                &mut visit,
                &current,
                path.as_deref(),
                root.cloned(),
                bundled_modules,
                open,
            );
            if self.graphs.len() == MAX_GRAPHS {
                self.graphs.pop_back();
            }
            Arc::new(graph)
        });
        self.graphs.push_front(Arc::clone(&graph));
        Workspace { graph, current }
    }
}

pub(crate) struct Workspace {
    graph: Arc<WorkspaceGraph>,
    current: String,
}

impl Workspace {
    pub(crate) fn documents(&self) -> impl Iterator<Item = (&str, &Document, bool)> {
        let connected = self
            .graph
            .sources
            .keys()
            .filter(|key| *key != &self.current)
            .filter_map(|key| {
                self.graph
                    .document(key)
                    .map(|doc| (self.graph.display_uri(key), doc, true))
            });
        let other_open =
            self.graph.open.iter().filter_map(|(key, (uri, doc))| {
                (key != &self.current
                    && !self.graph.sources.contains_key(key)
                    && self.graph.root.as_ref().is_some_and(|root| {
                        uri::path(key).is_some_and(|path| path.starts_with(root))
                    }))
                .then_some((uri.as_str(), doc.as_deref()?, false))
            });
        connected.chain(other_open)
    }

    pub(crate) fn variable_sources(&self, candidate: &str) -> bool {
        let key = key_for_uri(candidate);
        self.graph
            .sources
            .get(&self.current)
            .zip(self.graph.sources.get(&key))
            .is_some_and(|(current, candidate)| {
                current
                    .iter()
                    .any(|path| candidate.iter().any(|source| path.starts_with(source)))
            })
    }

    pub(crate) fn file_locations(&self, command: &CommandInvocation) -> impl Iterator<Item = &str> {
        self.graph
            .edges
            .get(&self.current)
            .and_then(|edges| edges.get(&command.span.start))
            .into_iter()
            .flatten()
            .map(|key| self.graph.display_uri(key))
    }
}

struct WorkspaceGraph {
    open: OpenFiles,
    disk: BTreeMap<String, Arc<Document>>,
    sources: BTreeMap<String, BTreeSet<PathBuf>>,
    edges: BTreeMap<String, BTreeMap<usize, BTreeSet<String>>>,
    display: BTreeMap<String, String>,
    entry: String,
    connected: bool,
    root: Option<PathBuf>,
    bundled_modules: Option<PathBuf>,
    readable_roots: Vec<PathBuf>,
    stamps: BTreeMap<PathBuf, Option<Stamp>>,
}

impl WorkspaceGraph {
    fn build(
        visit: &mut Visit<'_>,
        entry: &str,
        path: Option<&Path>,
        root: Option<PathBuf>,
        bundled_modules: Option<&Path>,
        open: OpenFiles,
    ) -> Self {
        let mut graph = Self {
            open,
            disk: BTreeMap::new(),
            sources: BTreeMap::new(),
            edges: BTreeMap::new(),
            display: BTreeMap::new(),
            entry: entry.into(),
            connected: false,
            root,
            bundled_modules: bundled_modules.map(Path::to_owned),
            readable_roots: Vec::new(),
            stamps: BTreeMap::new(),
        };
        let Some(path) = path else {
            return graph;
        };
        let parent = path.parent().unwrap_or(path);
        if graph.root.is_none() {
            let cwd = visit.host.initial_cwd();
            graph.root =
                if path.starts_with(&cwd) && graph.exists(visit, &cwd.join("CMakeLists.txt")) {
                    Some(cwd)
                } else {
                    parent
                        .ancestors()
                        .take(16)
                        .filter(|dir| graph.exists(visit, &dir.join("CMakeLists.txt")))
                        .last()
                        .map(Path::to_owned)
                };
        }
        graph.readable_roots = graph
            .root
            .iter()
            .chain(&graph.bundled_modules)
            .map(|root| {
                visit
                    .host
                    .canonicalize(root)
                    .unwrap_or_else(|_| root.clone())
            })
            .collect();
        let root = graph.root.clone();
        if let Some(root) = &root {
            graph.walk(
                visit,
                &root.join("CMakeLists.txt"),
                Some(root),
                &mut Scope::default(),
                false,
            );
        }
        graph.connected = graph.sources.contains_key(entry);
        if !graph.connected {
            let source = path
                .file_name()
                .is_some_and(|name| name == "CMakeLists.txt")
                .then_some(parent);
            graph.walk(visit, path, source, &mut Scope::default(), false);
        }
        graph.stamps = std::mem::take(&mut visit.stamps);
        graph
    }

    fn document(&self, key: &str) -> Option<&Document> {
        match self.open.get(key) {
            Some((_, doc)) => doc.as_deref(),
            None => self.disk.get(key).map(Arc::as_ref),
        }
    }

    fn display_uri<'a>(&'a self, key: &'a str) -> &'a str {
        self.open
            .get(key)
            .map(|(uri, _)| uri)
            .or_else(|| self.display.get(key))
            .map_or(key, String::as_str)
    }

    fn exists(&self, visit: &mut Visit<'_>, path: &Path) -> bool {
        let key = uri::file(path);
        match self.open.get(&key) {
            Some((_, doc)) => doc.is_some(),
            None => visit.stat(path).is_some(),
        }
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
        let stamp = visit.stat(path)?;
        if stamp.len > MAX_FILE_BYTES.min(visit.remaining_bytes) {
            return None;
        }
        let document = visit
            .cache
            .load(visit.host, path, stamp, &self.readable_roots)?;
        visit.remaining_bytes -= stamp.len;
        let commands = Arc::clone(&document.commands);
        self.disk.insert(key.into(), document);
        Some(commands)
    }

    fn walk(
        &mut self,
        visit: &mut Visit<'_>,
        path: &Path,
        source: Option<&Path>,
        scope: &mut Scope,
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
                    let context = PathContext {
                        current: path,
                        source,
                        root: self.root.as_deref(),
                        project: scope.project.as_deref(),
                    };
                    let paths = self.paths(visit, &context, &scope.modules, command);
                    let alternatives =
                        paths.len() > 1 || (name == b"include" && scope.modules.unknown);
                    if callable_depth == 0 {
                        scope.modules.update(&context, command, conditional);
                        if name == b"project" && !conditional && command.args.first().is_some() {
                            scope.project = source.map(Path::to_owned);
                        }
                    }
                    for destination in paths {
                        let target = uri::file(&destination);
                        self.display
                            .entry(target.clone())
                            .or_insert_with(|| uri::client_file(&destination, visit.roots));
                        self.edges
                            .entry(key.clone())
                            .or_default()
                            .entry(command.span.start)
                            .or_default()
                            .insert(target);
                        if name == b"add_subdirectory" {
                            self.walk(
                                visit,
                                &destination,
                                destination.parent(),
                                &mut scope.clone(),
                                conditional || alternatives,
                            );
                        } else {
                            self.walk(
                                visit,
                                &destination,
                                source,
                                scope,
                                conditional || alternatives,
                            );
                        }
                    }
                }
            }
        }
        visit.active.remove(&key);
    }

    fn paths(
        &self,
        visit: &mut Visit<'_>,
        context: &PathContext<'_>,
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
        let Some(name) = context.literal(name, arg.kind) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        if is_include && !name.is_absolute() {
            let in_bundled = self
                .bundled_modules
                .as_ref()
                .is_some_and(|root| context.current.starts_with(root));
            let mut module = name.as_os_str().to_owned();
            module.push(".cmake");
            let bundled = self.bundled_modules.as_ref().map(|root| root.join(&module));
            let bundled_first = in_bundled || modules.unknown;
            if bundled_first {
                candidates.extend(bundled.clone());
            }
            if let Some(source) = context.source {
                candidates.extend(
                    modules
                        .paths
                        .iter()
                        .map(|dir| source.join(dir).join(&module)),
                );
            }
            if !bundled_first {
                candidates.extend(bundled);
            }
        }
        let direct = if name.is_absolute() {
            Some(name)
        } else {
            context.source.map(|source| source.join(name))
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
                let path = uri::path(&key)?;
                (seen.insert(key) && self.exists(visit, &path)).then_some(path)
            })
            .take(if is_include && (modules.uncertain || modules.unknown) {
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
    roots: &'a [uri::Root],
    stamps: BTreeMap<PathBuf, Option<Stamp>>,
    remaining_bytes: u64,
    remaining_visits: usize,
    remaining_commands: usize,
    active: BTreeSet<String>,
}

impl Visit<'_> {
    fn stat(&mut self, path: &Path) -> Option<Stamp> {
        if let Some(stamp) = self.stamps.get(path) {
            return *stamp;
        }
        if self.stamps.len() >= MAX_PROBES {
            return None;
        }
        let stamp = Stamp::read(self.host, path);
        self.stamps.insert(path.to_owned(), stamp);
        stamp
    }
}

#[derive(Clone, Default)]
struct Scope {
    project: Option<PathBuf>,
    modules: Modules,
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
        context: &PathContext<'_>,
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
                if let Some(path) = context.literal(part, arg.kind) {
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

struct PathContext<'a> {
    current: &'a Path,
    source: Option<&'a Path>,
    root: Option<&'a Path>,
    project: Option<&'a Path>,
}

impl PathContext<'_> {
    fn literal(&self, name: &str, kind: ArgKind) -> Option<PathBuf> {
        if name.is_empty() || name.contains(['\0', '\\', ';']) {
            return None;
        }
        if kind == ArgKind::Bracket || !name.contains('$') {
            return Some(PathBuf::from(name));
        }
        for (variable, base) in [
            ("${CMAKE_CURRENT_LIST_DIR}", self.current.parent()),
            ("${CMAKE_CURRENT_SOURCE_DIR}", self.source),
            ("${CMAKE_SOURCE_DIR}", self.root),
            ("${PROJECT_SOURCE_DIR}", self.project.or(self.root)),
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
}

fn key_for_uri(key: &str) -> String {
    uri::path(key).map_or_else(|| key.into(), |path| uri::file(&path))
}

#[cfg(all(test, unix))]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn source_paths_and_bundled_modules_survive_unknown_entries() -> Result<(), &'static str> {
        let host = PureHost::default();
        for (path, contents) in [
            (
                "/work/CMakeLists.txt",
                concat!(
                    "project(Top)\n",
                    "list(APPEND CMAKE_MODULE_PATH \"${CMAKE_SOURCE_DIR}/cmake\" \"${UNKNOWN}\")\n",
                    "include(Local)\n",
                    "include(FetchContent)\n",
                    "add_subdirectory(nested)\n",
                ),
            ),
            (
                "/work/nested/CMakeLists.txt",
                concat!(
                    "project(Nested)\n",
                    "include(\"${PROJECT_SOURCE_DIR}/Nested.cmake\")\n",
                ),
            ),
            ("/work/cmake/Local.cmake", "add_library(root_lib INTERFACE)"),
            (
                "/work/nested/Nested.cmake",
                "add_library(nested_lib INTERFACE)",
            ),
            (
                "/bundled/FetchContent.cmake",
                "function(fetch)\nendfunction()",
            ),
        ] {
            host.seed_file(Path::new(path), contents.as_bytes())
                .map_err(|_| "Cannot create the listfile.")?;
        }
        let mut cache = WorkspaceCache::default();
        let roots = [uri::Root::new("file:///work").ok_or("Cannot read the workspace URI.")?];
        let root = "file:///work/CMakeLists.txt";
        let nested = "file:///work/nested/CMakeLists.txt";
        let bundled = Some(Path::new("/bundled"));
        let first = cache.workspace(&host, root, &roots, bundled, std::iter::empty());
        let second = cache.workspace(&host, nested, &roots, bundled, std::iter::empty());
        let mut paths: Vec<_> = first.documents().map(|(uri, _, _)| uri).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            [
                "file:///bundled/FetchContent.cmake",
                "file:///work/cmake/Local.cmake",
                nested,
                "file:///work/nested/Nested.cmake",
            ]
        );
        assert!(Arc::ptr_eq(&first.graph, &second.graph));
        Ok(())
    }
}
