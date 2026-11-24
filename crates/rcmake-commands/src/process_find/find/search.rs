use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, list};
use rcmake_diag::{FrameKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, Cx};
use rcmake_expr::ExpandedArg;
use rcmake_host::{CommandSpec, FileKind, Follow, Input, Output, ProcessResult, RunRequest, path};
use rcmake_syntax::ArgKind;

use super::super::search_paths::{cmake_paths, file_metadata};
use super::super::text;
use super::{Kind, Options, normalize, use_default};

struct IgnoredPaths {
    directories: BTreeSet<PathBuf>,
    prefixes: Vec<PathBuf>,
}

impl IgnoredPaths {
    fn new(cx: &Cx<'_>) -> Self {
        let paths = |keys: [&'static [u8]; 2]| {
            keys.into_iter()
                .flat_map(move |key| cmake_paths(cx, key))
                .map(move |dir| normalize(&cx.resolve_path(&dir)))
        };
        Self {
            directories: paths([b"CMAKE_IGNORE_PATH", b"CMAKE_SYSTEM_IGNORE_PATH"]).collect(),
            prefixes: paths([
                b"CMAKE_IGNORE_PREFIX_PATH",
                b"CMAKE_SYSTEM_IGNORE_PREFIX_PATH",
            ])
            .collect(),
        }
    }

    fn ignores_directory(&self, path: &Path) -> bool {
        self.directories.contains(path) || self.ignores_prefix(path)
    }

    fn ignores_prefix(&self, path: &Path) -> bool {
        self.prefixes.iter().any(|prefix| path.starts_with(prefix))
    }
}

pub(super) struct Search<'a> {
    kind: Kind,
    options: &'a Options,
    decoration: Option<LibraryNames>,
    ignored: IgnoredPaths,
    considered: Option<String>,
    match_path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum SearchPass {
    Regular,
    Framework,
    AppBundle,
}

#[derive(Clone, Copy)]
enum FrameworkOrder {
    First,
    Last,
    Only,
    Never,
}

impl FrameworkOrder {
    fn new(cx: &Cx<'_>) -> Self {
        match cx.var_or_empty(b"CMAKE_FIND_FRAMEWORK").as_slice() {
            b"FIRST" => Self::First,
            b"LAST" => Self::Last,
            b"ONLY" => Self::Only,
            b"NEVER" => Self::Never,
            _ if cx.host().system().sysname == b"Darwin" => Self::First,
            _ => Self::Never,
        }
    }

    fn passes(self) -> &'static [SearchPass] {
        use SearchPass::{Framework, Regular};

        match self {
            Self::First => &[Framework, Regular],
            Self::Last => &[Regular, Framework],
            Self::Only => &[Framework],
            Self::Never => &[Regular],
        }
    }
}

impl<'a> Search<'a> {
    pub(super) fn new(cx: &Cx<'_>, kind: Kind, options: &'a Options, var: &[u8]) -> Self {
        Self {
            kind,
            options,
            decoration: matches!(kind, Kind::Library).then(|| LibraryNames::new(cx)),
            ignored: IgnoredPaths::new(cx),
            considered: cx.find_variable_debug(var).then(String::new),
            match_path: None,
        }
    }

