use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_host::{
    FileKind, Follow, Host, ReadRange,
    path::{from_native, to_native},
};
use rcmake_store::{Digest, Domain, KeyHasher};

use rcmake_base::BString;

use crate::{
    FetchError, GitClone, atomic_write, clone_args::plain, invalid, offline, run, staging_path,
};

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
    root: PathBuf,
    timeout: Option<Duration>,
}

impl<'a> GitMirror<'a> {
    /// Use the `git` directory below an absolute cache root.
    pub fn new(host: &'a dyn Host, cache_root: &Path) -> Result<Self, FetchError> {
        if !cache_root.is_absolute() {
            return Err(invalid("Git cache root must be absolute."));
        }
        Ok(Self {
            host,
            root: cache_root.join("git"),
            timeout: None,
        })
    }

    /// Set the timeout for Git commands and checkout and mirror locks.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
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
            self.timeout,
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
        self.host.create_dir_all(&self.root)?;
        let stamp_path = mirror.with_added_extension("refresh");
        let before = self.refresh_stamp(&stamp_path);
        let _lock = self
            .host
            .lock_file(&mirror.with_added_extension("lock"), self.timeout)?;
        let present = exists(self.host, &mirror)?;
        if present
            && let Some(commit) = pin
            && self.has_commit(cwd, &mirror, commit)
        {
            if !offline(self.host) {
                self.keep_commit(cwd, &mirror, commit)?;
            }
            return use_mirror(&mirror);
        }
        if offline(self.host) {
            return Err(FetchError::Offline);
        }
        let mut key = KeyHasher::new(Domain::FetchV1);
        for flag in config {
            key.field(0, flag);
        }
        let context = key.finish().to_hex();
        let last = self.refresh_stamp(&stamp_path);
        if present
            && pin.is_none()
            && last != before
            && last
                .as_ref()
                .is_some_and(|(_, previous)| previous == &context)
        {
            return use_mirror(&mirror);
        }
        if present {
            self.mirror_git(
                cwd,
                &mirror,
                config,
                &[
                    "fetch",
                    "--prune",
                    "--",
                    "origin",
                    "+refs/*:refs/*",
                    "^refs/rcmake/pins/*",
                ],
                false,
            )?;
            self.refresh_head(cwd, &mirror, config)?;
        } else {
            let stage = staging_path(self.host, &mirror);
            if exists(self.host, &stage)? {
                return Err(invalid("Git staging directory already exists."));
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
                self.host.rename_noreplace(&stage, &mirror)?;
                Ok(())
            });
            if result.is_err() {
                let _ = self.remove_stage(&stage);
            }
            result?;
        }
        let generation = last.map_or(1, |(generation, _)| generation.wrapping_add(1));
        atomic_write(
            self.host,
            &stamp_path,
            format!("{generation} {context}\n").as_bytes(),
        )?;
        if let Some(commit) = pin {
            if !self.has_commit(cwd, &mirror, commit) {
                self.mirror_git(
                    cwd,
                    &mirror,
                    config,
                    &["fetch", "--no-tags", "--", "origin", commit],
                    false,
                )?;
            }
            if !self.has_commit(cwd, &mirror, commit) {
                return Err(invalid(
                    "The Git mirror does not contain the requested commit.",
                ));
            }
            self.keep_commit(cwd, &mirror, commit)?;
        }
        use_mirror(&mirror)
    }

    fn refresh_head(
        &self,
        cwd: &Path,
        mirror: &Path,
        config: &[BString],
    ) -> Result<(), FetchError> {
        let output = self.mirror_git(
            cwd,
            mirror,
            config,
            &["ls-remote", "--symref", "--", "origin", "HEAD"],
            false,
        )?;
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

    fn has_commit(&self, cwd: &Path, mirror: &Path, commit: &str) -> bool {
        self.mirror_git(cwd, mirror, &[], &["cat-file", "-t", commit], true)
            .is_ok_and(|output| output.trim_ascii() == b"commit")
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
        env.extend([
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
        Ok(run(self.host, "git", args, cwd, env, OUTPUT_LIMIT, self.timeout)?.stdout)
    }

    fn remove_stage(&self, stage: &Path) -> Result<(), FetchError> {
        match self.host.remove_dir_all(stage) {
            Ok(()) => Ok(()),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
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
