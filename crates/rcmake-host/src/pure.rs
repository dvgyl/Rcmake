//! Deterministic in-memory [`Host`] implementation for fast, reproducible unit tests.
//!
mod fs;

use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use jiff::tz::TimeZone;
use rcmake_base::BString;

use self::fs::{FileSystem, VirtualPath, root};
use crate::error::{HostError, HostResult, SyntheticError};
use crate::{
    CopyOptions, DirEntry, FileLockGuard, FileTime, Follow, Host, Metadata, ReadRange,
    RegistryRoot, RegistryView, RunOutput, RunRequest, Stream, SystemInfo, WriteMode, validate_run,
};

/// An ordered stdio output event recorded by [`PureHost`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoEvent {
    /// Target stream.
    pub stream: Stream,
    /// Bytes written in this event.
    pub bytes: BString,
}

type CommandHandler = dyn Fn(&PureHost, &RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>
    + Send
    + Sync;

type RegistryKey = (RegistryRoot, BString, Option<BString>, RegistryView);

type RegistryPathKey = (RegistryRoot, RegistryView, BString);

#[derive(Clone, Debug)]
enum PureRegistryValue {
    Scalar(BString),
    MultiSz(BString),
}

#[derive(Clone, Debug)]
struct PureRegistryEntry {
    original_name: Option<BString>,
    value: PureRegistryValue,
}

struct PureState {
    env: BTreeMap<BString, BString>,
    events: Vec<IoEvent>,
    cwd: PathBuf,
    exe: PathBuf,
    fs: FileSystem,
    stdin: VecDeque<u8>,
    handler: Option<Arc<CommandHandler>>,
    registry_view: Option<RegistryView>,
    registry: BTreeMap<RegistryKey, PureRegistryEntry>,
    registry_keys: BTreeMap<RegistryPathKey, BString>,
    system: SystemInfo,
}

/// Table of the files that [`PureHost::lock_file`] holds locks on.
#[derive(Debug, Default)]
struct LockTable {
    held: Mutex<BTreeSet<VirtualPath>>,
    released: Condvar,
}

/// Exclusive lock held by [`PureHost::lock_file`]. Drop releases the lock.
#[derive(Debug)]
struct PureFileLock {
    table: Arc<LockTable>,
    key: VirtualPath,
    path: PathBuf,
}

impl FileLockGuard for PureFileLock {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PureFileLock {
    fn drop(&mut self) {
        if let Ok(mut held) = self.table.held.lock() {
            held.remove(&self.key);
        }
        self.table.released.notify_all();
    }
}

/// In-memory deterministic [`Host`] with recorded stdio event log and virtual clock.
pub struct PureHost {
    state: Mutex<PureState>,
    env_epoch: AtomicU64,
    locks: Arc<LockTable>,
}

impl fmt::Debug for PureHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PureHost").finish_non_exhaustive()
    }
}

impl Default for PureHost {
    fn default() -> Self {
        Self::new(
            PathBuf::from("/work"),
            PathBuf::from("/usr/local/bin/cmake"),
        )
    }
}

impl PureHost {
    /// Create a new [`PureHost`] rooted at `cwd` with executable path `exe`.
    #[must_use]
    pub fn new(cwd: PathBuf, exe: PathBuf) -> Self {
        Self {
            state: Mutex::new(PureState {
                env: BTreeMap::new(),
                events: Vec::new(),
                fs: FileSystem::new(&cwd),
                cwd,
                exe,
                stdin: VecDeque::new(),
                handler: None,
                registry_view: None,
                registry: BTreeMap::new(),
                registry_keys: BTreeMap::new(),
                system: default_system(),
            }),
            env_epoch: AtomicU64::new(0),
            locks: Arc::default(),
        }
    }

    /// Set the simulated Windows registry host view for tests.
    pub fn set_registry_host_view(&self, view: RegistryView) -> HostResult<()> {
        self.lock()?.registry_view = Some(view);
        Ok(())
    }

    /// Add a scalar value (`REG_SZ` / `REG_DWORD` / etc.) to the simulated Windows registry.
    pub fn seed_registry_value(
        &self,
        root: RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: RegistryView,
        value: &[u8],
    ) -> HostResult<()> {
        let mut state = self.lock()?;
        register_registry_ancestors(&mut state.registry_keys, root, view, subkey);
        let original_name = name.filter(|name| !name.is_empty()).map(BString::from);
        state.registry.insert(
            registry_key(root, subkey, name.filter(|name| !name.is_empty()), view),
            PureRegistryEntry {
                original_name,
                value: PureRegistryValue::Scalar(value.into()),
            },
        );
        Ok(())
    }

