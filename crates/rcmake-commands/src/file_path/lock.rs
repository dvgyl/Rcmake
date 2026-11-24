use std::collections::{BTreeMap, btree_map::Entry};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::FrameKind;
use rcmake_eval::{Cx, Flow, UserCommandKind};
use rcmake_host::{
    FileKind, FileLockGuard, Follow, Host, WriteMode,
    path::{from_native, to_native},
};

use super::{Result, arg, error, set};

#[derive(Clone, Copy, Default)]
enum Guard {
    Function,
    File,
    #[default]
    Process,
}

struct Held {
    scope: Option<Weak<[u8]>>,
    _guard: Box<dyn FileLockGuard>,
}

enum Lock {
    Acquiring,
    Held(Held),
}

type Key = (usize, PathBuf);
static LOCKS: Mutex<BTreeMap<Key, Lock>> = Mutex::new(BTreeMap::new());

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [path, options @ ..] = args else {
        return Err(error("sub-command LOCK requires at least two arguments."));
    };
    let mut path = cx.resolve_path(path);
    let mut directory = false;
    let mut release = false;
    let mut guard = Guard::Process;
    let mut output = b"".as_slice();
    let mut timeout = None;
    let mut args = options.iter().copied();
    while let Some(option) = args.next() {
        match option {
            b"DIRECTORY" => directory = true,
            b"RELEASE" => release = true,
            b"RESULT_VARIABLE" => output = arg(&mut args, option)?,
            b"GUARD" => {
                guard = match arg(&mut args, option)? {
                    b"FUNCTION" => Guard::Function,
                    b"FILE" => Guard::File,
                    b"PROCESS" => Guard::Process,
                    _ => return Err(error("expected FUNCTION, FILE or PROCESS after GUARD")),
                }
            }
            b"TIMEOUT" => {
                let value = arg(&mut args, option)?;
                let secs = std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|&value| value <= i64::MAX as u64)
                    .ok_or_else(|| {
                        error(
                            [
                                b"TIMEOUT value \"",
                                value,
                                b"\" is not an unsigned integer.",
                            ]
                            .concat(),
                        )
                    })?;
                timeout = Some(Duration::from_secs(secs));
            }
            _ => {
                return Err(error(
                    [b"LOCK given unknown argument \"", option, b"\"."].concat(),
                ));
            }
        }
    }
    if directory {
        path.push("cmake.lock");
    }
    let host = cx.host();
    let path = to_native(&Lexical::new(&from_native(&path)).normalize());
    if let Some(parent) = path.parent() {
        host.create_dir_all(parent)
            .map_err(|err| error(err.to_string()))?;
    }
    if host
        .metadata(&path, Follow::Yes)
        .is_ok_and(|meta| meta.kind != FileKind::File)
    {
        return Err(error(format!(
            r#"LOCK requires a regular file at "{}"."#,
            path.display()
        )));
    }
    host.write(&path, b"", WriteMode::Append)
        .map_err(|err| error(err.to_string()))?;
    let path = host.canonicalize(&path).unwrap_or(path);
    let pure = host.system().sysname == b"PureOS";
    let owner = if pure {
        (host as *const dyn Host).cast::<()>() as usize
    } else {
        0
    };
    let result = if release {
        finish(&path, owner)
    } else {
        scope(cx, guard).and_then(|scope| acquire(cx, &path, owner, scope, timeout))
    };
    match result {
        Ok(()) if output.is_empty() => Ok(Flow::Normal),
        Ok(()) => set(cx, output, "0"),
        Err(reason) if !output.is_empty() => set(cx, output, reason),
        Err(reason) => Err(error(format!(
            "error locking file\n  \"{}\"\n{reason}.",
            path.display()
        ))),
    }
}

fn scope(cx: &Cx<'_>, guard: Guard) -> std::result::Result<Option<Weak<[u8]>>, BString> {
    let backtrace = cx.backtrace();
    match guard {
        Guard::Process => Ok(None),
        Guard::File => backtrace
            .frames()
            .find(|frame| frame.kind == FrameKind::ModuleInclude)
            .map(|frame| Some(Arc::downgrade(&frame.file)))
            .ok_or_else(|| BString::from("'GUARD FILE' not used in file scope")),
        Guard::Function => backtrace
            .frames()
            .skip(1)
            .find_map(|frame| {
                let name = frame.name.as_ref()?;
                (cx.user_command_kind(name) == Some(UserCommandKind::Function))
                    .then(|| Some(Arc::downgrade(name)))
            })
            .ok_or_else(|| BString::from("'GUARD FUNCTION' not used in function definition")),
    }
}

fn finish(path: &Path, owner: usize) -> std::result::Result<(), BString> {
    let mut locks = LOCKS.lock().map_err(|err| BString::from(err.to_string()))?;
    remove_expired(&mut locks);
    let key = (owner, path.to_path_buf());
    if matches!(locks.get(&key), Some(Lock::Held(_))) {
        locks.remove(&key);
    }
    Ok(())
}

// Check the scope frames before each lock operation.
fn remove_expired(locks: &mut BTreeMap<Key, Lock>) {
    locks.retain(|_, lock| match lock {
        Lock::Acquiring => true,
        Lock::Held(held) => held
            .scope
            .as_ref()
            .is_none_or(|scope| scope.strong_count() != 0),
    });
}

fn acquire(
    cx: &Cx<'_>,
    path: &Path,
    owner: usize,
    scope: Option<Weak<[u8]>>,
    timeout: Option<Duration>,
) -> std::result::Result<(), BString> {
    let key = (owner, path.to_path_buf());
    {
        let mut locks = LOCKS.lock().map_err(|err| BString::from(err.to_string()))?;
        remove_expired(&mut locks);
        let Entry::Vacant(entry) = locks.entry(key.clone()) else {
            return Err("File already locked".into());
        };
        entry.insert(Lock::Acquiring);
    }

    // Wait without the table mutex so other files can be locked or released.
    let host = cx.host();
    let budget = cx
        .limits()
        .wall_budget
        .map(|budget| budget.saturating_sub(host.monotonic()));
    let timeout = match (timeout, budget) {
        (Some(timeout), Some(budget)) => Some(timeout.min(budget)),
        _ => timeout.or(budget),
    };
    let result = host.lock_file(path, timeout);
    let mut locks = LOCKS.lock().map_err(|err| BString::from(err.to_string()))?;
    match result {
        Ok(guard) => {
            locks.insert(
                key,
                Lock::Held(Held {
                    scope,
                    _guard: guard,
                }),
            );
            Ok(())
        }
        Err(err) => {
            locks.remove(&key);
            if err.kind == ErrorKind::TimedOut {
                Err("Timeout reached".into())
            } else {
                Err(err.to_string().into())
            }
        }
    }
}
