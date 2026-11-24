//! State files in the build directory.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, Limits, hash::HashAlgo};
use rcmake_host::{FileTime, Follow, Host, HostError, ReadRange, WriteMode};

use crate::CtestError;

const TEMPORARY: &str = "Testing/Temporary";
const FAILURES: &str = "LastTestsFailed.log";
const LOG: &str = "LastTest.log";
const COSTS: &str = "CTestCostData.txt";
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// A failed test and its number in the full test list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedTest {
    pub(crate) number: usize,
    pub(crate) name: BString,
}

/// Timing data for one test.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CostEntry {
    pub(crate) runs: u32,
    pub(crate) average: f64,
}

impl CostEntry {
    /// Add the duration of one successful run, in seconds.
    pub(crate) fn record_pass(&mut self, seconds: f64) {
        self.runs = self.runs.saturating_add(1);
        self.average += (seconds - self.average) / f64::from(self.runs);
    }
}

/// Test timings and failures recorded by the scheduler.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CostData {
    pub(crate) tests: BTreeMap<BString, CostEntry>,
    pub(crate) failed: Vec<BString>,
}

fn access(path: &Path, source: HostError) -> CtestError {
    CtestError::Access {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn read_optional(host: &dyn Host, path: &Path) -> Result<Option<Vec<u8>>, CtestError> {
    match host.read(
        path,
        ReadRange::All {
            limit: Limits::default().max_value_bytes as u64,
        },
    ) {
        Ok(data) => Ok(Some(data)),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(access(path, error)),
    }
}

fn write(host: &dyn Host, parent: &Path, name: &str, data: &[u8]) -> Result<(), CtestError> {
    host.create_dir_all(parent)
        .map_err(|error| access(parent, error))?;
    let path = parent.join(name);
    for _ in 0..32 {
        let nonce = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
        let time = host.now();
        let pending = parent.join(format!(".{name}.{}.{}.{nonce}.tmp", time.secs, time.nanos));
        match host.write(&pending, data, WriteMode::CreateNew) {
            Ok(()) => {
                return host.rename(&pending, &path).map_err(|error| {
                    let _ = host.remove_file(&pending);
                    access(&path, error)
                });
            }
            Err(error) if error.kind == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                let _ = host.remove_file(&pending);
                return Err(access(&path, error));
            }
        }
    }
    Err(access(
        &path,
        HostError::from_io(ErrorKind::AlreadyExists.into(), Some(path.clone())),
    ))
}

pub(crate) fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
}

pub(crate) fn last_failed(host: &dyn Host, dir: &Path) -> Result<Vec<FailedTest>, CtestError> {
    let data = read_optional(host, &dir.join(TEMPORARY).join(FAILURES))?.unwrap_or_default();
    Ok(lines(&data)
        .filter_map(|line| {
            let mut parts = line.splitn(2, |&byte| byte == b':');
            let (number, name) = (parts.next()?, parts.next()?);
            let number = std::str::from_utf8(number).ok()?.parse().ok()?;
            (number > 0).then(|| FailedTest {
                number,
                name: name.into(),
            })
        })
        .collect())
}

/// Replace the failure log only if at least one test failed.
pub(crate) fn write_failed(
    host: &dyn Host,
    dir: &Path,
    failed: &[FailedTest],
) -> Result<(), CtestError> {
    if failed.is_empty() {
        return Ok(());
    }
    let mut data = Vec::new();
    for test in failed {
        data.extend_from_slice(test.number.to_string().as_bytes());
        data.push(b':');
        data.extend_from_slice(&test.name);
        data.push(b'\n');
    }
    write(host, &dir.join(TEMPORARY), FAILURES, &data)
}

#[cfg(test)]
pub(crate) fn last_log(host: &dyn Host, dir: &Path) -> Result<Option<Vec<u8>>, CtestError> {
    read_optional(host, &dir.join(TEMPORARY).join(LOG))
}

pub(crate) fn write_log(host: &dyn Host, dir: &Path, data: &[u8]) -> Result<(), CtestError> {
    write(host, &dir.join(TEMPORARY), LOG, data)
}

pub(crate) fn read_costs(host: &dyn Host, dir: &Path) -> Result<CostData, CtestError> {
    let data = read_optional(host, &dir.join(TEMPORARY).join(COSTS))?.unwrap_or_default();
    let mut result = CostData::default();
    let mut entries = lines(&data);
    for line in entries.by_ref() {
        if line == b"---" {
            break;
        }
        if let Some((name, entry)) = parse_cost(line) {
            result.tests.insert(name.into(), entry);
        }
    }
    result.failed = entries
        .filter(|line| !line.is_empty())
        .map(Into::into)
        .collect();
    Ok(result)
}

