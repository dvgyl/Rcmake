use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, truth::is_on};
use rcmake_diag::{DiagAction, DiagCategory, DiagState};
use rcmake_eval::{IncludeOptions, Interp, RunMode};
use rcmake_host::{FileKind, Follow, Host, Stream, WriteMode, path};
use rcmake_json::Value;
use rcmake_model::{Cache, CacheType};

use crate::options::{self, CacheEdit, DirectoryArg, Options};
use crate::{apply_warning_flag, cache, emit_cli_error, new_interp, presets, resolve_cli_path};

const HOME_DIR: &[u8] = b"CMAKE_HOME_DIRECTORY";
const GENERATOR: &[u8] = b"CMAKE_GENERATOR";
const DIAG_INIT: &[u8] = b"CMAKE_DIAGNOSTIC_INIT";

pub(crate) fn run(argv: &[BString], mut opts: Options, host: Arc<dyn Host>) -> i32 {
    let (mut src, mut bin) = resolve_directories(host.as_ref(), &opts.directories);
    let cwd = host.initial_cwd();
    let cache_in = bin.as_deref().unwrap_or(&cwd);
    let mut saved = match cache::load(host.as_ref(), cache_in) {
        Ok(entries) => entries,
        Err(err) => {
            return error(
                host.as_ref(),
                format!("Could not load cache from {}: {err}", cache_in.display()),
            );
        }
    };
    if src.is_none() {
        src = saved
            .get(HOME_DIR)
            .map(|entry| path::to_native(&entry.value));
    }
    let had_src = src.is_some();
    let had_bin = bin.is_some();
    let src = src.unwrap_or_else(|| cwd.clone());

    if let Some(mode) = &opts.list_presets {
        return presets::list(host.as_ref(), &src, opts.presets_file.as_deref(), mode);
    }
    let mut preset = presets::Application::default();
    if opts.preset.is_some() {
        preset = match presets::apply(host.as_ref(), &src, &mut opts) {
            Ok(result) => result,
            Err(code) => return code,
        };
        if bin.is_none()
            && let Some(dir) = preset.binary.take()
        {
            if dir != cwd {
                saved = match cache::load(host.as_ref(), &dir) {
                    Ok(entries) => entries,
                    Err(err) => {
                        return error(
                            host.as_ref(),
                            format!("Could not load cache from {}: {err}", dir.display()),
                        );
                    }
                };
            }
            bin = Some(dir);
        }
    }
    if !had_src && !had_bin && opts.preset.is_none() && opts.list_cache.is_none() {
        options::warn(
            host.as_ref(),
            "No source or binary directory provided.  Both will be assumed to be the\n  same as the current working directory, but note that this warning will\n  become a fatal error in future CMake releases.",
        );
    }
    let bin = bin.unwrap_or(cwd);
    if opts.fresh && !opts.view {
        if let Err(err) = clear_cache(host.as_ref(), &bin) {
            return error(
                host.as_ref(),
                format!("Could not remove the cache in {}: {err}", bin.display()),
            );
        }
        saved = Cache::default();
    }
    let generator = opts
        .generator
        .as_ref()
        .or_else(|| saved.get(GENERATOR).map(|entry| &entry.value))
        .cloned()
        .unwrap_or_else(|| options::default_generator(host.as_ref()));
    if !options::is_generator(&generator) {
        options::generator_error(host.as_ref(), Some(&generator));
        return 1;
    }
    if let Some(old) = saved.get(GENERATOR)
        && old.value != generator
    {
        return error(
            host.as_ref(),
            format!(
                "Error: generator : {generator}\nDoes not match the generator used previously: {}\nEither remove the CMakeCache.txt file and CMakeFiles directory or choose a different binary directory.",
                old.value
            ),
        );
    }
    if let Some(old) = saved.get(HOME_DIR) {
        let old_src = path::to_native(&old.value);
        if old_src != src
            && host
                .canonicalize(&old_src)
                .ok()
                .is_none_or(|old| host.canonicalize(&src).ok().as_ref() != Some(&old))
        {
            return error(
                host.as_ref(),
                format!(
                    "The source \"{}/CMakeLists.txt\" does not match the source \"{}/CMakeLists.txt\" used to generate cache.  Re-run cmake with a different source directory.",
                    src.display(),
                    old_src.display()
                ),
            );
        }
    }

    let has_cache = saved.contains(HOME_DIR);
    let mut interp = new_interp(argv, &opts, RunMode::Configure, Arc::clone(&host));
    *interp.cx().cache() = saved;
    cache::insert(
        interp.cx().cache(),
        GENERATOR,
        generator,
        Some(CacheType::Internal),
    );
    cache::insert(
        interp.cx().cache(),
        HOME_DIR,
        path::from_native(&src),
        Some(CacheType::Internal),
    );
    set_directory_vars(&mut interp, &src, &bin);
    let start = host.monotonic();

    let edits = std::mem::take(&mut opts.cache_edits);
    let mut preloads = Preloads {
        argv,
        opts: &opts,
        host: &host,
        src: &src,
        bin: &bin,
        results: Vec::new(),
        exit_code: 0,
        fatal: false,
    };
    let mut missing_preload = false;
    let mut configured_diagnostics = false;
    for edit in edits {
        match edit {
            CacheEdit::Define(def) => cache::apply(interp.cx().cache(), def),
            CacheEdit::Remove(pattern) => remove_entries(&mut interp, &pattern),
            CacheEdit::Preload(script) => {
                if !configured_diagnostics {
                    apply_diagnostics(
                        host.as_ref(),
                        &mut interp,
                        &preset.warnings,
                        &opts.warning_flags,
                    );
                    configured_diagnostics = true;
                }
                let path = host.initial_cwd().join(&script);
                if is_preload_file(host.as_ref(), &path) {
                    preloads.run(&mut interp, &script, true);
                } else {
                    options::out(
                        host.as_ref(),
                        &format!("loading initial cache file {}\n", script.display()),
                    );
                    emit_cli_error(host.as_ref(), format!("Not a file: {}", path.display()));
                    if opts.view || is_file(host.as_ref(), &src.join("CMakeLists.txt")) {
                        emit_cli_error(
                            host.as_ref(),
                            format!("Error processing file: {}", path.display()),
                        );
                    }
                    missing_preload = true;
                }
            }
        }
        if preloads.fatal {
            break;
        }
    }
    if !configured_diagnostics {
        apply_diagnostics(
            host.as_ref(),
            &mut interp,
            &preset.warnings,
            &opts.warning_flags,
        );
    }
    if !preset.log.is_empty() {
        options::out(host.as_ref(), &preset.log);
    }
    for (index, dir) in [&src, &bin].into_iter().enumerate() {
        if index == 1 && src == bin {
            break;
        }
        let file = dir.join("PreLoad.cmake");
        if is_file(host.as_ref(), &file) {
            preloads.run(&mut interp, &file, false);
        }
    }
    if opts.view {
        if let Some(list) = &opts.list_cache {
            list_cache(host.as_ref(), &mut interp, list);
        }
        return i32::from(!crate::write_sarif_if_requested(
            host.as_ref(),
            opts.sarif.as_deref(),
            &interp,
            preloads.results,
        ));
    }
    if preloads.fatal {
        return preloads.exit_code;
    }
    if !is_file(host.as_ref(), &src.join("CMakeLists.txt")) {
        let problem = if is_dir(host.as_ref(), &src) {
            "does not appear to contain CMakeLists.txt"
        } else {
            "does not exist"
        };
        return error(
            host.as_ref(),
            format!(
                "The source directory \"{}\" {problem}.\nSpecify --help for usage, or press the help button on the CMake GUI.",
                src.display()
            ),
        );
    }
    if missing_preload {
        return 1;
    }
    let toolchain =
        apply_environment_defaults(host.as_ref(), interp.cx().cache(), &src, &bin, has_cache);
    if let Some(toolchain) = toolchain
        && is_file(host.as_ref(), &toolchain)
    {
        let _ = interp
            .cx()
            .include_file(&toolchain, IncludeOptions::default());
    }
    let result = interp.run_configure(&src, &bin, &[]);
    if result.exit_code == 0
        && opts
            .log_level
            .is_none_or(|level| level >= rcmake_eval::LogLevel::Status)
    {
        let elapsed = host.monotonic().saturating_sub(start);
        options::out(
            host.as_ref(),
            &format!("-- Configuring done ({:.1}s)\n", elapsed.as_secs_f64()),
        );
    }
    if !crate::write_sarif_if_requested(
        host.as_ref(),
        opts.sarif.as_deref(),
        &interp,
        preloads.results,
    ) {
        return 1;
    }
    if let Some(list) = &opts.list_cache {
        list_cache(host.as_ref(), &mut interp, list);
    }
    if let Some(path) = &opts.profile_output {
        let elapsed = host.monotonic().saturating_sub(start);
        let profile = format!(
            "[{{\"name\":\"configure\",\"ph\":\"X\",\"ts\":0,\"dur\":{},\"pid\":{},\"tid\":0}}]",
            elapsed.as_micros(),
            std::process::id()
        );
        if let Err(err) = host.write(path, profile.as_bytes(), WriteMode::Truncate) {
            return error(
                host.as_ref(),
                format!("Could not write profiling output: {err}"),
            );
        }
    }
    if result.exit_code != 0 || preloads.exit_code != 0 {
        return if result.exit_code == 0 {
            preloads.exit_code
        } else {
            result.exit_code
        };
    }
    if let Some(graph) = &opts.graphviz
        && !write_graphviz(host.as_ref(), graph, &mut interp)
    {
        return 1;
    }
    if let Err(err) = cache::save(host.as_ref(), &bin, interp.cx().cache()) {
        return error(
            host.as_ref(),
            format!("Could not write CMakeCache.txt: {err}"),
        );
    }
    0
}

