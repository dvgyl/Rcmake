use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_host::{
    FileKind, Follow, Host, Input, ReadRange,
    path::{from_native, to_native},
};
use rcmake_store::{
    Digest, Domain, KeyHasher, canonical_path, ensure_private_dir, verify_private_dir,
};

use rcmake_base::BString;

use crate::{
    FetchError, GitClone, atomic_write, clone_args::plain, error::sanitized_url, invalid, offline,
    staging_path,
};

mod gc;
mod seed;
pub use gc::GitGcReport;

const OUTPUT_LIMIT: usize = 1024 * 1024;

pub(crate) fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn check_commit(value: &str) -> Result<String, FetchError> {
    if is_commit(value) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(invalid("Expected a full Git commit ID."))
    }
}

/// Shared bare Git mirrors with one file lock per repository.
pub struct GitMirror<'a> {
    host: &'a dyn Host,
    /// The shared cache root. It must pass the private directory checks of the store.
    cache_root: PathBuf,
    root: PathBuf,
    timeout: Option<Duration>,
    deadline: Option<Duration>,
    read_only: bool,
    offline_only: bool,
    program: BString,
}

impl<'a> GitMirror<'a> {
    /// Use the `git` directory below an absolute cache root.
    pub fn new(host: &'a dyn Host, cache_root: &Path) -> Result<Self, FetchError> {
        if !cache_root.is_absolute() {
            return Err(invalid("Git cache root must be absolute."));
        }
        // Resolve the root once, so that later operations do not depend on a link.
        let cache_root = canonical_path(host, cache_root);
        Ok(Self {
            host,
            root: cache_root.join("git"),
            cache_root,
            timeout: None,
            deadline: None,
            read_only: offline(host)
                && host
                    .env_get(b"RCMAKE_FETCH_GIT_READ_ONLY")
                    .is_some_and(|value| value == "1"),
            offline_only: false,
            program: "git".into(),
        })
    }

    /// Select the Git executable already resolved by the caller.
    pub fn with_program(mut self, program: BString) -> Self {
        self.program = program;
        self
    }

    /// Use local Git operations only even if the host environment changes.
    pub fn offline_only(mut self) -> Self {
        self.offline_only = true;
        self
    }

    /// Set the timeout for Git commands and checkout and mirror locks.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Use an immutable cache snapshot in offline mode without a writable mirror lock.
    ///
    /// The caller must ensure that no process can change the snapshot during use.
    /// A checkout still needs a writable destination outside the snapshot.
    pub fn read_only_cache(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Check the shared cache root before Git uses it, with the policy of the store.
    ///
    /// A mutable mirror creates a missing root with mode `0o700`. A read finds no root as
    /// a `NotFound` error. An immutable snapshot from [`Self::read_only_cache`] is the
    /// caller's responsibility and is not checked.
    pub(crate) fn admit_cache_root(&self, create: bool) -> Result<(), rcmake_host::HostError> {
        if self.read_only {
            return Ok(());
        }
        if create {
            ensure_private_dir(self.host, &self.cache_root).map(|_| ())
        } else {
            verify_private_dir(self.host, &self.cache_root).map(|_| ())
        }
    }

    pub(crate) fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = Some(deadline);
        self
    }