fn parse_cost(line: &[u8]) -> Option<(&[u8], CostEntry)> {
    let mut fields = line.rsplitn(3, |&byte| byte == b' ');
    let average: f64 = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let runs = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    let name = fields.next()?;
    average
        .is_finite()
        .then_some((name, CostEntry { runs, average }))
}

pub(crate) fn write_costs(host: &dyn Host, dir: &Path, costs: &CostData) -> Result<(), CtestError> {
    let mut data = Vec::new();
    for (name, entry) in &costs.tests {
        data.extend_from_slice(name);
        data.extend_from_slice(format!(" {} {}\n", entry.runs, entry.average).as_bytes());
    }
    data.extend_from_slice(b"---\n");
    for name in &costs.failed {
        data.extend_from_slice(name);
        data.push(b'\n');
    }
    write(host, &dir.join(TEMPORARY), COSTS, &data)
}

fn stamp_name(test: &[u8]) -> String {
    format!("{}.stamp", HashAlgo::Md5.hex_digest(test))
}

fn stamp_dir(dir: &Path) -> PathBuf {
    dir.join(TEMPORARY).join("LastTestRun")
}

pub(crate) fn read_stamp(
    host: &dyn Host,
    dir: &Path,
    test: &[u8],
) -> Result<Option<FileTime>, CtestError> {
    modified(host, &stamp_dir(dir).join(stamp_name(test)))
}

pub(crate) fn modified(host: &dyn Host, path: &Path) -> Result<Option<FileTime>, CtestError> {
    match host.metadata(path, Follow::Yes) {
        Ok(metadata) => Ok(Some(metadata.mtime)),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(access(path, error)),
    }
}

pub(crate) fn write_stamp(host: &dyn Host, dir: &Path, test: &[u8]) -> Result<(), CtestError> {
    write(host, &stamp_dir(dir), &stamp_name(test), b"")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn failure_log_uses_numbers_and_survives_a_passing_run() -> Result<(), CtestError> {
        let host = PureHost::default();
        let dir = Path::new("/work");
        assert!(last_failed(&host, dir)?.is_empty());
        let failed = [FailedTest {
            number: 2,
            name: b"name: with spaces\xff".as_slice().into(),
        }];
        write_failed(&host, dir, &failed)?;
        write_failed(&host, dir, &[])?;
        assert_eq!(last_failed(&host, dir)?, failed);
        assert_eq!(
            read_optional(&host, &dir.join(TEMPORARY).join(FAILURES))?.as_deref(),
            Some(b"2:name: with spaces\xff\n".as_slice()),
        );
        Ok(())
    }

    #[test]
    fn test_log_replaces_previous_bytes() -> Result<(), CtestError> {
        let host = PureHost::default();
        let dir = Path::new("/work");
        assert_eq!(last_log(&host, dir)?, None);
        write_log(&host, dir, b"old log\n")?;
        write_log(&host, dir, b"new\xff")?;
        assert_eq!(
            last_log(&host, dir)?.as_deref(),
            Some(b"new\xff".as_slice())
        );
        Ok(())
    }

    #[test]
    fn costs_preserve_names_with_spaces_and_update_average() -> Result<(), CtestError> {
        let host = PureHost::default();
        let dir = Path::new("/work");
        assert_eq!(read_costs(&host, dir)?, CostData::default());
        let mut entry = CostEntry::default();
        entry.record_pass(0.5);
        entry.record_pass(1.5);
        let costs = CostData {
            tests: BTreeMap::from([("test name".into(), entry)]),
            failed: vec!["previous failure".into()],
        };
        write_costs(&host, dir, &costs)?;
        assert_eq!(read_costs(&host, dir)?, costs);
        assert_eq!(
            read_optional(&host, &dir.join(TEMPORARY).join(COSTS))?.as_deref(),
            Some(b"test name 2 1\n---\nprevious failure\n".as_slice()),
        );
        Ok(())
    }

    #[test]
    fn stamp_uses_name_digest_and_updates_modification_time() -> Result<(), CtestError> {
        let host = PureHost::default();
        let dir = Path::new("/work");
        assert_eq!(read_stamp(&host, dir, b"abc")?, None);
        write_stamp(&host, dir, b"abc")?;
        assert_eq!(read_stamp(&host, dir, b"abc")?, Some(host.now()));
        assert_eq!(
            read_optional(
                &host,
                &stamp_dir(dir).join("900150983cd24fb0d6963f7d28e17f72.stamp")
            )?,
            Some(vec![])
        );
        host.sleep(Duration::from_secs(1));
        write_stamp(&host, dir, b"abc")?;
        assert_eq!(read_stamp(&host, dir, b"abc")?, Some(host.now()));
        Ok(())
    }
}
