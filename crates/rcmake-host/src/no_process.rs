//! A host adapter that denies child processes and records the denied pipelines.

#![forbid(unsafe_code)]

use core::time::Duration;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcmake_base::BString;

use crate::{
    CommandSpec, CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, GracefulTimeout, Host,
    HostError, HostResult, Metadata, ReadRange, RunOutput, RunRequest, Stream, WriteMode,
};

// Forward services that these adapters do not change.
macro_rules! forward_host_services {
    ($base:ident) => {
        fn registry_host_view(&self) -> Option<crate::RegistryView> {
            self.$base.registry_host_view()
        }

        fn registry_value(
            &self,
            root: crate::RegistryRoot,
            subkey: &[u8],
            name: Option<&[u8]>,
            view: crate::RegistryView,
        ) -> HostResult<Option<BString>> {
            self.$base.registry_value(root, subkey, name, view)
        }

        fn write_out(&self, stream: Stream, bytes: &[u8]) {
            self.$base.write_out(stream, bytes);
        }

        fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
            self.$base.read_stdin(limit)
        }

        fn is_terminal(&self, stream: Stream) -> bool {
            self.$base.is_terminal(stream)
        }

        fn now(&self) -> FileTime {
            self.$base.now()
        }

        fn monotonic(&self) -> Duration {
            self.$base.monotonic()
        }

        fn sleep(&self, duration: Duration) {
            self.$base.sleep(duration);
        }

        fn time_zone(&self) -> jiff::tz::TimeZone {
            self.$base.time_zone()
        }

        fn current_exe(&self) -> HostResult<PathBuf> {
            self.$base.current_exe()
        }

        fn initial_cwd(&self) -> PathBuf {
            self.$base.initial_cwd()
        }

        fn system(&self) -> crate::SystemInfo {
            self.$base.system()
        }
    };
}
pub(crate) use forward_host_services;

const DENIED_LIMIT: usize = 128;
const MAX_LOG_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_STAGES: usize = 16;
const MAX_ARGS: usize = 64;
const MAX_REASON_BYTES: usize = 1024;
pub(super) const DENIED_MESSAGE: &str = "process execution is disabled in this host mode";

/// Check whether the process-denial adapter produced this error.
#[must_use]
pub fn is_process_denial(error: &HostError) -> bool {
    error.kind == std::io::ErrorKind::Unsupported && error.os_message == DENIED_MESSAGE
}

/// Host method that received the process request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeniedMethod {
    /// The request used [`Host::run`].
    Run,
    /// The request used [`Host::run_with_timeout_signal`].
    RunWithTimeout,
    /// The request used [`Host::run_bounded`].
    RunBounded,
    /// The request used [`Host::run_controlled`].
    RunControlled,
}

/// A process request that this host denied.
///
/// The working directory and pipeline can contain sensitive data. The debug
/// representation shows only the method and counts.
#[derive(Clone, PartialEq, Eq)]
pub struct DeniedCommand {
    /// Working directory of the request. An empty value can indicate omission.
    pub cwd: PathBuf,
    /// Retained stages and leading arguments, in request order.
    pub pipeline: Vec<CommandSpec>,
    /// Method that received the request.
    pub method: DeniedMethod,
    /// Reason supplied when this host was created.
    pub reason: Arc<str>,
    /// Number of stages omitted from the record.
    pub omitted_stages: usize,
    /// Number of arguments omitted, including arguments of omitted stages.
    pub omitted_args: usize,
    /// Number of working directory, program, and argument bytes omitted.
    pub omitted_bytes: usize,
}

impl core::fmt::Debug for DeniedCommand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeniedCommand")
            .field("method", &self.method)
            .field("retained_stages", &self.pipeline.len())
            .field("omitted_stages", &self.omitted_stages)
            .field("omitted_args", &self.omitted_args)
            .field("omitted_bytes", &self.omitted_bytes)
            .finish_non_exhaustive()
    }
}

/// Records and eviction counts since the last call to [`NoProcessHost::take_denied`].
#[derive(Debug, Default)]
pub struct DeniedLog {
    /// Retained records, oldest first.
    pub commands: VecDeque<DeniedCommand>,
    /// Number of complete records evicted from the log.
    pub omitted_entries: usize,
    /// Total request bytes in evicted records, including bytes omitted at capture.
    pub omitted_bytes: usize,
}

#[derive(Default)]
struct LogState {
    log: DeniedLog,
    costs: VecDeque<(usize, usize)>,
    bytes: usize,
}