struct Preloads<'a> {
    argv: &'a [BString],
    opts: &'a Options,
    host: &'a Arc<dyn Host>,
    src: &'a Path,
    bin: &'a Path,
    results: Vec<Value>,
    exit_code: i32,
    fatal: bool,
}

impl Preloads<'_> {
    fn run(&mut self, configure: &mut Interp, script: &Path, initial_cache: bool) {
        let mut interp = new_interp(
            self.argv,
            self.opts,
            RunMode::Configure,
            Arc::clone(self.host),
        );
        *interp.cx().diag_state_mut() = *configure.cx().diag_state();
        *interp.cx().cache() = std::mem::take(configure.cx().cache());
        set_directory_vars(&mut interp, self.src, self.bin);
        if initial_cache {
            let _ = interp.run_preload_script(script);
        } else {
            let _ = interp.cx().include_file(script, IncludeOptions::default());
        }
        *configure.cx().cache() = std::mem::take(interp.cx().cache());
        let code = interp.exit_code();
        if code != 0 {
            self.exit_code = code;
        }
        self.fatal |= interp.fatal_occurred();
        if self.opts.sarif.is_some() {
            let mut sarif = interp.diag_engine().sarif_json();
            if let Some(results) = crate::sarif_results(&mut sarif) {
                self.results.append(results);
            }
        }
    }
}

