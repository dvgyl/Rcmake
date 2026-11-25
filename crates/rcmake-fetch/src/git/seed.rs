// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::Path;

use rcmake_host::{FileKind, path::from_native};
use rcmake_store::Digest;

use super::{GitMirror, check_commit, repository_at};
use crate::{FetchError, LockEntry, invalid, offline, staging_path};

impl GitMirror<'_> {
    /// Store a commit from an already populated checkout without using the remote.
    pub fn seed_checkout(
        &self,
        repository: &str,
        checkout: &Path,
        commit: &str,
    ) -> Result<(), FetchError> {
        if self.read_only || self.offline_only || offline(self.host) {
            return Ok(());
        }
        if !checkout.is_absolute() || !LockEntry::is_portable_source(repository) {
            return Err(invalid("Cannot seed a Git cache from this source."));
        }
        let cwd = checkout
            .parent()
            .ok_or_else(|| invalid("Git checkout has no parent."))?;
        let repository = repository_at(repository, cwd)?;
        let commit = check_commit(commit)?;
        self.host.create_dir_all(&self.root)?;
        let mirror = self.root.join(format!("{}.git", Digest::hash(&repository)));
        let _guard = self
            .host
            .lock_file(&mirror.with_added_extension("lock"), self.remaining()?)?;
        let previous = self.mirror_kind(&mirror)?;
        if previous == Some(FileKind::Dir) && self.valid_mirror(cwd, &mirror)? {
            return self.import_pin(cwd, &mirror, checkout, &commit);
        }

        let stage = staging_path(self.host, &mirror);
        if self.mirror_kind(&stage)?.is_some() {
            return Err(invalid("Git staging path already exists."));
        }
        let result = self
            .git(
                cwd,
                vec![
                    "clone".into(),
                    "--mirror".into(),
                    "--no-local".into(),
                    "--".into(),
                    from_native(checkout),
                    from_native(&stage),
                ],
                true,
            )
            .and_then(|_| {
                if !self.valid_mirror(cwd, &stage)? {
                    return Err(FetchError::DamagedGitMirror);
                }
                self.import_pin(cwd, &stage, checkout, &commit)?;
                self.restore_remote(&stage, "origin", &repository)?;
                self.publish_replacement(&mirror, &stage, previous)
            });
        if result.is_err() {
            let _ = self.remove_stage(&stage);
        }
        result
    }

    fn import_pin(
        &self,
        cwd: &Path,
        mirror: &Path,
        checkout: &Path,
        commit: &str,
    ) -> Result<(), FetchError> {
        if !self.has_commit(cwd, mirror, commit)? {
            self.git(
                cwd,
                vec![
                    "--git-dir".into(),
                    from_native(mirror),
                    "fetch".into(),
                    "--no-tags".into(),
                    "--".into(),
                    from_native(checkout),
                    commit.into(),
                ],
                true,
            )?;
        }
        self.keep_commit(cwd, mirror, commit)
    }
}
