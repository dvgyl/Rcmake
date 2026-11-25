// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{path::Path, sync::Arc};

use rcmake_base::hash::HashAlgo;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, Cx, FetchLockMode};
use rcmake_fetch::{ArchiveCache, ContentHash, LockEntry, UrlEntry};
use rcmake_host::{
    FileKind, Follow, ReadRange,
    path::{from_native_cmake, hash_file, to_native},
};
use rcmake_store::{Digest, Store, StoreMode};

use super::runtime::{Population, Runtime, Script, blocked, lock_mutex, required, runtime, stop};
use super::{ArchiveTransfer, BlockReason, DeclaredSource, sanitize_url};

pub(crate) struct ArchivePlan {
    runtime: Arc<Runtime>,
    dependency: String,
    urls: Vec<String>,
    url: String,
    expected: Option<ContentHash>,
    locked: Option<UrlEntry>,
    mode: FetchLockMode,
    offline: bool,
}

pub(crate) fn prepare_archive(
    cx: &mut Cx<'_>,
    url: &str,
    args: &[&[u8]],
) -> Result<Option<ArchivePlan>, CmdError> {
    let Some(runtime) = runtime(cx) else {
        return Ok(None);
    };
    let Some(pop) = runtime.population(cx) else {
        return Ok(None);
    };
    if runtime.script(cx, &pop) != Some(Script::Archive) {
        required(cx, &runtime, &pop, BlockReason::Unsupported)?;
        return Ok(None);
    }
    let download_dir = to_native(&cx.var_or_empty(b"_EP_DOWNLOAD_DIR"));
    if !download_dir.is_absolute()
        || !args
            .first()
            .is_some_and(|file| to_native(file).starts_with(&download_dir))
    {
        required(cx, &runtime, &pop, BlockReason::Unsupported)?;
        return Ok(None);
    }
    archive_plan(cx, runtime, &pop, url, args)
}

/// Local path of a `file://` URL or of a plain path. A Windows URL such as
/// `file:///C:/dir/file` names `C:/dir/file`. Returns `None` for other schemes.
pub(super) fn local_url_path(url: &str) -> Option<&str> {
    let Some(rest) = url.strip_prefix("file://") else {
        return (!url.contains("://")).then_some(url);
    };
    let drive = rest.as_bytes();
    if let [b'/', letter, b':', b'/' | b'\\', ..] = drive
        && letter.is_ascii_alphabetic()
    {
        return rest.get(1..);
    }
    Some(rest)
}

pub(super) fn prepare_local_archive(
    cx: &mut Cx<'_>,
    runtime: Arc<Runtime>,
    pop: &Population,
) -> Result<(), CmdError> {
    let url = cx.var_or_empty(b"_EP_URL");
    let Ok(url) = std::str::from_utf8(&url) else {
        return Ok(());
    };
    let Some(local) = local_url_path(url).filter(|_| !url.is_empty() && !url.contains(';')) else {
        return Ok(());
    };
    let archive = runtime.with_fetch(pop, |fetch| {
        fetch.get(&pop.dependency).is_some_and(|declaration| {
            declaration.supported()
                && matches!(&declaration.source, DeclaredSource::Archive { urls, .. }
                if matches!(urls.as_slice(), [only] if only == url))
        })
    });
    if !archive {
        return Ok(());
    }
    let path = cx.resolve_path(local.as_bytes());
    if runtime
        .host
        .metadata(&path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return Ok(());
    }
    let Some(plan) = archive_plan(cx, runtime, pop, url, &[b"local archive"])? else {
        return Ok(());
    };
    let Some(name) = path.file_name() else {
        return Ok(());
    };
    let file = to_native(&cx.var_or_empty(b"_EP_DOWNLOAD_DIR"))
        .join("rcmake-local")
        .join(name);
    if (plan.offline || plan.mode == FetchLockMode::Frozen) && plan.restore(cx, &file)? {
        cx.set_var(b"_EP_URL", from_native_cmake(&file));
        return Ok(());
    }
    if cx
        .host()
        .metadata(&path, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
        && !plan.complete(cx, &path)?
    {
        return Err(stop(
            cx,
            "The local archive does not match the required content.",
        ));
    }
    Ok(())
}

fn archive_plan(
    cx: &mut Cx<'_>,
    runtime: Arc<Runtime>,
    pop: &Population,
    url: &str,
    args: &[&[u8]],
) -> Result<Option<ArchivePlan>, CmdError> {
    let plan = runtime.with_fetch(pop, |fetch| {
        let transfer = fetch.match_archive(pop.site(), url, args)?;
        Ok(transfer
            .filter(ArchiveTransfer::is_portable)
            .map(|transfer| ArchivePlan {
                runtime: Arc::clone(&runtime),
                dependency: transfer.dependency.into(),
                urls: transfer.urls.to_vec(),
                url: transfer.url.into(),
                expected: transfer
                    .declared_hash
                    .and_then(|hash| ContentHash::parse(hash).ok()),
                locked: None,
                mode: fetch.mode,
                offline: fetch.offline,
            }))
    });
    let mut plan = match plan {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            required(cx, &runtime, pop, BlockReason::Unsupported)?;
            return Ok(None);
        }
        Err(error) => {
            blocked(cx, Some(error))?;
            return Ok(None);
        }
    };
    plan.find_lock(cx, pop)?;
    if plan.offline && plan.pin().is_none() {
        required(cx, &runtime, pop, BlockReason::Unpinned)?;
    }
    Ok(Some(plan))
}

