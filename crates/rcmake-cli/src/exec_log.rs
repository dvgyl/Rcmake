//! Append invocation records for the PATH-swap corpus.

#![forbid(unsafe_code)]

use std::path::Path;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    Host,
    path::{from_native, to_native},
};
use rcmake_json::{Value, write_compact};

use crate::{Tool, object};

/// Records one completed invocation.
pub(crate) struct ExecLogEntry<'a> {
    /// Original arguments, including the executable argument if available.
    /// Secret cache definitions are redacted and invalid UTF-8 is replaced.
    pub(crate) argv: &'a [BString],
    /// Tool name recorded as `persona` by the PATH-swap checker.
    pub(crate) persona: Tool,
    /// Working directory at startup.
    pub(crate) cwd: &'a Path,
    /// Process ID.
    pub(crate) pid: u32,
    /// Parent process ID, if available.
    pub(crate) parent_pid: Option<u32>,
    /// Tool exit code.
    pub(crate) exit_code: i32,
    /// Elapsed time for the tool invocation.
    pub(crate) duration: Duration,
}

/// Append a JSON line when `RCMAKE_EXEC_LOG` names a log file.
///
/// Reject symbolic links at the log and lock paths.
/// On Unix, restrict access to the file owner.
/// On Windows, select a trusted directory and restrict access to both files.
/// A logging error leaves the tool's output and exit code intact.
pub(crate) fn append_exec_log(host: &dyn Host, entry: &ExecLogEntry<'_>) {
    let Some(path) = host
        .env_get(b"RCMAKE_EXEC_LOG")
        .filter(|path| !path.is_empty())
    else {
        return;
    };
    let requested = host.initial_cwd().join(to_native(path.as_slice()));
    let _ = append_at_path(host, &requested, &json_line(entry));
}

#[cfg(not(any(unix, windows)))]
fn append_at_path(_host: &dyn Host, _path: &Path, _line: &[u8]) -> std::io::Result<()> {
    // The native log needs private creation and a no-follow open in one operation.
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(any(unix, windows))]
use private_log::append_at_path;

#[cfg(any(unix, windows))]
mod private_log {
    #[cfg(unix)]
    use std::fs::Permissions;
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::{self, ErrorKind, Read as _, Seek as _, SeekFrom, Write as _};
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    #[cfg(windows)]
    use std::os::windows::{
        ffi::OsStrExt as _,
        fs::{MetadataExt as _, OpenOptionsExt as _},
    };

    #[cfg(windows)]
    use rcmake_host::Follow;
    use rcmake_json::{ReadOptions, parse};

    use super::*;

    const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
    const LOCK_POLL: Duration = Duration::from_millis(20);
    const REPAIR_CHUNK_BYTES: u64 = 256 * 1024;
    const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;