    pub(super) fn run(
        &mut self,
        cx: &mut Cx<'_>,
        mut dirs: Vec<PathBuf>,
    ) -> Result<Option<PathBuf>, CmdError> {
        dirs.retain(|dir| !self.ignored.ignores_directory(dir));
        let mut dirs = self
            .options
            .root_mode
            .resolve(cx, self.kind.root_mode_setting())
            .reroot(cx, dirs);
        let mut seen = BTreeSet::new();
        dirs.retain(|dir| seen.insert(dir.clone()));
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
                        let dir = normalize(&framework.join(headers));
                        if self.ignored.ignores_directory(&dir) {
                            continue;
                        }
                        for name in &names {
                            if let Some(found) = self.in_directory(cx, &dir, name)? {
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
        let passes = match self.kind {
            Kind::Library => FrameworkOrder::new(cx).passes(),
            Kind::Program if cx.host().system().sysname == b"Darwin" => {
                use SearchPass::{AppBundle, Regular};
                match cx.var_or_empty(b"CMAKE_FIND_APPBUNDLE").as_slice() {
                    b"NEVER" => &[Regular][..],
                    b"ONLY" => &[AppBundle],
                    b"LAST" => &[Regular, AppBundle],
                    _ => &[AppBundle, Regular],
                }
            }
            _ => &[SearchPass::Regular],
        };
        for &pass in passes {
            if let Some(found) = self.scan(cx, &dirs, &names, pass)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    fn scan(
        &mut self,
        cx: &mut Cx<'_>,
        dirs: &[PathBuf],
        names: &[PathBuf],
        pass: SearchPass,
    ) -> Result<Option<PathBuf>, CmdError> {
        let search = match pass {
            SearchPass::Regular => Self::in_directory,
            SearchPass::Framework => Self::framework,
            SearchPass::AppBundle => Self::app_bundle,
        };
        if self.options.names_per_dir && !matches!(pass, SearchPass::AppBundle) {
            for name in names.iter().filter(|name| name.is_absolute()) {
                if let Some(found) = search(self, cx, Path::new(""), name)? {
                    return Ok(Some(found));
                }
            }
            for dir in dirs {
                for name in names.iter().filter(|name| !name.is_absolute()) {
                    if let Some(found) = search(self, cx, dir, name)? {
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
                    if let Some(found) = search(self, cx, dir, name)? {
                        return Ok(Some(found));
                    }
                }
            }
        }
        Ok(None)
    }

    fn framework(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
        name: &Path,
    ) -> Result<Option<PathBuf>, CmdError> {
        if name.is_absolute() {
            return Ok(None);
        }
        let name = path::from_native(name);
        for extension in [b".xcframework".as_slice(), b".framework"] {
            let path = normalize(&dir.join(path::to_native(&text(&[&name, extension]))));
            if self.ignored.ignores_prefix(&path)
                || !cx
                    .host()
                    .metadata(&path, Follow::Yes)
                    .is_ok_and(|metadata| metadata.kind == FileKind::Dir)
            {
                continue;
            }
            let path = if cx.host().system().sysname == b"Darwin" {
                disk_spelling(cx, &path)
            } else {
                path
            };
            if self.validate(cx, &path, false)? {
                self.match_path = Some(path.clone());
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn app_bundle(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
        name: &Path,
    ) -> Result<Option<PathBuf>, CmdError> {
        let Some(stem) = name.file_name() else {
            return Ok(None);
        };
        let mut bundle_name = name.as_os_str().to_os_string();
        bundle_name.push(".app");
        let bundle = dir.join(bundle_name);
        let executable = bundle_executable(cx, &bundle, stem);
        if !self.ignored.ignores_prefix(&executable) && candidate(cx, Kind::File, &executable) {
            self.match_path = Some(executable.clone());
            return Ok(Some(executable));
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
        let try_direct = self
            .decoration
            .as_ref()
            .is_none_or(|library| library.has_suffix(name));
        let alternatives = self
            .decoration
            .iter()
            .flat_map(|library| library.paths(dir, name));
        for path in try_direct
            .then(|| direct.clone())
            .into_iter()
            .chain(alternatives)
        {
            if let Some(found) = self.accept(cx, &path, dir)? {
                if self.considered.is_some() {
                    self.match_path = Some(path);
                }
                return Ok(Some(found));
            }
        }
        if self.considered.is_some() && !self.ignored.ignores_prefix(&direct) {
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
        if self.ignored.ignores_prefix(path) || !candidate(cx, self.kind, path) {
            return Ok(None);
        }
        let found = if matches!(self.kind, Kind::Path) {
            dir.to_path_buf()
        } else {
            path.to_path_buf()
        };
        if self.validate(cx, &found, matches!(self.kind, Kind::Path))? {
            Ok(Some(found))
        } else {
            Ok(None)
        }
    }

    fn validate(
        &self,
        cx: &mut Cx<'_>,
        found: &Path,
        trailing_slash: bool,
    ) -> Result<bool, CmdError> {
        if let Some(callback) = &self.options.validator {
            let mut value = path::from_native(found);
            if trailing_slash && !value.ends_with(b"/") {
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
            return Ok(valid);
        }
        Ok(true)
    }

    pub(super) fn log(&self, cx: &Cx<'_>, var: &[u8]) {
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
            let first = if label == "Framework" && matches!(self.kind, Kind::Library) {
                matches!(FrameworkOrder::new(cx), FrameworkOrder::First)
            } else {
                mode == b"FIRST" || mode.is_empty() && cx.host().system().sysname == b"Darwin"
            };
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

fn disk_spelling(cx: &Cx<'_>, path: &Path) -> PathBuf {
    // Keep symbolic links in the path and read the spelling of each component.
    let mut result = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            result.push(component);
            continue;
        };
        let requested = path::from_native(Path::new(name));
        let entries = cx.host().read_dir(&result).unwrap_or_default();
        let mut matches = entries
            .iter()
            .filter(|entry| entry.name.eq_ignore_ascii_case(&requested));
        let first = matches.next();
        let exact = matches.find(|entry| entry.name == requested);
        if let Some(entry) = exact.or(first) {
            result.push(path::to_native(&entry.name));
        } else {
            result.push(name);
        }
    }
    result
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

    fn has_suffix(&self, name: &Path) -> bool {
        name.file_name().is_some_and(|file| {
            let file = path::from_native(Path::new(file));
            self.suffixes
                .iter()
                .map(|suffix| suffix.as_slice())
                .chain([b".a".as_slice(), b".so", b".dylib", b".tbd", b".lib"])
                .any(|suffix| file.len() > suffix.len() && file.ends_with(suffix))
        })
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

fn bundle_executable(cx: &Cx<'_>, bundle: &Path, stem: &std::ffi::OsStr) -> PathBuf {
    let contents = bundle.join("Contents");
    let plist = contents.join("Info.plist");
    let host = cx.host();
    let executable = if host
        .metadata(&plist, Follow::Yes)
        .is_ok_and(|info| info.kind == FileKind::File)
    {
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/usr/bin/plutil".into(),
                args: vec![
                    "-extract".into(),
                    "CFBundleExecutable".into(),
                    "raw".into(),
                    "-o".into(),
                    "-".into(),
                    path::from_native(&plist),
                ],
            }],
            cwd: cx.current_binary_dir(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Null,
            timeout: Some(Duration::from_secs(2)),
            tee: false,
        };
        host.run(&request, &mut |_, _| {})
            .ok()
            .filter(|run| run.results == [ProcessResult::Exited(0)])
            .map(|run| path::to_native(run.stdout.strip_suffix(b"\n").unwrap_or(&run.stdout)))
            .filter(|name| {
                matches!(
                    name.components().collect::<Vec<_>>().as_slice(),
                    [Component::Normal(_)]
                )
            })
    } else {
        None
    };
    normalize(
        &contents
            .join("MacOS")
            .join(executable.as_deref().unwrap_or_else(|| Path::new(stem))),
    )
}

fn candidate(cx: &mut Cx<'_>, kind: Kind, path: &Path) -> bool {
    let Some(metadata) = file_metadata(cx, path) else {
        return false;
    };
    if !matches!(kind, Kind::Program) || cfg!(windows) {
        return true;
    }

    let executable = metadata.mode & 0o111 != 0;
    let policy = PolicyId::new(109);
    let state = cx.policy(policy);
    if state == PolicyStatus::New {
        return executable;
    }
    let readable = metadata.mode & 0o444 != 0
        && cx
            .host()
            .read(
                path,
                rcmake_host::ReadRange::Slice {
                    offset: 0,
                    len: Some(0),
                },
            )
            .is_ok();
    if state == PolicyStatus::Warn && readable != executable {
        let reason = if readable {
            "is readable but not executable.  CMake is using it for compatibility."
        } else {
            "is executable but not readable.  CMake is ignoring it for compatibility."
        };
        let body = format!("The file\n  {}\n{reason}", path.display());
        cx.issue_policy_warning(policy, b"", body.as_bytes());
    }
    readable
}