fn apply_diagnostics(host: &dyn Host, interp: &mut Interp, preset: &[BString], cli: &[BString]) {
    let state = initialize_diagnostics(host, interp.cx().cache(), preset, cli, true);
    *interp.cx().diag_state_mut() = state;
}

pub(crate) fn initialize_diagnostics(
    host: &dyn Host,
    cache: &mut Cache,
    preset: &[BString],
    cli: &[BString],
    configure: bool,
) -> DiagState {
    let mut state = DiagState::default();
    if let Some(entry) = cache.get(DIAG_INIT) {
        for part in rcmake_base::list::split(&entry.value, rcmake_base::list::Empty::Drop) {
            let Some(eq) = part.iter().position(|b| *b == b'=') else {
                continue;
            };
            if let (Some(cat), Some(action)) = (
                DiagCategory::parse(part.get(..eq).unwrap_or_default()),
                DiagAction::parse(part.get(eq + 1..).unwrap_or_default()),
            ) {
                state.set(cat, action, false);
            }
        }
    } else {
        for (name, cat, action, when) in [
            (
                b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS".as_slice(),
                DiagCategory::Author,
                DiagAction::Ignore,
                true,
            ),
            (
                b"CMAKE_SUPPRESS_DEVELOPER_ERRORS",
                DiagCategory::Author,
                DiagAction::SendError,
                false,
            ),
            (
                b"CMAKE_WARN_DEPRECATED",
                DiagCategory::Deprecated,
                DiagAction::Ignore,
                false,
            ),
            (
                b"CMAKE_ERROR_DEPRECATED",
                DiagCategory::Deprecated,
                DiagAction::SendError,
                true,
            ),
        ] {
            if cache
                .get(name)
                .is_some_and(|entry| is_on(&entry.value) == when)
            {
                state.set(cat, action, false);
            }
        }
    }
    let initial = state;
    for flag in preset.iter().chain(cli) {
        if let Some(prefix) = flag
            .strip_prefix(b"-W")
            .and_then(|v| v.strip_suffix(b"dev"))
            && matches!(prefix, b"" | b"no-" | b"error=" | b"no-error=")
        {
            let prefix = String::from_utf8_lossy(prefix);
            host.write_out(
                Stream::Stderr,
                format!("The {prefix}dev option is deprecated.  Use -W{prefix}author instead.\n")
                    .as_bytes(),
            );
        }
        apply_warning_flag(flag, &mut state);
    }
    if configure {
        let mut serialized = String::new();
        for cat in DiagCategory::ALL {
            if !serialized.is_empty() {
                serialized.push(';');
            }
            serialized.push_str(cat.as_str());
            serialized.push('=');
            serialized.push_str(state.get(cat).as_str());
        }
        cache::insert(
            cache,
            DIAG_INIT,
            serialized.into(),
            Some(CacheType::Internal),
        );
    }
    for (cat, vars) in [
        (
            DiagCategory::Author,
            [
                b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS".as_slice(),
                b"CMAKE_SUPPRESS_DEVELOPER_ERRORS",
            ],
        ),
        (
            DiagCategory::Deprecated,
            [
                b"CMAKE_WARN_DEPRECATED".as_slice(),
                b"CMAKE_ERROR_DEPRECATED",
            ],
        ),
    ] {
        let action = state.get(cat);
        if initial.get(cat) != action
            || preset.iter().chain(cli).any(|f| {
                f.ends_with(b"dev")
                    || f.ends_with(b"author")
                    || (cat == DiagCategory::Deprecated && f.ends_with(b"deprecated"))
            })
        {
            let enabled = if cat == DiagCategory::Author {
                [action == DiagAction::Ignore, action < DiagAction::SendError]
            } else {
                [
                    action != DiagAction::Ignore,
                    action >= DiagAction::SendError,
                ]
            };
            for (name, value) in vars.into_iter().zip(enabled) {
                cache::insert(
                    cache,
                    name,
                    (if value { "TRUE" } else { "FALSE" }).into(),
                    Some(CacheType::Internal),
                );
            }
        }
    }
    state
}