pub(super) fn frozen_pin(
    cx: &mut Cx<'_>,
    runtime: Arc<Runtime>,
    pop: &Population,
    urls: Vec<String>,
    expected: Option<ContentHash>,
) -> Result<Option<UrlEntry>, CmdError> {
    let url = urls.first().cloned().unwrap_or_default();
    let mut plan = ArchivePlan {
        runtime,
        dependency: pop.dependency.clone(),
        urls,
        url,
        expected,
        locked: None,
        mode: FetchLockMode::Frozen,
        offline: false,
    };
    plan.find_lock(cx, pop)?;
    if let (Some(expected), Some(pin)) = (&plan.expected, &plan.locked)
        && expected.algorithm() == HashAlgo::Sha256
        && *expected != ContentHash::sha256(pin.sha256)
    {
        required(cx, &plan.runtime, pop, BlockReason::LockMismatch)?;
    }
    Ok(plan.locked)
}

impl ArchivePlan {
    fn find_lock(&mut self, cx: &mut Cx<'_>, pop: &Population) -> Result<(), CmdError> {
        let selection = {
            let session = lock_mutex(&self.runtime.lock);
            match session.as_ref() {
                Some(Ok(lock)) => Ok(match lock.snapshot().get(&self.dependency) {
                    Some(LockEntry::Url(entry)) if entry.urls == self.urls => Some(entry.clone()),
                    _ => None,
                }),
                Some(Err(error))
                    if matches!(self.mode, FetchLockMode::Update | FetchLockMode::Frozen) =>
                {
                    Err(error.to_string())
                }
                _ => Ok(None),
            }
        };
        self.locked = selection.map_err(|error| stop(cx, error))?;
        if self.mode == FetchLockMode::Frozen && self.locked.is_none() {
            required(cx, &self.runtime, pop, BlockReason::LockMismatch)?;
        }
        Ok(())
    }

    fn enforced(&self) -> Option<&UrlEntry> {
        self.locked.as_ref().filter(|_| {
            self.mode == FetchLockMode::Frozen || (self.offline && self.expected.is_none())
        })
    }

    fn pin(&self) -> Option<ContentHash> {
        self.enforced()
            .map(|lock| ContentHash::sha256(lock.sha256))
            .or_else(|| self.expected.clone())
    }

    pub(crate) fn check_network(&self, cx: &mut Cx<'_>) -> Result<(), CmdError> {
        if self.offline {
            return Err(stop(
                cx,
                "Offline fetch could not find the archive in the fetch cache.",
            ));
        }
        crate::hermetic::check_fetch_network(cx, &self.url, self.pin().is_some(), &self.dependency)
    }

