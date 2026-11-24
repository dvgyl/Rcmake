//! Append invocation records for the PATH-swap corpus.

#![forbid(unsafe_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    FileKind, Follow, Host, WriteMode,
    path::{from_native, to_native},
};
use rcmake_json::{Value, write_compact};

use crate::{Tool, object};

const LOCK_TIMEOUT: Duration = Duration::from_millis(50);

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
/// A logging error leaves the tool's output and exit code intact.
pub(crate) fn append_exec_log(host: &dyn Host, entry: &ExecLogEntry<'_>) {
    let Some(path) = host
        .env_get(b"RCMAKE_EXEC_LOG")
        .filter(|path| !path.is_empty())
    else {
        return;
    };
    let path = host.initial_cwd().join(to_native(path.as_slice()));
    let Some(lock) = lock_path(&path) else {
        return;
    };
    let Ok(_guard) = host.lock_file(&lock, Some(LOCK_TIMEOUT)) else {
        return;
    };
    match host.metadata(&path, Follow::Yes) {
        Ok(metadata) if metadata.kind == FileKind::File => {}
        Err(error) if error.kind == ErrorKind::NotFound => {}
        _ => return,
    }
    let _ = host.write(&path, &json_line(entry), WriteMode::Append);
}

fn lock_path(path: &Path) -> Option<PathBuf> {
    // Use a separate file so Windows can append while the lock is held.
    let mut name = path.file_name()?.to_os_string();
    name.push(".lock");
    Some(path.with_file_name(name))
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
    use rcmake_host::{HostResult, PureHost, ReadRange};
    use rcmake_json::{ReadOptions, parse};

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
        let path = cwd.join("calls.jsonl");
        let lock = cwd.join("calls.jsonl.lock");
        let entry = ExecLogEntry {
            argv: &[],
            persona: Tool::Cmake,
            cwd: &cwd,
            pid: 1000,
            parent_pid: None,
            exit_code: 0,
            duration: Duration::ZERO,
        };
        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"calls.jsonl"));
        let guard = host.lock_file(&lock, Some(Duration::ZERO))?;
        append_exec_log(&host, &entry);
        assert!(host.metadata(&path, Follow::Yes).is_err());
        drop(guard);
        append_exec_log(&host, &entry);
        assert!(host.metadata(&path, Follow::Yes).is_ok());

        host.env_set(b"RCMAKE_EXEC_LOG", Some(b"missing/calls.jsonl"));
        append_exec_log(&host, &entry);
        assert!(host.events().is_empty());
        Ok(())
    }
}