fn resolve_directories(
    host: &dyn Host,
    args: &[DirectoryArg],
) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut source = None;
    let mut binary = None;
    let explicit_source = args
        .iter()
        .any(|arg| matches!(arg, DirectoryArg::Source(_)));
    let explicit_binary = args
        .iter()
        .any(|arg| matches!(arg, DirectoryArg::Binary(_)));
    let mut ignored = Vec::new();
    for arg in args {
        match arg {
            DirectoryArg::Source(path) => {
                if let Some(old) = source.replace(path.clone())
                    && old != *path
                {
                    ignored.push(old);
                }
            }
            DirectoryArg::Binary(path) => {
                if let Some(old) = binary.replace(path.clone())
                    && old != *path
                {
                    ignored.push(old);
                }
            }
            DirectoryArg::Positional(pos) => {
                let path = match pos.file_name().and_then(|name| name.to_str()) {
                    Some("CMakeCache.txt" | "CMakeLists.txt") => pos.parent().unwrap_or(pos),
                    _ => pos,
                };
                if !explicit_binary
                    && binary.is_none()
                    && is_file(host, &path.join("CMakeCache.txt"))
                {
                    binary = Some(path.to_path_buf());
                } else if explicit_source || is_file(host, &path.join("CMakeLists.txt")) {
                    if !explicit_source && is_file(host, &path.join("CMakeLists.txt")) {
                        if let Some(old) = source.replace(path.to_path_buf())
                            && old != path
                        {
                            ignored.push(old);
                        }
                    } else if !explicit_binary
                        && binary.is_none()
                        && !is_file(host, &path.join("CMakeLists.txt"))
                    {
                        binary = Some(path.to_path_buf());
                    } else {
                        ignored.push(path.to_path_buf());
                    }
                } else if source.is_none() {
                    source = Some(path.to_path_buf());
                } else {
                    ignored.push(path.to_path_buf());
                }
            }
        }
    }
    for path in ignored {
        if source.as_deref() != Some(&path) && binary.as_deref() != Some(&path) {
            options::warn(
                host,
                &format!(
                    "Ignoring extra path from command line:\n\n   \"{}\"",
                    path.display()
                ),
            );
        }
    }
    (source, binary)
}

