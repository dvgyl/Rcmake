use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, Host, HostError, HostResult, Metadata,
    Output, ReadRange, RunOutput, RunRequest, Stream, SystemInfo, WriteMode,
};

use super::TIMEOUT;

type HostCall = Box<dyn FnOnce(&dyn Host) + Send>;

/// The evaluator owns its host, but the command context only provides a borrow.
/// Keep the borrow on a scoped thread and forward child calls to that thread.
pub(super) struct CapturingHost {
    calls: mpsc::Sender<HostCall>,
    output: Mutex<BString>,
    pub(super) trace_output: OnceLock<Option<PathBuf>>,
    system: SystemInfo,
    cwd: PathBuf,
}

impl CapturingHost {
    pub(super) fn capture<T>(
        parent: &dyn Host,
        cwd: &Path,
        configure: impl FnOnce(Arc<Self>) -> T,
    ) -> HostResult<(T, BString)> {
        std::thread::scope(|scope| {
            let (calls, requests) = mpsc::channel::<HostCall>();
            std::thread::Builder::new()
                .name("rcmake-probe-host".to_owned())
                .spawn_scoped(scope, move || {
                    for call in requests {
                        call(parent);
                    }
                })
                .map_err(|error| HostError::from_io(error, None))?;
            let host = Arc::new(Self {
                calls,
                output: Mutex::default(),
                trace_output: OnceLock::new(),
                system: parent.system(),
                cwd: cwd.to_path_buf(),
            });
            let outcome = configure(host.clone());
            let output = std::mem::take(
                &mut *host
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            Ok((outcome, output))
        })
    }

    fn forward<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn Host) -> T + Send + 'static,
    ) -> HostResult<T> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Box::new(move |host| {
            let _ = reply.send(call(host));
        }))?;
        result
            .recv()
            .map_err(|_| HostError::unsupported("The test project host stopped."))
    }

    fn send(&self, call: HostCall) -> HostResult<()> {
        self.calls
            .send(call)
            .map_err(|_| HostError::unsupported("The test project host stopped."))
    }
}

macro_rules! forward_host {
    ($name:ident($($arg:ident: $ty:ty),*) -> $result:ty; $($owned:ident),* $(,)?) => {
        fn $name(&self, $($arg: $ty),*) -> HostResult<$result> {
            $(let $owned = $owned.to_owned();)*
            self.forward(move |host| host.$name($($arg.as_ref()),*))?
        }
    };
}

impl Host for CapturingHost {
    forward_host!(read_dir(p: &Path) -> Vec<DirEntry>; p);
    forward_host!(create_dir_all(p: &Path) -> (); p);
    forward_host!(remove_file(p: &Path) -> (); p);
    forward_host!(remove_dir(p: &Path) -> (); p);
    forward_host!(remove_dir_all(p: &Path) -> (); p);
    forward_host!(rename(from: &Path, to: &Path) -> (); from, to);
    forward_host!(rename_noreplace(from: &Path, to: &Path) -> (); from, to);
    forward_host!(symlink(target: &Path, link: &Path) -> (); target, link);
    forward_host!(hard_link(src: &Path, dst: &Path) -> (); src, dst);
    forward_host!(read_link(p: &Path) -> PathBuf; p);
    forward_host!(canonicalize(p: &Path) -> PathBuf; p);

    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        let p = p.to_owned();
        self.forward(move |host| host.metadata(&p, follow))?
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let p = p.to_owned();
        self.forward(move |host| host.read(&p, range))?
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        let data = data.to_vec();
        // The empty path routes trace lines. The parent already initialized its trace output.
        if p.as_os_str().is_empty() {
            return match self.trace_output.get() {
                Some(Some(path)) => {
                    let path = path.clone();
                    self.forward(move |host| host.write(&path, &data, mode))?
                }
                Some(None) => self.forward(move |host| host.write_out(Stream::Stderr, &data)),
                None => Ok(()),
            };
        }
        let p = p.to_owned();
        self.forward(move |host| host.write(&p, &data, mode))?
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        let path = path.to_owned();
        self.forward(move |host| host.lock_file(&path, timeout))?
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        let (from, to) = (from.to_owned(), to.to_owned());
        self.forward(move |host| host.copy_file(&from, &to, opts))?
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        let p = p.to_owned();
        self.forward(move |host| host.set_permissions(&p, mode))?
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        let p = p.to_owned();
        self.forward(move |host| host.set_times(&p, atime, mtime))?
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let key = key.to_vec();
        self.forward(move |host| host.env_get(&key)).ok().flatten()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        let key = key.to_vec();
        let value = value.map(<[u8]>::to_vec);
        let _ = self.forward(move |host| host.env_set(&key, value.as_deref()));
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.forward(|host| host.env_all()).unwrap_or_default()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        enum Event {
            Chunk(Stream, Vec<u8>),
            Done(HostResult<RunOutput>),
        }
        let mut req = req.clone();
        req.timeout = Some(req.timeout.map_or(TIMEOUT, |timeout| timeout.min(TIMEOUT)));
        let stdout = req.stdout == Output::Inherit;
        let stderr = req.stderr == Output::Inherit;
        if stdout {
            req.stdout = Output::Capture;
        }
        if stderr {
            req.stderr = Output::Capture;
        }
        if stdout || stderr {
            req.tee = true;
        }
        let (output, events) = mpsc::sync_channel(1);
        self.send(Box::new(move |host| {
            let result = host.run(&req, &mut |stream, bytes| {
                let _ = output.send(Event::Chunk(stream, bytes.to_vec()));
            });
            let _ = output.send(Event::Done(result));
        }))?;
        for event in events {
            match event {
                Event::Chunk(stream, bytes) => {
                    let inherit = if stream == Stream::Stdout {
                        stdout
                    } else {
                        stderr
                    };
                    if inherit {
                        self.write_out(stream, &bytes);
                    } else {
                        on_output(stream, &bytes);
                    }
                }
                Event::Done(result) => {
                    return result.map(|mut output| {
                        if stdout {
                            output.stdout.clear();
                        }
                        if stderr {
                            output.stderr.clear();
                        }
                        output
                    });
                }
            }
        }
        Err(HostError::unsupported("The test project host stopped."))
    }

    fn write_out(&self, _stream: Stream, bytes: &[u8]) {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.forward(move |host| host.read_stdin(limit))?
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.forward(move |host| host.is_terminal(stream))
            .unwrap_or(false)
    }

    fn now(&self) -> FileTime {
        self.forward(|host| host.now()).unwrap_or_default()
    }

    fn monotonic(&self) -> Duration {
        self.forward(|host| host.monotonic()).unwrap_or_default()
    }

    fn sleep(&self, duration: Duration) {
        let _ = self.forward(move |host| host.sleep(duration));
    }

    fn time_zone(&self) -> jiff::tz::TimeZone {
        self.forward(|host| host.time_zone())
            .unwrap_or(jiff::tz::TimeZone::UTC)
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.forward(|host| host.current_exe())?
    }

    fn initial_cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn system(&self) -> SystemInfo {
        self.system.clone()
    }
}
