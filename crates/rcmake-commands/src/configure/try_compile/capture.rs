use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, GracefulTimeout, Host, HostResult,
    Metadata, Output, ReadRange, RunOutput, RunRequest, Stream, SystemInfo, WriteMode,
};

use super::TIMEOUT;

/// Capture test-project output while forwarding operations to the parent host.
pub(super) struct CapturingHost {
    parent: Arc<dyn Host>,
    output: Mutex<BString>,
    /// Messages that the test project configure wrote to standard error. CMake shows
    /// them in the calling project's output.
    diagnostics: Mutex<BString>,
    pub(super) trace_output: OnceLock<Option<PathBuf>>,
    system: SystemInfo,
    cwd: PathBuf,
}

impl CapturingHost {
    fn new(parent: Arc<dyn Host>, cwd: &Path, system: SystemInfo) -> Arc<Self> {
        Arc::new(Self {
            system,
            parent,
            output: Mutex::default(),
            diagnostics: Mutex::default(),
            trace_output: OnceLock::new(),
            cwd: cwd.to_owned(),
        })
    }

    #[cfg(all(test, feature = "native", unix))]
    pub(super) fn for_test(parent: Arc<dyn Host>, cwd: &Path, system: SystemInfo) -> Arc<Self> {
        Self::new(parent, cwd, system)
    }

