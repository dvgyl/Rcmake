use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, path};
use rcmake_model::{CacheEntry, CacheType};

use super::{diagnostic, text};

#[derive(Clone, Copy)]
enum Kind {
    File,
    Path,
    Program,
    Library,
}

impl Kind {
    fn cache_type(self) -> CacheType {
        match self {
            Self::Path => CacheType::Path,
            _ => CacheType::Filepath,
        }
    }

    fn help(self) -> BString {
        match self {
            Self::File | Self::Path => "Path to a file.",
            Self::Program => "Path to a program.",
            Self::Library => "Path to a library.",
        }
        .into()
    }

    fn paths(self) -> SearchVariables {
        match self {
            Self::File | Self::Path => SearchVariables {
                cmake: b"CMAKE_INCLUDE_PATH",
                system: b"CMAKE_SYSTEM_INCLUDE_PATH",
                env: b"INCLUDE",
                prefix_subdirs: &["include"],
            },
            Self::Program => SearchVariables {
                cmake: b"CMAKE_PROGRAM_PATH",
                system: b"CMAKE_SYSTEM_PROGRAM_PATH",
                env: b"PATH",
                prefix_subdirs: &["bin", "sbin"],
            },
            Self::Library => SearchVariables {
                cmake: b"CMAKE_LIBRARY_PATH",
                system: b"CMAKE_SYSTEM_LIBRARY_PATH",
                env: b"LIB",
                prefix_subdirs: &["lib"],
            },
        }
    }
}

struct SearchVariables {
    cmake: &'static [u8],
    system: &'static [u8],
    env: &'static [u8],
    prefix_subdirs: &'static [&'static str],
}

#[derive(Default)]
struct Options {
    names: Vec<BString>,
    hints: Vec<BString>,
    paths: Vec<BString>,
    suffixes: Vec<BString>,
    doc: Option<BString>,
    no_cache: bool,
    required: bool,
    optional: bool,
    names_per_dir: bool,
    no_defaults: bool,
    no_cmake: bool,
    no_cmake_env: bool,
    no_system_env: bool,
    no_system: bool,
}

#[derive(Clone, Copy)]
enum Section {
    Names,
    Hints,
    Paths,
    Suffixes,
}

impl Options {
    fn parse(cx: &Cx<'_>, args: &[ExpandedArg]) -> Self {
        let mut options = Self::default();
        let mut section = Section::Names;
        let mut long_form = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.value.as_slice() {
                b"NAMES" => {
                    section = Section::Names;
                    long_form = true;
                }
                b"HINTS" => {
                    section = Section::Hints;
                    long_form = true;
                }
                b"PATHS" => {
                    section = Section::Paths;
                    long_form = true;
                }
                b"PATH_SUFFIXES" => section = Section::Suffixes,
                b"DOC" => options.doc = args.next().map(|arg| arg.value.clone()),
                b"NO_CACHE" => options.no_cache = true,
                b"REQUIRED" => options.required = true,
                b"OPTIONAL" => options.optional = true,
                b"NAMES_PER_DIR" => options.names_per_dir = true,
                b"NO_DEFAULT_PATH" => options.no_defaults = true,
                b"NO_CMAKE_PATH" => options.no_cmake = true,
                b"NO_CMAKE_ENVIRONMENT_PATH" => options.no_cmake_env = true,
                b"NO_SYSTEM_ENVIRONMENT_PATH" => options.no_system_env = true,
                b"NO_CMAKE_SYSTEM_PATH" => options.no_system = true,
                b"ENV" if matches!(section, Section::Hints | Section::Paths) => {
                    if let Some(value) = args.next().and_then(|key| cx.host().env_get(&key.value)) {
                        options
                            .section(section)
                            .extend(split_env(&value).map(BString::from));
                    }
                }
                _ => options.section(section).push(arg.value.clone()),
            }
        }
        if !long_form && options.names.len() > 1 {
            options.paths.extend(options.names.drain(1..));
        }
        options
    }

    fn section(&mut self, section: Section) -> &mut Vec<BString> {
        match section {
            Section::Names => &mut self.names,
            Section::Hints => &mut self.hints,
            Section::Paths => &mut self.paths,
            Section::Suffixes => &mut self.suffixes,
        }
    }
}

pub(super) fn find_file(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::File)
}

pub(super) fn find_path(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::Path)
}

pub(super) fn find_program(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::Program)
}

pub(super) fn find_library(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    find(cx, args, Kind::Library)
}

