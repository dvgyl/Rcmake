//! CLI integration for the clean-room `ccmake` interactive cache editor.
//!
//! Bridges [`rcmake_ccmake`] to `rcmake-cli`'s cache loader, distinct
//! configure/generate session phases ([`crate::configure::configure_only`] and
//! [`crate::configure::RetainedConfigure::generate`]), and an output/process
//! capturing [`Host`] wrapper so neither diagnostics nor child
//! `execute_process()` commands can corrupt the TUI display or steal input.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcmake_base::{BString, list};
use rcmake_ccmake::layout::ERR_INIT_FAILED;
use rcmake_ccmake::terminal::Terminal;
use rcmake_ccmake::{
    CacheDriver, CacheSnapshot, DirItem, DriverError, EntryEdit, EntrySnapshot, LogLine, RunReport,
};
use rcmake_eval::CacheDefine;
use rcmake_host::{
    ControlledOutput, DeadlineUpdate, FileKind, Follow, GracefulTimeout, Host, HostResult, Input,
    Output, RunOutput, RunRequest, Stream, WriteScope, forward_host, path,
};
use rcmake_model::CacheType;

use crate::configure::{self, RetainedConfigure};
use crate::options::{self, CacheEdit, ParseResult};
use crate::persona::Persona;
use crate::{Tool, cache, help, resolve_cli_path};

/// Maximum total captured bytes retained per configure/generate log (4 MiB).
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of log lines retained per configure/generate run.
const MAX_CAPTURE_LINES: usize = 16_384;

/// Entry point for the `ccmake` tool persona.
pub(crate) fn run(argv: &[BString], persona: Persona, host: Arc<dyn Host>) -> i32 {
    let rest = argv.get(1..).unwrap_or(&[]);
    if rest.is_empty() {
        host.write_out(Stream::Stdout, help::CCMAKE_SHORT_USAGE);
        return 0;
    }
    if let Some(code) = help::run(argv.first(), rest, Tool::Ccmake, persona, host.as_ref()) {
        return code;
    }

    #[cfg(unix)]
    {
        if !host.is_terminal(Stream::Stdout) {
            host.write_out(Stream::Stderr, ERR_INIT_FAILED.as_bytes());
            return 1;
        }
        let mut terminal = match rcmake_ccmake::terminal::native::NativeTerminal::open() {
            Ok(term) => term,
            Err(_) => {
                host.write_out(Stream::Stderr, ERR_INIT_FAILED.as_bytes());
                return 1;
            }
        };
        run_with_terminal(argv, persona, host, &mut terminal)
    }

    #[cfg(not(unix))]
    {
        let _ = (persona,);
        host.write_out(Stream::Stderr, ERR_INIT_FAILED.as_bytes());
        1
    }
}

/// Run `ccmake` against an explicit [`Terminal`] (used by native Unix startup and tests).
pub(crate) fn run_with_terminal<T: Terminal>(
    argv: &[BString],
    persona: Persona,
    host: Arc<dyn Host>,
    terminal: &mut T,
) -> i32 {
    let rest = argv.get(1..).unwrap_or(&[]);
    let parsed = rcmake_ccmake::cli::split_args(rest);
    let argv0 = argv
        .first()
        .cloned()
        .unwrap_or_else(|| BString::from("ccmake"));
    let mut driver = CliCacheDriver::new(argv0, parsed.cmake_args, persona, Arc::clone(&host));
    let mut stderr = HostStderrWriter {
        host: host.as_ref(),
    };
    rcmake_ccmake::run(terminal, &mut driver, &mut stderr)
}

struct HostStderrWriter<'a> {
    host: &'a dyn Host,
}