impl LogState {
    fn evict(&mut self) {
        if let Some((cost, request_bytes)) = self.costs.pop_front() {
            self.log.commands.pop_front();
            self.bytes -= cost;
            self.log.omitted_entries = self.log.omitted_entries.saturating_add(1);
            self.log.omitted_bytes = self.log.omitted_bytes.saturating_add(request_bytes);
        }
    }
}

/// Forwards non-process operations and denies all child processes.
pub struct NoProcessHost<H: Host + ?Sized> {
    base: Arc<H>,
    reason: Arc<str>,
    denied: Mutex<LogState>,
}

impl<H: Host> NoProcessHost<H> {
    /// Wrap an owned host.
    #[must_use]
    pub fn new(base: H) -> Self {
        Self::from_shared(Arc::new(base))
    }

    /// Wrap an owned host and record the caller's reason for denying processes.
    #[must_use]
    pub fn with_reason(base: H, reason: &str) -> Self {
        Self::from_shared_with_reason(Arc::new(base), reason)
    }
}

impl<H: Host + ?Sized> NoProcessHost<H> {
    /// Wrap a shared host, including a shared trait object.
    #[must_use]
    pub fn from_shared(base: Arc<H>) -> Self {
        Self::from_shared_with_reason(base, DENIED_MESSAGE)
    }

    /// Wrap a shared host and record up to 1024 UTF-8 bytes of the caller's reason.
    ///
    /// The host uses a fixed error message. The reason is available in the log.
    #[must_use]
    pub fn from_shared_with_reason(base: Arc<H>, reason: &str) -> Self {
        let reason = if reason.is_empty() {
            DENIED_MESSAGE
        } else {
            reason
        };
        let len = reason.floor_char_boundary(MAX_REASON_BYTES);
        Self {
            base,
            reason: Arc::from(reason.get(..len).unwrap_or_default()),
            denied: Mutex::default(),
        }
    }

    /// Copy the retained requests, oldest first.
    #[must_use]
    pub fn denied_commands(&self) -> Vec<DeniedCommand> {
        self.denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .log
            .commands
            .iter()
            .cloned()
            .collect()
    }

    /// Check if the log has retained or omitted requests.
    #[must_use]
    pub fn has_denied_commands(&self) -> bool {
        let state = self
            .denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.log.commands.is_empty() || state.log.omitted_entries != 0
    }

    /// Transfer the records and eviction counts to the caller and clear the log.
    #[must_use]
    pub fn take_denied(&self) -> DeniedLog {
        let mut state = self
            .denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *state).log
    }

    fn deny<T>(&self, req: &RunRequest, method: DeniedMethod) -> HostResult<T> {
        let cwd_bytes = req.cwd.as_os_str().as_encoded_bytes().len();
        let (request_bytes, args) =
            req.pipeline
                .iter()
                .fold((cwd_bytes, 0_usize), |(bytes, args), stage| {
                    (
                        stage
                            .args
                            .iter()
                            .fold(bytes.saturating_add(stage.program.len()), |b, arg| {
                                b.saturating_add(arg.len())
                            }),
                        args.saturating_add(stage.args.len()),
                    )
                });
        let fixed_cost = size_of::<DeniedCommand>() + size_of::<(usize, usize)>();
        let reserve = fixed_cost
            .saturating_add(request_bytes)
            .saturating_add(req.pipeline.len().min(MAX_STAGES) * size_of::<CommandSpec>())
            .saturating_add(args.min(MAX_ARGS) * size_of::<BString>())
            .min(MAX_RECORD_BYTES);
        let mut state = self
            .denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.log.commands.len() >= DENIED_LIMIT || reserve > MAX_LOG_BYTES - state.bytes {
            state.evict();
        }

        let mut left = reserve - fixed_cost;
        let mut kept_bytes = 0;
        let mut kept_args = 0;
        let mut pipeline = Vec::new();
        let cwd = if cwd_bytes <= left {
            left -= cwd_bytes;
            kept_bytes += cwd_bytes;
            req.cwd.clone()
        } else {
            PathBuf::new()
        };
        for stage in req.pipeline.iter().take(MAX_STAGES) {
            let cost = stage.program.len().saturating_add(size_of::<CommandSpec>());
            if cost > left {
                break;
            }
            left -= cost;
            kept_bytes += stage.program.len();
            let mut args = Vec::new();
            for arg in stage.args.iter().take(MAX_ARGS - kept_args) {
                let cost = arg.len().saturating_add(size_of::<BString>());
                if cost > left {
                    break;
                }
                left -= cost;
                kept_bytes += arg.len();
                kept_args += 1;
                args.reserve_exact(1);
                args.push(arg.clone());
            }
            pipeline.reserve_exact(1);
            pipeline.push(CommandSpec {
                program: stage.program.clone(),
                args,
            });
        }
        let cost = reserve - left;
        state.bytes += cost;
        state.costs.push_back((cost, request_bytes));
        state.log.commands.push_back(DeniedCommand {
            cwd,
            method,
            reason: Arc::clone(&self.reason),
            omitted_stages: req.pipeline.len() - pipeline.len(),
            omitted_args: args.saturating_sub(kept_args),
            omitted_bytes: request_bytes.saturating_sub(kept_bytes),
            pipeline,
        });
        Err(HostError::unsupported(DENIED_MESSAGE))
    }
}