fn find(cx: &mut Cx<'_>, args: &[ExpandedArg], kind: Kind) -> CmdResult {
    let Some((var, rest)) = args.split_first().filter(|(_, rest)| !rest.is_empty()) else {
        return Err(CmdError::Status(
            "called with incorrect number of arguments".into(),
        ));
    };
    let var = &var.value;
    let options = Options::parse(cx, rest);
    let had_normal = cx.var_normal_only(var).is_some();
    if cx
        .var(var)
        .is_some_and(|v| v.as_slice() != b"NOTFOUND" && !v.ends_with(b"-NOTFOUND"))
    {
        if let Some(entry) = cx.cache().get_mut(var)
            && entry.ty == CacheType::Uninitialized
        {
            entry.ty = kind.cache_type();
            entry.help = options.doc.unwrap_or_else(|| kind.help());
        }
        return Ok(Flow::Normal);
    }

    if options.names_per_dir && matches!(kind, Kind::File | Kind::Path) {
        return Err(CmdError::Status("does not support NAMES_PER_DIR".into()));
    }
    if options.optional && options.required {
        return Err(CmdError::Status(
            "cannot be both REQUIRED and OPTIONAL".into(),
        ));
    }

    let directories = directories(cx, kind, &options);
    let found = search(cx, kind, &options, &directories);
    let mut value = found
        .as_deref()
        .map_or_else(|| text(&[var, b"-NOTFOUND"]), path::from_native);
    if found.is_some() && options.no_cache && matches!(kind, Kind::Path) && !value.ends_with(b"/") {
        value.push(b'/');
    }
    if options.no_cache || had_normal {
        cx.set_var(var, value.clone());
    }
    if !options.no_cache {
        let entry = cx.cache().get(var).cloned();
        cx.cache().insert(
            var.clone(),
            CacheEntry {
                value,
                ty: kind.cache_type(),
                help: options.doc.unwrap_or_else(|| kind.help()),
                advanced: entry.as_ref().is_some_and(|entry| entry.advanced),
                props: entry.map_or_else(Default::default, |entry| entry.props),
            },
        );
    }
    if found.is_none()
        && (options.required || !options.optional && cx.var_is_on(b"CMAKE_FIND_REQUIRED"))
    {
        let what: &[u8] = match kind {
            Kind::File | Kind::Path => b"files",
            _ => b"names",
        };
        let mut names = Vec::new();
        for (index, name) in options.names.iter().enumerate() {
            names.extend_from_slice(if index == 0 { b" " } else { b", " });
            names.extend_from_slice(name);
        }
        return Err(diagnostic(
            cx,
            text(&[
                b"Could not find ",
                var,
                b" using the following ",
                what,
                b":",
                &names,
            ]),
        ));
    }
    Ok(Flow::Normal)
}

