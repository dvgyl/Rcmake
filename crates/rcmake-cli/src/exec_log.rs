//! Append invocation records for the PATH-swap corpus.

#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    FileKind, Follow, Host, HostError, HostResult, Metadata, ReadRange, WriteMode,
    path::{from_native, to_native},
};
use rcmake_json::{ReadOptions, Value, parse, write_compact};

use crate::{Tool, object};

const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const REPAIR_CHUNK_BYTES: u64 = 256 * 1024;
const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SYMLINK_HOPS: usize = 40;

/// Records one completed invocation.
pub(crate) struct ExecLogEntry<'a> {
    /// Original arguments, including the executable argument if available.
    /// The log replaces invalid UTF-8 sequences.
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
/// Use one lock for paths that resolve to the same file name. The log must be a
/// regular file with one hard link. Repair an incomplete tail with bounded reads.
///
/// A logging error leaves the tool's output and exit code intact.
pub(crate) fn append_exec_log(host: &dyn Host, entry: &ExecLogEntry<'_>) {
    let Some(path) = host
        .env_get(b"RCMAKE_EXEC_LOG")
        .filter(|path| !path.is_empty())
    else {
        return;
    };
    let line = json_line(entry);
    let requested = host.initial_cwd().join(to_native(path.as_slice()));
    let Some(path) = canonical_log_path(host, requested) else {
        return;
    };
    let Some(lock) = lock_path(&path) else {
        return;
    };
    let Ok(_guard) = host.lock_file(&lock, Some(LOCK_TIMEOUT)) else {
        return;
    };
    match host.metadata(&path, Follow::No) {
        Ok(metadata) if metadata.kind == FileKind::File && metadata.nlink == 1 => {
            if metadata.len > 0 {
                let range = ReadRange::Slice {
                    offset: metadata.len - 1,
                    len: Some(1),
                };
                let Ok(last_byte) = host.read(&path, range) else {
                    return;
                };
                if last_byte != b"\n" {
                    let _ = repair_and_append(host, &path, &metadata, &line, entry.pid);
                    return;
                }
            }
        }
        Err(error) if error.kind == ErrorKind::NotFound => {}
        _ => return,
    }
    let _ = host.write(&path, &line, WriteMode::Append);
}

fn canonical_log_path(host: &dyn Host, mut path: PathBuf) -> Option<PathBuf> {
    for _ in 0..=MAX_SYMLINK_HOPS {
        match host.canonicalize(&path) {
            Ok(path) => return Some(path),
            Err(error) if error.kind == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        match host.metadata(&path, Follow::No) {
            Ok(metadata) if metadata.kind == FileKind::Symlink => {
                path = path.parent()?.join(host.read_link(&path).ok()?);
            }
            // Another writer can create the log between the two lookups.
            Ok(_) => return host.canonicalize(&path).ok(),
            Err(error) if error.kind == ErrorKind::NotFound => {
                let parent = host.canonicalize(path.parent()?).ok()?;
                return Some(parent.join(path.file_name()?));
            }
            Err(_) => return None,
        }
    }
    None
}

fn lock_path(path: &Path) -> Option<PathBuf> {
    // Use a separate file so Windows can append while the lock is held.
    let mut name = path.file_name()?.to_os_string();
    name.push(".lock");
    Some(path.with_file_name(name))
}

fn repair_and_append(
    host: &dyn Host,
    path: &Path,
    metadata: &Metadata,
    line: &[u8],
    pid: u32,
) -> HostResult<()> {
    let tail_start = record_start(host, path, metadata.len)?;
    let tail_len = metadata.len - tail_start;
    let complete = if tail_len <= MAX_RECORD_BYTES {
        let tail = read_exact(host, path, tail_start, tail_len)?;
        let options = ReadOptions {
            allow_comments: false,
            dup_keys_last_wins: false,
        };
        matches!(parse(&tail, &options), Ok(Value::Object(_)))
    } else {
        // The canonical JSON writer ends each object with a numeric pid field.
        // A partial write can reach this suffix only if it wrote the whole object.
        const TRAILER_BYTES: u64 = 18;
        let trailer = read_exact(host, path, metadata.len - TRAILER_BYTES, TRAILER_BYTES)?;
        trailer
            .strip_suffix(b"}")
            .and_then(|object| object.rsplit(|&byte| byte == b',').next())
            .and_then(|field| field.strip_prefix(b"\"pid\":"))
            .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit))
    };
    let prefix = if complete { metadata.len } else { tail_start };

    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let now = host.now();
    let temp = path.with_file_name(format!(
        ".rcmake-exec-log-{pid}-{}-{}-{sequence}.tmp",
        now.secs, now.nanos
    ));
    // Prepare a replacement file before changing the existing log.
    host.write(&temp, b"", WriteMode::CreateNew)?;
    let result = host
        .set_permissions(&temp, metadata.mode)
        .and_then(|()| {
            let mut offset = 0;
            while offset < prefix {
                let len = (prefix - offset).min(REPAIR_CHUNK_BYTES);
                let chunk = read_exact(host, path, offset, len)?;
                host.write(&temp, &chunk, WriteMode::Append)?;
                offset += len;
            }
            if complete {
                host.write(&temp, b"\n", WriteMode::Append)?;
            }
            host.write(&temp, line, WriteMode::Append)
        })
        .and_then(|()| host.rename(&temp, path));
    if result.is_err() {
        let _ = host.remove_file(&temp);
    }
    result
}