impl io::Write for HostStderrWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.host.write_out(Stream::Stderr, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct CaptureLog {
    lines: Vec<LogLine>,
    stdout_partial: Vec<u8>,
    stderr_partial: Vec<u8>,
    total_bytes: usize,
    truncated: bool,
    has_non_status: bool,
}

impl CaptureLog {
    fn reset(&mut self) {
        self.lines.clear();
        self.stdout_partial.clear();
        self.stderr_partial.clear();
        self.total_bytes = 0;
        self.truncated = false;
        self.has_non_status = false;
    }

    fn push(&mut self, stream: Stream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let is_error = stream == Stream::Stderr;
        if is_error {
            self.has_non_status = true;
        }
        if self.truncated {
            return;
        }

        for &byte in bytes {
            if byte == b'\n' {
                let mut line = if is_error {
                    std::mem::take(&mut self.stderr_partial)
                } else {
                    std::mem::take(&mut self.stdout_partial)
                };
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.push_line(is_error, line);
                if self.truncated {
                    return;
                }
            } else {
                let buf = if is_error {
                    &mut self.stderr_partial
                } else {
                    &mut self.stdout_partial
                };
                if buf.len() < 64 * 1024 {
                    buf.push(byte);
                }
            }
        }
    }

    fn push_line(&mut self, is_error: bool, line: Vec<u8>) {
        if self.lines.len() >= MAX_CAPTURE_LINES
            || self.total_bytes.saturating_add(line.len()) > MAX_CAPTURE_BYTES
        {
            self.truncated = true;
            self.lines.push(LogLine {
                is_error: false,
                text: BString::from("... [output truncated]"),
            });
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(line.len());
        self.lines.push(LogLine {
            is_error,
            text: BString::from(line),
        });
    }

    fn drain(&mut self) -> (Vec<LogLine>, bool) {
        let mut out_partial = std::mem::take(&mut self.stdout_partial);
        if !out_partial.is_empty() && !self.truncated {
            if out_partial.last() == Some(&b'\r') {
                out_partial.pop();
            }
            self.push_line(false, out_partial);
        }
        let mut err_partial = std::mem::take(&mut self.stderr_partial);
        if !err_partial.is_empty() && !self.truncated {
            if err_partial.last() == Some(&b'\r') {
                err_partial.pop();
            }
            self.push_line(true, err_partial);
        }
        let has_non_status = self.has_non_status;
        let lines = std::mem::take(&mut self.lines);
        self.reset();
        (lines, has_non_status)
    }
}

/// [`Host`] wrapper active during `ccmake` configure and generate runs.
///
/// Intercepts BOTH [`Host::write_out`] AND all process execution methods so
/// inherited child stdout/stderr streams are captured into the bounded TUI log
/// and inherited stdin is replaced with non-interactive `/dev/null` (`Input::Null`).
/// Explicit file redirections (`Output::File`) and output variables
/// (`Output::Capture`) are preserved untouched.
struct CaptureHost {
    inner: Arc<dyn Host>,
    log: Mutex<CaptureLog>,
}

impl CaptureHost {
    fn new(inner: Arc<dyn Host>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            log: Mutex::default(),
        })
    }

    fn reset(&self) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reset();
    }

    fn drain(&self) -> (Vec<LogLine>, bool) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
    }

    fn capture(&self, stream: Stream, bytes: &[u8]) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(stream, bytes);
    }

    fn run_captured(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
        exec: impl FnOnce(&RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>,
    ) -> HostResult<RunOutput> {
        let (req, cap_stdout, cap_stderr) = capture_request(req);
        let mut saw_stdout = false;
        let mut saw_stderr = false;
        let mut output = exec(&req, &mut |stream, bytes| match stream {
            Stream::Stdout if cap_stdout => {
                saw_stdout = true;
                self.capture(Stream::Stdout, bytes);
            }
            Stream::Stderr if cap_stderr => {
                saw_stderr = true;
                self.capture(Stream::Stderr, bytes);
            }
            _ => on_output(stream, bytes),
        })?;
        if cap_stdout {
            if !saw_stdout && !output.stdout.is_empty() {
                self.capture(Stream::Stdout, &output.stdout);
            }
            output.stdout.clear();
        }
        if cap_stderr {
            if !saw_stderr && !output.stderr.is_empty() {
                self.capture(Stream::Stderr, &output.stderr);
            }
            output.stderr.clear();
        }
        Ok(output)
    }

    fn run_bounded_captured(
        &self,
        req: &RunRequest,
        exec: impl FnOnce(&RunRequest) -> HostResult<RunOutput>,
    ) -> HostResult<RunOutput> {
        let (mut req, cap_stdout, cap_stderr) = capture_request(req);
        req.tee = false;
        let mut result = exec(&req)?;
        if cap_stdout {
            let bytes = std::mem::take(&mut result.stdout);
            self.capture(Stream::Stdout, &bytes);
        }
        if cap_stderr {
            let bytes = std::mem::take(&mut result.stderr);
            self.capture(Stream::Stderr, &bytes);
        }
        Ok(result)
    }
}