    /// Add a `REG_MULTI_SZ` value (raw NUL-separated bytes) to the simulated Windows registry.
    pub fn seed_registry_multi_sz(
        &self,
        root: RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: RegistryView,
        raw_value: &[u8],
    ) -> HostResult<()> {
        let mut state = self.lock()?;
        register_registry_ancestors(&mut state.registry_keys, root, view, subkey);
        let original_name = name.filter(|name| !name.is_empty()).map(BString::from);
        state.registry.insert(
            registry_key(root, subkey, name.filter(|name| !name.is_empty()), view),
            PureRegistryEntry {
                original_name,
                value: PureRegistryValue::MultiSz(raw_value.into()),
            },
        );
        Ok(())
    }

    /// Register an empty key path in the simulated Windows registry so `SUBKEYS` and
    /// `VALUE_NAMES` enumeration observe it even when it has no values.
    pub fn seed_registry_key(
        &self,
        root: RegistryRoot,
        subkey: &[u8],
        view: RegistryView,
    ) -> HostResult<()> {
        let mut state = self.lock()?;
        register_registry_ancestors(&mut state.registry_keys, root, view, subkey);
        Ok(())
    }

    fn lock(&self) -> HostResult<MutexGuard<'_, PureState>> {
        self.state
            .lock()
            .map_err(|_| HostError::unsupported("PureHost lock poisoned"))
    }

    /// Replace the command handler. The handler receives the full request and an output
    /// callback that forwards chunks only when the request enables tee output.
    ///
    /// The handler supplies the process results and can use the host to simulate I/O.
    pub fn set_command_handler(
        &self,
        handler: impl Fn(&Self, &RunRequest, &mut dyn FnMut(Stream, &[u8])) -> HostResult<RunOutput>
        + Send
        + Sync
        + 'static,
    ) -> HostResult<()> {
        self.lock()?.handler = Some(Arc::new(handler));
        Ok(())
    }

    /// Replace the bytes available to subsequent calls to [`Host::read_stdin`].
    pub fn seed_stdin(&self, bytes: &[u8]) -> HostResult<()> {
        self.lock()?.stdin = bytes.iter().copied().collect();
        Ok(())
    }

    /// Create any missing parent directories, then write the file.
    pub fn seed_file(&self, path: &Path, bytes: &[u8]) -> HostResult<()> {
        self.lock()?.fs.seed_file(path, bytes)
    }

    /// Set the simulated effective user ID. Entries created later are owned by this user.
    ///
    /// The default is root (`Some(0)`). `None` simulates a host with no POSIX user IDs.
    pub fn set_effective_uid(&self, uid: Option<u32>) -> HostResult<()> {
        self.lock()?.fs.effective_uid = uid;
        Ok(())
    }

    /// Set the owner of the entry at `path` without following a final symlink.
    pub fn set_owner(&self, path: &Path, owner: Option<u32>) -> HostResult<()> {
        self.lock()?.fs.set_owner(path, owner)
    }

    /// Replace the operating system identity that [`Host::system`] returns.
    pub fn set_system(&self, system: SystemInfo) -> HostResult<()> {
        self.lock()?.system = system;
        Ok(())
    }

    /// Return a snapshot of all recorded stdio events in order.
    #[must_use]
    pub fn events(&self) -> Vec<IoEvent> {
        self.state
            .lock()
            .map(|s| s.events.clone())
            .unwrap_or_default()
    }

    /// Concatenate all bytes written to `stream`.
    #[must_use]
    pub fn stream_bytes(&self, stream: Stream) -> BString {
        let mut out = Vec::new();
        if let Ok(guard) = self.state.lock() {
            for ev in &guard.events {
                if ev.stream == stream {
                    out.extend_from_slice(ev.bytes.as_slice());
                }
            }
        }
        BString::from(out)
    }
}

fn normalize_registry_subkey(key: &[u8]) -> BString {
    key.iter()
        .map(|byte| {
            if *byte == b'/' {
                b'\\'
            } else {
                byte.to_ascii_lowercase()
            }
        })
        .collect::<Vec<u8>>()
        .into()
}