fn record_start(host: &dyn Host, path: &Path, mut end: u64) -> HostResult<u64> {
    while end > 0 {
        let offset = end.saturating_sub(REPAIR_CHUNK_BYTES);
        let chunk = read_exact(host, path, offset, end - offset)?;
        if let Some(index) = chunk.iter().rposition(|&byte| byte == b'\n') {
            return Ok(offset + index as u64 + 1);
        }
        end = offset;
    }
    Ok(0)
}

fn read_exact(host: &dyn Host, path: &Path, offset: u64, len: u64) -> HostResult<BString> {
    let bytes = host.read(
        path,
        ReadRange::Slice {
            offset,
            len: Some(len),
        },
    )?;
    if bytes.len() as u64 != len {
        return Err(HostError::from_io(
            std::io::Error::from(ErrorKind::UnexpectedEof),
            Some(path.to_path_buf()),
        ));
    }
    Ok(bytes)
}

fn json_line(entry: &ExecLogEntry<'_>) -> BString {
    let duration_ms = u64::try_from(entry.duration.as_millis()).unwrap_or(u64::MAX);
    let cwd = from_native(entry.cwd);
    let mut line = write_compact(&object([
        (
            "argv",
            Value::Array(
                entry
                    .argv
                    .iter()
                    .map(|arg| json_text(arg.as_slice()))
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

fn json_text(bytes: &[u8]) -> Value {
    Value::string(String::from_utf8_lossy(bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn appends_one_line_per_tool_with_corpus_fields() -> HostResult<()> {
        let host = PureHost::default();
        let cwd = host.initial_cwd();
        let path = cwd.join("calls.jsonl");
        let range = ReadRange::All { limit: 4096 };
        let ctest_argv = ["/shim/ctest".into()];
        let cpack_argv = ["/shim/cpack".into()];
        let mut entry = ExecLogEntry {
            argv: &[
                "/shim/cmake".into(),
                "line\nquote\"".into(),
                BString::from(b"\xff".as_slice()),
            ],
            persona: Tool::Cmake,
            cwd: &cwd,
            pid: host.system().pid,
            parent_pid: Some(7),
            exit_code: 1,
            duration: Duration::from_micros(16_750),
        };
        append_exec_log(&host, &entry);
        assert!(host.read(&path, range).is_err());
        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"calls.jsonl"));
        append_exec_log(&host, &entry);
        entry.persona = Tool::Ctest;
        entry.argv = &ctest_argv;
        entry.parent_pid = None;
        append_exec_log(&host, &entry);
        entry.persona = Tool::Cpack;
        entry.argv = &cpack_argv;
        append_exec_log(&host, &entry);

        let bytes = host.read(&path, range)?;
        assert!(bytes.ends_with(b"\n"));
        let mut lines = bytes
            .strip_suffix(b"\n")
            .unwrap_or(&bytes)
            .split(|byte| *byte == b'\n');
        let first = lines.next().unwrap_or_default();
        let expected = object([
            (
                "argv",
                Value::Array(vec![
                    Value::string("/shim/cmake"),
                    Value::string("line\nquote\""),
                    Value::string("\u{fffd}"),
                ]),
            ),
            ("cwd", Value::String(from_native(&cwd))),
            ("duration_ms", Value::Int(16)),
            ("exit_code", Value::Int(1)),
            ("parent_pid", Value::Int(7)),
            ("persona", Value::string("cmake")),
            ("pid", Value::Int(i64::from(entry.pid))),
        ]);
        assert_eq!(parse(first, &ReadOptions::default()), Ok(expected));
        for tool in ["ctest", "cpack"] {
            let parsed = parse(lines.next().unwrap_or_default(), &ReadOptions::default());
            assert!(matches!(parsed, Ok(Value::Object(fields)) if
                fields.get(b"persona".as_slice()) == Some(&Value::string(tool)) &&
                fields.get(b"parent_pid".as_slice()) == Some(&Value::Null)));
        }
        assert_eq!(lines.next(), None);
        assert!(host.events().is_empty());
        Ok(())
    }

    #[test]
    fn skips_locked_logs_and_ignores_io_errors() -> HostResult<()> {
        let host = PureHost::default();
        let cwd = host.initial_cwd();
        let directory = cwd.join("logs");
        let path = directory.join("calls.jsonl");
        let lock = directory.join("calls.jsonl.lock");
        host.create_dir_all(&directory)?;
        host.symlink(&directory, &cwd.join("logs-alias"))?;
        host.symlink(
            Path::new("logs-alias/calls.jsonl"),
            &cwd.join("alias.jsonl"),
        )?;
        let entry = ExecLogEntry {
            argv: &[],
            persona: Tool::Cmake,
            cwd: &cwd,
            pid: 1000,
            parent_pid: None,
            exit_code: 0,
            duration: Duration::ZERO,
        };
        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"alias.jsonl"));
        let guard = host.lock_file(&lock, Some(Duration::ZERO))?;
        append_exec_log(&host, &entry);
        assert!(host.metadata(&path, Follow::Yes).is_err());
        drop(guard);
        append_exec_log(&host, &entry);
        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"logs-alias/./calls.jsonl"));
        append_exec_log(&host, &entry);
        assert_eq!(
            host.read(&path, ReadRange::All { limit: 4096 })?,
            json_line(&entry).repeat(2),
        );
        assert!(
            host.metadata(&cwd.join("alias.jsonl.lock"), Follow::No)
                .is_err()
        );

        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"missing/calls.jsonl"));
        append_exec_log(&host, &entry);
        assert!(host.events().is_empty());
        Ok(())
    }

    #[test]
    fn repairs_incomplete_final_record_and_keeps_complete_records() -> HostResult<()> {
        let host = PureHost::default();
        let cwd = host.initial_cwd();
        let path = cwd.join("calls.jsonl");
        let range = ReadRange::All { limit: 4096 };
        let mut entry = ExecLogEntry {
            argv: &[],
            persona: Tool::Cmake,
            cwd: &cwd,
            pid: 1000,
            parent_pid: None,
            exit_code: 0,
            duration: Duration::ZERO,
        };
        let complete = json_line(&entry);
        entry.exit_code = 1;
        let next = json_line(&entry);
        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"calls.jsonl"));
        host.write(&path, &complete, WriteMode::CreateNew)?;
        host.write(&path, b"{\"argv\":[", WriteMode::Append)?;
        append_exec_log(&host, &entry);
        let expected = [complete.as_slice(), next.as_slice()].concat();
        assert_eq!(host.read(&path, range)?, expected);

        let without_newline = complete.strip_suffix(b"\n").unwrap_or_default();
        host.write(&path, without_newline, WriteMode::Truncate)?;
        append_exec_log(&host, &entry);
        assert_eq!(host.read(&path, range)?, expected);

        let alias = cwd.join("hardlink.jsonl");
        host.hard_link(&path, &alias)?;
        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"hardlink.jsonl"));
        append_exec_log(&host, &entry);
        assert_eq!(host.read(&path, range)?, expected);
        Ok(())
    }
}
