use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, path_lex::Lexical, truth::is_on};
use rcmake_diag::{
    Backtrace, DiagAction, DiagCategory, DiagState, Diagnostic, Effect, Frame, FrameKind, Location,
    MessageKind, SourceFile, SourceId, SourceMap,
    annotate::{Annotations, Confidence, Fix},
    rich::{RichOptions, SourceLookup, render_rich},
};
use rcmake_eval::Interp;
use rcmake_host::{Host, Stream, forward_host, path};
use rcmake_model::{Cache, CacheType};

use crate::{apply_warning_flag, cache};

const DIAG_INIT: &[u8] = b"CMAKE_DIAGNOSTIC_INIT";

pub(crate) fn initialize_diagnostics(
    host: &dyn Host,
    cache: &mut Cache,
    preset: &[BString],
    cli: &[BString],
    configure: bool,
) -> DiagState {
    let mut state = DiagState::default();
    if configure {
        state.set(DiagCategory::UnusedCli, DiagAction::Warn, false);
    }
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
        ] {
            if cache
                .get(name)
                .is_some_and(|entry| is_on(&entry.value) == when)
            {
                state.set(cat, action, false);
            }
        }
    }
    for (name, on, off) in [
        (
            b"CMAKE_WARN_DEPRECATED".as_slice(),
            DiagAction::Warn,
            DiagAction::Ignore,
        ),
        (
            b"CMAKE_ERROR_DEPRECATED",
            DiagAction::SendError,
            DiagAction::Warn,
        ),
    ] {
        if let Some(entry) = cache.get(name) {
            let current = state.get(DiagCategory::Deprecated);
            let next = if is_on(&entry.value) {
                current.max(on)
            } else {
                current.min(off)
            };
            state.set(DiagCategory::Deprecated, next, false);
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
        cache::insert_internal(
            cache,
            DIAG_INIT,
            serialized.into(),
            "Set initial state for CMake diagnostics; used to persist state set by command-line options across invocations.",
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
        if (configure && cat == DiagCategory::Deprecated)
            || initial.get(cat) != action
            || preset.iter().chain(cli).any(|f| {
                f.ends_with(b"dev")
                    || f.ends_with(b"author")
                    || (cat == DiagCategory::Deprecated && f.ends_with(b"deprecated"))
            })
        {
            let enabled = if cat == DiagCategory::Author {
                [action == DiagAction::Ignore, action < DiagAction::SendError]
            } else {
                [action >= DiagAction::Warn, action >= DiagAction::SendError]
            };
            for (name, value) in vars.into_iter().zip(enabled) {
                cache::insert(
                    cache,
                    name,
                    (if configure && cat == DiagCategory::Deprecated {
                        if value { "ON" } else { "OFF" }
                    } else if value {
                        "TRUE"
                    } else {
                        "FALSE"
                    })
                    .into(),
                    Some(CacheType::Internal),
                );
                if configure
                    && let Some(option) = legacy_warning_option(name)
                    && let Some(entry) = cache.get_mut(name)
                {
                    entry.help = format!("Deprecated.  Use {option} instead.").into();
                }
            }
        }
    }
    state
}

fn legacy_warning_option(name: &[u8]) -> Option<&'static str> {
    match name {
        b"CMAKE_WARN_DEPRECATED" => Some("-W[no-]deprecated"),
        b"CMAKE_ERROR_DEPRECATED" => Some("-W[no-]error=deprecated"),
        _ => None,
    }
}

pub(super) fn warn_legacy_define(host: &dyn Host, name: &[u8]) {
    if let Some(option) = legacy_warning_option(name) {
        let variable = BString::from(name);
        host.write_out(
            Stream::Stderr,
            format!("The {variable} variable is deprecated.  Use {option} instead.\n").as_bytes(),
        );
    }
}

/// `cmGlobalGenerator::Compute`: the multi-configuration variables are an error with
/// a generator that does not support them, even when they are set to an empty value.
pub(super) fn unsupported_config_variable(interp: &mut Interp) -> Option<String> {
    const VARIABLES: [&str; 3] = [
        "CMAKE_DEFAULT_BUILD_TYPE",
        "CMAKE_CROSS_CONFIGS",
        "CMAKE_DEFAULT_CONFIGS",
    ];
    let mut cx = interp.cx();
    let generator = cx.project().generator?;
    if generator == rcmake_model::GeneratorKind::NinjaMultiConfig {
        return None;
    }
    let cached = VARIABLES.map(|name| cx.cache().get(name.as_bytes()).is_some());
    let vars = &cx.project().dir(rcmake_model::DirId::ROOT).vars;
    let (name, _) = VARIABLES
        .into_iter()
        .zip(cached)
        .find(|(name, cached)| *cached || vars.contains_key(name.as_bytes()))?;
    Some(format!(
        "CMake Error:\n  Generator\n\n    {}\n\n  does not support variable\n\n    {name}\n\n  but it has been specified.\n\n\n",
        generator.name()
    ))
}