fn set_directory_vars(interp: &mut Interp, src: &Path, bin: &Path) {
    let mut cx = interp.cx();
    let source = path::from_native(src);
    let binary = path::from_native(bin);
    cx.set_var(b"CMAKE_SOURCE_DIR", source.clone());
    cx.set_var(b"CMAKE_CURRENT_SOURCE_DIR", source);
    cx.set_var(b"CMAKE_BINARY_DIR", binary.clone());
    cx.set_var(b"CMAKE_CURRENT_BINARY_DIR", binary);
}

fn clear_cache(host: &dyn Host, bin: &Path) -> rcmake_host::HostResult<()> {
    let ignore_missing = |result: rcmake_host::HostResult<()>| {
        result.or_else(|err| {
            if err.kind == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(err)
            }
        })
    };
    ignore_missing(host.remove_file(&bin.join("CMakeCache.txt")))?;
    let files = bin.join("CMakeFiles");
    if host
        .metadata(&files, Follow::No)
        .is_ok_and(|m| m.kind == FileKind::Symlink)
    {
        ignore_missing(host.remove_file(&files))
    } else {
        ignore_missing(host.remove_dir_all(&files))
    }
}

fn apply_environment_defaults(
    host: &dyn Host,
    cache: &mut Cache,
    src: &Path,
    bin: &Path,
    has_cache: bool,
) -> Option<PathBuf> {
    for (name, ty) in [
        (b"CMAKE_TOOLCHAIN_FILE".as_slice(), CacheType::Filepath),
        (b"CMAKE_INSTALL_PREFIX", CacheType::Path),
        (b"CMAKE_BUILD_TYPE", CacheType::String),
        (b"CMAKE_EXPORT_COMPILE_COMMANDS", CacheType::Bool),
        (b"CMAKE_COLOR_DIAGNOSTICS", CacheType::Bool),
    ] {
        if !has_cache
            && !cache.contains(name)
            && let Some(value) = host.env_get(name).filter(|v| !v.is_empty())
        {
            let value = if name == b"CMAKE_COLOR_DIAGNOSTICS" {
                BString::from(if is_on(&value) { "ON" } else { "OFF" })
            } else {
                value
            };
            cache::insert(cache, name, value, Some(ty));
        }
        if let Some(entry) = cache.get_mut(name)
            && entry.ty == CacheType::Uninitialized
        {
            entry.ty = ty;
        }
    }
    if cache
        .get(GENERATOR)
        .is_some_and(|g| g.value.windows(4).any(|window| window == b"Make"))
        && !cache.contains(b"CMAKE_COLOR_MAKEFILE")
    {
        cache::insert(
            cache,
            b"CMAKE_COLOR_MAKEFILE",
            "ON".into(),
            Some(CacheType::Bool),
        );
    }
    let entry = cache
        .get_mut(b"CMAKE_TOOLCHAIN_FILE")
        .filter(|entry| !entry.value.is_empty())?;
    let path = path::to_native(&entry.value);
    let absolute = if path.is_absolute() {
        resolve_cli_path(host, &entry.value)
    } else {
        let build_path = bin.join(&path);
        if is_file(host, &build_path) {
            build_path
        } else {
            src.join(&path)
        }
    };
    entry.value = path::from_native(&absolute);
    Some(absolute)
}

