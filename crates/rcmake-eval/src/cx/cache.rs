use rcmake_base::{BString, list, truth};
use rcmake_diag::{Location, PolicyId, PolicyStatus};
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_model::{Cache, CacheEntry, CacheType, ReservationId};

use super::Cx;
use crate::interp::IssueClass;

impl Cx<'_> {
    /// Internal property for an unset cache entry marked as advanced.
    pub const CACHE_PLACEHOLDER: &'static [u8] = b"\0mark_as_advanced_placeholder";

    /// Apply CMP0126 after a command writes a cache entry.
    pub fn clear_cache_shadow(&mut self, name: &[u8]) {
        let cmp0126 = PolicyId::new(126);
        let policy = self.policy(cmp0126);
        if policy != PolicyStatus::New {
            if policy == PolicyStatus::Warn
                && self.var_is_on(b"CMAKE_POLICY_WARNING_CMP0126")
                && self.var_normal_only(name).is_some()
            {
                self.interp.issue_at(
                    IssueClass::policy(cmp0126),
                    Location::Backtrace(self.interp.bt.clone()),
                    crate::util::cat(&[
                        b"Policy CMP0126 is not set: set(CACHE) does not remove a normal variable of the same name.  Run \"cmake --help-policy CMP0126\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n",
                        b"For compatibility with older versions of CMake, normal variable \"",
                        name,
                        b"\" will be removed from the current scope.",
                    ]),
                    false,
                );
            }
            self.unset_var(name);
        }
    }

    /// Normalize the paths in a path-valued cache entry.
    pub fn cache_paths(&self, value: &[u8]) -> BString {
        let directory = self.host().initial_cwd();
        let home = self.host().env_get(b"HOME").filter(|home| !home.is_empty());
        list::join(
            list::split(value, list::Empty::Drop)
                .into_iter()
                .map(|path| {
                    if truth::is_off(&path) {
                        return path;
                    }
                    let path = match (home.as_ref(), path.as_slice()) {
                        (Some(home), b"~") => home.clone(),
                        (Some(home), path) if path.starts_with(b"~/") => {
                            crate::util::cat(&[home, path.get(1..).unwrap_or_default()])
                        }
                        _ => path,
                    };
                    let path = directory.join(to_native(&path));
                    let mut clean = std::path::PathBuf::new();
                    for component in path.components() {
                        match component {
                            std::path::Component::ParentDir => {
                                clean.pop();
                            }
                            _ => clean.push(component),
                        }
                    }
                    from_native_cmake(&clean)
                }),
        )
    }

    /// Write a cache entry and preserve its additional properties.
    ///
    /// Return `false` after a fatal error if the cache would pass `limits.max_cache_bytes`.
    pub fn write_cache(&mut self, name: &[u8], value: BString, ty: CacheType, help: &[u8]) -> bool {
        let mut entry = self
            .cache()
            .get(name)
            .cloned()
            .unwrap_or_else(|| CacheEntry {
                value: BString::default(),
                ty,
                help: BString::default(),
                advanced: false,
                props: Default::default(),
            });
        entry.value = if matches!(ty, CacheType::Path | CacheType::Filepath) {
            let home = self.host().env_get(b"HOME");
            list::join(
                list::split(&value, list::Empty::Drop)
                    .into_iter()
                    .map(|path| {
                        cache_path_slashes(&path, home.as_deref().map(|value| value.as_ref()))
                    }),
            )
        } else {
            value
        };
        entry.ty = ty;
        entry.help = BString::from(help);
        entry.props.unset(Self::CACHE_PLACEHOLDER);
        self.insert_cache_entry(name.into(), entry)
    }

    /// Insert a cache entry and record the write if history is enabled.
    ///
    /// If the cache would pass `limits.max_cache_bytes`, report a fatal error, leave the
    /// cache unchanged, and return `false`.
    pub fn insert_cache_entry(&mut self, name: BString, entry: CacheEntry) -> bool {
        if !self.admit_cache_entry(&name, &entry) {
            return false;
        }
        self.interp.note_replay_cache_write(&name);
        self.interp
            .record_var_write(&name, crate::VarOp::CacheSet, Some(&entry.value));
        self.cache().insert(name, entry);
        true
    }

    /// Insert a cache entry like the raw [`Cache::insert`], after the
    /// `limits.max_cache_bytes` check of [`Self::insert_cache_entry`].
    ///
    /// Commands that compute a cache value from script input use this method when they
    /// must not add a history record. Report a fatal error and return `false` if the
    /// cache would pass the limit.
    pub fn insert_cache_entry_unrecorded(&mut self, name: BString, entry: CacheEntry) -> bool {
        if !self.admit_cache_entry(&name, &entry) {
            return false;
        }
        self.cache().insert(name, entry);
        true
    }

    /// Reserve the cost of `entry` for `name`, after the `limits.max_cache_bytes` check.
    /// Report a fatal error and return `None` if the entry does not fit.
    ///
    /// Until [`Self::release_cache_reservation`] of the returned id, `name` counts at
    /// least the cost of `entry`, also if a callback (for example of `variable_watch()`)
    /// shrinks or removes its live entry, so that no other cache write can use the bytes.
    pub fn reserve_cache_entry(
        &mut self,
        name: &[u8],
        entry: &CacheEntry,
    ) -> Option<ReservationId> {
        if !self.admit_cache_entry(name, entry) {
            return None;
        }
        Some(self.cache().reserve(name, entry))
    }

    /// Release the reservation `id` that [`Self::reserve_cache_entry`] returned.
    pub fn release_cache_reservation(&mut self, id: ReservationId) {
        self.cache().release(id);
    }

    /// Return `true` if the cache stays within `limits.max_cache_bytes` after an insert
    /// of `entry` as `name`. Otherwise report a fatal error and return `false`.
    ///
    /// A command that must check a cache write before other effects calls this method
    /// first, then inserts the entry later.
    pub fn admit_cache_entry(&mut self, name: &[u8], entry: &CacheEntry) -> bool {
        let limit = self.interp.config.limits.max_cache_bytes;
        if self.cache().bytes_after_insert(name, entry) <= limit {
            return true;
        }
        if !self.interp.fatal {
            self.interp
                .error_here(format!("Cache storage would exceed the {limit}-byte limit.").into());
            self.interp.fatal = true;
        }
        false
    }

    /// Record a `set(... CACHE ...)` or `option(...)` declaration that kept an existing value.
    pub fn record_cache_declare(&mut self, name: &[u8]) {
        let value = self
            .interp
            .project
            .cache
            .get(name)
            .map(|entry| entry.value.clone());
        if let Some(value) = value {
            self.interp
                .record_var_write(name, crate::VarOp::CacheDeclare, Some(&value));
        }
    }

    /// Remove an existing cache entry and record the write if history is enabled.
    pub fn remove_cache_entry(&mut self, name: &[u8]) {
        if self.cache().get(name).is_some() {
            self.interp.note_replay_cache_write(name);
            self.interp
                .record_var_write(name, crate::VarOp::CacheUnset, None);
            self.cache().remove(name);
        }
    }

    /// Mutable access to the [`Cache`].
    pub fn cache(&mut self) -> &mut Cache {
        self.interp.invalidate_recursion_limit();
        &mut self.interp.project.cache
    }
}

fn cache_path_slashes(value: &[u8], home: Option<&[u8]>) -> BString {
    let slash = |byte| matches!(byte, b'/' | b'\\');
    let preserve_double = value.first().copied().is_some_and(slash)
        && value.get(1).copied().is_some_and(slash)
        && !value.get(2).copied().is_some_and(slash);
    let mut path = BString::default();
    for (index, &byte) in value.iter().enumerate() {
        if !slash(byte) {
            path.push(byte);
        } else if !path.ends_with(b"/") || index == 1 && preserve_double {
            path.push(b'/');
        }
    }
    if let Some(home) = home
        && (path == b"~" || path.starts_with(b"~/"))
    {
        path = [home, path.get(1..).unwrap_or_default()].concat().into();
    }
    if path.len() > 1
        && path.ends_with(b"/")
        && !path.ends_with(b"//")
        && !(path.len() == 3 && path.get(1) == Some(&b':'))
    {
        path.pop();
    }
    path
}
