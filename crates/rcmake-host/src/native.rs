//! Real OS [`Host`] implementation backed by `std::fs`, `std::process`, and `rustix`.
//!
#![allow(clippy::disallowed_methods)]

use core::time::Duration;
use std::collections::BTreeMap;
use std::fs::{self, File, FileType, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, PipeReader, PipeWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::RwLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use jiff::tz::TimeZone;
use rcmake_base::BString;
use rustix::fs::{AtFlags, CWD, OFlags, Timespec, Timestamps, UTIME_OMIT};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};

use crate::error::{HostError, HostResult, from_errno};
use crate::{
    CopyOptions, DirEntry, FileKind, FileTime, Follow, Host, Input, Metadata, Output,
    ProcessResult, ReadRange, RunOutput, RunRequest, Stream, SystemInfo, WriteMode,
    require_absolute, validate_run,
};

/// Production [`Host`] backed by the operating system with an edition-2024-safe
/// interior-mutable environment overlay.
#[derive(Debug)]
pub struct NativeHost {
    env: RwLock<BTreeMap<BString, BString>>,
    initial_cwd: PathBuf,
    start: Instant,
}

impl NativeHost {
    /// Capture the current process environment and working directory into a new [`NativeHost`].
    pub fn new() -> HostResult<Self> {
        let mut map = BTreeMap::new();
        for (k, v) in std::env::vars_os() {
            map.insert(
                crate::path::from_native(Path::new(&k)),
                crate::path::from_native(Path::new(&v)),
            );
        }
        let initial_cwd = std::env::current_dir().map_err(|e| HostError::from_io(e, None))?;
        Ok(Self {
            env: RwLock::new(map),
            initial_cwd,
            start: Instant::now(),
        })
    }
}

fn io_at<T>(result: io::Result<T>, path: &Path) -> HostResult<T> {
    result.map_err(|err| HostError::from_io(err, Some(path.to_path_buf())))
}

fn file_kind(kind: FileType) -> FileKind {
    if kind.is_file() {
        FileKind::File
    } else if kind.is_dir() {
        FileKind::Dir
    } else if kind.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    }
}

fn timestamp(time: Option<FileTime>) -> Timespec {
    match time {
        Some(time) => Timespec {
            tv_sec: time.secs,
            tv_nsec: time.nanos.into(),
        },
        None => Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
    }
}

fn open_output(path: &Path, mode: WriteMode) -> HostResult<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    match mode {
        WriteMode::Truncate => options.create(true).truncate(true),
        WriteMode::Append => options.create(true).append(true),
        WriteMode::CreateNew => options.create_new(true),
    };
    io_at(options.open(path), path)
}

