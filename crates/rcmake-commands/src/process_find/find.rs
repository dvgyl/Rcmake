use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use rcmake_base::{BString, list};
use rcmake_diag::{FrameKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, UserCommandKind};
use rcmake_expr::ExpandedArg;
use rcmake_host::{FileKind, Follow, path};
use rcmake_model::{CacheEntry, CacheType};
use rcmake_syntax::ArgKind;

use super::{diagnostic, text};

#[derive(Clone, Copy)]
enum Kind {
    File,
    Path,
    Program,
    Library,
}

impl Kind {
    fn command(self) -> &'static str {
        match self {
            Self::File => "find_file",
            Self::Path => "find_path",
            Self::Program => "find_program",
            Self::Library => "find_library",
        }
    }

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
    no_install: bool,
    validator: Option<BString>,
}

#[derive(Clone, Copy)]
enum Section {
    Names,
    Hints,
    Paths,
    Suffixes,
}

impl Options {
    fn parse(cx: &Cx<'_>, args: &[ExpandedArg]) -> Result<Self, CmdError> {
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
                b"REGISTRY_VIEW" => {
                    let view = option_value(&mut args, "REGISTRY_VIEW")?;
                    if !matches!(
                        view,
                        b"64" | b"32" | b"64_32" | b"32_64" | b"HOST" | b"TARGET" | b"BOTH"
                    ) {
                        return Err(status(&[b"given invalid value for REGISTRY_VIEW: ", view]));
                    }
                }
                b"VALIDATOR" => {
                    let name = option_value(&mut args, "VALIDATOR")?;
                    if !cx.command_exists(name) {
                        return Err(status(&[
                            b"command specified for VALIDATOR is undefined: ",
                            name,
                            b".",
                        ]));
                    }
                    if cx.user_command_kind(name) != Some(UserCommandKind::Function) {
                        return Err(status(&[
                            b"command specified for VALIDATOR is not a function: ",
                            name,
                            b".",
                        ]));
                    }
                    options.validator = Some(name.into());
                }
                b"NO_CMAKE_INSTALL_PREFIX" => options.no_install = true,
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
        Ok(options)
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

fn option_value<'a>(
    args: &mut impl Iterator<Item = &'a ExpandedArg>,
    keyword: &str,
) -> Result<&'a [u8], CmdError> {
    args.next()
        .map(|arg| arg.value.as_slice())
        .ok_or_else(|| status(&[b"missing required argument for ", keyword.as_bytes()]))
}