fn register_registry_ancestors(
    keys: &mut BTreeMap<RegistryPathKey, BString>,
    root: RegistryRoot,
    view: RegistryView,
    subkey: &[u8],
) {
    keys.entry((root, view, BString::default())).or_default();
    let mut normalized = Vec::new();
    let mut original = Vec::new();
    for part in subkey
        .split(|&byte| byte == b'/' || byte == b'\\')
        .filter(|part| !part.is_empty())
    {
        if !normalized.is_empty() {
            normalized.push(b'\\');
            original.push(b'\\');
        }
        normalized.extend(part.iter().map(u8::to_ascii_lowercase));
        original.extend_from_slice(part);
        keys.entry((root, view, BString::from(normalized.as_slice())))
            .or_insert_with(|| BString::from(original.as_slice()));
    }
}

fn registry_key(
    root: RegistryRoot,
    key: &[u8],
    name: Option<&[u8]>,
    view: RegistryView,
) -> RegistryKey {
    let trimmed = key
        .split(|&byte| byte == b'/' || byte == b'\\')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(b"\\".as_slice());
    (
        root,
        normalize_registry_subkey(&trimmed),
        name.map(|name| name.to_ascii_lowercase().into()),
        view,
    )
}

impl Host for PureHost {
    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        self.lock()?.fs.metadata(p, follow)
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.lock()?.fs.read(p, range)
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        self.lock()?.fs.write(p, data, mode)
    }

    fn read_dir(&self, p: &Path) -> HostResult<Vec<DirEntry>> {
        self.lock()?.fs.read_dir(p)
    }

    fn create_dir_all(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.create_dir_all(p)
    }

    fn create_private_dir(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.create_private_dir(p)
    }

    fn remove_file(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.remove_file(p)
    }

    fn remove_dir(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.remove_dir(p)
    }

    fn remove_dir_all(&self, p: &Path) -> HostResult<()> {
        self.lock()?.fs.remove_dir_all(p)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.lock()?.fs.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        require_absolute(from)?;
        require_absolute(to)?;
        let mut state = self.lock()?;
        if state.fs.metadata(to, Follow::No).is_ok() {
            return Err(SyntheticError::AlreadyExists.at(Some(to)));
        }
        state.fs.rename(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<Duration>,
    ) -> HostResult<Box<dyn FileLockGuard>> {
        require_absolute(path)?;
        let key = {
            let mut state = self.lock()?;
            // Create the file if it is missing. An empty append keeps the content.
            state.fs.write(path, b"", WriteMode::Append)?;
            state.fs.canonical_key(path)?
        };
        let poisoned = || HostError::unsupported("PureHost lock table poisoned");
        let mut held = self.locks.held.lock().map_err(|_| poisoned())?;
        match timeout {
            None => {
                while held.contains(&key) {
                    held = self.locks.released.wait(held).map_err(|_| poisoned())?;
                }
            }
            Some(limit) => {
                let (guard, result) = self
                    .locks
                    .released
                    .wait_timeout_while(held, limit, |held| held.contains(&key))
                    .map_err(|_| poisoned())?;
                held = guard;
                if result.timed_out() && held.contains(&key) {
                    return Err(HostError::timed_out(
                        "Timeout reached",
                        Some(path.to_path_buf()),
                    ));
                }
            }
        }
        held.insert(key.clone());
        Ok(Box::new(PureFileLock {
            table: Arc::clone(&self.locks),
            path: key.to_path_buf(),
            key,
        }))
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        self.lock()?.fs.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.lock()?.fs.symlink(target, link)
    }

    fn hard_link(&self, src: &Path, dst: &Path) -> HostResult<()> {
        self.lock()?.fs.hard_link(src, dst)
    }

    fn read_link(&self, p: &Path) -> HostResult<PathBuf> {
        self.lock()?.fs.read_link(p)
    }

    fn canonicalize(&self, p: &Path) -> HostResult<PathBuf> {
        self.lock()?.fs.canonicalize(p)
    }

    fn path_on_disk(&self, p: &Path) -> PathBuf {
        crate::path::path_on_disk_with(p, |dir| self.read_dir(dir))
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        self.lock()?.fs.set_permissions(p, mode)
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.lock()?.fs.set_times(p, atime, mtime, Follow::Yes)
    }

    fn set_symlink_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        self.lock()?.fs.set_times(p, atime, mtime, Follow::No)
    }

    fn registry_host_view(&self) -> Option<RegistryView> {
        self.lock().ok()?.registry_view
    }

    fn registry_value(
        &self,
        root: RegistryRoot,
        key: &[u8],
        name: Option<&[u8]>,
        view: RegistryView,
    ) -> HostResult<Option<BString>> {
        self.registry_value_with_separator(root, key, name, view, None)
    }

    fn registry_value_with_separator(
        &self,
        root: RegistryRoot,
        key: &[u8],
        name: Option<&[u8]>,
        view: RegistryView,
        separator: Option<&[u8]>,
    ) -> HostResult<Option<BString>> {
        let state = self.lock()?;
        if state.registry_view == Some(RegistryView::Bits32) && view == RegistryView::Bits64 {
            return Ok(None);
        }
        let lookup = registry_key(root, key, name.filter(|name| !name.is_empty()), view);
        Ok(state.registry.get(&lookup).map(|entry| match &entry.value {
            PureRegistryValue::Scalar(value) => value.clone(),
            PureRegistryValue::MultiSz(raw) => crate::format_multi_sz(raw, separator),
        }))
    }

    fn registry_value_names(
        &self,
        root: RegistryRoot,
        key: &[u8],
        view: RegistryView,
    ) -> HostResult<Option<Vec<BString>>> {
        let state = self.lock()?;
        if state.registry_view == Some(RegistryView::Bits32) && view == RegistryView::Bits64 {
            return Ok(None);
        }
        let target = registry_key(root, key, None, view).1;
        if !state
            .registry_keys
            .contains_key(&(root, view, target.clone()))
        {
            return Ok(None);
        }
        let names = state
            .registry
            .iter()
            .filter(|((entry_root, entry_key, _, entry_view), _)| {
                *entry_root == root && *entry_view == view && *entry_key == target
            })
            .map(|(_, entry)| {
                entry
                    .original_name
                    .clone()
                    .unwrap_or_else(|| BString::from("(default)"))
            })
            .collect();
        Ok(Some(names))
    }

    fn registry_subkeys(
        &self,
        root: RegistryRoot,
        key: &[u8],
        view: RegistryView,
    ) -> HostResult<Option<Vec<BString>>> {
        let state = self.lock()?;
        if state.registry_view == Some(RegistryView::Bits32) && view == RegistryView::Bits64 {
            return Ok(None);
        }
        let target = registry_key(root, key, None, view).1;
        if !state
            .registry_keys
            .contains_key(&(root, view, target.clone()))
        {
            return Ok(None);
        }
        let mut subkeys = Vec::new();
        for ((entry_root, entry_view, entry_key), original_path) in &state.registry_keys {
            if *entry_root != root || *entry_view != view || entry_key.is_empty() {
                continue;
            }
            let suffix = if target.is_empty() {
                Some(entry_key.as_slice())
            } else {
                entry_key
                    .strip_prefix(target.as_slice())
                    .and_then(|rest| rest.strip_prefix(b"\\"))
            };
            if let Some(child) = suffix
                && !child.is_empty()
                && !child.contains(&b'\\')
                && let Some(original_leaf) = original_path.rsplit(|&byte| byte == b'\\').next()
            {
                subkeys.push(BString::from(original_leaf));
            }
        }
        Ok(Some(subkeys))
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        self.state.lock().ok()?.env.get(key).cloned()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        if let Ok(mut guard) = self.state.lock() {
            self.env_epoch.fetch_add(1, Ordering::AcqRel);
            match value {
                Some(v) => {
                    guard.env.insert(BString::from(key), BString::from(v));
                }
                None => {
                    guard.env.remove(key);
                }
            }
        }
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.state
            .lock()
            .map(|g| g.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn env_epoch(&self) -> Option<u64> {
        Some(self.env_epoch.load(Ordering::Acquire))
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        validate_run(req, require_absolute)?;
        let handler = self
            .lock()?
            .handler
            .clone()
            .ok_or_else(|| HostError::unsupported("No command handler is registered"))?;
        let mut sent = Vec::new();
        let mut output = handler(self, req, &mut |stream, bytes| {
            let live = crate::is_streamed(req, stream);
            if live && !sent.contains(&stream) {
                sent.push(stream);
            }
            if req.tee || live {
                on_output(stream, bytes);
            }
        })?;
        for (stream, bytes) in [
            (Stream::Stdout, &mut output.stdout),
            (Stream::Stderr, &mut output.stderr),
        ] {
            if crate::is_streamed(req, stream) {
                let bytes = std::mem::take(bytes);
                if !sent.contains(&stream) && !bytes.is_empty() {
                    on_output(stream, &bytes);
                }
            }
        }
        Ok(output)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        if let Ok(mut guard) = self.state.lock() {
            guard.events.push(IoEvent {
                stream,
                bytes: BString::from(bytes),
            });
        }
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        let mut state = self.lock()?;
        let count = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(state.stdin.len());
        Ok(state.stdin.drain(..count).collect())
    }

    fn is_terminal(&self, _stream: Stream) -> bool {
        false
    }

    fn now(&self) -> FileTime {
        self.state.lock().map(|g| g.fs.now).unwrap_or_default()
    }

    fn monotonic(&self) -> Duration {
        self.state
            .lock()
            .map(|g| g.fs.monotonic)
            .unwrap_or(Duration::ZERO)
    }

    fn sleep(&self, duration: Duration) {
        if let Ok(mut guard) = self.state.lock() {
            guard.fs.advance(duration);
        }
    }

    fn time_zone(&self) -> TimeZone {
        TimeZone::UTC
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        Ok(self.lock()?.exe.clone())
    }

    fn initial_cwd(&self) -> PathBuf {
        self.state
            .lock()
            .map(|g| g.cwd.clone())
            .unwrap_or_else(|_| PathBuf::from("/work"))
    }

    fn system(&self) -> SystemInfo {
        self.state
            .lock()
            .map(|state| state.system.clone())
            .unwrap_or_else(|_| default_system())
    }

    fn effective_uid(&self) -> Option<u32> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.fs.effective_uid)
    }
}

