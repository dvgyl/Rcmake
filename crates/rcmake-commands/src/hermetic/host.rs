//! Check input operations before the host uses them.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use jiff::tz::TimeZone;
use rcmake_base::BString;
use rcmake_eval::HermeticMode;
use rcmake_eval::hermetic::{AuditDecision, ViolationKind};
use rcmake_host::{
    CopyOptions, DirEntry, FileKind, FileLockGuard, FileTime, Follow, GracefulTimeout, Host,
    HostResult, Input, Metadata, ReadRange, RunOutput, RunRequest, Stream, SystemInfo, WriteMode,
    path,
};

use super::{HermeticHost, InputKind, require_allowed, transfer};

impl Host for HermeticHost {
    fn metadata(&self, path: &Path, follow: Follow) -> HostResult<Metadata> {
        let checked = self.guard_path(path)?;
        let path = if follow == Follow::Yes {
            checked.as_ref()
        } else {
            path
        };
        self.inner.metadata(path, follow)
    }

    fn read(&self, path: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.inner.read(&self.guard_path(path)?, range)
    }

    fn read_chunks(&self, path: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.inner.read_chunks(&self.guard_path(path)?, consume)
    }

    fn write(&self, path: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        self.inner.write(path, data, mode)
    }

    fn read_dir(&self, path: &Path) -> HostResult<Vec<DirEntry>> {
        self.inner.read_dir(&self.guard_path(path)?)
    }

    fn create_dir_all(&self, path: &Path) -> HostResult<()> {
        self.inner.create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_dir(path)
    }

