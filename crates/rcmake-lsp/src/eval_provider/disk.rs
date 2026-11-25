// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_host::{Follow, Host, HostResult, ReadRange};

use super::{MAX_TRACKED_BYTES, MAX_TRACKED_FILES};

struct Read {
    range: ReadRange,
    bytes: Box<[u8]>,
}

struct Input {
    resolved: PathBuf,
    reads: Vec<Read>,
}

#[derive(Default)]
pub(super) struct DiskReads {
    files: BTreeMap<PathBuf, Input>,
    aliases: BTreeMap<PathBuf, PathBuf>,
    missing: BTreeSet<PathBuf>,
    count: usize,
    bytes: usize,
    incomplete: bool,
}

impl DiskReads {
    pub(super) fn record(
        &mut self,
        host: &dyn Host,
        path: &Path,
        origin: &Path,
        range: ReadRange,
        result: &HostResult<Vec<u8>>,
    ) {
        let Ok(bytes) = result else {
            return;
        };
        let Ok(resolved) = host.canonicalize(path) else {
            self.incomplete = true;
            return;
        };
        if path != origin && host.canonicalize(origin).as_ref() == Ok(&resolved) {
            if self.aliases.len() < MAX_TRACKED_FILES || self.aliases.contains_key(origin) {
                if self
                    .aliases
                    .insert(origin.to_owned(), resolved.clone())
                    .is_some_and(|old| old != resolved)
                {
                    self.incomplete = true;
                }
            } else {
                self.incomplete = true;
            }
        }
        if let Some(input) = self.files.get(path) {
            if input.resolved != resolved {
                self.incomplete = true;
                return;
            }
            if let Some(previous) = input.reads.iter().find(|record| record.range == range) {
                self.incomplete |= previous.bytes.as_ref() != bytes.as_slice();
                return;
            }
        }
        if !path.is_absolute()
            || self.count >= MAX_TRACKED_FILES
            || self.bytes.saturating_add(bytes.len()) > MAX_TRACKED_BYTES
        {
            self.incomplete = true;
            return;
        }
        self.files
            .entry(path.to_owned())
            .or_insert_with(|| Input {
                resolved,
                reads: Vec::new(),
            })
            .reads
            .push(Read {
                range,
                bytes: bytes.as_slice().into(),
            });
        self.count += 1;
        self.bytes += bytes.len();
    }

    pub(super) fn retain_missing(&mut self, host: &dyn Host, previous: &Self) {
        for path in previous.paths() {
            if !self.files.contains_key(path) && missing(host, path) {
                if self.missing.len() >= MAX_TRACKED_FILES {
                    self.incomplete = true;
                    break;
                }
                self.missing.insert(path.to_owned());
            }
        }
    }

    pub(super) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.files
            .keys()
            .chain(self.aliases.keys())
            .chain(&self.missing)
            .map(PathBuf::as_path)
    }

    pub(super) fn complete(&self) -> bool {
        !self.incomplete
    }

    pub(super) fn changed(&self, host: &dyn Host) -> bool {
        self.missing.iter().any(|path| !missing(host, path))
            || self
                .aliases
                .iter()
                .any(|(path, resolved)| host.canonicalize(path).as_ref() != Ok(resolved))
            || self.files.iter().any(|(path, input)| {
                host.canonicalize(path).as_ref() != Ok(&input.resolved)
                    || input.reads.iter().any(|read| {
                        // One extra byte detects growth without an unbounded read.
                        let bound = (read.bytes.len() as u64).saturating_add(1);
                        let range = match read.range {
                            ReadRange::All { limit } => ReadRange::All {
                                limit: limit.min(bound),
                            },
                            ReadRange::Slice { offset, len } => ReadRange::Slice {
                                offset,
                                len: Some(len.map_or(bound, |len| len.min(bound))),
                            },
                        };
                        host.read(path, range).as_deref() != Ok(read.bytes.as_ref())
                    })
            })
    }
}

fn missing(host: &dyn Host, path: &Path) -> bool {
    host.metadata(path, Follow::Yes)
        .is_err_and(|error| error.kind == ErrorKind::NotFound)
}
