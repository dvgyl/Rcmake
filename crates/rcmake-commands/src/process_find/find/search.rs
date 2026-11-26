use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rcmake_base::{BString, ByteSlice, list};
use rcmake_diag::{FrameKind, PolicyId, PolicyStatus};
use rcmake_eval::{CmdError, Cx, FindEvent, HermeticMode, RunMode, hermetic::HermeticContext};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, ReadRange, RunRequest, path,
};
use rcmake_syntax::ArgKind;

use super::super::RootMode;
use super::super::search_paths::{cmake_paths, file_metadata};
use super::{Kind, Options, join_normalized, join_preserving_parent, normalize_owned, use_default};

struct IgnoredPaths {
    directories: HashSet<PathBuf>,
    prefixes: Vec<PathBuf>,
}

impl IgnoredPaths {
    fn new(cx: &Cx<'_>) -> Self {
        let paths = |keys: [&'static [u8]; 2]| {
            keys.into_iter()
                .flat_map(move |key| cmake_paths(cx, key))
                .map(move |dir| normalize_owned(cx.resolve_path(&dir)))
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

    fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.prefixes.is_empty()
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
    is_darwin: bool,
    hermetic: bool,
    decoration: Option<LibraryNames>,
    ignored: IgnoredPaths,
    considered: Option<String>,
    match_path: Option<PathBuf>,
    event: Option<SearchEvent>,
}

#[derive(Default)]
struct SearchEvent {
    candidates: Vec<BString>,
    attempted: Vec<BString>,
    bytes: usize,
}

#[derive(Clone, Copy)]
enum SearchPass {
    Regular,
    Framework,
    AppBundle,
}

enum SearchDirectory {
    Unlisted,
    Existing,
    Unknown,
    Missing,
    Listed {
        names: Option<HashSet<Box<[u8]>>>,
        frameworks: Vec<PathBuf>,
        has_framework: bool,
    },
}

impl SearchDirectory {
    fn read(host: &dyn Host, root: &Path, build_name_set: bool) -> Self {
        match host.read_dir(root) {
            Ok(entries) => {
                let mut framework_entries = Vec::new();
                let mut has_framework = false;
                let mut ascii = build_name_set && !cfg!(windows) && !unlisted_filesystem(root);
                for entry in &entries {
                    if entry.kind != FileKind::File {
                        if entry.name.ends_with(b".framework") {
                            framework_entries.push(entry.name.clone());
                            has_framework = true;
                        } else if ends_with_ignore_ascii_case(&entry.name, b".framework") {
                            has_framework = true;
                        }
                    }
                    if ascii && !entry.name.is_ascii() {
                        ascii = false;
                    }
                }
                framework_entries.sort_unstable();
                let frameworks = framework_entries
                    .into_iter()
                    .map(|name| root.join(path::to_native(&name)).join("Headers"))
                    .collect();
                let names = ascii.then(|| {
                    let mut set = HashSet::with_capacity(entries.len().min(4096));
                    for entry in entries {
                        let mut bytes: Vec<u8> = entry.name.into();
                        bytes.make_ascii_lowercase();
                        set.insert(bytes.into_boxed_slice());
                    }
                    set
                });
                Self::Listed {
                    names,
                    frameworks,
                    has_framework,
                }
            }
            Err(error) if missing_directory(error.kind) => Self::Missing,
            Err(_) => Self::Unknown,
        }
    }

    fn may_contain(&self, root: &Path, candidate: &Path) -> bool {
        let Some(name) = candidate.strip_prefix(root).ok().and_then(single_component) else {
            return true;
        };
        self.may_contain_leaf(name.as_encoded_bytes())
    }

    fn may_contain_framework(&self, root: &Path, candidate: &Path) -> bool {
        let Some(name) = candidate.strip_prefix(root).ok().and_then(single_component) else {
            return true;
        };
        self.may_contain_framework_leaf(name.as_encoded_bytes())
    }

    fn may_contain_framework_leaf(&self, name: &[u8]) -> bool {
        if matches!(
            self,
            Self::Listed {
                has_framework: false,
                ..
            }
        ) && ends_with_ignore_ascii_case(name, b".framework")
        {
            return false;
        }
        self.may_contain_leaf(name)
    }

    fn may_contain_leaf(&self, name: &[u8]) -> bool {
        match self {
            Self::Missing => false,
            // The filesystem can fold or normalize non-ASCII names. On Windows a short
            // DOS name can also resolve to a different name in the directory listing.
            Self::Listed { names, .. } => {
                let Some(names) = names else {
                    return true;
                };
                if !name.is_ascii() {
                    return true;
                }
                if name.iter().any(u8::is_ascii_uppercase) {
                    names.contains(name.to_ascii_lowercase().as_slice())
                } else {
                    names.contains(name)
                }
            }
            _ => true,
        }
    }
}

fn unlisted_filesystem(path: &Path) -> bool {
    ["/proc", "/dev", "/sys", "/net"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn ends_with_ignore_ascii_case(haystack: &[u8], suffix: &[u8]) -> bool {
    haystack
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| haystack.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

fn single_component(path: &Path) -> Option<&std::ffi::OsStr> {
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => Some(name),
        _ => None,
    }
}

fn is_relative_descendant(name: &Path) -> bool {
    let mut has_normal = false;
    for component in name.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    has_normal
}

fn missing_directory(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::NotFound | ErrorKind::NotADirectory)
}

fn hermetic_active(cx: &Cx<'_>) -> bool {
    cx.services().speed.hermetic != HermeticMode::Off
        || cx.session_data::<HermeticContext>().is_some()
}

fn has_find_roots(cx: &Cx<'_>) -> bool {
    [
        b"CMAKE_FIND_ROOT_PATH".as_slice(),
        b"CMAKE_SYSROOT_COMPILE",
        b"CMAKE_SYSROOT_LINK",
        b"CMAKE_SYSROOT",
    ]
    .into_iter()
    .any(|key| !cx.var_or_empty(key).is_empty())
}

#[derive(Clone, Copy)]
enum FrameworkOrder {
    First,
    Last,
    Only,
    Never,
}

impl FrameworkOrder {
    fn new(cx: &Cx<'_>, is_darwin: bool) -> Self {
        match cx.var_or_empty(b"CMAKE_FIND_FRAMEWORK").as_slice() {
            b"FIRST" => Self::First,
            b"LAST" => Self::Last,
            b"ONLY" => Self::Only,
            b"NEVER" => Self::Never,
            _ if is_darwin => Self::First,
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
        let is_darwin = cx.host().system().sysname == b"Darwin";
        let hermetic = hermetic_active(cx);
        let debug = cx.find_variable_debug(var)
            || cx
                .var_normal_only(b"CMAKE_FIND_PACKAGE_NAME")
                .is_some_and(|package| cx.find_package_debug(&package));
        Self {
            kind,
            options,
            is_darwin,
            hermetic,
            decoration: matches!(kind, Kind::Library).then(|| LibraryNames::new(cx, is_darwin)),
            ignored: IgnoredPaths::new(cx),
            considered: debug.then(String::new),
            match_path: None,
            event: (cx.mode() == RunMode::Configure
                && (debug || !cx.var_is_on(b"CMAKE_FIND_DEBUG_MODE_NO_IMPLICIT_CONFIGURE_LOG")))
            .then(SearchEvent::default),
        }
    }

    pub(super) fn run(
        &mut self,
        cx: &mut Cx<'_>,
        mut dirs: Vec<PathBuf>,
    ) -> Result<Option<PathBuf>, CmdError> {
        if !self.ignored.is_empty() {
            dirs.retain(|dir| !self.ignored.ignores_directory(dir));
        }
        let root_mode = self
            .options
            .root_mode
            .resolve(cx, self.kind.root_mode_setting());
        let rerooted = !matches!(root_mode, RootMode::Never) && has_find_roots(cx);
        let mut dirs = if rerooted {
            root_mode.reroot(cx, dirs)
        } else {
            dirs
        };
        let arch_rewritten = if matches!(self.kind, Kind::Library) {
            let (rewritten, changed) = super::architecture::rewrite(cx, dirs)?;
            dirs = rewritten;
            changed
        } else {
            false
        };
        if rerooted || arch_rewritten {
            let mut seen = HashSet::with_capacity(dirs.len().min(4096));
            dirs.retain(|dir| {
                if seen.contains(dir) {
                    false
                } else {
                    seen.insert(dir.clone());
                    true
                }
            });
        }
        if let Some(event) = &mut self.event {
            event.candidates.reserve(dirs.len());
            event
                .candidates
                .extend(dirs.iter().map(|dir| directory_path(dir)));
        }
        let names: Vec<_> = self
            .options
            .names
            .iter()
            .map(|name| path::to_native(name))
            .collect();
        let passes = match self.kind {
            Kind::Library | Kind::File | Kind::Path => {
                FrameworkOrder::new(cx, self.is_darwin).passes()
            }
            Kind::Program if self.is_darwin => {
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
        let mut directories = Vec::with_capacity(dirs.len());
        directories.resize_with(dirs.len(), || {
            if arch_rewritten {
                SearchDirectory::Existing
            } else {
                SearchDirectory::Unlisted
            }
        });
        for &pass in passes {
            if let Some(found) =
                self.scan(cx, &dirs, &names, pass, passes.len(), &mut directories)?
            {
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
        passes_len: usize,
        directories: &mut [SearchDirectory],
    ) -> Result<Option<PathBuf>, CmdError> {
        let search = match pass {
            SearchPass::Regular => Self::in_directory,
            SearchPass::Framework if matches!(self.kind, Kind::File | Kind::Path) => {
                Self::framework_header
            }
            SearchPass::Framework => Self::framework,
            SearchPass::AppBundle => Self::app_bundle,
        };
        let speculative = passes_len > 1;
        let precheck_dir = !self.hermetic
            && self.options.validator.is_none()
            && (names.len() > 1 || passes_len > 1 || matches!(self.kind, Kind::Library));
        let list_frameworks =
            matches!(pass, SearchPass::Framework) && matches!(self.kind, Kind::File | Kind::Path);
        let build_name_set = list_frameworks && names.len() > 2 && passes_len > 1;
        let unknown = SearchDirectory::Unknown;
        let mut probe =
            |this: &mut Self, cx: &mut Cx<'_>, dir_idx: Option<usize>, dir: &Path, name: &Path| {
                // A validator can change the filesystem before the next candidate.
                // Hermetic mode can permit an exact candidate without granting its parent directory.
                let directory = if this.hermetic
                    || this.options.validator.is_some()
                    || !dir.is_absolute()
                {
                    &unknown
                } else if let Some(slot) = dir_idx.and_then(|idx| directories.get_mut(idx)) {
                    if list_frameworks {
                        if matches!(slot, SearchDirectory::Unlisted | SearchDirectory::Existing) {
                            *slot = SearchDirectory::read(cx.host(), dir, build_name_set);
                        }
                    } else if precheck_dir && matches!(slot, SearchDirectory::Unlisted) {
                        *slot = match cx.host().metadata(dir, Follow::Yes) {
                            Ok(meta) if meta.kind == FileKind::Dir => SearchDirectory::Existing,
                            Ok(_) => SearchDirectory::Missing,
                            Err(error) if missing_directory(error.kind) => SearchDirectory::Missing,
                            Err(_) => SearchDirectory::Unknown,
                        };
                    }
                    &*slot
                } else {
                    &unknown
                };
                search(this, cx, dir, name, directory, speculative)
            };
        if self.options.names_per_dir
            && matches!(self.kind, Kind::Library | Kind::Program)
            && !matches!(pass, SearchPass::AppBundle)
        {
            for name in names.iter().filter(|name| name.is_absolute()) {
                if let Some(found) = probe(self, cx, None, Path::new(""), name)? {
                    return Ok(Some(found));
                }
            }
            for (dir_idx, dir) in dirs.iter().enumerate() {
                for name in names.iter().filter(|name| !name.is_absolute()) {
                    if let Some(found) = probe(self, cx, Some(dir_idx), dir, name)? {
                        return Ok(Some(found));
                    }
                }
            }
        } else {
            for name in names {
                if name.is_absolute() {
                    if let Some(found) = probe(self, cx, None, Path::new(""), name)? {
                        return Ok(Some(found));
                    }
                } else {
                    for (dir_idx, dir) in dirs.iter().enumerate() {
                        if let Some(found) = probe(self, cx, Some(dir_idx), dir, name)? {
                            return Ok(Some(found));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn framework_header(
        &mut self,
        cx: &mut Cx<'_>,
        root: &Path,
        name: &Path,
        directory: &SearchDirectory,
        speculative: bool,
    ) -> Result<Option<PathBuf>, CmdError> {
        if name.is_absolute() {
            return Ok(None);
        }
        let name_text = path::from_native_cmake(name);
        if let Some((framework, header)) = name_text.split_once_str(b"/") {
            let framework_name: BString = [framework, b".framework"].concat().into();
            let framework = root.join(path::to_native(&framework_name));
            let path = framework.join("Headers").join(path::to_native(header));
            let found = if matches!(self.kind, Kind::Path) {
                &framework
            } else {
                &path
            };
            let allowed_probe = !(speculative
                && self.hermetic
                && !crate::hermetic::should_probe_speculative_path(cx, &path));
            if allowed_probe
                && !self.ignored.ignores_prefix(&path)
                && directory.may_contain_framework(root, &framework)
                && candidate(cx, Kind::File, &path)
                && self.validate(cx, found, false)?
            {
                self.match_path = Some(path.clone());
                return Ok(Some(found.clone()));
            }
            let attempt = path::from_native_cmake(&path);
            self.record(&attempt.to_string());
            if let Some(event) = &mut self.event {
                event.bytes = event.bytes.saturating_add(attempt.len() + 8);
                if event.bytes <= 1024 * 1024 {
                    event.attempted.push(attempt);
                }
            }
        }

        let fallback;
        let headers = match directory {
            SearchDirectory::Listed { frameworks, .. } => frameworks,
            SearchDirectory::Missing => return Ok(None),
            SearchDirectory::Unknown | SearchDirectory::Unlisted | SearchDirectory::Existing => {
                if speculative
                    && self.hermetic
                    && !crate::hermetic::should_probe_speculative_path(cx, root)
                {
                    return Ok(None);
                }
                fallback = match SearchDirectory::read(cx.host(), root, false) {
                    SearchDirectory::Listed { frameworks, .. } => frameworks,
                    _ => return Ok(None),
                };
                &fallback
            }
        };
        for headers in headers {
            let path = headers.join(name);
            if speculative
                && self.hermetic
                && !crate::hermetic::should_probe_speculative_path(cx, &path)
            {
                continue;
            }
            if !self.ignored.ignores_prefix(&path) && candidate(cx, Kind::File, &path) {
                self.match_path = Some(path.clone());
                return Ok(Some(if matches!(self.kind, Kind::Path) {
                    headers.clone()
                } else {
                    path
                }));
            }
        }
        Ok(None)
    }

    fn framework(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
        name: &Path,
        directory: &SearchDirectory,
        speculative: bool,
    ) -> Result<Option<PathBuf>, CmdError> {
        if name.is_absolute()
            || (matches!(directory, SearchDirectory::Missing) && is_relative_descendant(name))
        {
            return Ok(None);
        }
        let name_bytes = path::from_native(name);
        let direct_child = single_component(name).is_some();
        let mut buf = Vec::with_capacity(name_bytes.len() + b".xcframework".len());
        for extension in [b".xcframework".as_slice(), b".framework"] {
            buf.clear();
            buf.extend_from_slice(&name_bytes);
            buf.extend_from_slice(extension);
            if direct_child && !directory.may_contain_framework_leaf(&buf) {
                continue;
            }
            let path = join_preserving_parent(dir, &path::to_native(&buf));
            if self.ignored.ignores_prefix(&path)
                || (!direct_child && !directory.may_contain_framework(dir, &path))
                || (speculative
                    && self.hermetic
                    && !crate::hermetic::should_probe_speculative_path(cx, &path))
                || !cx
                    .host()
                    .metadata(&path, Follow::Yes)
                    .is_ok_and(|metadata| metadata.kind == FileKind::Dir)
            {
                continue;
            }
            let path = if self.is_darwin {
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
        directory: &SearchDirectory,
        speculative: bool,
    ) -> Result<Option<PathBuf>, CmdError> {
        if matches!(directory, SearchDirectory::Missing) && is_relative_descendant(name) {
            return Ok(None);
        }
        let Some(stem) = name.file_name() else {
            return Ok(None);
        };
        let mut bundle_name = name.as_os_str().to_os_string();
        bundle_name.push(".app");
        let raw_bundle = join_preserving_parent(dir, Path::new(&bundle_name));
        if !directory.may_contain(dir, &raw_bundle) {
            return Ok(None);
        }
        let probe_bundle =
            !self.hermetic || crate::hermetic::should_probe_speculative_path(cx, &raw_bundle);
        if probe_bundle
            && !cx
                .host()
                .metadata(&raw_bundle, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            return Ok(None);
        }
        let raw_executable = bundle_executable(cx, &raw_bundle, stem, self.hermetic);
        if speculative
            && self.hermetic
            && !crate::hermetic::should_probe_speculative_path(cx, &raw_executable)
        {
            return Ok(None);
        }
        if !self.ignored.ignores_prefix(&raw_executable)
            && file_metadata(cx, &raw_executable).is_some()
        {
            let found = if raw_bundle
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                let normalized = normalize_owned(raw_executable.clone());
                let can_probe_normalized = !self.hermetic
                    || crate::hermetic::should_probe_speculative_path(cx, &normalized);
                if can_probe_normalized
                    && cx
                        .host()
                        .canonicalize(&raw_executable)
                        .is_ok_and(|raw| cx.host().canonicalize(&normalized).as_ref() == Ok(&raw))
                {
                    normalized
                } else {
                    raw_executable
                }
            } else {
                normalize_owned(raw_executable)
            };
            self.match_path = Some(found.clone());
            return Ok(Some(found));
        }
        Ok(None)
    }

    fn in_directory(
        &mut self,
        cx: &mut Cx<'_>,
        dir: &Path,
        name: &Path,
        directory: &SearchDirectory,
        _speculative: bool,
    ) -> Result<Option<PathBuf>, CmdError> {
        if name.is_absolute() && !matches!(self.kind, Kind::Program) {
            return Ok(None);
        }
        let missing_descendant =
            matches!(directory, SearchDirectory::Missing) && is_relative_descendant(name);
        let try_direct = !missing_descendant
            && self
                .decoration
                .as_ref()
                .is_none_or(|library| library.has_suffix(name));
        let windows_program =
            !missing_descendant && cfg!(windows) && matches!(self.kind, Kind::Program);
        let record_direct = self.considered.is_some()
            || (self.event.is_some()
                && (!matches!(self.kind, Kind::Library) || !self.ignored.prefixes.is_empty()));
        let direct = (try_direct || windows_program || record_direct).then(|| {
            if matches!(self.kind, Kind::Path | Kind::File) {
                join_preserving_parent(dir, name)
            } else {
                join_normalized(dir, name)
            }
        });

        if !missing_descendant {
            if windows_program && let Some(direct) = &direct {
                let raw = name.as_os_str().as_encoded_bytes();
                for suffix in [".com", ".exe"] {
                    if !raw.ends_with(suffix.as_bytes()) {
                        let mut candidate_path = direct.as_os_str().to_os_string();
                        candidate_path.push(suffix);
                        let candidate_path = PathBuf::from(candidate_path);
                        if directory.may_contain(dir, &candidate_path)
                            && let Some(found) = self.accept(cx, &candidate_path, dir)?
                        {
                            if self.considered.is_some() {
                                self.match_path = Some(candidate_path);
                            }
                            return Ok(Some(found));
                        }
                    }
                }
            }
            if try_direct
                && let Some(direct) = &direct
                && directory.may_contain(dir, direct)
                && let Some(found) = self.accept(cx, direct, dir)?
            {
                if self.considered.is_some() {
                    self.match_path = Some(direct.clone());
                }
                return Ok(Some(found));
            }
            if let Some((matched, found)) = self.try_library_variants(cx, dir, name, directory)? {
                if self.considered.is_some() {
                    self.match_path = Some(matched);
                }
                return Ok(Some(found));
            }
        }

        let not_ignored = self.ignored.prefixes.is_empty()
            || direct
                .as_deref()
                .is_none_or(|path| !self.ignored.ignores_prefix(path));
        if not_ignored {
            if let Some(event) = &mut self.event {
                let attempt = if matches!(self.kind, Kind::Library) {
                    directory_path(dir)
                } else if let Some(direct) = &direct {
                    path::from_native_cmake(direct)
                } else {
                    directory_path(dir)
                };
                event.bytes = event.bytes.saturating_add(attempt.len() + 8);
                if event.bytes <= 1024 * 1024 {
                    event.attempted.push(attempt);
                }
            }
            if self.considered.is_some() {
                let path = self.decoration.as_ref().map_or_else(
                    || {
                        direct
                            .as_deref()
                            .map_or_else(String::new, |p| path::from_native_cmake(p).to_string())
                    },
                    |library| library.pattern(dir, name),
                );
                self.record(&path);
            }
        }
        Ok(None)
    }

    fn try_library_variants(
        &self,
        cx: &mut Cx<'_>,
        dir: &Path,
        name: &Path,
        directory: &SearchDirectory,
    ) -> Result<Option<(PathBuf, PathBuf)>, CmdError> {
        let Some(library) = &self.decoration else {
            return Ok(None);
        };
        let Some(file) = name.file_name() else {
            return Ok(None);
        };
        let file = path::from_native(Path::new(file));
        let parent = name
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        let direct_child = parent.is_none();
        let nested_dir = parent.map(|parent| join_normalized(dir, parent));
        let base_dir = nested_dir.as_deref().unwrap_or(dir);
        let mut filename = Vec::new();
        for prefix in &library.prefixes {
            for suffix in &library.suffixes {
                filename.clear();
                filename.extend_from_slice(prefix);
                filename.extend_from_slice(&file);
                filename.extend_from_slice(suffix);
                let plain_leaf = direct_child
                    && !cfg!(windows)
                    && !filename.is_empty()
                    && filename.as_slice() != b"."
                    && filename.as_slice() != b".."
                    && !filename.contains(&b'/');
                if plain_leaf && !directory.may_contain_leaf(&filename) {
                    continue;
                }
                let candidate_path = if plain_leaf {
                    base_dir.join(path::to_native(&filename))
                } else {
                    join_normalized(base_dir, &path::to_native(&filename))
                };
                if !plain_leaf && !directory.may_contain(dir, &candidate_path) {
                    continue;
                }
                if let Some(found) = self.accept(cx, &candidate_path, dir)? {
                    return Ok(Some((candidate_path, found)));
                }
            }
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
            let mut value = path::from_native_cmake(found);
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

    pub(super) fn log_event(
        &self,
        cx: &mut Cx<'_>,
        var: &[u8],
        found: Option<&Path>,
        was_defined: bool,
    ) {
        let Some(event) = &self.event else { return };
        if was_defined && found.is_none() && self.considered.is_none() {
            return;
        }
        let mode = self
            .kind
            .command()
            .strip_prefix("find_")
            .unwrap_or_default();
        let description = self.options.doc.clone().unwrap_or_else(|| self.kind.help());
        let is_darwin = self.is_darwin;
        let order = |key: &[u8]| {
            let value = cx.var_or_empty(key);
            if value.is_empty() {
                if is_darwin {
                    "FIRST".into()
                } else {
                    "NEVER".into()
                }
            } else {
                value
            }
        };
        let settings = [
            ("CMAKE_FIND_USE_CMAKE_PATH", self.options.no_cmake),
            (
                "CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH",
                self.options.no_cmake_env,
            ),
            (
                "CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH",
                self.options.no_system_env,
            ),
            ("CMAKE_FIND_USE_CMAKE_SYSTEM_PATH", self.options.no_system),
            ("CMAKE_FIND_USE_INSTALL_PREFIX", self.options.no_install),
        ]
        .map(|(key, disabled)| {
            (
                key,
                use_default(cx, self.options.no_defaults || disabled, key.as_bytes()),
            )
        });
        let framework = order(b"CMAKE_FIND_FRAMEWORK");
        let app_bundle = order(b"CMAKE_FIND_APPBUNDLE");
        let found = found.map(path::from_native_cmake);
        cx.log_configure_find(FindEvent {
            mode,
            variable: var,
            description: &description,
            framework: &framework,
            app_bundle: &app_bundle,
            settings: &settings,
            names: &self.options.names,
            candidates: &event.candidates,
            attempted: &event.attempted,
            found: found.as_ref().map(|value| value.as_slice()),
        });
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
                matches!(
                    FrameworkOrder::new(cx, self.is_darwin),
                    FrameworkOrder::First
                )
            } else {
                mode == b"FIRST" || mode.is_empty() && self.is_darwin
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
        if options.no_defaults {
            log.push_str("    NO_DEFAULT_PATH Enabled\n");
        } else {
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
                let enabled = use_default(cx, disabled, name.as_bytes());
                let _ = writeln!(log, "    {name}: {}", u8::from(enabled));
            }
        }
        let _ = writeln!(log, "\n  {command} considered the following locations:\n");
        log.push_str(trace);
        if let Some(found) = &self.match_path {
            let _ = writeln!(log, "\n  The item was found at\n\n    {}", found.display());
        } else {
            if !trace.is_empty() {
                log.push('\n');
            }
            log.push_str("  The item was not found.\n");
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
    fn new(cx: &Cx<'_>, is_darwin: bool) -> Self {
        let prefixes = library_parts(cx, b"CMAKE_FIND_LIBRARY_PREFIXES", b"lib");
        let defaults: &[u8] = if is_darwin {
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

#[derive(Debug, PartialEq, Eq)]
enum PlistExecutable<'a> {
    Found(&'a [u8]),
    Missing,
    Fallback,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum XmlTagKind {
    Open,
    Close,
    SelfClosing,
}

struct XmlTag<'a> {
    name: &'a str,
    kind: XmlTagKind,
}

enum PlistContainer {
    Dict { expect_key: bool },
    Array,
}

fn is_xml_ws(byte: &u8) -> bool {
    matches!(*byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn trim_xml_ws(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\r', '\n'])
}

fn is_xml_name_byte(byte: &u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b':')
}

fn validate_xml_decl(mut rest: &str) -> bool {
    let mut saw_version = false;
    let mut saw_encoding = false;
    let mut saw_standalone = false;
    loop {
        rest = trim_xml_ws(rest);
        if rest.is_empty() {
            return saw_version;
        }
        let name_len = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_alphabetic())
            .count();
        let Some((name, after_name)) = rest.split_at_checked(name_len) else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        let after_name = trim_xml_ws(after_name);
        let Some(after_eq) = after_name.strip_prefix('=') else {
            return false;
        };
        let after_eq = trim_xml_ws(after_eq);
        let (quote, after_quote) = if let Some(tail) = after_eq.strip_prefix('"') {
            ('"', tail)
        } else if let Some(tail) = after_eq.strip_prefix('\'') {
            ('\'', tail)
        } else {
            return false;
        };
        let Some((value, after_value)) = after_quote.split_once(quote) else {
            return false;
        };
        match name {
            "version" if !saw_version && !value.is_empty() => saw_version = true,
            "encoding" if !saw_encoding => {
                if !value.eq_ignore_ascii_case("UTF-8") {
                    return false;
                }
                saw_encoding = true;
            }
            "standalone" if !saw_standalone && matches!(value, "yes" | "no") => {
                saw_standalone = true;
            }
            _ => return false,
        }
        if !after_value.is_empty() && !after_value.as_bytes().first().is_some_and(is_xml_ws) {
            return false;
        }
        rest = after_value;
    }
}

fn parse_xml_tag(input: &str) -> Option<(XmlTag<'_>, &str)> {
    let body = input.strip_prefix('<')?;
    if body.starts_with('!') || body.starts_with('?') {
        return None;
    }
    if let Some(close_body) = body.strip_prefix('/') {
        let name_len = close_body.bytes().take_while(is_xml_name_byte).count();
        if name_len == 0 {
            return None;
        }
        let (name, after_name) = close_body.split_at_checked(name_len)?;
        let after_ws = trim_xml_ws(after_name);
        let rest = after_ws.strip_prefix('>')?;
        return Some((
            XmlTag {
                name,
                kind: XmlTagKind::Close,
            },
            rest,
        ));
    }
    let name_len = body.bytes().take_while(is_xml_name_byte).count();
    if name_len == 0 {
        return None;
    }
    let (name, after_name) = body.split_at_checked(name_len)?;
    if !after_name
        .as_bytes()
        .first()
        .is_some_and(|byte| *byte == b'>' || *byte == b'/' || is_xml_ws(byte))
    {
        return None;
    }
    let bytes = after_name.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if let Some(active) = quote {
            if byte == b'<' {
                return None;
            }
            if byte == active {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => {
                quote = Some(byte);
                index += 1;
            }
            b'<' => return None,
            b'>' => {
                let rest = after_name.get(index + 1..)?;
                return Some((
                    XmlTag {
                        name,
                        kind: XmlTagKind::Open,
                    },
                    rest,
                ));
            }
            b'/' => {
                if bytes.get(index + 1) == Some(&b'>') {
                    let rest = after_name.get(index + 2..)?;
                    return Some((
                        XmlTag {
                            name,
                            kind: XmlTagKind::SelfClosing,
                        },
                        rest,
                    ));
                }
                return None;
            }
            _ => index += 1,
        }
    }
    None
}

fn split_until_lt(input: &str) -> Option<(&str, &str)> {
    let pos = input.find('<')?;
    input.split_at_checked(pos)
}

fn parse_xml_plist_executable(bytes: &[u8]) -> PlistExecutable<'_> {
    const MAX_NESTING: usize = 64;

    if bytes.starts_with(b"bplist")
        || bytes.starts_with(b"\xFE\xFF")
        || bytes.starts_with(b"\xFF\xFE")
    {
        return PlistExecutable::Fallback;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return PlistExecutable::Fallback;
    };
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text);
    if rest.starts_with("<?") {
        let Some(after_xml) = rest.strip_prefix("<?xml") else {
            return PlistExecutable::Fallback;
        };
        if !after_xml.as_bytes().first().is_some_and(is_xml_ws) {
            return PlistExecutable::Fallback;
        }
        let Some((decl, after_decl)) = after_xml.split_once("?>") else {
            return PlistExecutable::Fallback;
        };
        if !validate_xml_decl(decl) {
            return PlistExecutable::Fallback;
        }
        rest = after_decl;
    }
    rest = trim_xml_ws(rest);
    if let Some(after_doctype) = rest.strip_prefix("<!DOCTYPE") {
        if !after_doctype.as_bytes().first().is_some_and(is_xml_ws) {
            return PlistExecutable::Fallback;
        }
        let Some((inside, after_gt)) = after_doctype.split_once('>') else {
            return PlistExecutable::Fallback;
        };
        if inside.contains('[') || inside.contains('<') {
            return PlistExecutable::Fallback;
        }
        rest = trim_xml_ws(after_gt);
    }
    let Some((plist_open, after_plist)) = parse_xml_tag(rest) else {
        return PlistExecutable::Fallback;
    };
    if plist_open.name != "plist" || plist_open.kind != XmlTagKind::Open {
        return PlistExecutable::Fallback;
    }
    rest = trim_xml_ws(after_plist);
    let Some((root_tag, after_root)) = parse_xml_tag(rest) else {
        return PlistExecutable::Fallback;
    };
    if root_tag.name != "dict" {
        return PlistExecutable::Fallback;
    }
    rest = after_root;
    let mut found: Option<&str> = None;
    if root_tag.kind == XmlTagKind::Open {
        let mut stack = vec![PlistContainer::Dict { expect_key: true }];
        let mut capture_next = false;
        while !stack.is_empty() {
            rest = trim_xml_ws(rest);
            let Some((tag, after_tag)) = parse_xml_tag(rest) else {
                return PlistExecutable::Fallback;
            };
            rest = after_tag;
            match tag.kind {
                XmlTagKind::Close => match (stack.pop(), tag.name) {
                    (Some(PlistContainer::Dict { expect_key: true }), "dict")
                    | (Some(PlistContainer::Array), "array") => {}
                    _ => return PlistExecutable::Fallback,
                },
                XmlTagKind::Open | XmlTagKind::SelfClosing => {
                    if tag.name == "key" {
                        if tag.kind != XmlTagKind::Open {
                            return PlistExecutable::Fallback;
                        }
                        let Some(PlistContainer::Dict { expect_key }) = stack.last_mut() else {
                            return PlistExecutable::Fallback;
                        };
                        if !*expect_key {
                            return PlistExecutable::Fallback;
                        }
                        *expect_key = false;
                        let Some((key_text, after_text)) = split_until_lt(rest) else {
                            return PlistExecutable::Fallback;
                        };
                        let Some((close, after_close)) = parse_xml_tag(after_text) else {
                            return PlistExecutable::Fallback;
                        };
                        if close.kind != XmlTagKind::Close || close.name != "key" {
                            return PlistExecutable::Fallback;
                        }
                        rest = after_close;
                        if key_text.contains('&') {
                            return PlistExecutable::Fallback;
                        }
                        if stack.len() == 1 && key_text == "CFBundleExecutable" {
                            if found.is_some() {
                                return PlistExecutable::Fallback;
                            }
                            capture_next = true;
                        }
                        continue;
                    }
                    if let Some(PlistContainer::Dict { expect_key }) = stack.last_mut() {
                        if *expect_key {
                            return PlistExecutable::Fallback;
                        }
                        *expect_key = true;
                    }
                    if capture_next {
                        capture_next = false;
                        if tag.name != "string" || tag.kind != XmlTagKind::Open {
                            return PlistExecutable::Fallback;
                        }
                        let Some((value, after_text)) = split_until_lt(rest) else {
                            return PlistExecutable::Fallback;
                        };
                        let Some((close, after_close)) = parse_xml_tag(after_text) else {
                            return PlistExecutable::Fallback;
                        };
                        if close.kind != XmlTagKind::Close || close.name != "string" {
                            return PlistExecutable::Fallback;
                        }
                        rest = after_close;
                        if value.is_empty()
                            || value
                                .bytes()
                                .any(|byte| matches!(byte, b'&' | b'<' | b'>' | 0))
                        {
                            return PlistExecutable::Fallback;
                        }
                        found = Some(value);
                        continue;
                    }
                    match (tag.name, tag.kind) {
                        ("dict", XmlTagKind::Open) => {
                            if stack.len() >= MAX_NESTING {
                                return PlistExecutable::Fallback;
                            }
                            stack.push(PlistContainer::Dict { expect_key: true });
                        }
                        ("array", XmlTagKind::Open) => {
                            if stack.len() >= MAX_NESTING {
                                return PlistExecutable::Fallback;
                            }
                            stack.push(PlistContainer::Array);
                        }
                        (
                            "dict" | "array" | "true" | "false" | "string" | "integer" | "real"
                            | "date" | "data",
                            XmlTagKind::SelfClosing,
                        ) => {}
                        ("string" | "integer" | "real" | "date" | "data", XmlTagKind::Open) => {
                            let Some((_, after_text)) = split_until_lt(rest) else {
                                return PlistExecutable::Fallback;
                            };
                            let Some((close, after_close)) = parse_xml_tag(after_text) else {
                                return PlistExecutable::Fallback;
                            };
                            if close.kind != XmlTagKind::Close || close.name != tag.name {
                                return PlistExecutable::Fallback;
                            }
                            rest = after_close;
                        }
                        _ => return PlistExecutable::Fallback,
                    }
                }
            }
        }
    } else if root_tag.kind != XmlTagKind::SelfClosing {
        return PlistExecutable::Fallback;
    }
    rest = trim_xml_ws(rest);
    let Some((plist_close, trailing)) = parse_xml_tag(rest) else {
        return PlistExecutable::Fallback;
    };
    if plist_close.name != "plist"
        || plist_close.kind != XmlTagKind::Close
        || !trim_xml_ws(trailing).is_empty()
    {
        return PlistExecutable::Fallback;
    }
    match found {
        Some(value) => PlistExecutable::Found(value.as_bytes()),
        None => PlistExecutable::Missing,
    }
}

fn bundle_executable(
    cx: &Cx<'_>,
    bundle: &Path,
    stem: &std::ffi::OsStr,
    hermetic: bool,
) -> PathBuf {
    const MAX_PLIST_BYTES: u64 = 256 * 1024;

    let contents = bundle.join("Contents");
    let plist = contents.join("Info.plist");
    let host = cx.host();
    let probe_plist = !hermetic || crate::hermetic::should_probe_speculative_path(cx, &plist);
    let executable = if probe_plist
        && host
            .metadata(&plist, Follow::Yes)
            .is_ok_and(|info| info.kind == FileKind::File)
    {
        let parsed = host
            .read(
                &plist,
                ReadRange::All {
                    limit: MAX_PLIST_BYTES,
                },
            )
            .ok()
            .and_then(|bytes| match parse_xml_plist_executable(&bytes) {
                PlistExecutable::Found(value) => Some(Some(path::to_native(value))),
                PlistExecutable::Missing => Some(None),
                PlistExecutable::Fallback => None,
            });
        let raw = parsed.unwrap_or_else(|| {
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
        });
        raw.filter(|name| single_component(name).is_some())
    } else {
        None
    };
    join_preserving_parent(
        &contents.join("MacOS"),
        executable.as_deref().unwrap_or_else(|| Path::new(stem)),
    )
}

fn candidate(cx: &mut Cx<'_>, kind: Kind, path: &Path) -> bool {
    if matches!(kind, Kind::Path | Kind::File) {
        return cx.host().metadata(path, Follow::Yes).is_ok();
    }
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
                ReadRange::Slice {
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

fn directory_path(dir: &Path) -> BString {
    let mut path = path::from_native_cmake(dir);
    if !path.ends_with(b"/") {
        path.push(b'/');
    }
    path
}

#[cfg(test)]
mod plist_tests {
    use super::{PlistExecutable, parse_xml_plist_executable};

    #[test]
    fn accepts_verified_utf8_top_level_key_and_ignores_nested_dicts() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>NSHumanReadableCopyright</key>
  <string>Copyright &amp; Co</string>
  <key>CFBundleURLTypes</key>
  <array>
    <dict>
      <key>CFBundleExecutable</key>
      <string>NestedBin</string>
    </dict>
  </array>
  <key>CFBundleExecutable</key>
  <string>café</string>
</dict>
</plist>
"#
        .as_bytes();
        assert_eq!(
            parse_xml_plist_executable(plist),
            PlistExecutable::Found("café".as_bytes())
        );

        let no_decl = br#"<plist version="1.0"><dict><key>CFBundleExecutable</key><string>Tool</string></dict></plist>"#;
        assert_eq!(
            parse_xml_plist_executable(no_decl),
            PlistExecutable::Found(b"Tool")
        );
    }

    #[test]
    fn falls_back_on_iso_8859_1_encoding_and_invalid_utf8() {
        let latin1_bytes = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n\
<plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>caf\xe9</string></dict></plist>";
        assert_eq!(
            parse_xml_plist_executable(latin1_bytes),
            PlistExecutable::Fallback
        );

        let latin1_ascii = br#"<?xml version="1.0" encoding="ISO-8859-1"?>
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>AsciiTool</string></dict></plist>"#;
        assert_eq!(
            parse_xml_plist_executable(latin1_ascii),
            PlistExecutable::Fallback
        );
    }

    #[test]
    fn rejects_array_nested_key_and_newline_nested_dict() {
        let in_array = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Items</key>
  <array>
    <key>CFBundleExecutable</key>
    <string>WrongFromArray</string>
  </array>
</dict>
</plist>"#;
        assert_eq!(
            parse_xml_plist_executable(in_array),
            PlistExecutable::Fallback
        );

        let in_newline_array = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Items</key>
  <array
  >
    <key>CFBundleExecutable</key>
    <string>WrongFromNewlineArray</string>
  </array>
</dict>
</plist>"#;
        assert_eq!(
            parse_xml_plist_executable(in_newline_array),
            PlistExecutable::Fallback
        );

        let in_newline_dict = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Nested</key>
  <dict
  >
    <key>CFBundleExecutable</key>
    <string>WrongFromNewlineDict</string>
  </dict>
</dict>
</plist>"#;
        assert_eq!(
            parse_xml_plist_executable(in_newline_dict),
            PlistExecutable::Missing
        );
    }

    #[test]
    fn falls_back_on_comments_cdata_entities_and_duplicates() {
        for sample in [
            br#"<plist version="1.0"><!-- comment --><dict><key>CFBundleExecutable</key><string>A</string></dict></plist>"#.as_slice(),
            br#"<plist version="1.0"><dict><key>CFBundleExecutable</key><string><![CDATA[A]]></string></dict></plist>"#,
            br#"<plist version="1.0"><dict><key>CFBundleExecutable</key><string>A&amp;B</string></dict></plist>"#,
            br#"<plist version="1.0"><dict><key>CFBundle&#69;xecutable</key><string>A</string></dict></plist>"#,
            br#"<plist version="1.0"><dict><key>CFBundleExecutable</key><string>A</string><key>CFBundleExecutable</key><string>B</string></dict></plist>"#,
        ] {
            assert_eq!(
                parse_xml_plist_executable(sample),
                PlistExecutable::Fallback
            );
        }
    }
}