impl Host for NativeHost {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        require_absolute(p)?;
        let m = io_at(
            match follow {
                Follow::Yes => fs::metadata(p),
                Follow::No => fs::symlink_metadata(p),
            },
            p,
        )?;
        Ok(Metadata {
            kind: file_kind(m.file_type()),
            len: m.len(),
            mode: m.mode() & 0o7777,
            mtime: FileTime {
                secs: m.mtime(),
                nanos: u32::try_from(m.mtime_nsec()).unwrap_or_default(),
            },
            atime: FileTime {
                secs: m.atime(),
                nanos: u32::try_from(m.atime_nsec()).unwrap_or_default(),
            },
            dev: m.dev(),
            ino: m.ino(),
            nlink: m.nlink(),
        })
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        require_absolute(p)?;
        let mut file = io_at(File::open(p), p)?;
        let metadata = io_at(file.metadata(), p)?;
        if metadata.is_dir() {
            return Err(from_errno(Errno::ISDIR, Some(p)));
        }
        let mut bytes = Vec::new();
        match range {
            ReadRange::All { limit } => {
                if metadata.is_file() && metadata.len() > limit {
                    return Err(from_errno(Errno::FBIG, Some(p)));
                }
                let mut reader = (&mut file).take(limit);
                io_at(reader.read_to_end(&mut bytes), p)?;
                if reader.limit() == 0 {
                    match file.read_exact(&mut [0]) {
                        Ok(()) => return Err(from_errno(Errno::FBIG, Some(p))),
                        Err(err) if err.kind() == ErrorKind::UnexpectedEof => {}
                        Err(err) => return io_at(Err(err), p),
                    }
                }
            }
            ReadRange::Slice { offset, len } => {
                if offset != 0 {
                    if metadata.is_file() && i64::try_from(offset).is_err() {
                        return Ok(bytes);
                    }
                    io_at(file.seek(SeekFrom::Start(offset)), p)?;
                }
                io_at(
                    file.take(len.unwrap_or(u64::MAX)).read_to_end(&mut bytes),
                    p,
                )?;
            }
        }
        Ok(bytes)
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        require_absolute(p)?;
        io_at(open_output(p, mode)?.write_all(data), p)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        require_absolute(p)?;
        io_at(fs::read_dir(p), p)?
            .map(|entry| {
                let entry = io_at(entry, p)?;
                Ok(DirEntry {
                    name: crate::path::from_native(Path::new(&entry.file_name())),
                    kind: file_kind(io_at(entry.file_type(), &entry.path())?),
                })
            })
            .collect()
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        require_absolute(p)?;
        io_at(fs::create_dir_all(p), p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        require_absolute(p)?;
        // macOS reports EPERM when unlinking a directory.
        if io_at(fs::symlink_metadata(p), p)?.is_dir() {
            return Err(from_errno(Errno::ISDIR, Some(p)));
        }
        io_at(fs::remove_file(p), p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        let path = crate::path::directory_removal_path(p)?;
        if !io_at(fs::symlink_metadata(&path), p)?.is_dir() {
            return Err(from_errno(Errno::NOTDIR, Some(p)));
        }
        io_at(fs::remove_dir(path), p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        let path = crate::path::directory_removal_path(p)?;
        io_at(fs::remove_dir_all(path), p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        io_at(fs::rename(from, to), from)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        let mut src = io_at(File::open(from), from)?;
        let metadata = io_at(src.metadata(), from)?;
        if !metadata.is_file() {
            return Err(from_errno(Errno::INVAL, Some(from)));
        }
        let mut dst = io_at(
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(to),
            to,
        )?;
        let dst_metadata = io_at(dst.metadata(), to)?;
        if (metadata.dev(), metadata.ino()) == (dst_metadata.dev(), dst_metadata.ino()) {
            return Err(from_errno(Errno::INVAL, Some(to)));
        }
        io_at(dst.set_len(0), to)?;
        io_at(io::copy(&mut src, &mut dst), to)?;
        if opts.preserve_permissions {
            io_at(dst.set_permissions(metadata.permissions()), to)?;
        }
        if opts.preserve_mtime {
            rustix::fs::futimens(
                &dst,
                &Timestamps {
                    last_access: timestamp(None),
                    last_modification: Timespec {
                        tv_sec: metadata.mtime(),
                        tv_nsec: metadata.mtime_nsec(),
                    },
                },
            )
            .map_err(|err| from_errno(err, Some(to)))?;
        }
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        require_absolute(link)?;
        io_at(symlink(target, link), link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        require_absolute(src)?;
        require_absolute(dst)?;
        rustix::fs::linkat(CWD, src, CWD, dst, AtFlags::empty())
            .map_err(|err| from_errno(err, Some(dst)))
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        require_absolute(p)?;
        io_at(fs::read_link(p), p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        require_absolute(p)?;
        // macOS realpath can simplify ".." after an entry that is not a directory.
        io_at(fs::metadata(p), p)?;
        io_at(fs::canonicalize(p), p)
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        require_absolute(p)?;
        io_at(
            fs::set_permissions(p, fs::Permissions::from_mode(mode & 0o7777)),
            p,
        )
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        require_absolute(p)?;
        if atime
            .into_iter()
            .chain(mtime)
            .any(|time| time.nanos >= 1_000_000_000)
        {
            return Err(from_errno(Errno::INVAL, Some(p)));
        }
        if atime.is_none() && mtime.is_none() {
            return self.metadata(p, Follow::Yes).map(|_| ());
        }
        rustix::fs::utimensat(
            CWD,
            p,
            &Timestamps {
                last_access: timestamp(atime),
                last_modification: timestamp(mtime),
            },
            AtFlags::empty(),
        )
        .map_err(|err| from_errno(err, Some(p)))
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let guard = self.env.read().ok()?;
        guard.get(key).cloned()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        if let Ok(mut guard) = self.env.write() {
            match value {
                Some(v) => {
                    guard.insert(BString::from(key), BString::from(v));
                }
                None => {
                    guard.remove(key);
                }
            }
        }
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.env
            .read()
            .map(|g| g.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        validate_run(req)?;
        run_pipeline(req, on_output)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        match stream {
            Stream::Stdout => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(bytes);
                let _ = out.flush();
            }
            Stream::Stderr => {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(bytes);
                let _ = err.flush();
            }
        }
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        let mut buf = Vec::new();
        std::io::stdin()
            .take(limit)
            .read_to_end(&mut buf)
            .map_err(|e| HostError::from_io(e, None))?;
        Ok(buf)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        match stream {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        }
    }

    fn now(&self) -> FileTime {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => FileTime {
                secs: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                nanos: d.subsec_nanos(),
            },
            Err(_) => FileTime::default(),
        }
    }

    fn monotonic(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn time_zone(&self) -> TimeZone {
        TimeZone::system()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        std::env::current_exe().map_err(|e| HostError::from_io(e, None))
    }

    fn initial_cwd(&self) -> PathBuf {
        self.initial_cwd.clone()
    }

    fn system(&self) -> SystemInfo {
        let u = rustix::system::uname();
        let cpus = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(1))
            .unwrap_or(1);
        SystemInfo {
            sysname: BString::from(u.sysname().to_bytes()),
            release: BString::from(u.release().to_bytes()),
            version: BString::from(u.version().to_bytes()),
            machine: BString::from(u.machine().to_bytes()),
            hostname: BString::from(u.nodename().to_bytes()),
            cpu_count: cpus,
            pid: std::process::id(),
        }
    }
}

enum Destination {
    Inherit,
    Null,
    Fd(OwnedFd),
}

impl Destination {
    fn stdio(&self) -> io::Result<Stdio> {
        Ok(match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Fd(fd) => Stdio::from(fd.try_clone()?),
        })
    }
}

struct Capture {
    pipe: PipeReader,
    stream: Stream,
}

fn nonblocking(fd: impl AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

fn destination(
    output: &Output,
    stream: Stream,
    capture: &mut Vec<Capture>,
) -> HostResult<Destination> {
    Ok(match output {
        Output::Capture => {
            let (reader, writer) = io_at_process(io::pipe())?;
            io_at_process(nonblocking(&reader))?;
            capture.push(Capture {
                pipe: reader,
                stream,
            });
            Destination::Fd(writer.into())
        }
        Output::File { path, append } => {
            let mode = if *append {
                WriteMode::Append
            } else {
                WriteMode::Truncate
            };
            Destination::Fd(open_output(path, mode)?.into())
        }
        Output::Inherit => Destination::Inherit,
        Output::Null => Destination::Null,
        Output::SharedWithStdout => return Err(from_errno(Errno::INVAL, None)),
    })
}

struct Feed<'a> {
    pipe: PipeWriter,
    bytes: &'a [u8],
}

fn process_input(input: &Input) -> HostResult<(Stdio, Option<Feed<'_>>)> {
    match input {
        Input::Inherit => Ok((Stdio::inherit(), None)),
        Input::Null => Ok((Stdio::null(), None)),
        Input::File(path) => Ok((io_at(File::open(path), path)?.into(), None)),
        Input::Bytes(bytes) => {
            let (reader, writer) = io_at_process(io::pipe())?;
            io_at_process(nonblocking(&writer))?;
            Ok((
                reader.into(),
                Some(Feed {
                    pipe: writer,
                    bytes,
                }),
            ))
        }
    }
}

fn io_at_process<T>(result: io::Result<T>) -> HostResult<T> {
    result.map_err(|err| HostError::from_io(err, None))
}

struct Stage {
    child: Option<Child>,
    result: Option<ProcessResult>,
}

struct Pipeline {
    stages: Vec<Stage>,
    grouped: bool,
}

fn child_status(child: &mut Child, grouped: bool) -> io::Result<Option<ProcessResult>> {
    if grouped {
        // Keep the leader's PID reserved until we finish or signal its group.
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        let status = waitid(WaitId::Pid(Pid::from_child(child)), options)?;
        Ok(status.and_then(|status| {
            status
                .exit_status()
                .map(ProcessResult::Exited)
                .or_else(|| status.terminating_signal().map(ProcessResult::Signaled))
        }))
    } else {
        Ok(child.try_wait()?.and_then(|status| {
            status
                .code()
                .map(ProcessResult::Exited)
                .or_else(|| status.signal().map(ProcessResult::Signaled))
        }))
    }
}

impl Pipeline {
    fn collect_status(&mut self) -> io::Result<bool> {
        let mut done = true;
        for stage in &mut self.stages {
            if stage.result.is_none()
                && let Some(child) = &mut stage.child
            {
                match child_status(child, self.grouped) {
                    Ok(status) => stage.result = status,
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(err) => return Err(err),
                }
                done &= stage.result.is_some();
            }
        }
        Ok(done)
    }

    fn reap(&mut self) {
        for stage in &mut self.stages {
            if let Some(mut child) = stage.child.take() {
                let _ = child.wait();
            }
        }
    }

    fn stop(&mut self) {
        for stage in &mut self.stages {
            if let Some(child) = &mut stage.child {
                if self.grouped {
                    let group = Pid::from_child(child);
                    let _ = kill_process_group(group, Signal::KILL);
                }
                if stage.result.is_none() {
                    let _ = child.kill();
                }
            }
        }
        self.reap();
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_pipeline(
    req: &RunRequest,
    input: Stdio,
    stdout: Destination,
    stderr: Option<Destination>,
    pipeline: &mut Pipeline,
    start: Instant,
) -> HostResult<()> {
    let mut next_input = Some(input);
    let mut specs = req.pipeline.iter().peekable();
    while let Some(spec) = specs.next() {
        if req
            .timeout
            .is_some_and(|timeout| start.elapsed() >= timeout)
        {
            pipeline.stages.push(Stage {
                child: None,
                result: Some(ProcessResult::TimedOut),
            });
            continue;
        }
        let stage_in = next_input
            .take()
            .ok_or_else(|| from_errno(Errno::INVAL, None))?;
        let stage_out = if specs.peek().is_some() {
            let (reader, writer) = io_at_process(io::pipe())?;
            next_input = Some(reader.into());
            writer.into()
        } else {
            io_at_process(stdout.stdio())?
        };
        let stage_err = io_at_process(stderr.as_ref().unwrap_or(&stdout).stdio())?;
        let program = crate::path::to_native(spec.program.as_slice());
        let mut command = Command::new(&program);
        command
            .args(
                spec.args
                    .iter()
                    .map(|arg| crate::path::to_native(arg.as_slice())),
            )
            .env_clear()
            .envs(req.env.iter().map(|(k, v)| {
                (
                    crate::path::to_native(k.as_slice()),
                    crate::path::to_native(v.as_slice()),
                )
            }))
            .current_dir(&req.cwd)
            .stdin(stage_in)
            .stdout(stage_out)
            .stderr(stage_err);
        if pipeline.grouped {
            // Each stage leads its own group. A fast first stage can exit before the next spawn.
            command.process_group(0);
        }
        let stage = match command.spawn() {
            Ok(child) => Stage {
                child: Some(child),
                result: None,
            },
            Err(err) => Stage {
                child: None,
                result: Some(ProcessResult::SpawnFailed(HostError::from_io(
                    err,
                    Some(program),
                ))),
            },
        };
        pipeline.stages.push(stage);
    }
    Ok(())
}

fn pump_input(feed: &mut Option<Feed<'_>>) -> io::Result<bool> {
    let Some(input) = feed else {
        return Ok(false);
    };
    if !input.bytes.is_empty() {
        match input.pipe.write(input.bytes) {
            Ok(n) => input.bytes = input.bytes.get(n..).unwrap_or_default(),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                return Ok(false);
            }
            Err(err) if err.kind() == ErrorKind::BrokenPipe => input.bytes = &[],
            Err(err) => return Err(err),
        }
    }
    if input.bytes.is_empty() {
        *feed = None;
    }
    Ok(true)
}

fn pump_output(
    captures: &mut Vec<Capture>,
    output: &mut RunOutput,
    tee: bool,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> io::Result<bool> {
    let mut busy = false;
    let mut buffer = [0u8; 8192];
    let mut error = None;
    captures.retain_mut(|capture| match capture.pipe.read(&mut buffer) {
        Ok(0) => false,
        Ok(n) => {
            let bytes = buffer.get(..n).unwrap_or_default();
            match capture.stream {
                Stream::Stdout => output.stdout.extend_from_slice(bytes),
                Stream::Stderr => output.stderr.extend_from_slice(bytes),
            }
            if tee {
                on_output(capture.stream, bytes);
            }
            busy = true;
            true
        }
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => true,
        Err(err) => {
            error = Some(err);
            false
        }
    });
    error.map_or(Ok(busy), Err)
}

fn run_pipeline(
    req: &RunRequest,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> HostResult<RunOutput> {
    let start = Instant::now();
    let (input, mut feed) = process_input(&req.stdin)?;
    let mut captures = Vec::new();
    let mut stdout = destination(&req.stdout, Stream::Stdout, &mut captures)?;
    let stderr = if req.stderr == Output::SharedWithStdout {
        if matches!(stdout, Destination::Inherit) {
            stdout = Destination::Fd(io_at_process(io::stdout().as_fd().try_clone_to_owned())?);
        }
        None
    } else {
        Some(destination(&req.stderr, Stream::Stderr, &mut captures)?)
    };
    let mut pipeline = Pipeline {
        stages: Vec::new(),
        grouped: req.timeout.is_some(),
    };
    spawn_pipeline(req, input, stdout, stderr, &mut pipeline, start)?;
    let mut output = RunOutput::default();
    loop {
        let done = io_at_process(pipeline.collect_status())?;
        let read = io_at_process(pump_output(&mut captures, &mut output, req.tee, on_output))?;
        if done && captures.is_empty() {
            pipeline.reap();
            break;
        }
        if req
            .timeout
            .is_some_and(|timeout| start.elapsed() >= timeout)
        {
            if done
                && let Some(last) = pipeline
                    .stages
                    .iter_mut()
                    .rfind(|stage| stage.child.is_some())
            {
                last.result = Some(ProcessResult::TimedOut);
            }
            pipeline.stop();
            break;
        }
        let wrote = io_at_process(pump_input(&mut feed))?;
        if !read && !wrote {
            let pause = Duration::from_millis(2);
            let remaining = req
                .timeout
                .map_or(pause, |timeout| timeout.saturating_sub(start.elapsed()));
            std::thread::sleep(pause.min(remaining));
        }
    }
    output.results = std::mem::take(&mut pipeline.stages)
        .into_iter()
        .map(|stage| stage.result.unwrap_or(ProcessResult::TimedOut))
        .collect();
    Ok(output)
}