fn capture_request(request: &RunRequest) -> (RunRequest, bool, bool) {
    let mut req = request.clone();
    if req.stdin == Input::Inherit {
        req.stdin = Input::Null;
    }
    let cap_stdout = req.stdout == Output::Inherit;
    let cap_stderr = req.stderr == Output::Inherit;
    if cap_stdout {
        req.stdout = Output::Capture;
    }
    if cap_stderr {
        req.stderr = Output::Capture;
    }
    req.tee |= cap_stdout || cap_stderr;
    (req, cap_stdout, cap_stderr)
}

impl Host for CaptureHost {
    forward_host!(inner, fs, env);

    fn supports_process_execution(&self) -> bool {
        self.inner.supports_process_execution()
    }

    fn open_jobserver(&self, auth: &[u8]) -> Option<rcmake_host::JobServerPipe> {
        self.inner.open_jobserver(auth)
    }

    fn open_read_with_metadata(
        &self,
        path: &Path,
    ) -> HostResult<(
        Box<dyn rcmake_host::ReadSeek + '_>,
        Option<rcmake_host::Metadata>,
    )> {
        self.inner.open_read_with_metadata(path)
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |r, cb| self.inner.run(r, cb))
    }

    fn run_confined(
        &self,
        req: &RunRequest,
        scope: WriteScope<'_>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |r, cb| {
            self.inner.run_confined(r, scope, cb)
        })
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |r, cb| {
            self.inner.run_with_timeout_signal(r, timeout, cb)
        })
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<std::time::Duration>,
        update: &mut DeadlineUpdate<'_>,
    ) -> HostResult<ControlledOutput> {
        let (req, cap_stdout, cap_stderr) = capture_request(req);
        let mut saw_stdout = false;
        let mut saw_stderr = false;
        let mut result =
            self.inner
                .run_controlled(&req, graceful, hard_limit, &mut |stream, bytes| {
                    match stream {
                        Stream::Stdout if cap_stdout => {
                            saw_stdout = true;
                            self.capture(Stream::Stdout, bytes);
                        }
                        Stream::Stderr if cap_stderr => {
                            saw_stderr = true;
                            self.capture(Stream::Stderr, bytes);
                        }
                        _ => {}
                    }
                    update(stream, bytes)
                })?;
        if cap_stdout {
            if !saw_stdout && !result.output.stdout.is_empty() {
                self.capture(Stream::Stdout, &result.output.stdout);
            }
            result.output.stdout.clear();
        }
        if cap_stderr {
            if !saw_stderr && !result.output.stderr.is_empty() {
                self.capture(Stream::Stderr, &result.output.stderr);
            }
            result.output.stderr.clear();
        }
        Ok(result)
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.run_bounded_captured(req, |r| self.inner.run_bounded(r, limit))
    }

    fn run_bounded_confined(
        &self,
        req: &RunRequest,
        limit: usize,
        scope: WriteScope<'_>,
    ) -> HostResult<RunOutput> {
        self.run_bounded_captured(req, |r| self.inner.run_bounded_confined(r, limit, scope))
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.inner.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<BString>> {
        self.inner.registry_value(root, subkey, name, view)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.capture(stream, bytes);
    }

    fn read_stdin(&self, _limit: u64) -> HostResult<Vec<u8>> {
        Ok(Vec::new())
    }

    fn is_terminal(&self, _stream: Stream) -> bool {
        false
    }

    fn terminal_width(&self, _stream: Stream) -> Option<usize> {
        None
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

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        self.inner.system()
    }

    fn effective_uid(&self) -> Option<u32> {
        self.inner.effective_uid()
    }
}

struct CliCacheDriver {
    argv0: BString,
    cmake_args: Vec<BString>,
    persona: Persona,
    base_host: Arc<dyn Host>,
    capture_host: Arc<CaptureHost>,
    initial_configured: bool,
    binary_dir: Option<PathBuf>,
    retained: Option<RetainedConfigure>,
}

