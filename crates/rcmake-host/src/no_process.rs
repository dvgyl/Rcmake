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
const DENIED_MESSAGE: &str = "process execution is disabled in this host mode";

/// A denied request's working directory and command pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeniedCommand {
    /// Working directory requested for the process.
    pub cwd: PathBuf,
    /// Command stages in the requested order.
    pub pipeline: Vec<CommandSpec>,
}

/// Forwards non-process operations to the base host and denies all child processes.
pub struct NoProcessHost<H: Host + ?Sized> {
    base: Arc<H>,
    denied: Mutex<VecDeque<DeniedCommand>>,
}

impl<H: Host> NoProcessHost<H> {
    /// Wrap an owned host.
    #[must_use]
    pub fn new(base: H) -> Self {
        Self::from_shared(Arc::new(base))
    }
}

impl<H: Host + ?Sized> NoProcessHost<H> {
    /// Wrap a shared host, including a shared trait object.
    #[must_use]
    pub fn from_shared(base: Arc<H>) -> Self {
        Self {
            base,
            denied: Mutex::default(),
        }
    }

    /// Return up to 128 denied requests, from oldest to newest.
    #[must_use]
    pub fn denied_commands(&self) -> Vec<DeniedCommand> {
        self.denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn deny(&self, req: &RunRequest) -> HostResult<RunOutput> {
        let mut denied = self
            .denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if denied.len() == DENIED_LIMIT {
            denied.pop_front();
        }
        denied.push_back(DeniedCommand {
            cwd: req.cwd.clone(),
            pipeline: req.pipeline.clone(),
        });
        Err(HostError::unsupported(DENIED_MESSAGE))
    }
}

impl<H: Host + ?Sized> Host for NoProcessHost<H> {
    fn run(
        &self,
        req: &RunRequest,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.deny(req)
    }

    fn run_with_timeout_signal(
        &self,
        req: &RunRequest,
        _timeout: GracefulTimeout,
        _on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.deny(req)
    }

    fn run_bounded(&self, req: &RunRequest, _limit: usize) -> HostResult<RunOutput> {
        self.deny(req)
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
        let expected = [normal, timed, bounded].map(|req| DeniedCommand {
            cwd: req.cwd,
            pipeline: req.pipeline,
        });
        assert_eq!(host.denied_commands(), expected);
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
        assert_eq!(overlay.audit_clean_base().overlay_files, 0);
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
        assert_eq!(overlay.audit_clean_base().environment_overrides, 1);
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