    pub(crate) fn restore(&self, cx: &mut Cx<'_>, file: &Path) -> Result<bool, CmdError> {
        if let Some(pin) = self.pin()
            && self.runtime.config.mode != StoreMode::Off
            && let Ok(store) = Store::new(self.runtime.host.as_ref(), self.runtime.config.clone())
        {
            self.runtime.wait_for_archive(&self.url, &pin);
            let cache = ArchiveCache::new(self.runtime.host.as_ref(), &store);
            if let Ok(Some(bytes)) = cache.lookup(&self.url, &pin) {
                if !self.verify(&bytes) {
                    return Err(stop(
                        cx,
                        "Cached archive does not match the required content.",
                    ));
                }
                if let Some(parent) = file.parent() {
                    cx.host()
                        .create_dir_all(parent)
                        .map_err(|_| stop(cx, "Cannot create the download directory."))?;
                }
                ArchiveCache::write_verified(cx.host(), file, &bytes)
                    .map_err(|_| stop(cx, "Cannot write the cached archive."))?;
                self.record(cx, UrlEntry::from_bytes(self.urls.clone(), &bytes))?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn verify(&self, bytes: &[u8]) -> bool {
        self.expected
            .as_ref()
            .is_none_or(|hash| hash.verify(bytes).is_ok())
            && self.enforced().is_none_or(|entry| {
                entry.size == bytes.len() as u64
                    && ContentHash::sha256(entry.sha256).verify(bytes).is_ok()
            })
    }

    /// Verify the original result and publish a cache entry when the bytes fit.
    ///
    /// A frozen mismatch returns false so the bundled script can try another mirror.
    pub(crate) fn complete(&self, cx: &mut Cx<'_>, file: &Path) -> Result<bool, CmdError> {
        if self.expected.is_none() && self.locked.is_none() && self.mode != FetchLockMode::Update {
            return Ok(true);
        }
        let limit = cx.limits().max_file_read;
        let file_size = cx
            .host()
            .metadata(file, Follow::Yes)
            .ok()
            .map(|meta| meta.len);
        if let Some(size) = file_size.filter(|&size| size > limit) {
            return self.complete_large(cx, file, size);
        }
        let bytes = match cx.host().read(file, ReadRange::All { limit }) {
            Ok(bytes) => bytes,
            Err(_) if !matches!(self.mode, FetchLockMode::Update | FetchLockMode::Frozen) => {
                return Ok(true);
            }
            Err(_) => {
                return Err(stop(
                    cx,
                    "Could not read the downloaded archive to check the fetch lock.",
                ));
            }
        };
        if !self.verify(&bytes) {
            return Ok(self.mode != FetchLockMode::Frozen && !self.offline);
        }
        let entry = UrlEntry::from_bytes(self.urls.clone(), &bytes);
        if let Some(pin) = self.pin().or_else(|| {
            (self.mode == FetchLockMode::Update).then(|| ContentHash::sha256(entry.sha256))
        }) && self.runtime.config.mode != StoreMode::Off
            && let Ok(store) = Store::new(self.runtime.host.as_ref(), self.runtime.config.clone())
        {
            let cache = ArchiveCache::new(self.runtime.host.as_ref(), &store);
            let _ = cache.insert(&self.url, &pin, &bytes);
        }
        self.record(cx, entry)?;
        Ok(true)
    }

    fn complete_large(&self, cx: &mut Cx<'_>, file: &Path, size: u64) -> Result<bool, CmdError> {
        if self
            .expected
            .as_ref()
            .is_some_and(|hash| hash.verify_file(cx.host(), file).is_err())
        {
            return Ok(self.mode != FetchLockMode::Frozen && !self.offline);
        }
        if self.locked.is_none() && self.mode != FetchLockMode::Update {
            return Ok(true);
        }
        let Some(sha256) = hash_file(cx.host(), file, HashAlgo::Sha256)
            .ok()
            .and_then(|hex| Digest::from_hex(&hex))
        else {
            return Err(stop(
                cx,
                "Could not hash the downloaded archive to check the fetch lock.",
            ));
        };
        if self
            .enforced()
            .is_some_and(|entry| entry.size != size || entry.sha256 != sha256)
        {
            return Ok(false);
        }
        self.record(cx, UrlEntry::from_digest(self.urls.clone(), sha256, size))?;
        Ok(true)
    }

    fn record(&self, cx: &mut Cx<'_>, entry: UrlEntry) -> Result<(), CmdError> {
        if self.mode == FetchLockMode::Auto
            && self.expected.is_none()
            && self
                .locked
                .as_ref()
                .is_some_and(|lock| lock.size != entry.size || lock.sha256 != entry.sha256)
        {
            cx.issue(
                MessageKind::Warning,
                Effect::None,
                None,
                format!(
                    "The archive at {} differs from the fetch lock entry.",
                    sanitize_url(&self.url),
                )
                .into(),
            );
        }
        if self.mode == FetchLockMode::Update
            && let Some(Ok(lock)) = lock_mutex(&self.runtime.lock).as_mut()
            && lock
                .record(&self.dependency, LockEntry::Url(entry))
                .is_err()
        {
            return Err(stop(
                cx,
                "Could not record the archive in the fetch lockfile.",
            ));
        }
        Ok(())
    }
}
