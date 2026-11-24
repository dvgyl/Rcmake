use std::collections::{BTreeMap, btree_map::Entry};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{FileLockGuard, Host};

use crate::Cx;

/// Lifetime of a `file(LOCK)` lock.
#[derive(Clone, Copy)]
pub enum FileLockScope {
    /// Release the lock when the current function returns.
    Function,
    /// Release the lock when the current list file finishes.
    File,
    /// Release the lock on explicit release or process exit.
    Process,
}

type Key = (usize, PathBuf);

enum Lock {
    Acquiring,
    Held {
        _guard: Box<dyn FileLockGuard>,
        scope: Option<Arc<()>>,
        _pure_host: Option<Arc<dyn Host>>,
    },
}

static LOCKS: Mutex<BTreeMap<Key, Lock>> = Mutex::new(BTreeMap::new());

#[derive(Default)]
pub(crate) struct ScopeLocks(BTreeMap<PathBuf, ScopedLock>);

struct ScopedLock {
    key: Key,
    scope: Arc<()>,
}

impl Drop for ScopedLock {
    fn drop(&mut self) {
        if let Ok(mut locks) = LOCKS.lock()
            && let Entry::Occupied(entry) = locks.entry(self.key.clone())
            && matches!(entry.get(), Lock::Held { scope: Some(scope), .. }
                if Arc::ptr_eq(scope, &self.scope))
        {
            entry.remove();
        }
    }
}

impl Cx<'_> {
    /// Lock an absolute path. Return a CMake lock status message on failure.
    ///
    /// Use the canonical file path so equivalent paths refer to the same lock.
    pub fn lock_file(
        &mut self,
        path: &Path,
        scope: FileLockScope,
        timeout: Option<Duration>,
    ) -> Result<(), BString> {
        let interp = &mut self.interp;
        let scope = match scope {
            FileLockScope::Process => None,
            FileLockScope::File => Some(
                interp
                    .file_locks
                    .last_mut()
                    .ok_or_else(|| BString::from("'GUARD FILE' not used in file scope"))?,
            ),
            FileLockScope::Function => Some(interp.function_locks.last_mut().ok_or_else(|| {
                BString::from("'GUARD FUNCTION' not used in function definition")
            })?),
        };

        let host = &interp.config.host;
        let pure = host.system().sysname == b"PureOS";
        let key = lock_key(pure, host.as_ref(), path);
        {
            let mut locks = LOCKS.lock().map_err(|err| BString::from(err.to_string()))?;
            let Entry::Vacant(entry) = locks.entry(key.clone()) else {
                return Err("File already locked".into());
            };
            entry.insert(Lock::Acquiring);
        }

        let budget =
            interp.config.limits.wall_budget.map(|budget| {
                budget.saturating_sub(host.monotonic().saturating_sub(interp.started))
            });
        let timeout = match (timeout, budget) {
            (Some(timeout), Some(budget)) => Some(timeout.min(budget)),
            _ => timeout.or(budget),
        };
        let result = host.lock_file(path, timeout);
        let mut locks = LOCKS.lock().map_err(|err| BString::from(err.to_string()))?;
        let guard = match result {
            Ok(guard) => guard,
            Err(err) => {
                locks.remove(&key);
                return Err(if err.kind == ErrorKind::TimedOut {
                    "Timeout reached".into()
                } else {
                    err.to_string().into()
                });
            }
        };
        let registration = scope.as_ref().map(|_| Arc::new(()));
        locks.insert(
            key.clone(),
            Lock::Held {
                _guard: guard,
                scope: registration.clone(),
                _pure_host: pure.then(|| Arc::clone(host)),
            },
        );
        drop(locks);
        if let (Some(scope), Some(registration)) = (scope, registration) {
            scope.0.insert(
                path.to_owned(),
                ScopedLock {
                    key,
                    scope: registration,
                },
            );
        }
        Ok(())
    }

    /// Release a held lock. Release of an unlocked path succeeds.
    pub fn unlock_file(&self, path: &Path) -> Result<(), BString> {
        let host = self.host();
        let key = lock_key(host.system().sysname == b"PureOS", host, path);
        let mut locks = LOCKS.lock().map_err(|err| BString::from(err.to_string()))?;
        if matches!(locks.get(&key), Some(Lock::Held { .. })) {
            locks.remove(&key);
        }
        Ok(())
    }
}

fn lock_key(pure: bool, host: &dyn Host, path: &Path) -> Key {
    let owner = if pure {
        (host as *const dyn Host).cast::<()>() as usize
    } else {
        0
    };
    (owner, path.to_owned())
}