    pub(super) fn append_at_path(host: &dyn Host, requested: &Path, line: &[u8]) -> io::Result<()> {
        let parent = requested.parent().ok_or(ErrorKind::InvalidInput)?;
        let parent = host
            .canonicalize(parent)
            .map_err(|error| io::Error::from(error.kind))?;
        let name = requested.file_name().ok_or(ErrorKind::InvalidInput)?;
        let path = parent.join(name);
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let lock = open_log_file(host, &parent.join(lock_name), false)?;
        let start = host.monotonic();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    let left = LOCK_TIMEOUT.saturating_sub(host.monotonic().saturating_sub(start));
                    if left.is_zero() {
                        return Err(ErrorKind::TimedOut.into());
                    }
                    std::thread::sleep(left.min(LOCK_POLL));
                }
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        let mut file = open_log_file(host, &path, true)?;
        let end = file.metadata()?.len();
        if end != 0 && read_exact(&mut file, end - 1, 1)? != b"\n" {
            repair_tail(&mut file, end)?;
        }
        #[cfg(windows)]
        file.seek(SeekFrom::End(0))?;
        file.write_all(line)
    }

    #[cfg(unix)]
    fn open_log_file(_host: &dyn Host, path: &Path, append: bool) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(!append)
            .append(append)
            .create(true)
            .mode(0o600)
            .custom_flags(open_flags()?)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(ErrorKind::InvalidInput.into());
        }
        if metadata.mode() & 0o7777 != 0o600 {
            file.set_permissions(Permissions::from_mode(0o600))?;
            if file.metadata()?.mode() & 0o7777 != 0o600 {
                return Err(ErrorKind::PermissionDenied.into());
            }
        }
        Ok(file)
    }

    #[cfg(unix)]
    fn open_flags() -> io::Result<i32> {
        // O_NOFOLLOW and O_NONBLOCK from the native fcntl ABI.
        if cfg!(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        )) {
            Ok(0x100 | 0x4)
        } else if cfg!(any(target_os = "linux", target_os = "android")) {
            match std::env::consts::ARCH {
                "arm" | "aarch64" | "powerpc" | "powerpc64" => Ok(0x8000 | 0x800),
                "x86" | "x86_64" | "riscv32" | "riscv64" | "s390x" => Ok(0x20000 | 0x800),
                _ => Err(ErrorKind::Unsupported.into()),
            }
        } else {
            Err(ErrorKind::Unsupported.into())
        }
    }

    #[cfg(windows)]
    fn open_log_file(host: &dyn Host, path: &Path, _append: bool) -> io::Result<File> {
        use std::fs::symlink_metadata;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;

        // Reject alternate data streams.
        let name = path.file_name().ok_or(ErrorKind::InvalidInput)?;
        if name.encode_wide().any(|unit| unit == u16::from(b':')) {
            return Err(ErrorKind::InvalidInput.into());
        }
        match symlink_metadata(path) {
            Ok(metadata) => check_windows_file(&metadata)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        // The flag prevents the open from following a reparse point added after the path check.
        // New files inherit directory access controls. The standard library cannot set or
        // verify those controls, or open the file relative to a directory handle.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        check_windows_file(&file.metadata()?)?;
        // Omit FILE_SHARE_DELETE so Host can check the hard link count while the file is open.
        let metadata = host
            .metadata(path, Follow::No)
            .map_err(|error| io::Error::from(error.kind))?;
        if metadata.nlink != 1 {
            return Err(ErrorKind::InvalidInput.into());
        }
        Ok(file)
    }

    #[cfg(windows)]
    fn check_windows_file(metadata: &std::fs::Metadata) -> io::Result<()> {
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

        if metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !metadata.is_file()
        {
            return Err(ErrorKind::InvalidInput.into());
        }
        Ok(())
    }

    fn repair_tail(file: &mut File, end: u64) -> io::Result<()> {
        let start = record_start(file, end)?;
        let len = end - start;
        let complete = if len <= MAX_RECORD_BYTES {
            let tail = read_exact(file, start, len)?;
            let options = ReadOptions {
                allow_comments: false,
                dup_keys_last_wins: false,
            };
            matches!(parse(&tail, &options), Ok(Value::Object(_)))
        } else {
            // A complete record ends in a numeric pid and a closing brace.
            const TRAILER_BYTES: u64 = 18;
            let trailer = read_exact(file, end - TRAILER_BYTES, TRAILER_BYTES)?;
            trailer
                .strip_suffix(b"}")
                .and_then(|object| object.rsplit(|&byte| byte == b',').next())
                .and_then(|field| field.strip_prefix(b"\"pid\":"))
                .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit))
        };
        if complete {
            file.write_all(b"\n")
        } else {
            file.set_len(start)
        }
    }

    fn record_start(file: &mut File, mut end: u64) -> io::Result<u64> {
        while end > 0 {
            let offset = end.saturating_sub(REPAIR_CHUNK_BYTES);
            let chunk = read_exact(file, offset, end - offset)?;
            if let Some(index) = chunk.iter().rposition(|&byte| byte == b'\n') {
                return Ok(offset + index as u64 + 1);
            }
            end = offset;
        }
        Ok(0)
    }

    fn read_exact(file: &mut File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let len = usize::try_from(len).map_err(|_| ErrorKind::InvalidInput)?;
        let mut bytes = vec![0; len];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

fn json_line(entry: &ExecLogEntry<'_>) -> BString {
    let duration_ms = u64::try_from(entry.duration.as_millis()).unwrap_or(u64::MAX);
    let cwd = from_native(entry.cwd);
    let mut define_next = false;
    let mut line = write_compact(&object([
        (
            "argv",
            Value::Array(
                entry
                    .argv
                    .iter()
                    .map(|arg| {
                        let arg = arg.as_slice();
                        let define = std::mem::replace(&mut define_next, arg == b"-D");
                        json_arg(arg, define)
                    })
                    .collect(),
            ),
        ),
        ("cwd", json_text(cwd.as_slice())),
        ("duration_ms", Value::UInt(duration_ms)),
        ("exit_code", Value::Int(i64::from(entry.exit_code))),
        (
            "parent_pid",
            entry
                .parent_pid
                .map_or(Value::Null, |pid| Value::UInt(u64::from(pid))),
        ),
        ("persona", Value::string(entry.persona.name())),
        ("pid", Value::UInt(u64::from(entry.pid))),
    ]));
    line.push(b'\n');
    line
}

fn json_arg(arg: &[u8], separate_define: bool) -> Value {
    let define = if separate_define {
        Some(arg)
    } else {
        arg.strip_prefix(b"-D")
    };
    if let Some(define) = define
        && let Some(eq) = define.iter().position(|byte| *byte == b'=')
        && let Some(name) = define.get(..eq)
    {
        let name = name.split(|byte| *byte == b':').next().unwrap_or_default();
        let name = name.to_ascii_uppercase();
        if ["TOKEN", "SECRET", "PASSWORD", "KEY", "CREDENTIAL", "AUTH"]
            .iter()
            .any(|marker| {
                name.windows(marker.len())
                    .any(|part| part == marker.as_bytes())
            })
            && let Some(prefix) = arg.get(..arg.len() - define.len() + eq + 1)
        {
            let mut redacted = BString::from(prefix);
            redacted.extend_from_slice(b"<redacted>");
            return json_text(&redacted);
        }
    }
    json_text(arg)
}

fn json_text(bytes: &[u8]) -> Value {
    Value::string(String::from_utf8_lossy(bytes).as_bytes())
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use rcmake_host::{Follow, HostResult, NativeHost, ReadRange, WriteMode};
    use rcmake_json::{ReadOptions, parse};

    use super::*;

    #[test]
    fn redacts_secret_definitions_and_preserves_regular_arguments() {
        for arg in [
            "-DAPI_TOKEN=secret",
            "-Dpassword:STRING=secret",
            "-DMY_AUTH=secret",
        ] {
            let prefix = arg.trim_end_matches("secret");
            assert_eq!(
                json_arg(arg.as_bytes(), false),
                Value::string(format!("{prefix}<redacted>"))
            );
        }
        assert_eq!(
            json_arg(b"-DOPTION=ON", false),
            Value::string("-DOPTION=ON")
        );
        assert_eq!(
            json_arg(b"API_TOKEN=secret", true),
            Value::string("API_TOKEN=<redacted>")
        );
    }

    #[test]
    fn log_and_lock_reject_symlinks_and_allow_regular_files() -> HostResult<()> {
        let host = NativeHost::new()?;
        let pid = host.system().pid;
        let now = host.now();
        let root = std::env::temp_dir().join(format!(
            "rcmake-exec-log-test-{pid}-{}-{}",
            now.secs, now.nanos
        ));
        host.create_dir_all(&root)?;
        let result = check_log(&host, &root);
        host.remove_dir_all(&root)?;
        result
    }

    fn check_log(host: &dyn Host, root: &Path) -> HostResult<()> {
        const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

        let path = root.join("calls.jsonl");
        let lock = root.join("calls.jsonl.lock");
        let target = root.join("target");
        let range = ReadRange::All { limit: 4096 };
        let argv = ["/shim/cmake".into(), "-DAPI_KEY=secret".into()];
        let entry = ExecLogEntry {
            argv: &argv,
            persona: Tool::Cmake,
            cwd: root,
            pid: host.system().pid,
            parent_pid: Some(7),
            exit_code: 1,
            duration: Duration::from_micros(16_750),
        };
        let log_name = from_native(&path);
        host.env_set(b"RCMAKE_EXEC_LOG", Some(&log_name));
        host.write(&target, b"unchanged", WriteMode::CreateNew)?;
        match host.symlink(&target, &lock) {
            Ok(()) => {
                append_exec_log(host, &entry);
                assert!(host.metadata(&path, Follow::No).is_err());
                host.remove_file(&lock)?;
                host.symlink(&target, &path)?;
                append_exec_log(host, &entry);
                assert_eq!(host.read(&target, range)?, b"unchanged");
                host.remove_file(&path)?;
            }
            // Windows can require a privilege to create symbolic links.
            Err(error) if cfg!(windows) && error.errno == Some(ERROR_PRIVILEGE_NOT_HELD) => {}
            Err(error) => return Err(error),
        }

        append_exec_log(host, &entry);
        #[cfg(unix)]
        for private in [&path, &lock] {
            assert_eq!(host.metadata(private, Follow::No)?.mode & 0o7777, 0o600);
        }
        let first = host.read(&path, range)?;
        assert!(
            matches!(parse(&first, &ReadOptions::default()), Ok(Value::Object(fields))
            if fields.get(b"persona".as_slice()) == Some(&Value::string("cmake")))
        );
        assert!(!String::from_utf8_lossy(&first).contains("secret"));
        host.write(&path, b"{\"argv\":[", WriteMode::Append)?;
        append_exec_log(host, &entry);
        assert_eq!(host.read(&path, range)?, first.repeat(2));
        Ok(())
    }
}