fn remove_entries(interp: &mut Interp, pattern: &[u8]) {
    let mut cx = interp.cx();
    let names: Vec<_> = cx
        .cache()
        .iter()
        .filter(|(name, entry)| entry.ty != CacheType::Static && matches_glob(pattern, name))
        .map(|(name, _)| name.clone())
        .collect();
    for name in names {
        cx.cache().remove(&name);
    }
}

pub(crate) fn matches_glob(mut pattern: &[u8], mut name: &[u8]) -> bool {
    let mut wildcard = None;
    while let Some((byte, tail)) = name.split_first() {
        match pattern.split_first() {
            Some((b'*', rest)) => {
                wildcard = Some((rest, name));
                pattern = rest;
            }
            Some((matched, rest)) if matched == byte || *matched == b'?' => {
                pattern = rest;
                name = tail;
            }
            _ => {
                let Some((rest, [_, next @ ..])) = wildcard else {
                    return false;
                };
                name = next;
                pattern = rest;
                wildcard = Some((rest, next));
            }
        }
    }
    pattern.iter().all(|byte| *byte == b'*')
}

fn list_cache(host: &dyn Host, interp: &mut Interp, list: &options::ListCache) {
    let mut cx = interp.cx();
    let filter = list
        .pattern
        .as_ref()
        .and_then(|pattern| cx.regex(pattern).ok());
    let mut budget = cx.limits().regex_steps;
    let mut out = BString::from("-- Cache values\n");
    for (name, entry) in cache::sorted(cx.cache()) {
        if matches!(
            entry.ty,
            CacheType::Internal | CacheType::Uninitialized | CacheType::Static
        ) || (!list.advanced && entry.advanced)
            || filter.as_ref().is_some_and(|r| {
                !r.find_at(name, 0, Default::default(), &mut budget)
                    .is_ok_and(|m| m.is_some())
            })
        {
            continue;
        }
        if list.help {
            out.extend_from_slice(b"// ");
            out.extend_from_slice(&entry.help);
            out.push(b'\n');
        }
        out.extend_from_slice(name);
        out.extend_from_slice(format!(":{}=", entry.ty.as_str()).as_bytes());
        out.extend_from_slice(&entry.value);
        out.push(b'\n');
        if list.help {
            out.push(b'\n');
        }
    }
    host.write_out(Stream::Stdout, &out);
}

fn write_graphviz(host: &dyn Host, path: &Path, interp: &mut Interp) -> bool {
    let mut cx = interp.cx();
    let name = cx.var_or_empty(b"CMAKE_PROJECT_NAME");
    let mut graph = format!("digraph \"{}\" {{\n", dot_escape(&name));
    for (_, target) in cx.project().targets() {
        graph.push_str(&format!("  \"{}\";\n", dot_escape(&target.name)));
    }
    graph.push_str("}\n");
    if let Err(err) = host.write(path, graph.as_bytes(), WriteMode::Truncate) {
        error(host, format!("Could not write Graphviz output: {err}"));
        return false;
    }
    options::out(host, &format!("Generate graphviz: {}\n", path.display()));
    true
}

fn dot_escape(value: &[u8]) -> String {
    String::from_utf8_lossy(value)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn is_preload_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes).is_ok_and(|meta| {
        meta.kind == FileKind::File
            || (meta.kind == FileKind::Other
                && path.parent().is_some_and(|dir| {
                    dir == Path::new("/dev/fd") || dir == Path::new("/proc/self/fd")
                }))
    })
}

pub(crate) fn is_file(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
}

pub(crate) fn is_dir(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
}

fn error(host: &dyn Host, message: String) -> i32 {
    emit_cli_error(host, message);
    1
}