fn status(parts: &[&[u8]]) -> CmdError {
    CmdError::Status(text(parts))
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
    let options = Options::parse(cx, rest)?;
    let had_normal = cx.var_normal_only(var).is_some();
    if cx
        .var(var)
        .is_some_and(|v| v.as_slice() != b"NOTFOUND" && !v.ends_with(b"-NOTFOUND"))
    {
        use_existing(cx, kind, var, &options);
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
    let mut search = Search::new(cx, kind, &options);
    let found = search.run(cx, &directories)?;
    search.log(cx, var);
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

fn use_existing(cx: &mut Cx<'_>, kind: Kind, var: &[u8], options: &Options) {
    let new = cx.policy(PolicyId::new(125)) == PolicyStatus::New;
    let normal = cx.var_normal_only(var);
    let cached = cx.cache().get(var).cloned();
    let untyped = cached
        .as_ref()
        .is_some_and(|entry| entry.ty == CacheType::Uninitialized);
    let value = if new {
        normal.or_else(|| cached.as_ref().map(|entry| entry.value.clone()))
    } else {
        cached
            .as_ref()
            .filter(|_| untyped || options.no_cache)
            .map(|entry| entry.value.clone())
            .or(normal)
    };
    let Some(mut value) = value else { return };
    if new && !value.is_empty() && !path::to_native(&value).is_absolute() {
        let relative = cx.current_binary_dir().join(path::to_native(&value));
        if cx.host().metadata(&relative, Follow::Yes).is_ok() {
            value = path::from_native(&normalize(&relative));
        }
    }
    if options.no_cache {
        cx.set_var(var, value);
    } else if let Some(entry) = cx.cache().get_mut(var)
        && untyped
    {
        entry.ty = kind.cache_type();
        entry.help = options.doc.clone().unwrap_or_else(|| kind.help());
        entry.value = value;
    }
}

fn directories(cx: &Cx<'_>, kind: Kind, options: &Options) -> Vec<PathBuf> {
    let variables = kind.paths();
    let mut paths = Vec::new();

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
    paths.extend(options.hints.iter().map(|dir| cx.resolve_path(dir)));
    if use_default(
        cx,
        options.no_defaults || options.no_system_env,
        b"CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
    ) {
        for key in [variables.env, b"PATH"] {
            paths.extend(env_paths(cx, key).iter().map(|dir| cx.resolve_path(dir)));
        }
    }
    if use_default(
        cx,
        options.no_defaults || options.no_system,
        b"CMAKE_FIND_USE_CMAKE_SYSTEM_PATH",
    ) {
        let mut prefixes = if cx.var_defined(b"CMAKE_SYSTEM_PREFIX_PATH") {
            cmake_paths(cx, b"CMAKE_SYSTEM_PREFIX_PATH")
        } else {
            let mut prefixes = vec!["/usr/local".into(), "/usr".into(), "/".into()];
            if cx.host().system().sysname.as_slice() == b"Darwin" {
                prefixes.push("/opt/homebrew".into());
            }
            prefixes
        };
        if use_default(cx, options.no_install, b"CMAKE_FIND_USE_INSTALL_PREFIX") {
            for key in [b"CMAKE_INSTALL_PREFIX".as_slice(), b"CMAKE_STAGING_PREFIX"] {
                let prefix = cx.var_or_empty(key);
                if !prefix.is_empty() {
                    prefixes.push(prefix);
                }
            }
        }
        add_prefixes(cx, &mut paths, prefixes, &variables);
        paths.extend(
            cmake_paths(cx, variables.system)
                .iter()
                .map(|dir| cx.resolve_path(dir)),
        );
    }
    paths.extend(options.paths.iter().map(|dir| cx.resolve_path(dir)));
    let mut paths = with_suffixes(paths, &options.suffixes);
    let ignored: Vec<_> = [b"CMAKE_IGNORE_PATH".as_slice(), b"CMAKE_SYSTEM_IGNORE_PATH"]
        .into_iter()
        .flat_map(|key| cmake_paths(cx, key))
        .map(|dir| normalize(&cx.resolve_path(&dir)))
        .collect();
    let prefixes: Vec<_> = [
        b"CMAKE_IGNORE_PREFIX_PATH".as_slice(),
        b"CMAKE_SYSTEM_IGNORE_PREFIX_PATH",
    ]
    .into_iter()
    .flat_map(|key| cmake_paths(cx, key))
    .map(|dir| normalize(&cx.resolve_path(&dir)))
    .collect();
    paths.retain(|dir| {
        !ignored.contains(dir) && !prefixes.iter().any(|prefix| dir.starts_with(prefix))
    });
    paths
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

struct Search<'a> {
    kind: Kind,
    options: &'a Options,
    decoration: Option<LibraryNames>,
    considered: Option<String>,
    match_path: Option<PathBuf>,
}

impl<'a> Search<'a> {
    fn new(cx: &Cx<'_>, kind: Kind, options: &'a Options) -> Self {
        Self {
            kind,
            options,
            decoration: matches!(kind, Kind::Library).then(|| LibraryNames::new(cx)),
            considered: cx.var_is_on(b"CMAKE_FIND_DEBUG_MODE").then(String::new),
            match_path: None,
        }
    }

    fn run(&mut self, cx: &mut Cx<'_>, dirs: &[PathBuf]) -> Result<Option<PathBuf>, CmdError> {
        let names: Vec<_> = self
            .options
            .names
            .iter()
            .map(|name| path::to_native(name))
            .collect();
        if matches!(self.kind, Kind::File | Kind::Path)
            && cx.host().system().sysname == b"Darwin"
            && !matches!(
                cx.var_or_empty(b"CMAKE_FIND_FRAMEWORK").as_slice(),
                b"NEVER" | b"LAST"
            )
        {
            for root in cmake_paths(cx, b"CMAKE_FRAMEWORK_PATH") {
                let mut children = cx
                    .host()
                    .read_dir(&cx.resolve_path(&root))
                    .unwrap_or_default();
                children.sort_by(|a, b| a.name.cmp(&b.name));
                for entry in children
                    .iter()
                    .filter(|entry| entry.name.ends_with(b".framework"))
                {
                    let framework = cx.resolve_path(&root).join(path::to_native(&entry.name));
                    for headers in ["Headers", "PrivateHeaders"] {
                        for name in &names {
                            if let Some(found) =
                                self.in_directory(cx, &framework.join(headers), name)?
                            {
                                return Ok(Some(found));
                            }
                        }
                    }
                }
            }
            if cx.var_or_empty(b"CMAKE_FIND_FRAMEWORK") == b"ONLY" {
                return Ok(None);
            }
        }
        if self.options.names_per_dir {
            for name in names.iter().filter(|name| name.is_absolute()) {
                if let Some(found) = self.in_directory(cx, Path::new(""), name)? {
                    return Ok(Some(found));
                }
            }
            for dir in dirs {
                for name in names.iter().filter(|name| !name.is_absolute()) {
                    if let Some(found) = self.in_directory(cx, dir, name)? {
                        return Ok(Some(found));
                    }
                }
            }
        } else {
            for name in names {
                let search_dirs = if name.is_absolute() {
                    &[PathBuf::new()][..]
                } else {
                    dirs
                };
                for dir in search_dirs {
                    if let Some(found) = self.in_directory(cx, dir, &name)? {
                        return Ok(Some(found));
                    }
                }
            }
        }
        Ok(None)
    }

    fn in_directory(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
        name: &Path,
    ) -> Result<Option<PathBuf>, CmdError> {
        if name.is_absolute() && !matches!(self.kind, Kind::Program) {
            return Ok(None);
        }
        let direct = normalize(&dir.join(name));
        let alternatives = self
            .decoration
            .iter()
            .flat_map(|library| library.paths(dir, name));
        for path in std::iter::once(direct.clone()).chain(alternatives) {
            if let Some(found) = self.accept(cx, &path, dir)? {
                if self.considered.is_some() {
                    self.match_path = Some(path);
                }
                return Ok(Some(found));
            }
        }
        if self.considered.is_some() {
            let path = self.decoration.as_ref().map_or_else(
                || direct.display().to_string(),
                |library| library.pattern(dir, name),
            );
            self.record(&path);
        }
        Ok(None)
    }

    fn record(&mut self, path: &str) {
        const MAX_TRACE_BYTES: usize = 1024 * 1024;
        if let Some(trace) = &mut self.considered
            && trace.len().saturating_add(path.len()).saturating_add(5) <= MAX_TRACE_BYTES
        {
            let _ = writeln!(trace, "    {path}");
        }
    }

    fn accept(
        &self,
        cx: &mut Cx<'_>,
        path: &Path,
        dir: &Path,
    ) -> Result<Option<PathBuf>, CmdError> {
        if !candidate(cx, self.kind, path) {
            return Ok(None);
        }
        let found = if matches!(self.kind, Kind::Path) {
            dir.to_path_buf()
        } else {
            path.to_path_buf()
        };
        if let Some(callback) = &self.options.validator {
            let mut value = path::from_native(&found);
            if matches!(self.kind, Kind::Path) && !value.ends_with(b"/") {
                value.push(b'/');
            }
            let key = (0..)
                .map(|index| format!("CMAKE_FIND_VALIDATOR_STATUS_{index}"))
                .find(|key| !cx.var_defined(key.as_bytes()))
                .unwrap_or_default();
            let key = key.as_bytes();
            cx.set_var(key, "TRUE".into());
            let args = [BString::from(key), value]
                .into_iter()
                .map(|value| ExpandedArg {
                    value,
                    kind: ArgKind::Bracket,
                    line: cx.current_line(),
                })
                .collect();
            let result = cx.call_command(callback, args);
            let valid = cx.var_is_on(key);
            cx.unset_var(key);
            result?;
            if !valid {
                return Ok(None);
            }
        }
        Ok(Some(found))
    }

    fn log(&self, cx: &Cx<'_>, var: &[u8]) {
        use rcmake_base::ByteSlice as _;

        let Some(trace) = &self.considered else {
            return;
        };
        let command = self.kind.command();
        let options = self.options;
        let file = cx.current_list_file();
        let file_path = path::to_native(&file);
        let base = path::to_native(&cx.var_or_empty(b"CMAKE_SOURCE_DIR"));
        let file = file_path.strip_prefix(&base).unwrap_or(&file_path);
        let mut log = format!(
            "CMake Debug Log at {}:{} ({command}):\n  {command} called with the following settings:\n\n    VAR: {}\n",
            file.display(),
            cx.current_line(),
            var.to_str_lossy(),
        );
        for (index, name) in options.names.iter().enumerate() {
            let label = if index == 0 { "NAMES:" } else { "      " };
            let _ = writeln!(log, "    {label} \"{}\"", name.to_str_lossy());
        }
        let doc = options.doc.clone().unwrap_or_else(|| self.kind.help());
        let _ = writeln!(log, "    Documentation: {}", doc.to_str_lossy());
        for (label, key) in [
            ("Framework", b"CMAKE_FIND_FRAMEWORK".as_slice()),
            ("AppBundle", b"CMAKE_FIND_APPBUNDLE"),
        ] {
            let mode = cx.var_or_empty(key);
            let first =
                mode == b"FIRST" || mode.is_empty() && cx.host().system().sysname == b"Darwin";
            let item = if label == "Framework" {
                "Frameworks"
            } else {
                label
            };
            let _ = writeln!(
                log,
                "    {label}\n      Only Search {item}: {}\n      Search {item} Last: {}\n      Search {item} First: {}",
                u8::from(mode == b"ONLY"),
                u8::from(mode == b"LAST"),
                u8::from(first)
            );
        }
        for (name, disabled) in [
            ("CMAKE_FIND_USE_CMAKE_PATH", options.no_cmake),
            (
                "CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH",
                options.no_cmake_env,
            ),
            (
                "CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
                options.no_system_env,
            ),
            ("CMAKE_FIND_USE_CMAKE_SYSTEM_PATH", options.no_system),
            ("CMAKE_FIND_USE_INSTALL_PREFIX", options.no_install),
        ] {
            let enabled = use_default(cx, options.no_defaults || disabled, name.as_bytes());
            let _ = writeln!(log, "    {name}: {}", u8::from(enabled));
        }
        let _ = writeln!(log, "\n  {command} considered the following locations:\n");
        log.push_str(trace);
        if let Some(found) = &self.match_path {
            let _ = writeln!(log, "\n  The item was found at\n\n    {}", found.display());
        } else {
            log.push_str("\n  The item was not found.\n");
        }
        log.push('\n');
        let backtrace = cx.backtrace();
        let mut callers = backtrace
            .frames()
            .skip(1)
            .filter(|frame| frame.kind != FrameKind::ModuleInclude)
            .peekable();
        if callers.peek().is_some() {
            log.push_str("Call Stack (most recent call first):\n");
            for frame in callers {
                let file = path::to_native(&frame.file);
                let display = file.strip_prefix(&base).unwrap_or(&file);
                let _ = write!(log, "  {}:{}", display.display(), frame.line);
                if let Some(name) = &frame.name {
                    let _ = write!(log, " ({})", name.to_str_lossy());
                }
                log.push('\n');
            }
        }
        log.push_str("\n\n");
        cx.host()
            .write_out(rcmake_host::Stream::Stderr, log.as_bytes());
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

    fn pattern(&self, dir: &Path, name: &Path) -> String {
        use rcmake_base::ByteSlice as _;

        let dir = dir.join(name.parent().unwrap_or_else(|| Path::new("")));
        let file = name.file_name().unwrap_or_default().to_string_lossy();
        let prefixes: Vec<_> = self
            .prefixes
            .iter()
            .map(|prefix| regex_literal(&prefix.to_str_lossy()))
            .collect();
        let suffixes: Vec<_> = self
            .suffixes
            .iter()
            .map(|suffix| regex_literal(&suffix.to_str_lossy()))
            .collect();
        let prefix = if let [one] = prefixes.as_slice() {
            one.clone()
        } else {
            format!("({})", prefixes.join("|"))
        };
        format!(
            "{}//{prefix}{}({})",
            dir.display(),
            regex_literal(&file),
            suffixes.join("|")
        )
    }

    fn paths(&self, dir: &Path, name: &Path) -> impl Iterator<Item = PathBuf> {
        let file = name
            .file_name()
            .map(|file| path::from_native(Path::new(file)));
        let dir = dir.join(name.parent().unwrap_or_else(|| Path::new("")));
        self.prefixes.iter().flat_map(move |prefix| {
            let file = file.clone();
            let dir = dir.clone();
            self.suffixes.iter().filter_map(move |suffix| {
                let filename = text(&[prefix, file.as_ref()?, suffix]);
                Some(normalize(&dir.join(path::to_native(&filename))))
            })
        })
    }
}

fn regex_literal(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        if ".^$[]()|*+?\\".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
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

fn candidate(cx: &Cx<'_>, kind: Kind, candidate: &Path) -> bool {
    cx.host()
        .metadata(candidate, Follow::Yes)
        .is_ok_and(|metadata| {
            metadata.kind == FileKind::File
                && (!matches!(kind, Kind::Program) || cfg!(windows) || metadata.mode & 0o111 != 0)
        })
}