impl CliCacheDriver {
    fn new(
        argv0: BString,
        cmake_args: Vec<BString>,
        persona: Persona,
        base_host: Arc<dyn Host>,
    ) -> Self {
        let capture_host = CaptureHost::new(Arc::clone(&base_host));
        Self {
            argv0,
            cmake_args,
            persona,
            base_host,
            capture_host,
            initial_configured: false,
            binary_dir: None,
            retained: None,
        }
    }

    fn resolve_bin_and_opts(&mut self) -> Result<(PathBuf, options::Options), i32> {
        match options::parse(&self.cmake_args, self.persona, self.capture_host.as_ref()) {
            ParseResult::Run(opts) => {
                let bin = configure::resolve_binary_dir(&opts, self.capture_host.as_ref());
                self.binary_dir = Some(bin.clone());
                Ok((bin, *opts))
            }
            ParseResult::Exit(code) => Err(code),
        }
    }

    fn build_argv(&self) -> Vec<BString> {
        let mut argv = Vec::with_capacity(self.cmake_args.len() + 1);
        argv.push(self.argv0.clone());
        argv.extend_from_slice(&self.cmake_args);
        argv
    }
}

impl CacheDriver for CliCacheDriver {
    fn load(&mut self) -> Result<CacheSnapshot, DriverError> {
        self.capture_host.reset();
        let (bin, initial_opts) = if let Some(bin) = self.binary_dir.clone() {
            (bin, None)
        } else {
            match self.resolve_bin_and_opts() {
                Ok((bin, opts)) => (bin, Some(opts)),
                Err(_) => {
                    // Fall back to cwd if CLI args cannot be resolved yet; configure will report it.
                    (self.base_host.initial_cwd(), None)
                }
            }
        };

        let mut loaded = cache::load(self.capture_host.as_ref(), &bin)
            .map_err(|err| DriverError::new(err.to_string()))?;

        // Before the first configure, reflect typed CLI -D/-U flags in the initial view.
        if !self.initial_configured
            && let Some(opts) = initial_opts
        {
            for edit in opts.cache_edits {
                match edit {
                    CacheEdit::Define(def) => {
                        let def = cache::normalize_definition(self.base_host.as_ref(), def);
                        cache::apply(&mut loaded, def);
                    }
                    CacheEdit::Remove(pattern) => {
                        configure::remove_cache_glob(&mut loaded, &pattern);
                    }
                    CacheEdit::Preload(_) => {}
                }
            }
        }

        Ok(snapshot_from_cache(&loaded))
    }

    fn invalidate(&mut self) {
        self.retained = None;
    }

    fn configure(&mut self, edits: &[EntryEdit]) -> RunReport {
        self.retained = None;
        self.capture_host.reset();

        let (bin, mut opts) = match self.resolve_bin_and_opts() {
            Ok(resolved) => resolved,
            Err(code) => {
                let (lines, has_non_status) = self.capture_host.drain();
                return RunReport {
                    exit_code: if code == 0 { 1 } else { code },
                    lines,
                    has_non_status: has_non_status || code != 0,
                    new_entries: BTreeSet::new(),
                };
            }
        };

        for (ignore_key, ignore) in [
            (
                "_RCMAKE_IGNORE_LINK_WARNING_AS_ERROR",
                opts.ignore_link_warning_as_error,
            ),
            (
                "_RCMAKE_NO_WARNING_AS_ERROR",
                opts.ignore_compile_warning_as_error,
            ),
        ] {
            if ignore {
                opts.cache_edits.push(CacheEdit::Define(CacheDefine {
                    name: ignore_key.into(),
                    ty: Some(CacheType::Internal),
                    value: "ON".into(),
                }));
            } else if !opts.regenerate_during_build {
                opts.cache_edits.push(CacheEdit::Remove(ignore_key.into()));
            }
        }

        if self.initial_configured {
            // Do not re-apply original CLI -D/-U/-C/--fresh after the initial configure.
            opts.cache_edits.clear();
            opts.fresh = false;
        }

        let cache_exists = self
            .base_host
            .metadata(&bin.join("CMakeCache.txt"), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File);

        if !edits.is_empty() {
            if cache_exists {
                if let Ok(mut disk_cache) = cache::load(self.capture_host.as_ref(), &bin) {
                    apply_tui_edits(self.base_host.as_ref(), &mut disk_cache, edits);
                    let _ = cache::save(self.capture_host.as_ref(), &bin, &disk_cache);
                }
            } else {
                for edit in edits {
                    match edit {
                        EntryEdit::Set { name, ty, value } => {
                            opts.cache_edits.push(CacheEdit::Define(CacheDefine {
                                name: name.clone(),
                                ty: Some(*ty),
                                value: value.clone(),
                            }));
                        }
                        EntryEdit::Remove { name } => {
                            opts.cache_edits.push(CacheEdit::Remove(name.clone()));
                        }
                    }
                }
            }
        }

        let argv = self.build_argv();
        let host: Arc<dyn Host> = Arc::clone(&self.capture_host) as Arc<dyn Host>;
        let result = configure::configure_only(argv, opts, host);

        if self
            .base_host
            .metadata(&bin.join("CMakeCache.txt"), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            self.initial_configured = true;
        }

        let (lines, has_non_status) = self.capture_host.drain();
        match result {
            Ok(retained) => {
                self.retained = Some(retained);
                RunReport {
                    exit_code: 0,
                    lines,
                    has_non_status,
                    new_entries: BTreeSet::new(),
                }
            }
            Err(code) => RunReport {
                exit_code: if code == 0 { 1 } else { code },
                lines,
                has_non_status: true,
                new_entries: BTreeSet::new(),
            },
        }
    }

