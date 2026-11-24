//! File layout and publication through the host.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, ReadRange, WriteMode, path};
use rcmake_json::{ReadOptions, Value, parse, write_styled};

use crate::{InstrumentError, invalid};

const JSON_LIMIT: u64 = 32 * 1024 * 1024;
const FILE_NAME_ATTEMPTS: u64 = 65_536;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub(crate) fn absolute(host: &dyn Host, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        host.initial_cwd().join(path)
    }
}

pub(crate) fn api_dir(build_dir: &Path) -> PathBuf {
    build_dir.join(".cmake/instrumentation/v1")
}

pub(crate) fn json_files(host: &dyn Host, dir: &Path) -> Result<Vec<BString>, HostError> {
    let entries = match host.read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            matches!(entry.kind, FileKind::File | FileKind::Symlink)
                && !entry.name.starts_with(b".")
                && entry.name.ends_with(b".json")
        })
        .map(|entry| entry.name)
        .collect();
    files.sort();
    Ok(files)
}

pub(crate) fn read_bytes(host: &dyn Host, file: &Path) -> Result<Vec<u8>, HostError> {
    if host.metadata(file, Follow::Yes)?.kind != FileKind::File {
        return Err(HostError::from_io(
            std::io::Error::new(ErrorKind::InvalidInput, "Expected a regular file."),
            Some(file.to_path_buf()),
        ));
    }
    host.read(file, ReadRange::All { limit: JSON_LIMIT })
}

pub(crate) fn read_json(host: &dyn Host, file: &Path) -> Result<Value, InstrumentError> {
    parse(&read_bytes(host, file)?, &ReadOptions::default())
        .map_err(|error| invalid(format!("{}: {error}", file.display())))
}

pub(crate) fn timestamp(host: &dyn Host) -> String {
    let now = host.now();
    format!("{:020}-{:09}", now.secs.max(0), now.nanos)
}

pub(crate) fn epoch_millis(host: &dyn Host) -> u64 {
    let time = host.now();
    u64::try_from(time.secs)
        .unwrap_or_default()
        .saturating_mul(1000)
        .saturating_add(u64::from(time.nanos / 1_000_000))
}

pub(crate) fn command_hash(host: &dyn Host, command: &[u8]) -> String {
    let mut hasher = HashAlgo::Sha3_256.hasher();
    hasher.update(command);
    hasher.update(host.system().pid.to_string().as_bytes());
    let mut digest = hasher.finish_hex();
    digest.truncate(20);
    digest
}

/// Publish a staged file without replacing an existing file.
pub(crate) fn publish(
    host: &dyn Host,
    dir: &Path,
    value: &Value,
    filename: impl Fn(u64) -> String,
) -> Result<PathBuf, InstrumentError> {
    publish_with(
        host,
        dir,
        value,
        filename,
        |from, to| host.rename_noreplace(from, to),
        |from, to| host.hard_link(from, to),
    )
}

fn publish_with(
    host: &dyn Host,
    dir: &Path,
    value: &Value,
    filename: impl Fn(u64) -> String,
    rename: impl Fn(&Path, &Path) -> HostResult<()>,
    link: impl Fn(&Path, &Path) -> HostResult<()>,
) -> Result<PathBuf, InstrumentError> {
    host.create_dir_all(dir)?;
    let data = write_styled(value, "\t");
    let staged = (0..FILE_NAME_ATTEMPTS)
        .find_map(|_| {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temp = dir.join(format!(".instrument-{}-{serial}.tmp", host.system().pid));
            match host.write(&temp, &data, WriteMode::CreateNew) {
                Ok(()) => Some(Ok(temp)),
                Err(error) if error.kind == ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error.into())),
            }
        })
        .unwrap_or_else(|| {
            Err(invalid(
                "No temporary instrumentation file name is available.",
            ))
        })?;

    let result = (0..FILE_NAME_ATTEMPTS)
        .find_map(|n| {
            let path = dir.join(filename(n));
            let result = rename(&staged, &path)
                .or_else(|error| publication_fallback(error, || link(&staged, &path)))
                .or_else(|error| {
                    publication_fallback(error, || host.write(&path, &data, WriteMode::CreateNew))
                });
            match result {
                Ok(()) => Some(Ok(path)),
                Err(error) if error.kind == ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error.into())),
            }
        })
        .unwrap_or_else(|| Err(invalid("No instrumentation file name is available.")));
    let _ = host.remove_file(&staged);
    result
}

fn publication_fallback(
    error: HostError,
    fallback: impl FnOnce() -> HostResult<()>,
) -> HostResult<()> {
    match error.kind {
        ErrorKind::Unsupported | ErrorKind::PermissionDenied => fallback(),
        _ => Err(error),
    }
}

pub(crate) fn publish_at(
    host: &dyn Host,
    dir: &Path,
    stem: &str,
    value: &Value,
) -> Result<PathBuf, InstrumentError> {
    publish(host, dir, value, |n| {
        if n == 0 {
            format!("{stem}.json")
        } else {
            format!("{stem}-{n:020}.json")
        }
    })
}

pub(crate) fn newest_content(
    host: &dyn Host,
    data: &Path,
) -> Result<Option<BString>, InstrumentError> {
    Ok(json_files(host, &data.join("content"))?
        .into_iter()
        .filter(|name| name.starts_with(b"cmake-"))
        .max_by(|left, right| file_stem(left).cmp(file_stem(right)))
        .map(|name| path::from_native(&Path::new("content").join(path::to_native(&name)))))
}

pub(crate) fn file_stem(name: &[u8]) -> &[u8] {
    name.strip_suffix(b".json").unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn publish_without_rename_or_hard_links() -> Result<(), InstrumentError> {
        let host = PureHost::new("/work".into(), "/bin/rcmake".into());
        let dir = Path::new("/work");
        let existing = dir.join("0.json");
        host.write(&existing, b"existing", WriteMode::CreateNew)?;
        let unsupported = |_: &Path, _: &Path| Err(HostError::unsupported("Not supported."));
        let value = Value::string("new");
        let file = publish_with(
            &host,
            dir,
            &value,
            |n| format!("{n}.json"),
            unsupported,
            unsupported,
        )?;
        assert_eq!(file, dir.join("1.json"));
        assert_eq!(read_bytes(&host, &file)?, write_styled(&value, "\t"));
        assert_eq!(read_bytes(&host, &existing)?, b"existing");
        assert_eq!(host.read_dir(dir)?.len(), 2);
        Ok(())
    }
}