/// Source lookup backed strictly by the evaluator's in-memory [`SourceMap`].
/// Never performs disk reads.
pub(crate) struct MapSourceLookup<'a> {
    sources: &'a SourceMap,
    source_dir: &'a Path,
    cmake_root: &'a Path,
}

impl<'a> MapSourceLookup<'a> {
    pub(crate) fn new(sources: &'a SourceMap, source_dir: &'a Path, cmake_root: &'a Path) -> Self {
        Self {
            sources,
            source_dir,
            cmake_root,
        }
    }
}

impl SourceLookup for MapSourceLookup<'_> {
    fn source(&self, id: SourceId) -> Option<&SourceFile> {
        self.sources.get(id)
    }

    fn source_by_path(&self, raw: &[u8]) -> Option<&SourceFile> {
        if raw.is_empty() {
            return None;
        }
        if let Some(id) = self.sources.latest_file(raw) {
            return self.sources.get(id);
        }
        let native = path::to_native(raw);
        let resolved = if native.is_absolute() {
            native
        } else {
            self.source_dir.join(native)
        };
        let normalized = Lexical::new(&path::from_native_cmake(&resolved)).normalize();
        self.sources
            .latest_file(&normalized)
            .and_then(|id| self.sources.get(id))
    }

    fn is_bundled_module(&self, raw: &[u8]) -> bool {
        !self.cmake_root.as_os_str().is_empty()
            && path::to_native(raw).starts_with(self.cmake_root.join("Modules"))
    }
}

/// Host wrapper active only during generation in rich diagnostic mode.
/// Converts plain CMake generate-time diagnostics into rich diagnostics with snippets and hints
/// using only sources already recorded in the evaluator's [`SourceMap`].
pub(crate) struct RichGenerateHost<'a> {
    inner: &'a dyn Host,
    sources: &'a SourceMap,
    source_dir: PathBuf,
    cmake_root: PathBuf,
    rich: RichOptions,
}

impl<'a> RichGenerateHost<'a> {
    pub(crate) fn new(
        inner: &'a dyn Host,
        sources: &'a SourceMap,
        source_dir: PathBuf,
        cmake_root: PathBuf,
        rich: RichOptions,
    ) -> Self {
        Self {
            inner,
            sources,
            source_dir,
            cmake_root,
            rich,
        }
    }
}

impl Host for RichGenerateHost<'_> {
    forward_host!(inner, fs, env, process);

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.inner.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> rcmake_host::HostResult<Option<BString>> {
        self.inner.registry_value(root, subkey, name, view)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if stream == Stream::Stderr
            && let Some(rendered) = render_rich_cmake_diagnostic(
                bytes,
                self.sources,
                &self.source_dir,
                &self.cmake_root,
                self.rich,
            )
        {
            self.inner.write_out(stream, &rendered);
            return;
        }
        self.inner.write_out(stream, bytes);
    }

    fn read_stdin(&self, limit: u64) -> rcmake_host::HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn terminal_width(&self, stream: Stream) -> Option<usize> {
        self.inner.terminal_width(stream)
    }

    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> std::time::Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> jiff::tz::TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> rcmake_host::HostResult<PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        self.inner.system()
    }
}