    fn generate(&mut self) -> RunReport {
        self.capture_host.reset();
        let Some(retained) = self.retained.take() else {
            return RunReport {
                exit_code: 1,
                lines: vec![LogLine {
                    is_error: true,
                    text: BString::from("CMake Error: Configure must succeed before generating."),
                }],
                has_non_status: true,
                new_entries: BTreeSet::new(),
            };
        };
        let exit_code = retained.generate();
        let (lines, has_non_status) = self.capture_host.drain();
        RunReport {
            exit_code,
            lines,
            has_non_status: has_non_status || exit_code != 0,
            new_entries: BTreeSet::new(),
        }
    }

    fn list_dir(&mut self, dir: &[u8]) -> Vec<DirItem> {
        let path = resolve_cli_path(self.base_host.as_ref(), dir);
        let Ok(entries) = self.base_host.read_dir(&path) else {
            return Vec::new();
        };
        let mut items: Vec<DirItem> = entries
            .into_iter()
            .filter(|entry| !entry.name.starts_with(b"._"))
            .map(|entry| {
                let is_dir = match entry.kind {
                    FileKind::Dir => true,
                    FileKind::Symlink => {
                        let full = path.join(path::to_native(entry.name.as_slice()));
                        self.base_host
                            .metadata(&full, Follow::Yes)
                            .is_ok_and(|meta| meta.kind == FileKind::Dir)
                    }
                    _ => false,
                };
                DirItem {
                    name: entry.name,
                    is_dir,
                }
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }
}

fn apply_tui_edits(host: &dyn Host, disk_cache: &mut rcmake_model::Cache, edits: &[EntryEdit]) {
    for edit in edits {
        match edit {
            EntryEdit::Set { name, ty, value } => {
                let normalized = cache::typed_value(host, Some(*ty), value);
                let changed = disk_cache
                    .get(name.as_slice())
                    .is_none_or(|existing| existing.value != normalized);
                cache::insert(disk_cache, name.as_slice(), normalized, Some(*ty));
                if changed && let Some(entry) = disk_cache.get_mut(name.as_slice()) {
                    entry.props.set("MODIFIED".into(), "ON".into());
                }
            }
            EntryEdit::Remove { name } => {
                disk_cache.remove(name.as_slice());
            }
        }
    }
}

fn snapshot_from_cache(loaded: &rcmake_model::Cache) -> CacheSnapshot {
    let entries = loaded
        .iter()
        .map(|(name, entry)| {
            let strings = entry
                .props
                .get(b"STRINGS")
                .map(|raw| list::split(raw.as_slice(), list::Empty::Drop))
                .unwrap_or_default();
            EntrySnapshot {
                name: name.clone(),
                ty: entry.ty,
                value: entry.value.clone(),
                help: entry.help.clone(),
                advanced: entry.advanced,
                strings,
            }
        })
        .collect();
    CacheSnapshot { entries }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use rcmake_base::BString;
    use rcmake_ccmake::terminal::scripted::ScriptedTerminal;
    use rcmake_host::{
        Follow, Host, HostResult, Input, Output, ProcessResult, PureHost, ReadRange, RunOutput,
        Stream,
    };

    use super::*;

    fn seed_two_option_project(host: &PureHost) -> HostResult<()> {
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            br#"cmake_minimum_required(VERSION 4.5)
project(TwoOpt NONE)
option(OPT "Enable feature" OFF)
option(OTHER "Other feature" ON)
if(OPT)
  file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/feature.txt" "enabled\n")
else()
  file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/feature.txt" "disabled\n")
endif()
"#,
        )?;
        Ok(())
    }

    fn argv(items: &[&str]) -> Vec<BString> {
        items.iter().copied().map(BString::from).collect()
    }

    #[test]
    fn non_tty_ccmake_emits_ncurses_init_error_and_exits_one() {
        let host = crate::driver_tests::project_host();
        let code = crate::run(&argv(&["ccmake", "-S", "src", "-B", "bld"]), host.clone());
        assert_eq!(code, 1);
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
        assert_eq!(
            host.stream_bytes(Stream::Stderr),
            BString::from("Error: ncurses initialization failed\n")
        );
    }

    #[test]
    fn c_writes_cache_without_build_ninja_and_g_generates_without_reconfiguring() -> HostResult<()>
    {
        let host = crate::driver_tests::project_host();
        let configure_count = Arc::new(AtomicUsize::new(0));
        let counter_ref = Arc::clone(&configure_count);
        host.seed_file(Path::new("/tools/probe"), b"")?;
        host.set_permissions(Path::new("/tools/probe"), 0o755)?;
        host.set_command_handler(move |_, req, _| {
            if req.pipeline[0].program == "/tools/probe" {
                counter_ref.fetch_add(1, Ordering::SeqCst);
                // Verify Audit Finding #7: inherited stdin became Null and outputs became Capture!
                assert_eq!(req.stdin, Input::Null);
                assert_eq!(req.stdout, Output::Capture);
                assert_eq!(req.stderr, Output::Capture);
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: b"CHILD_STDOUT_MARKER\n".to_vec(),
                    stderr: b"CHILD_STDERR_MARKER\n".to_vec(),
                });
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: b"1.12.1\n".to_vec(),
                stderr: Vec::new(),
            })
        })?;

        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            br#"cmake_minimum_required(VERSION 4.5)