impl<H: Host + ?Sized> Host for NoProcessHost<H> {
    fn supports_process_execution(&self) -> bool {
        false
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn crate::ReadSeek + '_>> {
        self.base.open_read(p)
    }

    fn scratch_file(&self) -> HostResult<Box<dyn crate::ScratchFile + '_>> {
        self.base.scratch_file()
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn crate::ScratchFile + '_>> {
        self.base.scratch_file_in(dir)
    }

    fn write_from(
        &self,
        p: &Path,
        mode: WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        self.base.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        _graceful: Option<GracefulTimeout>,
        _hard_limit: Option<std::time::Duration>,
        _update: &mut crate::DeadlineUpdate<'_>,
    ) -> HostResult<crate::ControlledOutput> {
        self.deny(req, DeniedMethod::RunControlled)
    }

    fn run(
        &self,
        req: &RunRequest,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.deny(req, DeniedMethod::Run)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        _timeout: GracefulTimeout,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.deny(req, DeniedMethod::RunWithTimeout)
    }

    fn run_bounded(&self, req: &RunRequest, _limit: usize) -> HostResult<RunOutput> {
        self.deny(req, DeniedMethod::RunBounded)
    }

    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.base.metadata(p, follow)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.base.read(p, range)
    }

    fn read_chunks(&self, p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.base.read_chunks(p, consume)
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        self.base.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.base.read_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.base.create_dir_all(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.base.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.base.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.base.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.base.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.base.rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        self.base.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.base.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.base.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.base.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.base.canonicalize(p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.base.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.base.set_times(p, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.base.set_symlink_times(p, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.base.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.base.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.base.env_all()
    }

    forward_host_services!(base);
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use super::*;
    use crate::overlay::OverlayHost;
    use crate::{Input, Output, PureHost, TimeoutSignal};

    const ALL: ReadRange = ReadRange::All { limit: 128 };

    fn assert_missing<T>(result: HostResult<T>) {
        assert!(matches!(result, Err(err) if err.kind == ErrorKind::NotFound));
    }

    fn request(program: &str) -> RunRequest {
        RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args: vec!["argument".into()],
            }],
            cwd: PathBuf::from("/work"),
            env: vec![("KEY".into(), "VALUE".into())],
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: true,
        }
    }

    #[test]
    fn denies_each_process_method_without_calling_base_or_callback() -> HostResult<()> {
        let base = Arc::new(PureHost::default());
        base.set_command_handler(|host, _, _| {
            host.env_set(b"RAN", Some(b"yes"));
            Ok(RunOutput::default())
        })?;
        let host = NoProcessHost::from_shared(Arc::clone(&base));
        let normal = request("normal");
        let mut timed = request("timed");
        timed.pipeline.push(CommandSpec {
            program: "second".into(),
            args: vec!["second argument".into()],
        });
        let bounded = request("bounded");
        let mut callbacks = 0;
        let mut callback = |_, _: &[u8]| callbacks += 1;
        for result in [
            host.run(&normal, &mut callback),
            host.run_with_timeout_signal(
                &timed,
                GracefulTimeout {
                    signal: TimeoutSignal::Terminate,
                    grace: Duration::from_millis(20),
                },
                &mut callback,
            ),
            host.run_bounded(&bounded, 32),
        ] {
            assert!(matches!(
                result,
                Err(err) if err.kind == ErrorKind::Unsupported && err.os_message == DENIED_MESSAGE
            ));
        }
        assert_eq!(callbacks, 0);
        assert_eq!(base.env_get(b"RAN"), None);
        let log = host.take_denied();
        assert_eq!(log.omitted_entries, 0);
        assert_eq!(log.commands.len(), 3);
        let methods = [
            DeniedMethod::Run,
            DeniedMethod::RunWithTimeout,
            DeniedMethod::RunBounded,
        ];
        for ((record, req), method) in log
            .commands
            .iter()
            .zip([normal, timed, bounded])
            .zip(methods)
        {
            assert_eq!(record.cwd, req.cwd);
            assert!(record.pipeline == req.pipeline);
            assert_eq!(record.method, method);
            assert_eq!(&*record.reason, DENIED_MESSAGE);
        }
        assert!(host.take_denied().commands.is_empty());
        host.env_set(b"FORWARDED", Some(b"yes"));
        assert_eq!(base.env_get(b"FORWARDED"), Some("yes".into()));
        Ok(())
    }

    #[test]
    fn retains_the_most_recent_requests() {
        let host = NoProcessHost::new(PureHost::default());
        for id in 0..DENIED_LIMIT + 2 {
            assert!(host.run_bounded(&request(&id.to_string()), 0).is_err());
        }
        let expected: Vec<_> = (2..DENIED_LIMIT + 2)
            .map(|id| request(&id.to_string()).pipeline)
            .collect();
        let pipelines: Vec<_> = host
            .denied_commands()
            .into_iter()
            .map(|entry| entry.pipeline)
            .collect();
        assert_eq!(pipelines, expected);
    }

    #[test]
    fn bounds_record_bytes_stages_and_arguments() {
        let host = NoProcessHost::with_reason(PureHost::default(), "Local configure is required");
        let mut req = request("private-program");
        req.pipeline = vec![CommandSpec {
            program: "private-program".into(),
            args: vec!["private-arg".into(); MAX_ARGS + 1],
        }];
        req.pipeline.extend((0..MAX_STAGES).map(|_| CommandSpec {
            program: "stage".into(),
            args: vec!["arg".into()],
        }));
        assert!(host.run_bounded(&req, 0).is_err());

        let mut large = request("program");
        large.pipeline = vec![CommandSpec {
            program: "program".into(),
            args: vec![BString::from(vec![b'x'; MAX_RECORD_BYTES + 1])],
        }];
        assert!(host.run_bounded(&large, 0).is_err());

        let log = host.take_denied();
        assert_eq!(log.commands.len(), 2);
        let mut records = log.commands.into_iter();
        if let (Some(record), Some(large)) = (records.next(), records.next()) {
            assert_eq!(record.pipeline.len(), MAX_STAGES);
            assert_eq!(
                record
                    .pipeline
                    .iter()
                    .map(|stage| stage.args.len())
                    .sum::<usize>(),
                MAX_ARGS
            );
            assert_eq!(record.omitted_stages, 1);
            assert_eq!(record.omitted_args, MAX_STAGES + 1);
            assert!(!format!("{record:?}").contains("private-arg"));
            assert_eq!(&*record.reason, "Local configure is required");
            assert_eq!(large.omitted_bytes, MAX_RECORD_BYTES + 1);
            assert_eq!(large.omitted_args, 1);
        }
        assert!(host.take_denied().commands.is_empty());
    }

    #[test]
    fn reports_evictions_when_the_log_reaches_its_byte_limit() {
        let host = NoProcessHost::new(PureHost::default());
        let mut req = request("program");
        req.pipeline = vec![CommandSpec {
            program: "program".into(),
            args: vec![BString::from(vec![b'x'; MAX_RECORD_BYTES - 1024])],
        }];
        for _ in 0..DENIED_LIMIT {
            assert!(host.run_bounded(&req, 0).is_err());
        }
        let log = host.take_denied();
        assert!(log.omitted_entries > 0);
        assert_eq!(log.omitted_entries + log.commands.len(), DENIED_LIMIT);
        assert!(log.omitted_bytes >= log.omitted_entries * (MAX_RECORD_BYTES - 1024));
        assert_eq!(host.take_denied().omitted_entries, 0);
    }

    #[test]
    fn renames_a_base_tree_with_overlay_entries_and_tombstones() -> HostResult<()> {
        let base = Arc::new(PureHost::default());
        let old = Path::new("/work/old");
        let moved = Path::new("/work/moved");
        base.seed_file(&old.join("a"), b"old")?;
        base.seed_file(&old.join("b"), b"keep")?;
        base.seed_file(&old.join("c"), b"delete")?;
        base.symlink(Path::new("a"), &old.join("link"))?;
        let shared: Arc<dyn Host> = base.clone();
        let overlay = Arc::new(OverlayHost::from_shared(shared));
        let host = NoProcessHost::from_shared(Arc::clone(&overlay));
        host.write(&old.join("a"), b"+change", WriteMode::Append)?;
        host.remove_file(&old.join("c"))?;
        host.write(&old.join("d"), b"new", WriteMode::CreateNew)?;
        host.rename_noreplace(old, moved)?;
        let names: Vec<_> = host
            .read_dir(moved)?
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["a", "b", "d", "link"].map(BString::from));
        assert_eq!(host.canonicalize(&moved.join("link"))?, moved.join("a"));
        assert_eq!(
            overlay.read_overlay_file(&moved.join("link")),
            Some(b"old+change".to_vec())
        );
        assert_eq!(host.read(&moved.join("b"), ALL)?, b"keep");
        assert_missing(host.metadata(&moved.join("c"), Follow::No));
        assert_missing(host.read_dir(old));
        assert_eq!(
            overlay.deleted_paths(),
            [moved.join("c"), old.to_path_buf()]
        );
        assert_eq!(
            overlay.written_paths(),
            [moved.to_path_buf(), moved.join("a"), moved.join("d")]
        );

        host.remove_dir_all(moved)?;
        host.create_dir_all(moved)?;
        host.create_dir_all(old)?;
        assert!(host.read_dir(moved)?.is_empty());
        assert!(host.read_dir(old)?.is_empty());
        host.create_dir_all(&moved.join("child"))?;
        assert_eq!(host.metadata(moved, Follow::No)?.nlink, 3);
        assert_eq!(overlay.audit().overlay_files, 0);
        assert_eq!(base.read(&old.join("a"), ALL)?, b"old");
        assert_eq!(base.read(&old.join("c"), ALL)?, b"delete");
        assert_missing(base.metadata(&old.join("d"), Follow::No));
        assert_missing(base.read_dir(moved));
        Ok(())
    }

    #[test]
    fn keeps_metadata_hard_links_locks_and_environment_in_the_overlay() -> HostResult<()> {
        let base = Arc::new(PureHost::default());
        let src = Path::new("/work/file");
        let alias = Path::new("/work/dirs/hard");
        base.seed_file(src, b"base")?;
        base.seed_file(alias, b"replace")?;
        base.remove_file(alias)?;
        base.hard_link(src, alias)?;
        base.env_set(b"MODE", Some(b"base"));
        let original = base.metadata(src, Follow::No)?;
        let overlay = Arc::new(OverlayHost::from_shared(Arc::clone(&base)));
        let host = NoProcessHost::from_shared(Arc::clone(&overlay));
        let lock = host.lock_file(src, Some(Duration::ZERO))?;
        assert!(
            matches!(host.lock_file(alias, Some(Duration::ZERO)), Err(err) if err.kind == ErrorKind::TimedOut)
        );
        drop(lock);
        host.write(alias, b"new", WriteMode::Truncate)?;
        host.set_permissions(alias, 0o700)?;
        host.set_times(src, None, Some(FileTime { secs: 10, nanos: 5 }))?;
        let dest = Path::new("/work/copy");
        host.copy_file(
            alias,
            dest,
            CopyOptions {
                preserve_permissions: true,
                preserve_mtime: true,
            },
        )?;
        host.remove_dir_all(Path::new("/work/dirs"))?;
        assert_eq!(host.metadata(src, Follow::No)?.nlink, 1);
        assert_eq!(host.metadata(dest, Follow::Yes)?.mode, 0o700);
        assert_eq!(
            host.metadata(dest, Follow::Yes)?.mtime,
            FileTime { secs: 10, nanos: 5 }
        );
        assert_eq!(host.read(src, ALL)?, b"new");
        host.env_set(b"MODE", None);
        assert_eq!(host.env_get(b"MODE"), None);
        assert_eq!(overlay.audit().environment_overrides, 1);
        assert_eq!(base.env_get(b"MODE"), Some("base".into()));
        assert_eq!(base.metadata(src, Follow::No)?, original);
        assert_eq!(base.read(alias, ALL)?, b"base");
        assert_missing(base.read(dest, ALL));
        Ok(())
    }

    #[test]
    fn overlay_denies_a_process_with_a_filesystem_output() -> HostResult<()> {
        let base = Arc::new(PureHost::default());
        let escaped = Path::new("/work/escaped");
        base.set_command_handler(move |base, _, _| {
            base.seed_file(escaped, b"unexpected")?;
            Ok(RunOutput::default())
        })?;
        let host = OverlayHost::from_shared(Arc::clone(&base));
        let mut req = request("mock");
        req.stdout = Output::File {
            path: escaped.to_path_buf(),
            append: false,
        };
        assert!(
            matches!(host.run_bounded(&req, 64), Err(err) if err.kind == ErrorKind::Unsupported)
        );
        assert_missing(base.read(escaped, ALL));
        Ok(())
    }
}