/// Convert a plain CMake diagnostic block emitted during generation into a rich diagnostic.
pub(crate) fn render_rich_cmake_diagnostic(
    bytes: &[u8],
    sources: &SourceMap,
    source_dir: &Path,
    cmake_root: &Path,
    rich: RichOptions,
) -> Option<BString> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_start_matches('\n');
    let mut lines = trimmed.lines();
    let header = lines.next()?;
    let (kind, effect, category, rest) = if let Some(rest) = header.strip_prefix("CMake Error") {
        (MessageKind::Error, Effect::SetError, None, rest)
    } else if let Some(rest) = header.strip_prefix("CMake Warning (dev)") {
        (
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            rest,
        )
    } else if let Some(rest) = header.strip_prefix("CMake Warning") {
        (MessageKind::Warning, Effect::None, None, rest)
    } else if let Some(rest) = header.strip_prefix("CMake Deprecation Warning") {
        (
            MessageKind::DeprecationWarning,
            Effect::None,
            Some(DiagCategory::Deprecated),
            rest,
        )
    } else {
        let rest = header.strip_prefix("CMake Deprecation Error")?;
        (
            MessageKind::DeprecationError,
            Effect::SetError,
            Some(DiagCategory::Deprecated),
            rest,
        )
    };

    let (top_frame, inline_rest) = if let Some(after_at) = rest.strip_prefix(" at ") {
        let site_spec = after_at.strip_suffix(':')?;
        let (file, line, cmd) = parse_site_spec(site_spec)?;
        (
            Some(Frame {
                file: Arc::from(file.as_bytes()),
                line,
                name: cmd.map(|name| Arc::from(name.as_bytes())),
                kind: FrameKind::Command,
            }),
            None,
        )
    } else {
        let after_colon = rest.strip_prefix(':')?;
        let inline = after_colon.trim();
        (None, (!inline.is_empty()).then_some(inline))
    };

    let mut body_lines: Vec<String> = Vec::new();
    if let Some(inline) = inline_rest {
        body_lines.push(inline.to_owned());
    }
    let mut stack_frames = Vec::new();
    let mut in_stack = false;
    for line in lines {
        if line == "Call Stack (most recent call first):" {
            in_stack = true;
            continue;
        }
        if in_stack {
            let entry = line.trim();
            if entry.is_empty() {
                continue;
            }
            if let Some((file, line_num, cmd)) = parse_site_spec(entry) {
                stack_frames.push(Frame {
                    file: Arc::from(file.as_bytes()),
                    line: line_num,
                    name: cmd.map(|name| Arc::from(name.as_bytes())),
                    kind: FrameKind::Command,
                });
            }
        } else {
            body_lines.push(line.strip_prefix("  ").unwrap_or(line).to_owned());
        }
    }
    while body_lines.last().is_some_and(String::is_empty) {
        body_lines.pop();
    }
    if body_lines.is_empty() {
        return None;
    }
    let body = body_lines.join("\n");

    let location = match (top_frame, stack_frames.is_empty()) {
        (None, true) => Location::None,
        (Some(top), true) => Location::Backtrace(Backtrace::default().push(top)),
        (top, false) => {
            let mut trace = Backtrace::default();
            for frame in stack_frames.into_iter().rev() {
                trace = trace.push(frame);
            }
            if let Some(top) = top {
                trace = trace.push(top);
            }
            Location::Backtrace(trace)
        }
    };

    let annotations = missing_target_fix(&body).map(|fix| Annotations::default().with_fix(fix));
    let diag = Diagnostic {
        kind,
        effect,
        category,
        policy: None,
        location,
        body: body.into(),
        annotations: None,
    };
    let lookup = MapSourceLookup::new(sources, source_dir, cmake_root);
    Some(render_rich(&diag, annotations.as_ref(), &lookup, rich))
}

fn missing_target_fix(body: &str) -> Option<Fix> {
    if !body.contains("but the target was not found.") {
        return None;
    }
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if line.trim_end().ends_with("links to:") {
            for candidate in lines.by_ref() {
                let name = candidate.trim();
                if !name.is_empty() {
                    let message = if let Some((prefix, _)) = name.split_once("::") {
                        format!(
                            "add find_package({prefix} REQUIRED) before this line, or check the spelling of {name}"
                        )
                    } else {
                        format!(
                            "check the spelling of {name}, or define the target before generation"
                        )
                    };
                    return Some(Fix {
                        message: message.into(),
                        edits: Vec::new(),
                        confidence: Confidence::MaybeIncorrect,
                    });
                }
            }
        }
    }
    None
}

fn parse_site_spec(spec: &str) -> Option<(&str, u32, Option<&str>)> {
    let spec = spec.trim();
    let (before_cmd, cmd) = if let Some(open) = spec.rfind(" (")
        && let Some(inner) = spec.get(open + 2..spec.len().saturating_sub(1))
        && spec.ends_with(')')
        && !inner.is_empty()
    {
        (spec.get(..open)?, Some(inner))
    } else {
        (spec, None)
    };
    let (file, line_str) = before_cmd.rsplit_once(':')?;
    let line_num = line_str.parse::<u32>().ok()?;
    Some((file, line_num, cmd))
}