fn directories(cx: &Cx<'_>, kind: Kind, options: &Options) -> Vec<PathBuf> {
    let variables = kind.paths();
    let mut paths = Vec::new();

    paths.extend(options.hints.iter().map(|dir| cx.resolve_path(dir)));
    if use_default(
        cx,
        options.no_defaults || options.no_cmake,
        b"CMAKE_FIND_USE_CMAKE_PATH",
    ) {
        add_prefixes(
            cx,
            &mut paths,
            cmake_paths(cx, b"CMAKE_PREFIX_PATH"),
            &variables,
        );
        paths.extend(
            cmake_paths(cx, variables.cmake)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    if use_default(
        cx,
        options.no_defaults || options.no_cmake_env,
        b"CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH",
    ) {
        add_prefixes(
            cx,
            &mut paths,
            env_paths(cx, b"CMAKE_PREFIX_PATH"),
            &variables,
        );
        paths.extend(
            env_paths(cx, variables.cmake)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    if use_default(
        cx,
        options.no_defaults || options.no_system_env,
        b"CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
    ) {
        paths.extend(
            env_paths(cx, variables.env)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    paths.extend(options.paths.iter().map(|dir| cx.resolve_path(dir)));
    if use_default(
        cx,
        options.no_defaults || options.no_system,
        b"CMAKE_FIND_USE_CMAKE_SYSTEM_PATH",
    ) {
        let prefixes = if cx.var_defined(b"CMAKE_SYSTEM_PREFIX_PATH") {
            cmake_paths(cx, b"CMAKE_SYSTEM_PREFIX_PATH")
        } else {
            let mut prefixes = vec!["/usr/local".into(), "/usr".into(), "/".into()];
            if cx.host().system().sysname.as_slice() == b"Darwin" {
                prefixes.push("/opt/homebrew".into());
            }
            prefixes
        };
        add_prefixes(cx, &mut paths, prefixes, &variables);
        paths.extend(
            cmake_paths(cx, variables.system)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    with_suffixes(paths, &options.suffixes)
}

fn add_prefixes(
    cx: &Cx<'_>,
    paths: &mut Vec<PathBuf>,
    prefixes: Vec<BString>,
    variables: &SearchVariables,
) {
    let architecture = cx.var_or_empty(b"CMAKE_LIBRARY_ARCHITECTURE");
    for prefix in prefixes {
        let prefix = cx.resolve_path(&prefix);
        for dir in variables.prefix_subdirs {
            let subdir = prefix.join(dir);
            if !architecture.is_empty() && matches!(*dir, "include" | "lib") {
                paths.push(subdir.join(path::to_native(&architecture)));
            }
            paths.push(subdir);
        }
        paths.push(prefix);
    }
}

fn with_suffixes(paths: Vec<PathBuf>, suffixes: &[BString]) -> Vec<PathBuf> {
    let suffixes: Vec<_> = suffixes
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| {
            path::to_native(s)
                .components()
                .filter(|component| {
                    matches!(component, Component::Normal(_) | Component::ParentDir)
                })
                .collect::<PathBuf>()
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut dirs = Vec::new();
    for dir in paths {
        for suffix in &suffixes {
            let path = normalize(&dir.join(suffix));
            if seen.insert(path.clone()) {
                dirs.push(path);
            }
        }
        let path = normalize(&dir);
        if seen.insert(path.clone()) {
            dirs.push(path);
        }
    }
    dirs
}

fn normalize(input: &Path) -> PathBuf {
    let mut path = PathBuf::new();
    for component in input.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            _ => path.push(component),
        }
    }
    path
}

fn use_default(cx: &Cx<'_>, disabled: bool, setting: &[u8]) -> bool {
    !disabled && (!cx.var_defined(setting) || cx.var_is_on(setting))
}

fn cmake_paths(cx: &Cx<'_>, var: &[u8]) -> Vec<BString> {
    list::split(&cx.var_or_empty(var), list::Empty::Drop)
}

fn split_env(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let separator = if cfg!(windows) { b';' } else { b':' };
    bytes
        .split(move |byte| *byte == separator)
        .filter(|path| !path.is_empty())
}

fn env_paths(cx: &Cx<'_>, var: &[u8]) -> Vec<BString> {
    cx.host().env_get(var).map_or_else(Vec::new, |value| {
        split_env(&value).map(BString::from).collect()
    })
}

fn search(cx: &Cx<'_>, kind: Kind, options: &Options, dirs: &[PathBuf]) -> Option<PathBuf> {
    let decoration = matches!(kind, Kind::Library).then(|| LibraryNames::new(cx));

    let search_in = |dir: &Path, name: &BString| {
        let path = path::to_native(name);
        if path.is_absolute() {
            return None;
        }
        if let Some(found) = candidate(cx, kind, dir.join(&path)) {
            if matches!(kind, Kind::Path) {
                return Some(dir.to_path_buf());
            }
            return Some(found);
        }
        decoration.as_ref()?.find(cx, dir, &path)
    };
    let absolute_program = |name: &BString| {
        let path = path::to_native(name);
        if matches!(kind, Kind::Program) && path.is_absolute() {
            candidate(cx, kind, path)
        } else {
            None
        }
    };
    if options.names_per_dir {
        options.names.iter().find_map(absolute_program).or_else(|| {
            dirs.iter()
                .find_map(|dir| options.names.iter().find_map(|name| search_in(dir, name)))
        })
    } else {
        options.names.iter().find_map(|name| {
            absolute_program(name).or_else(|| dirs.iter().find_map(|dir| search_in(dir, name)))
        })
    }
}

struct LibraryNames {
    prefixes: Vec<BString>,
    suffixes: Vec<BString>,
}

impl LibraryNames {
    fn new(cx: &Cx<'_>) -> Self {
        let prefixes = library_parts(cx, b"CMAKE_FIND_LIBRARY_PREFIXES", b"lib");
        let defaults: &[u8] = if cx.host().system().sysname.as_slice() == b"Darwin" {
            b".tbd;.dylib;.so;.a"
        } else {
            b".so;.a"
        };
        let suffixes = library_parts(cx, b"CMAKE_FIND_LIBRARY_SUFFIXES", defaults);
        Self { prefixes, suffixes }
    }

    fn find(&self, cx: &Cx<'_>, dir: &Path, name: &Path) -> Option<PathBuf> {
        let file = path::from_native(Path::new(name.file_name()?));
        let dir = dir.join(name.parent()?);
        self.prefixes.iter().find_map(|prefix| {
            self.suffixes.iter().find_map(|suffix| {
                let filename = text(&[prefix, &file, suffix]);
                candidate(cx, Kind::Library, dir.join(path::to_native(&filename)))
            })
        })
    }
}

fn library_parts(cx: &Cx<'_>, var: &[u8], default: &[u8]) -> Vec<BString> {
    let value = if cx.var_defined(var) {
        cx.var_or_empty(var)
    } else {
        BString::from(default)
    };
    if value.is_empty() {
        vec![BString::default()]
    } else {
        list::split(&value, list::Empty::Keep)
    }
}

fn candidate(cx: &Cx<'_>, kind: Kind, candidate: PathBuf) -> Option<PathBuf> {
    let candidate = normalize(&candidate);
    let metadata = cx.host().metadata(&candidate, Follow::Yes).ok()?;
    match kind {
        Kind::Program if metadata.kind != FileKind::File || metadata.mode & 0o111 == 0 => None,
        Kind::Library if metadata.kind != FileKind::File => None,
        _ => Some(candidate),
    }
}