project(PhaseCheck NONE)
option(OPT "Enable feature" OFF)
execute_process(COMMAND /tools/probe)
"#,
        )?;

        // Phase 1: run ONLY `c` then `e` (dismiss stderr log) then `q` — assert NO build.ninja!
        let mut term1 = ScriptedTerminal::new(80, 24);
        term1.push_bytes(b"ceq");
        let args = argv(&["ccmake", "-S", "src", "-B", "bld", "-G", "Ninja"]);
        assert_eq!(
            run_with_terminal(&args, Persona::Compat, host.clone(), &mut term1),
            0
        );
        assert_eq!(configure_count.load(Ordering::SeqCst), 1);
        assert!(
            host.metadata(Path::new("/work/bld/CMakeCache.txt"), Follow::Yes)
                .is_ok(),
            "c must write CMakeCache.txt"
        );
        assert!(
            host.metadata(Path::new("/work/bld/build.ninja"), Follow::Yes)
                .is_err(),
            "c must NOT write build.ninja before g"
        );
        let screen_bytes = String::from_utf8_lossy(term1.output_bytes());
        assert!(
            screen_bytes.contains("CHILD_STDOUT_MARKER"),
            "log must capture child stdout: {screen_bytes}"
        );
        assert!(
            screen_bytes.contains("CHILD_STDERR_MARKER"),
            "log must capture child stderr: {screen_bytes}"
        );
        // Host stdout/stderr must remain completely untouched by child output!
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
        assert!(host.stream_bytes(Stream::Stderr).is_empty());

        // Phase 2: open existing build dir, toggle OPT -> ON, press `c`, `e`, then `g`.
        // Assert `g` writes build.ninja WITHOUT incrementing configure_count!
        let mut term2 = ScriptedTerminal::new(80, 24);
        term2.push_bytes(b"/OPT\n\rceg");
        assert_eq!(
            run_with_terminal(&args, Persona::Compat, host.clone(), &mut term2),
            0
        );
        // Exactly ONE more configure ran (from `c`); `g` did NOT re-run configure!
        assert_eq!(configure_count.load(Ordering::SeqCst), 2);
        assert!(
            host.metadata(Path::new("/work/bld/build.ninja"), Follow::Yes)
                .is_ok(),
            "g must generate build.ninja"
        );
        let cache_bytes = host.read(
            Path::new("/work/bld/CMakeCache.txt"),
            ReadRange::All { limit: 1 << 20 },
        )?;
        let cache_text = String::from_utf8_lossy(&cache_bytes);
        assert!(cache_text.contains("OPT:BOOL=ON\n"), "{cache_text}");
        assert!(
            cache_text.contains("OPT-MODIFIED:INTERNAL=ON\n"),
            "{cache_text}"
        );
        Ok(())
    }

    #[test]
    fn ccmake_c_then_g_matches_plain_cmake_byte_for_byte() -> HostResult<()> {
        let host_ccmake = crate::driver_tests::project_host();
        seed_two_option_project(&host_ccmake)?;
        let mut term = ScriptedTerminal::new(80, 24);
        // Cold configure (`c` adds new entries), second `c` enables `[g]`, then `g` generates.
        term.push_bytes(b"ccg");
        let code = run_with_terminal(
            &argv(&[
                "ccmake", "-S", "src", "-B", "bld", "-G", "Ninja", "-DOPT=ON",
            ]),
            Persona::Compat,
            host_ccmake.clone(),
            &mut term,
        );
        assert_eq!(code, 0);

        let host_cmake = crate::driver_tests::project_host();
        seed_two_option_project(&host_cmake)?;
        // Run cold + warm `cmake` matching the two configure passes of `c`, `c`, `g`.
        for _ in 0..2 {
            assert_eq!(
                crate::run(
                    &argv(&["cmake", "-S", "src", "-B", "bld", "-G", "Ninja", "-DOPT=ON"]),
                    host_cmake.clone(),
                ),
                0
            );
        }

        let read = |h: &PureHost, rel: &str| -> HostResult<Vec<u8>> {
            h.read(
                &Path::new("/work/bld").join(rel),
                ReadRange::All { limit: 1 << 20 },
            )
        };
        assert_eq!(
            read(&host_ccmake, "CMakeCache.txt")?,
            read(&host_cmake, "CMakeCache.txt")?
        );
        assert_eq!(
            read(&host_ccmake, "build.ninja")?,
            read(&host_cmake, "build.ninja")?
        );
        assert_eq!(read(&host_ccmake, "feature.txt")?, b"enabled\n");
        Ok(())
    }

    #[test]
    fn ccmake_interactive_toggle_generates_ninja_matching_cmake_define() -> HostResult<()> {
        let host_ccmake = crate::driver_tests::project_host();
        seed_two_option_project(&host_ccmake)?;
        let mut term = ScriptedTerminal::new(80, 24);
        // Configure cold (`c`), search OPT and toggle OFF -> ON, configure (`c`), generate (`g`).
        term.push_bytes(b"c/OPT\n\rcg");
        let code = run_with_terminal(
            &argv(&["ccmake", "-S", "src", "-B", "bld", "-G", "Ninja"]),
            Persona::Compat,
            host_ccmake.clone(),
            &mut term,
        );
        assert_eq!(code, 0);

        let host_cmake = crate::driver_tests::project_host();
        seed_two_option_project(&host_cmake)?;
        // Run cold + warm `cmake` matching the two configure passes of `c`, `c`, `g`.
        for _ in 0..2 {
            assert_eq!(
                crate::run(
                    &argv(&["cmake", "-S", "src", "-B", "bld", "-G", "Ninja", "-DOPT=ON"]),
                    host_cmake.clone(),
                ),
                0
            );
        }

        let read = |h: &PureHost, rel: &str| -> HostResult<Vec<u8>> {
            h.read(
                &Path::new("/work/bld").join(rel),
                ReadRange::All { limit: 1 << 20 },
            )
        };
        assert_eq!(
            read(&host_ccmake, "build.ninja")?,
            read(&host_cmake, "build.ninja")?
        );
        assert_eq!(read(&host_ccmake, "feature.txt")?, b"enabled\n");
        Ok(())
    }
}