fn default_system() -> SystemInfo {
    SystemInfo {
        sysname: BString::from("PureOS"),
        release: BString::from("1.0.0"),
        version: BString::from("PureHost"),
        machine: BString::from("arm64"),
        hostname: BString::from("purehost"),
        cpu_count: 4,
        total_memory: 0,
        pid: 1000,
    }
}

fn require_absolute(path: &Path) -> HostResult<()> {
    if root(&crate::path::from_native(path)).is_some() {
        Ok(())
    } else {
        Err(HostError::not_absolute(path.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_times_on_the_link_and_preserves_omitted_values() -> HostResult<()> {
        let host = PureHost::default();
        let target = Path::new("/work/target");
        let link = Path::new("/work/link");
        host.seed_file(target, b"file")?;
        host.symlink(target, link)?;
        let target_before = host.metadata(target, Follow::No)?;
        let link_before = host.metadata(link, Follow::No)?;

        let mtime = FileTime { secs: 42, nanos: 7 };
        host.set_symlink_times(link, None, Some(mtime))?;
        let updated = host.metadata(link, Follow::No)?;
        assert_eq!(updated.mtime, mtime);
        assert_eq!(updated.atime, link_before.atime);
        assert_eq!(host.metadata(target, Follow::No)?, target_before);

        host.remove_file(target)?;
        let atime = FileTime { secs: 55, nanos: 9 };
        host.set_symlink_times(link, Some(atime), None)?;
        let updated = host.metadata(link, Follow::No)?;
        assert_eq!(updated.atime, atime);
        assert_eq!(updated.mtime, mtime);
        Ok(())
    }

    #[test]
    fn env_epoch_increments_on_set_and_unset() {
        let host = PureHost::default();
        let e0 = host.env_epoch();
        assert_eq!(e0, Some(0));
        host.env_set(b"RCMAKE_TEST_EPOCH", Some(b"1"));
        let e1 = host.env_epoch();
        assert_eq!(e1, Some(1));
        host.env_set(b"RCMAKE_TEST_EPOCH", Some(b"2"));
        let e2 = host.env_epoch();
        assert_eq!(e2, Some(2));
        host.env_set(b"RCMAKE_TEST_EPOCH", None);
        let e3 = host.env_epoch();
        assert_eq!(e3, Some(3));
    }

    #[test]
    fn pure_host_enumerates_registry_values_subkeys_and_multi_sz() -> HostResult<()> {
        let host = PureHost::default();
        host.set_registry_host_view(RegistryView::Bits64)?;
        let key = b"Software/Classes/CLSID/CMake-Tests/chsi-registry";
        host.seed_registry_value(
            RegistryRoot::CurrentUser,
            key,
            None,
            RegistryView::Bits64,
            b"default 64bit",
        )?;
        host.seed_registry_multi_sz(
            RegistryRoot::CurrentUser,
            key,
            Some(b"VALUE_MULTI_SZ"),
            RegistryView::Bits64,
            b"data1\0data2\0",
        )?;
        host.seed_registry_multi_sz(
            RegistryRoot::CurrentUser,
            key,
            Some(b"VALUE2_MULTI_SZ"),
            RegistryView::Bits64,
            b"data1|data2\0",
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey1",
            RegistryView::Bits64,
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey2",
            RegistryView::Bits64,
        )?;
        host.seed_registry_key(
            RegistryRoot::CurrentUser,
            b"Software/Classes/CLSID/CMake-Tests/chsi-registry/subkey3",
            RegistryView::Bits32,
        )?;

        assert_eq!(
            host.registry_value_with_separator(
                RegistryRoot::CurrentUser,
                key,
                Some(b"VALUE_MULTI_SZ"),
                RegistryView::Bits64,
                None,
            )?,
            Some("data1;data2".into())
        );
        assert_eq!(
            host.registry_value_with_separator(
                RegistryRoot::CurrentUser,
                key,
                Some(b"VALUE2_MULTI_SZ"),
                RegistryView::Bits64,
                Some(b"|"),
            )?,
            Some("data1;data2".into())
        );
        let mut names = host
            .registry_value_names(RegistryRoot::CurrentUser, key, RegistryView::Bits64)?
            .unwrap_or_default();
        names.sort();
        assert_eq!(
            names,
            vec![
                BString::from("(default)"),
                BString::from("VALUE2_MULTI_SZ"),
                BString::from("VALUE_MULTI_SZ"),
            ]
        );
        assert_eq!(
            host.registry_subkeys(RegistryRoot::CurrentUser, key, RegistryView::Bits64)?,
            Some(vec![BString::from("subkey1"), BString::from("subkey2")])
        );
        assert_eq!(
            host.registry_subkeys(RegistryRoot::CurrentUser, key, RegistryView::Bits32)?,
            Some(vec![BString::from("subkey3")])
        );
        Ok(())
    }

    #[test]
    fn path_on_disk_recovers_component_case_and_windows_drive_letter() -> HostResult<()> {
        let host = PureHost::new(PathBuf::from("/Work"), PathBuf::from("/bin/cmake"));
        host.seed_file(
            Path::new("/Work/Tests/RunCMake/CommandLine/P_PathOnDisk.cmake"),
            b"",
        )?;
        assert_eq!(
            host.path_on_disk(Path::new(
                "/work/tests/runcmake/commandline/../CommandLine/p_pathondisk.cmake"
            )),
            PathBuf::from("/Work/Tests/RunCMake/CommandLine/P_PathOnDisk.cmake")
        );

        let win_case = crate::path::path_on_disk_with(
            Path::new(r"d:/t/tests/runcmake/commandline/p_working-dir-build"),
            |dir| {
                let s = crate::path::from_native(dir);
                let name = match s.as_slice() {
                    b"D:/" => "t",
                    b"D:/t" => "Tests",
                    b"D:/t/Tests" => "RunCMake",
                    b"D:/t/Tests/RunCMake" => "CommandLine",
                    b"D:/t/Tests/RunCMake/CommandLine" => "P_working-dir-build",
                    _ => return Ok(Vec::new()),
                };
                Ok(vec![DirEntry {
                    name: name.into(),
                    kind: crate::FileKind::Dir,
                }])
            },
        );
        assert_eq!(
            win_case,
            PathBuf::from("D:/t/Tests/RunCMake/CommandLine/P_working-dir-build")
        );
        Ok(())
    }

    #[test]
    fn windows_interactive_debug_creation_flags_require_explicit_opt_in() {
        assert_eq!(
            crate::windows_child_creation_flags(true),
            crate::WINDOWS_CREATE_DEFAULT_ERROR_MODE
        );
        assert_eq!(crate::windows_child_creation_flags(false), 0);
    }
}