    fn remove_dir_all(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.guard_path(from)?;
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.guard_path(from)?;
        self.inner.rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        self.inner.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.inner.copy_file(&self.guard_path(from)?, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.inner.symlink(target, link)
    }

    fn hard_link(&self, source: &Path, destination: &Path) -> HostResult<()> {
        self.guard_path(source)?;
        self.inner.hard_link(source, destination)
    }

    fn read_link(&self, path: &Path) -> HostResult<PathBuf> {
        self.guard_path(path)?;
        self.inner.read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> HostResult<PathBuf> {
        self.inner.canonicalize(&self.guard_path(path)?)
    }

    fn set_permissions(&self, path: &Path, mode: u32) -> HostResult<()> {
        self.inner.set_permissions(path, mode)
    }

    fn set_times(
        &self,
        path: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.inner.set_times(path, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        path: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.inner.set_symlink_times(path, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let site = self.context.site();
        if self.audit(|audit| audit.check_env(&site, key)) != AuditDecision::Allow {
            return None;
        }
        self.state().inputs.env(key);
        self.inner
            .env_get(key)
            .or_else(|| self.is_source_date_epoch(key).then(|| "0".into()))
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.inner.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        let mut state = self.state();
        let mut env = state.auditor.process_env(self.inner.env_all());
        self.seed_source_date_epoch(&mut env);
        env.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (key, _) in &env {
            state.inputs.env(key);
        }
        env
    }

    fn supports_process_execution(&self) -> bool {
        self.inner.supports_process_execution()
    }

    fn run(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.inner.run(&self.process_request(request)?, on_output)
    }

    fn run_with_timeout_signal(
        &self,
        request: &RunRequest,
        timeout: GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        self.inner
            .run_with_timeout_signal(&self.process_request(request)?, timeout, on_output)
    }

    fn run_bounded(&self, request: &RunRequest, limit: usize) -> HostResult<RunOutput> {
        self.inner
            .run_bounded(&self.process_request(request)?, limit)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.inner.write_out(stream, bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn now(&self) -> FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> SystemInfo {
        self.inner.system()
    }
}

impl HermeticHost {
    fn process_request(&self, request: &RunRequest) -> HostResult<RunRequest> {
        if !self.inner.supports_process_execution() {
            return Ok(request.clone());
        }
        if self.context.denied() {
            return require_allowed(AuditDecision::Deny, ViolationKind::UnapprovedProcess)
                .map(|()| request.clone());
        }
        let site = self.context.site();
        let mut request = request.clone();
        if let Input::File(input) = &mut request.stdin {
            *input = self.guard_path(input)?.into_owned();
        }
        let fetch = self.context.fetch_cache().is_some();
        {
            let state = self.state();
            request.env.retain(|(key, _)| {
                state.auditor.allows_process_env(key) || (fetch && trusted_git_env(key))
            });
        }
        self.seed_source_date_epoch(&mut request.env);
        let path_value = request.env.iter().rev().find_map(|(name, value)| {
            (name == b"PATH" || (self.windows && name.eq_ignore_ascii_case(b"PATH")))
                .then_some(value.as_slice())
        });
        for stage in &mut request.pipeline {
            let program = path::to_native(&stage.program);
            let executable = self
                .resolve_executable(&program, &request.cwd, path_value)
                .unwrap_or_else(|| program.clone());
            transfer::check_curl(
                self,
                &site,
                &program,
                &executable,
                &stage.args,
                &request.cwd,
                &request.env,
            )?;
            let decision =
                self.audit(|audit| audit.check_process(self.inner.as_ref(), &site, &executable));
            require_allowed(decision, ViolationKind::UnapprovedProcess)?;
            if decision == AuditDecision::Allow {
                self.state().inputs.path(InputKind::Program, &executable);
                if self.mode == HermeticMode::Strict {
                    stage.program = path::from_native(&executable);
                }
            }
        }
        Ok(request)
    }

    fn is_source_date_epoch(&self, key: &[u8]) -> bool {
        key == b"SOURCE_DATE_EPOCH"
            || (self.windows && key.eq_ignore_ascii_case(b"SOURCE_DATE_EPOCH"))
    }

    fn seed_source_date_epoch(&self, env: &mut Vec<(BString, BString)>) {
        if !env.iter().any(|(key, _)| self.is_source_date_epoch(key)) {
            env.push(("SOURCE_DATE_EPOCH".into(), "0".into()));
        }
    }

    fn resolve_executable(
        &self,
        program: &Path,
        cwd: &Path,
        path_value: Option<&[u8]>,
    ) -> Option<PathBuf> {
        let candidate = |file: PathBuf| {
            let is_executable = |file: &Path| {
                self.inner.metadata(file, Follow::Yes).is_ok_and(|entry| {
                    entry.kind == FileKind::File && (self.windows || entry.mode & 0o111 != 0)
                })
            };
            if is_executable(&file) {
                return self.inner.canonicalize(&file).ok();
            }
            if self.windows && file.extension().is_none() {
                let executable = file.with_extension("exe");
                if is_executable(&executable) {
                    return self.inner.canonicalize(&executable).ok();
                }
                let extensions = self
                    .inner
                    .env_get(b"PATHEXT")
                    .unwrap_or_else(|| ".EXE;.COM;.BAT;.CMD".into());
                for extension in extensions.split(|&byte| byte == b';') {
                    let Some(extension) = extension.strip_prefix(b".").filter(|extension| {
                        !extension.is_empty() && extension.iter().all(u8::is_ascii_alphanumeric)
                    }) else {
                        continue;
                    };
                    let executable = file.with_extension(path::to_native(extension));
                    if is_executable(&executable) {
                        return self.inner.canonicalize(&executable).ok();
                    }
                }
            }
            None
        };
        let mut components = program.components();
        if !matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        ) || path::from_native(program).contains(&b'/')
        {
            return candidate(cwd.join(program));
        }
        let from_search_path = |value: &[u8]| {
            #[cfg(any(unix, windows))]
            let native_path = path::to_native(value);
            #[cfg(any(unix, windows))]
            let directories = std::env::split_paths(native_path.as_os_str());
            #[cfg(not(any(unix, windows)))]
            let directories = value.split(|&byte| byte == b':').map(path::to_native);
            directories
                .map(|dir| cwd.join(dir).join(program))
                .find_map(candidate)
        };
        if let Some(executable) = path_value.and_then(from_search_path) {
            return Some(executable);
        }
        if self.windows {
            let exe = self.inner.current_exe().ok();
            let system_root = self
                .inner
                .env_get(b"SYSTEMROOT")
                .map(|value| path::to_native(&value))
                .filter(|path| path.is_absolute());
            let system_directories = system_root
                .as_ref()
                .into_iter()
                .flat_map(|root| [root.join("System32"), root.clone()]);
            if let Some(resolved) = exe
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_owned)
                .into_iter()
                .chain(system_directories)
                .find_map(|dir| candidate(dir.join(program)))
            {
                return Some(resolved);
            }
            return from_search_path(&self.inner.env_get(b"PATH")?);
        }
        None
    }
}

fn trusted_git_env(name: &[u8]) -> bool {
    matches!(
        name,
        b"LC_ALL"
            | b"GIT_TERMINAL_PROMPT"
            | b"GIT_NO_LAZY_FETCH"
            | b"GIT_NO_REPLACE_OBJECTS"
            | b"GIT_ALLOW_PROTOCOL"
            | b"GIT_CONFIG_NOSYSTEM"
            | b"GIT_CONFIG_GLOBAL"
            | b"GIT_TEMPLATE_DIR"
    )
}