    fn remaining(&self) -> Result<Option<Duration>, FetchError> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_sub(self.host.monotonic());
            if remaining.is_zero() {
                return Err(invalid("Git prefetch reached its time limit."));
            }
            return Ok(Some(
                self.timeout
                    .map_or(remaining, |timeout| timeout.min(remaining)),
            ));
        }
        Ok(self.timeout)
    }

    /// Store a pinned commit in a bare mirror and return the mirror path.
    ///
    /// Concurrent requests check for the commit again under the mirror lock.
    pub fn prefetch(&self, repository: &str, commit: &str) -> Result<PathBuf, FetchError> {
        let commit = check_commit(commit)?;
        let cwd = self.host.initial_cwd();
        let repository = repository_at(repository, &cwd)?;
        self.with_mirror(&repository, Some(&commit), &[], &cwd, |mirror| {
            Ok(mirror.to_owned())
        })
    }

    /// Clone a pinned repository into a new source directory and detach at the commit.
    ///
    /// The checkout owns its objects, so a later mirror update cannot remove them.
    pub fn checkout(
        &self,
        repository: &str,
        commit: &str,
        source_dir: &Path,
    ) -> Result<(), FetchError> {
        let commit = check_commit(commit)?;
        let cwd = self.host.initial_cwd();
        let source_dir = cwd.join(source_dir);
        let normalized = repository_at(repository, &cwd)?;
        self.with_mirror(&normalized, Some(&commit), &[], &cwd, |mirror| {
            self.staged_checkout(&source_dir, |stage| {
                self.git(
                    &cwd,
                    vec![
                        "clone".into(),
                        "--no-checkout".into(),
                        "--no-hardlinks".into(),
                        "--".into(),
                        from_native(mirror),
                        from_native(stage),
                    ],
                    true,
                )?;
                self.restore_remote(stage, "origin", &normalized)?;
                self.checkout_commit(stage, &commit)
            })
        })
    }

    /// Run a classified clone through the mirror and restore the declared remote URL.
    ///
    /// Pass the frozen lock pin when one applies. A pinned shallow clone can need
    /// the commit separately because the requested depth can omit it.
    pub fn clone_template(
        &self,
        clone: &GitClone<'_>,
        cwd: &Path,
        pin: Option<&str>,
    ) -> Result<(), FetchError> {
        let pin = pin.map(check_commit).transpose()?;
        let repository = repository_at(clone.repository, cwd)?;
        let source_dir = cwd.join(clone.directory);
        let config = clone
            .global
            .iter()
            .cloned()
            .chain(
                clone
                    .options
                    .windows(2)
                    .filter_map(|pair| match pair {
                        [key, val] if key == "-c" || key == "--config" => {
                            Some([BString::from("-c"), val.clone()])
                        }
                        _ => None,
                    })
                    .flatten(),
            )
            .collect::<Vec<_>>();
        self.with_mirror(&repository, pin.as_deref(), &config, cwd, |mirror| {
            self.staged_checkout(&source_dir, |stage| {
                if let Some(commit) = &pin
                    && !self.read_only
                    && !clone.options.iter().any(|flag| flag == "--depth")
                    && self.owns_full_objects(mirror)
                {
                    let mut args = clone.global.to_vec();
                    args.extend([BString::from("clone"), "--no-hardlinks".into()]);
                    args.extend_from_slice(clone.options);
                    args.extend(["--".into(), from_native(mirror), from_native(stage)]);
                    if self.git(cwd, args, true).is_ok()
                        && self.owns_full_objects(&stage.join(".git"))
                        && self
                            .has_commit(stage, &stage.join(".git"), commit)
                            .is_ok_and(|found| found)
                    {
                        return self.restore_remote(stage, clone.origin, &repository);
                    }
                    self.remove_stage(stage)?;
                }
                let mut args = clone.global.to_vec();
                args.extend([BString::from("clone"), "--no-local".into()]);
                args.extend_from_slice(clone.options);
                args.extend(["--".into(), from_native(mirror), from_native(stage)]);
                self.git(cwd, args, true)?;
                if let Some(commit) = &pin {
                    self.git(
                        stage,
                        vec![
                            "fetch".into(),
                            "--no-tags".into(),
                            "--".into(),
                            from_native(mirror),
                            commit.as_str().into(),
                        ],
                        true,
                    )?;
                }
                self.restore_remote(stage, clone.origin, &repository)
            })
        })
    }

    fn owns_full_objects(&self, repository: &Path) -> bool {
        [
            "shallow",
            "objects/info/alternates",
            "objects/info/http-alternates",
        ]
        .iter()
        .all(|path| {
            self.host
                .metadata(&repository.join(path), Follow::No)
                .is_err_and(|error| error.kind == ErrorKind::NotFound)
        })
    }

    /// Detach an existing checkout at a full commit ID without network access.
    pub fn checkout_commit(&self, source_dir: &Path, commit: &str) -> Result<(), FetchError> {
        let commit = check_commit(commit)?;
        self.git(
            source_dir,
            vec![
                "checkout".into(),
                "--detach".into(),
                commit.into(),
                "--".into(),
            ],
            true,
        )?;
        Ok(())
    }

    /// Read the full commit ID checked out in an existing source directory.
    pub fn head(&self, source_dir: &Path) -> Result<String, FetchError> {
        let bytes = self.git(
            source_dir,
            vec!["rev-parse".into(), "--verify".into(), "HEAD".into()],
            true,
        )?;
        check_commit(std::str::from_utf8(&bytes).unwrap_or_default().trim())
    }

    /// Read HEAD only if Git confirms the expected checkout root.
    ///
    /// # Errors
    ///
    /// Return an error if the directory has no checkout or Git cannot read HEAD.
    pub fn checkout_head(&self, source_dir: &Path) -> Result<String, FetchError> {
        let root = self.host.canonicalize(source_dir)?;
        let metadata = self
            .host
            .metadata(&root.join(".git"), rcmake_host::Follow::Yes)?;
        if !matches!(
            metadata.kind,
            rcmake_host::FileKind::Dir | rcmake_host::FileKind::File
        ) {
            return Err(invalid("Missing Git checkout metadata."));
        }
        let bytes = self.git(
            &root,
            vec![
                "rev-parse".into(),
                "--show-toplevel".into(),
                "--verify".into(),
                "HEAD".into(),
            ],
            true,
        )?;
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let split = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .ok_or_else(|| invalid("Git did not report the checkout root and HEAD."))?;
        let (worktree, head) = bytes.split_at(split);
        let worktree = worktree.strip_suffix(b"\r").unwrap_or(worktree);
        let worktree = rcmake_host::path::to_native(worktree);
        if !worktree.is_absolute() || self.host.canonicalize(&worktree)? != root {
            return Err(invalid("Git reported a different checkout root."));
        }
        check_commit(std::str::from_utf8(head).unwrap_or_default().trim())
    }

    fn restore_remote(
        &self,
        source_dir: &Path,
        origin: &str,
        repository: &BString,
    ) -> Result<(), FetchError> {
        self.git(
            source_dir,
            vec![
                "remote".into(),
                "set-url".into(),
                "--".into(),
                origin.into(),
                repository.clone(),
            ],
            true,
        )?;
        Ok(())
    }

    fn staged_checkout(
        &self,
        source: &Path,
        populate: impl FnOnce(&Path) -> Result<(), FetchError>,
    ) -> Result<(), FetchError> {
        let parent = source
            .parent()
            .ok_or_else(|| invalid("Git source directory has no parent."))?;
        if source.file_name().is_none() {
            return Err(invalid("Invalid Git source directory."));
        }
        self.host.create_dir_all(parent)?;
        let _lock = self.host.lock_file(
            &source.with_added_extension("rcmake-fetch.guard"),
            self.remaining()?,
        )?;
        if exists(self.host, source)? && !self.host.read_dir(source)?.is_empty() {
            return Err(invalid("Git source directory already contains files."));
        }
        let stage = staging_path(self.host, source);
        if exists(self.host, &stage)? {
            return Err(invalid("Git staging directory already exists."));
        }
        let result = populate(&stage).and_then(|()| {
            if exists(self.host, source)? {
                self.host.remove_dir(source)?;
            }
            self.host.rename_noreplace(&stage, source)?;
            Ok(())
        });
        if result.is_err() {
            let _ = self.remove_stage(&stage);
        }
        result
    }

    fn with_mirror<T>(
        &self,
        repository: &BString,
        pin: Option<&str>,
        config: &[BString],
        cwd: &Path,
        use_mirror: impl FnOnce(&Path) -> Result<T, FetchError>,
    ) -> Result<T, FetchError> {
        let digest = Digest::hash(repository);
        let mirror = self.root.join(format!("{digest}.git"));
        let result = self.use_locked_mirror(repository, &mirror, pin, config, cwd, use_mirror);
        result.map_err(|cause| FetchError::Git {
            repository: sanitized_url(&String::from_utf8_lossy(repository)),
            commit: pin.map(str::to_owned),
            path: mirror,
            cause: Box::new(cause),
        })
    }

    fn use_locked_mirror<T>(
        &self,
        repository: &BString,
        mirror: &Path,
        pin: Option<&str>,
        config: &[BString],
        cwd: &Path,
        use_mirror: impl FnOnce(&Path) -> Result<T, FetchError>,
    ) -> Result<T, FetchError> {
        let offline = self.offline_only || self.read_only || offline(self.host);
        if !offline {
            self.admit_cache_root(true)?;
            self.host.create_dir_all(&self.root)?;
        } else if pin.is_none() {
            return Err(invalid("Offline Git fetch requires a full commit pin."));
        } else {
            match self.admit_cache_root(false) {
                Ok(()) => {}
                Err(error) if error.kind == ErrorKind::NotFound => return Err(offline_git_miss()),
                Err(error) => return Err(error.into()),
            }
            if self.mirror_kind(mirror)?.is_none() {
                return Err(offline_git_miss());
            }
        }
        let stamp_path = mirror.with_added_extension("refresh");
        let before = self.refresh_stamp(&stamp_path);
        let _lock = if self.read_only {
            None
        } else {
            match self
                .host
                .lock_file(&mirror.with_added_extension("lock"), self.remaining()?)
            {
                Ok(guard) => Some(guard),
                Err(cause) if offline && cause.kind == ErrorKind::ReadOnlyFilesystem => {
                    return Err(FetchError::ReadOnlyGitCache { cause });
                }
                Err(error) => return Err(error.into()),
            }
        };
        let kind = self.mirror_kind(mirror)?;
        let mut valid = match kind {
            Some(FileKind::Dir) => self.valid_mirror(cwd, mirror)?,
            _ => false,
        };
        if valid && let Some(commit) = pin {
            match self.has_commit(cwd, mirror, commit) {
                Ok(true) => {
                    if !offline {
                        self.keep_commit(cwd, mirror, commit)?;
                    }
                    return use_mirror(mirror);
                }
                Ok(false) => {}
                Err(FetchError::DamagedGitMirror) => valid = false,
                Err(error) => return Err(error),
            }
        }
        if offline && kind.is_some() && !valid {
            return Err(FetchError::DamagedGitMirror);
        }
        if offline {
            return Err(offline_git_miss());
        }
        let mut key = KeyHasher::new(Domain::FetchV1);
        for flag in config {
            key.field(0, flag);
        }
        let context = key.finish().to_hex();
        let last = self.refresh_stamp(&stamp_path);
        if valid
            && pin.is_none()
            && last != before
            && last
                .as_ref()
                .is_some_and(|(_, previous)| previous == &context)
        {
            return use_mirror(mirror);
        }
        if valid {
            let result = self.refresh(cwd, mirror, config, repository);
            if let Err(error) = result {
                // A network failure leaves a valid mirror intact.
                if !self.valid_mirror(cwd, mirror)? {
                    self.rebuild(cwd, mirror, repository, config, kind, pin)?;
                    valid = false;
                } else {
                    return Err(error);
                }
            } else {
                self.clean_cached_remote(mirror, repository)?;
            }
        } else {
            self.rebuild(cwd, mirror, repository, config, kind, pin)?;
        }
        if valid && let Some(commit) = pin {
            self.prepare_pin(cwd, mirror, repository, config, commit)?;
        }
        let generation = last.map_or(1, |(generation, _)| generation.wrapping_add(1));
        atomic_write(
            self.host,
            &stamp_path,
            format!("{generation} {context}\n").as_bytes(),
        )?;
        use_mirror(mirror)
    }

    fn prepare_pin(
        &self,
        cwd: &Path,
        mirror: &Path,
        repository: &BString,
        config: &[BString],
        commit: &str,
    ) -> Result<(), FetchError> {
        if !self.has_commit(cwd, mirror, commit)? {
            let mut args = config.to_vec();
            args.extend([
                "--git-dir".into(),
                from_native(mirror),
                "fetch".into(),
                "--no-tags".into(),
                "--".into(),
                repository.clone(),
                commit.into(),
            ]);
            self.git(cwd, args, false)?;
            if !self.has_commit(cwd, mirror, commit)? {
                return Err(invalid(
                    "The Git mirror does not contain the requested commit. Check the repository and pin.",
                ));
            }
        }
        self.keep_commit(cwd, mirror, commit)
    }

    fn mirror_kind(&self, path: &Path) -> Result<Option<FileKind>, FetchError> {
        match self.host.metadata(path, Follow::No) {
            Ok(metadata) => Ok(Some(metadata.kind)),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn valid_mirror(&self, cwd: &Path, mirror: &Path) -> Result<bool, FetchError> {
        if self.mirror_kind(&mirror.join("HEAD"))? != Some(FileKind::File)
            || self.mirror_kind(&mirror.join("objects"))? != Some(FileKind::Dir)
        {
            return Ok(false);
        }
        match self.mirror_git(
            cwd,
            mirror,
            &[],
            &["rev-parse", "--is-bare-repository"],
            true,
        ) {
            Ok(output) => Ok(output.trim_ascii() == b"true"),
            Err(error) if invalid_repository_metadata(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn rebuild(
        &self,
        cwd: &Path,
        mirror: &Path,
        repository: &BString,
        config: &[BString],
        previous: Option<FileKind>,
        pin: Option<&str>,
    ) -> Result<(), FetchError> {
        let stage = staging_path(self.host, mirror);
        if self.mirror_kind(&stage)?.is_some() {
            return Err(invalid("Git staging path already exists."));
        }
        let mut args = config.to_vec();
        args.extend([
            "clone".into(),
            "--mirror".into(),
            "--".into(),
            repository.clone(),
            from_native(&stage),
        ]);
        let result = self.git(cwd, args, false).and_then(|_| {
            if !self.valid_mirror(cwd, &stage)? {
                return Err(FetchError::DamagedGitMirror);
            }
            self.clean_cached_remote(&stage, repository)?;
            if let Some(commit) = pin {
                self.prepare_pin(cwd, &stage, repository, config, commit)?;
            }
            self.publish_replacement(mirror, &stage, previous)
        });
        if result.is_err() {
            let _ = self.remove_stage(&stage);
        }
        result
    }

    fn publish_replacement(
        &self,
        mirror: &Path,
        stage: &Path,
        previous: Option<FileKind>,
    ) -> Result<(), FetchError> {
        let Some(kind) = previous else {
            self.host.rename_noreplace(stage, mirror)?;
            return Ok(());
        };
        let damaged = staging_path(self.host, mirror);
        self.host.rename_noreplace(mirror, &damaged)?;
        if let Err(error) = self.host.rename_noreplace(stage, mirror) {
            self.host.rename_noreplace(&damaged, mirror)?;
            return Err(error.into());
        }
        if kind == FileKind::Dir {
            let _ = self.host.remove_dir_all(&damaged);
        } else {
            let _ = self.host.remove_file(&damaged);
        }
        Ok(())
    }

    fn clean_cached_remote(&self, mirror: &Path, repository: &BString) -> Result<(), FetchError> {
        if let Ok(original) = std::str::from_utf8(repository) {
            let sanitized = sanitized_url(original);
            if sanitized != original {
                self.restore_remote(mirror, "origin", &sanitized.into())?;
            }
        }
        Ok(())
    }

    fn refresh(
        &self,
        cwd: &Path,
        mirror: &Path,
        config: &[BString],
        repository: &BString,
    ) -> Result<(), FetchError> {
        let mut args = config.to_vec();
        args.extend([
            "--git-dir".into(),
            from_native(mirror),
            "fetch".into(),
            "--prune".into(),
            "--".into(),
            repository.clone(),
            "+refs/*:refs/*".into(),
            "^refs/rcmake/pins/*".into(),
        ]);
        self.git(cwd, args, false)?;
        self.refresh_head(cwd, mirror, config, repository)
    }

    fn refresh_head(
        &self,
        cwd: &Path,
        mirror: &Path,
        config: &[BString],
        repository: &BString,
    ) -> Result<(), FetchError> {
        let mut args = config.to_vec();
        args.extend([
            "--git-dir".into(),
            from_native(mirror),
            "ls-remote".into(),
            "--symref".into(),
            "--".into(),
            repository.clone(),
            "HEAD".into(),
        ]);
        let output = self.git(cwd, args, false)?;
        for line in output.split(|&byte| byte == b'\n') {
            let Some(target) = line.strip_suffix(b"\tHEAD") else {
                continue;
            };
            let target =
                std::str::from_utf8(target).map_err(|_| invalid("Invalid remote Git HEAD."))?;
            if let Some(reference) = target.strip_prefix("ref: refs/") {
                self.mirror_git(
                    cwd,
                    mirror,
                    &[],
                    &["symbolic-ref", "HEAD", &format!("refs/{reference}")],
                    true,
                )?;
                break;
            }
            if is_commit(target) {
                self.mirror_git(
                    cwd,
                    mirror,
                    &[],
                    &["update-ref", "--no-deref", "HEAD", target],
                    true,
                )?;
                break;
            }
            return Err(invalid("Invalid remote Git HEAD."));
        }
        Ok(())
    }

    fn refresh_stamp(&self, path: &Path) -> Option<(u64, String)> {
        let bytes = self.host.read(path, ReadRange::All { limit: 128 }).ok()?;
        let (generation, context) = std::str::from_utf8(&bytes)
            .ok()?
            .trim_ascii()
            .split_once(' ')?;
        Some((
            generation.parse().ok()?,
            Digest::from_hex(context)?.to_hex(),
        ))
    }

    fn keep_commit(&self, cwd: &Path, mirror: &Path, commit: &str) -> Result<(), FetchError> {
        self.mirror_git(
            cwd,
            mirror,
            &[],
            &["update-ref", &format!("refs/rcmake/pins/{commit}"), commit],
            true,
        )?;
        Ok(())
    }

    fn has_commit(&self, cwd: &Path, mirror: &Path, commit: &str) -> Result<bool, FetchError> {
        let tree = format!("{commit}^{{tree}}");
        let output = match self.git_with_input(
            cwd,
            vec![
                "--git-dir".into(),
                from_native(mirror),
                "cat-file".into(),
                "--batch-check=%(objectname) %(objecttype)".into(),
            ],
            true,
            Input::Bytes(format!("{commit}\n{tree}\n").into_bytes()),
        ) {
            Ok(output) => output,
            Err(FetchError::Process { status, .. }) if status.starts_with("exit code 128") => {
                return Err(FetchError::DamagedGitMirror);
            }
            Err(error) => return Err(error),
        };
        let mut lines = output.split(|byte| *byte == b'\n');
        if lines.next() != Some(format!("{commit} commit").as_bytes()) {
            return Ok(false);
        }
        if let Some(tree) = lines.next().and_then(|line| line.strip_suffix(b" tree"))
            && std::str::from_utf8(tree).is_ok_and(is_commit)
        {
            Ok(true)
        } else {
            Err(FetchError::DamagedGitMirror)
        }
    }

    fn mirror_git(
        &self,
        cwd: &Path,
        mirror: &Path,
        config: &[BString],
        args: &[&str],
        local: bool,
    ) -> Result<Vec<u8>, FetchError> {
        let mut command = config.to_vec();
        command.extend(["--git-dir".into(), from_native(mirror)]);
        command.extend(args.iter().map(|arg| BString::from(*arg)));
        self.git(cwd, command, local)
    }

    fn git(&self, cwd: &Path, args: Vec<BString>, local: bool) -> Result<Vec<u8>, FetchError> {
        self.git_with_input(cwd, args, local, Input::Null)
    }

    fn git_with_input(
        &self,
        cwd: &Path,
        args: Vec<BString>,
        local: bool,
        input: Input,
    ) -> Result<Vec<u8>, FetchError> {
        let mut env = self.host.env_all();
        let windows = self.host.system().sysname == "Windows";
        env.retain(|(key, _)| {
            if windows {
                let key = key.to_ascii_uppercase();
                !key.starts_with(b"GIT_") || (!local && network_git_env(&key))
            } else {
                !key.starts_with(b"GIT_") || (!local && network_git_env(key))
            }
        });
        env.retain(|(key, _)| !key.eq_ignore_ascii_case(b"LC_ALL"));
        env.extend([
            ("LC_ALL".into(), "C".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_NO_LAZY_FETCH".into(), "1".into()),
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
            (
                "GIT_ALLOW_PROTOCOL".into(),
                if local {
                    "file".into()
                } else {
                    "file:git:http:https:ssh".into()
                },
            ),
        ]);
        if local {
            let null = if windows { "NUL" } else { "/dev/null" };
            env.extend([
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("GIT_CONFIG_GLOBAL".into(), null.into()),
                ("GIT_TEMPLATE_DIR".into(), "".into()),
            ]);
        }
        Ok(crate::run_command(
            self.host,
            "git",
            rcmake_host::CommandSpec {
                program: self.program.clone(),
                args,
            },
            cwd,
            env,
            input,
            OUTPUT_LIMIT,
            self.remaining()?,
        )?
        .stdout)
    }

    fn remove_stage(&self, stage: &Path) -> Result<(), FetchError> {
        match self.host.remove_dir_all(stage) {
            Ok(()) => Ok(()),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn offline_git_miss() -> FetchError {
    invalid(
        "The requested Git commit is missing from the offline cache. Seed this commit online and check the mirror path.",
    )
}

fn invalid_repository_metadata(error: &FetchError) -> bool {
    matches!(error, FetchError::Process { program: "git", status }
        if status.ends_with("(invalid repository metadata)"))
}

fn network_git_env(key: &[u8]) -> bool {
    key.starts_with(b"GIT_CONFIG_")
        || key.starts_with(b"GIT_SSL_")
        || key.starts_with(b"GIT_CURL_")
        || matches!(
            key,
            b"GIT_SSH"
                | b"GIT_SSH_COMMAND"
                | b"GIT_SSH_VARIANT"
                | b"GIT_ASKPASS"
                | b"GIT_PROXY_COMMAND"
                | b"GIT_HTTP_PROXY_AUTHMETHOD"
        )
}

fn exists(host: &dyn Host, path: &Path) -> Result<bool, FetchError> {
    match host.metadata(path, Follow::No) {
        Ok(metadata) if metadata.kind == FileKind::Dir => Ok(true),
        Ok(_) => Err(invalid("Git cache or source path is not a directory.")),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn repository_at(repository: &str, cwd: &Path) -> Result<BString, FetchError> {
    if plain(repository.as_bytes()).is_none() {
        return Err(invalid("Invalid Git repository."));
    }
    let path = to_native(repository.as_bytes());
    if path.is_absolute() {
        return Ok(from_native(&path));
    }
    // Git uses a colon before the first slash for SSH shorthand and URL schemes.
    let first = repository.split('/').next().unwrap_or_default();
    if first.contains(':') {
        return Ok(repository.into());
    }
    Ok(from_native(&cwd.join(path)))
}