    /// Run `configure` and return its result, all captured output, and the messages
    /// that the test project wrote to standard error.
    pub(super) fn capture<T>(
        parent: Arc<dyn Host>,
        cwd: &Path,
        configure: impl FnOnce(Arc<Self>) -> T,
    ) -> (T, BString, BString) {
        let system = parent.system();
        let host = Self::new(parent, cwd, system);
        let outcome = configure(Arc::clone(&host));
        let take = |buffer: &Mutex<BString>| {
            std::mem::take(
                &mut *buffer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        };
        (outcome, take(&host.output), take(&host.diagnostics))
    }

    /// Keep the inherited standard error of a child process for the test result only.
    #[cfg(all(feature = "native", any(unix, windows)))]
    pub(super) fn capture_inherited_stderr(&self, bytes: &[u8]) {
        self.capture_process_output(bytes);
    }

    /// Keep process output for the test result only.
    fn capture_process_output(&self, bytes: &[u8]) {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
    }

    fn run_captured(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
        run: impl FnOnce(&RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>,
    ) -> HostResult<RunOutput> {
        let (request, stdout, stderr) = capture_request(request);
        let mut output = run(&request, &mut |stream, bytes| {
            if (stream == Stream::Stdout && stdout) || (stream == Stream::Stderr && stderr) {
                self.capture_process_output(bytes);
            } else {
                on_output(stream, bytes);
            }
        })?;
        if stdout {
            output.stdout.clear();
        }
        if stderr {
            output.stderr.clear();
        }
        Ok(output)
    }

    fn run_bounded_captured(
        &self,
        req: &RunRequest,
        run: impl FnOnce(&RunRequest) -> HostResult<RunOutput>,
    ) -> HostResult<RunOutput> {
        let (mut req, stdout, stderr) = capture_request(req);
        req.tee = false;
        let mut result = run(&req)?;
        if stdout {
            self.capture_process_output(&std::mem::take(&mut result.stdout));
        }
        if stderr {
            self.capture_process_output(&std::mem::take(&mut result.stderr));
        }
        Ok(result)
    }
}

fn capture_request(request: &RunRequest) -> (RunRequest, bool, bool) {
    let mut request = request.clone();
    request.timeout = Some(
        request
            .timeout
            .map_or(TIMEOUT, |timeout| timeout.min(TIMEOUT)),
    );
    let stdout = request.stdout == Output::Inherit;
    let stderr = request.stderr == Output::Inherit;
    if stdout {
        request.stdout = Output::Capture;
    }
    if stderr {
        request.stderr = Output::Capture;
    }
    request.tee |= stdout || stderr;
    (request, stdout, stderr)
}

macro_rules! forward_host {
    ($name:ident($($arg:ident: $ty:ty),* $(,)?) -> $result:ty) => {
        fn $name(&self, $($arg: $ty),*) -> $result {
            self.parent.$name($($arg),*)
        }
    };
}

impl Host for CapturingHost {
    forward_host!(read_dir(p: &Path) -> HostResult<Vec<DirEntry>>);
    forward_host!(create_dir_all(p: &Path) -> HostResult<()>);
    forward_host!(create_private_dir(p: &Path) -> HostResult<()>);
    forward_host!(remove_file(p: &Path) -> HostResult<()>);
    forward_host!(remove_dir(p: &Path) -> HostResult<()>);
    forward_host!(remove_dir_all(p: &Path) -> HostResult<()>);
    forward_host!(rename(from: &Path, to: &Path) -> HostResult<()>);
    forward_host!(rename_noreplace(from: &Path, to: &Path) -> HostResult<()>);
    forward_host!(symlink(target: &Path, link: &Path) -> HostResult<()>);
    forward_host!(hard_link(src: &Path, dst: &Path) -> HostResult<()>);
    forward_host!(read_link(p: &Path) -> HostResult<PathBuf>);
    forward_host!(canonicalize(p: &Path) -> HostResult<PathBuf>);
    forward_host!(canonicalize_fresh(p: &Path) -> HostResult<PathBuf>);

    forward_host!(metadata(p: &Path, follow: Follow) -> HostResult<Metadata>);
    forward_host!(metadata_fresh(p: &Path, follow: Follow) -> HostResult<Metadata>);
    forward_host!(read(p: &Path, range: ReadRange) -> HostResult<Vec<u8>>);
    forward_host!(read_chunks(p: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()>);
    forward_host!(
        lock_file(path: &Path, timeout: Option<Duration>) -> HostResult<Box<dyn FileLockGuard>>
    );
    forward_host!(copy_file(from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()>);
    forward_host!(set_permissions(p: &Path, mode: u32) -> HostResult<()>);
    forward_host!(
        set_times(p: &Path, atime: Option<FileTime>, mtime: Option<FileTime>) -> HostResult<()>
    );
    forward_host!(
        set_symlink_times(
            p: &Path,
            atime: Option<FileTime>,
            mtime: Option<FileTime>,
        ) -> HostResult<()>
    );
    forward_host!(env_get(key: &[u8]) -> Option<BString>);
    forward_host!(env_set(key: &[u8], value: Option<&[u8]>) -> ());
    forward_host!(env_all() -> Vec<(BString, BString)>);
    forward_host!(supports_process_execution() -> bool);

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        // The empty path routes trace lines to the parent's trace destination.
        if p.as_os_str().is_empty() {
            return match self.trace_output.get() {
                Some(Some(path)) => self.parent.write(path, data, mode),
                Some(None) => {
                    self.parent.write_out(Stream::Stderr, data);
                    Ok(())
                }
                None => Ok(()),
            };
        }
        self.parent.write(p, data, mode)
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.parent.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<BString>> {
        self.parent.registry_value(root, subkey, name, view)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn rcmake_host::ReadSeek + '_>> {
        self.parent.open_read(p)
    }

    fn scratch_file(&self) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.parent.scratch_file()
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.parent.scratch_file_in(dir)
    }

    fn write_from(
        &self,
        p: &Path,
        mode: WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        self.parent.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<GracefulTimeout>,
        hard_limit: Option<std::time::Duration>,
        update: &mut rcmake_host::DeadlineUpdate<'_>,
    ) -> HostResult<rcmake_host::ControlledOutput> {
        let (req, stdout, stderr) = capture_request(req);
        let mut result = self.parent.run_controlled(
            &req,
            graceful,
            Some(hard_limit.map_or(TIMEOUT, |limit| limit.min(TIMEOUT))),
            &mut |stream, bytes| {
                if (stream == Stream::Stdout && stdout) || (stream == Stream::Stderr && stderr) {
                    self.capture_process_output(bytes);
                }
                update(stream, bytes)
            },
        )?;
        if stdout {
            result.output.stdout.clear();
        }
        if stderr {
            result.output.stderr.clear();
        }
        Ok(result)
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |req, output| self.parent.run(req, output))
    }

    fn run_confined(
        &self,
        req: &RunRequest,
        scope: rcmake_host::WriteScope<'_>,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |req, output| {
            self.parent.run_confined(req, scope, output)
        })
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.run_captured(req, on_output, |req, output| {
            self.parent.run_with_timeout_signal(req, timeout, output)
        })
    }

    fn run_bounded(&self, req: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.run_bounded_captured(req, |req| self.parent.run_bounded(req, limit))
    }

    fn run_bounded_confined(
        &self,
        req: &RunRequest,
        limit: usize,
        scope: rcmake_host::WriteScope<'_>,
    ) -> HostResult<RunOutput> {
        self.run_bounded_captured(req, |req| {
            self.parent.run_bounded_confined(req, limit, scope)
        })
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if stream == Stream::Stderr {
            self.diagnostics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
        }
        self.capture_process_output(bytes);
    }

    forward_host!(read_stdin(limit: u64) -> HostResult<Vec<u8>>);
    forward_host!(is_terminal(stream: Stream) -> bool);
    forward_host!(now() -> FileTime);
    forward_host!(monotonic() -> Duration);
    forward_host!(sleep(duration: Duration) -> ());
    forward_host!(time_zone() -> jiff::tz::TimeZone);
    forward_host!(current_exe() -> HostResult<PathBuf>);
    forward_host!(effective_uid() -> Option<u32>);

    fn initial_cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn system(&self) -> SystemInfo {
        self.system.clone()
    }
}
