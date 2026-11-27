// SPDX-License-Identifier: MIT OR Apache-2.0

//! Export and import of probe entries for portable cache archives.
//!
//! Export reads only probe entries and the blobs that they reference. Import
//! checks a complete archive before it writes anything. A trusted import
//! publishes entries under the collection lock, with blobs before entries. An
//! untrusted import writes candidates outside the live probe namespace. A
//! candidate can only restore blobs for a live entry that has the same frame,
//! so a candidate can never establish its own pin.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use rcmake_host::HostError;
use rcmake_json::Value;

use crate::remote::{CacheEntryKind, CacheManifest, ManifestEntry, RemoteError};
use crate::remote::{decode_blob, decode_entry, read_frame};
use crate::{
    CacheRead, Digest, Domain, Entry, LockedStore, Store, VerifyOutcome, blob_limit, frame, shard,
};

const IMPORT_WAIT: Duration = Duration::from_secs(10);
const PROBE_BLOB_BYTES: usize = 16 * 1024 * 1024;
const MEMBER_BYTES: usize = 64 * 1024 * 1024;
const EXPORT_BYTES: usize = 480 * 1024 * 1024;
const DECODED_BYTES: usize = 1024 * 1024 * 1024;
/// Candidates that no live entry used within this time are removed by collection.
pub(crate) const CANDIDATE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Counts of probe entries that an export left out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportSkips {
    /// Entries whose recorded inputs no longer match this machine.
    pub stale: usize,
    /// Entries with an invalid frame or record.
    pub invalid: usize,
    /// Entries with a missing or invalid referenced blob.
    pub missing_blob: usize,
    /// Entries that do not fit in the archive size limits.
    pub over_budget: usize,
}

impl ExportSkips {
    /// Return the total number of skipped entries.
    pub fn total(&self) -> usize {
        self.stale + self.invalid + self.missing_blob + self.over_budget
    }
}

/// Verified probe entries and blobs for a portable archive.
#[derive(Clone, Debug, Default)]
pub struct ProbeExport {
    /// One record for each exported entry.
    pub manifest: CacheManifest,
    /// Entry frames by key.
    pub entries: BTreeMap<Digest, Vec<u8>>,
    /// Blob frames by digest.
    pub blobs: BTreeMap<Digest, Vec<u8>>,
    /// Entries that the export left out.
    pub skipped: ExportSkips,
}

/// Probe data read from a portable archive. [`Store::import_probes`] checks it.
#[derive(Clone, Debug, Default)]
pub struct ProbeBundle {
    /// The `manifest.json` bytes.
    pub manifest: Vec<u8>,
    /// Entry frames by key.
    pub entries: BTreeMap<Digest, Vec<u8>>,
    /// Blob frames by digest.
    pub blobs: BTreeMap<Digest, Vec<u8>>,
}

/// How an import admits archive data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportTrust {
    /// The archive digest matches a digest from a trusted producer. Publish entries.
    Trusted,
    /// No trusted digest. Stage entries as candidates outside the live probe namespace.
    Untrusted,
}

/// Result of one import.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportReport {
    /// Entries published into the live probe namespace.
    pub entries_published: usize,
    /// Blobs written into the live blob store.
    pub blobs_published: usize,
    /// Entries already present with the same frame and all blobs.
    pub unchanged: usize,
    /// Live entries with the same frame whose missing blobs the import restored.
    pub recovered: usize,
    /// Entries written as candidates. A candidate is not used for a cache hit.
    pub staged: usize,
    /// Entries for which a different valid local entry was kept.
    pub conflicts: usize,
}

/// Reason for a failed import.
#[derive(Debug)]
pub enum ImportError {
    /// The archive content failed a check. The store is unchanged.
    Invalid(String),
    /// A store operation failed.
    Store(HostError),
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => formatter.write_str(reason),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<HostError> for ImportError {
    fn from(error: HostError) -> Self {
        Self::Store(error)
    }
}

fn invalid(reason: impl Into<String>) -> ImportError {
    ImportError::Invalid(reason.into())
}

fn remote_invalid(error: RemoteError) -> ImportError {
    invalid(error.to_string())
}

/// One checked archive entry.
struct Checked<'a> {
    pin: ManifestEntry,
    frame: &'a [u8],
    trace: crate::Trace,
}

impl Store<'_> {
    /// Read the verified probe entries and their blobs for an export.
    ///
    /// The read takes no lock, so collection can run at the same time. An
    /// entry is exported only if its frame, its current input trace, and all
    /// of its blobs are valid when they are read. Other entries are counted in
    /// [`ProbeExport::skipped`].
    pub fn export_probes(&self) -> Result<ProbeExport, HostError> {
        let mut export = ProbeExport::default();
        if !self.readable()? {
            return Ok(export);
        }
        let directory = self
            .config
            .root
            .join("entries")
            .join(Domain::ProbeV1.as_str());
        let mut files = self.cache_files(&directory, "entry")?;
        files.sort_unstable_by_key(|file| file.digest);
        let mut bytes = 0usize;
        for file in files {
            let key = file.digest;
            let (pin, encoded) = match crate::remote::export_entry(self, CacheEntryKind::Probe, key)
            {
                Ok(Some(found)) => found,
                Ok(None) => continue,
                Err(_) => {
                    export.skipped.invalid += 1;
                    continue;
                }
            };
            let Ok(entry) = decode_entry::<Value>(CacheEntryKind::Probe, key, &encoded, Some(&pin))
            else {
                export.skipped.invalid += 1;
                continue;
            };
            if self.verify_inputs(&entry.trace) != VerifyOutcome::Hit {
                export.skipped.stale += 1;
                continue;
            }
            let mut blobs = Vec::new();
            let mut complete = true;
            for &digest in &pin.blobs {
                if export.blobs.contains_key(&digest) {
                    continue;
                }
                match crate::remote::export_blob(self, digest) {
                    Ok(Some(frame)) => blobs.push((digest, frame)),
                    Ok(None) | Err(_) => {
                        complete = false;
                        break;
                    }
                }
            }
            if !complete {
                export.skipped.missing_blob += 1;
                continue;
            }
            let size = blobs.iter().fold(encoded.len(), |total, (_, frame)| {
                total.saturating_add(frame.len())
            });
            if encoded.len() > MEMBER_BYTES
                || blobs.iter().any(|(_, frame)| frame.len() > MEMBER_BYTES)
                || bytes.saturating_add(size) > EXPORT_BYTES
                || export.manifest.insert(pin).is_err()
            {
                export.skipped.over_budget += 1;
                continue;
            }
            bytes = bytes.saturating_add(size);
            export.entries.insert(key, encoded);
            export.blobs.extend(blobs);
        }
        Ok(export)
    }

    /// Check a complete archive, then admit its probe entries.
    ///
    /// Every check runs before the first write. A failed check returns
    /// [`ImportError::Invalid`] and leaves the store unchanged. Writes run under
    /// the collection lock, with blobs before the entry that references them.
    /// If the import stops early, each published entry is complete, and a
    /// repeated import finishes the rest. A valid local entry is never replaced.
    pub fn import_probes(
        &self,
        bundle: &ProbeBundle,
        trust: ImportTrust,
    ) -> Result<ImportReport, ImportError> {
        let checked = check_bundle(bundle)?;
        if !self.writable() {
            return Err(invalid("The probe cache is not writable."));
        }
        let locked = self.lock_cache(IMPORT_WAIT)?;
        let mut report = ImportReport::default();
        for entry in &checked {
            #[cfg(test)]
            self.test_event(
                crate::TestEvent::ImportStep,
                &self.entry_path(Domain::ProbeV1, entry.pin.key),
            );
            self.admit(&locked, bundle, entry, trust, &mut report)?;
        }
        Ok(report)
    }

    fn admit(
        &self,
        locked: &LockedStore<'_, '_>,
        bundle: &ProbeBundle,
        entry: &Checked<'_>,
        trust: ImportTrust,
        report: &mut ImportReport,
    ) -> Result<(), ImportError> {
        let key = entry.pin.key;
        let path = self.entry_path(Domain::ProbeV1, key);
        let live = read_frame(self, &path, frame::ENTRY_LIMIT).map_err(remote_invalid)?;
        if let Some(live) = &live
            && live.as_slice() == entry.frame
        {
            // The live entry is an independent pin with the same frame.
            let missing = self.missing_blobs(&entry.pin.blobs)?;
            if missing.is_empty() {
                report.unchanged += 1;
            } else if trust == ImportTrust::Trusted
                || self.verify_inputs(&entry.trace) == VerifyOutcome::Hit
            {
                report.blobs_published += self.publish_blobs(locked, bundle, &missing)?;
                report.recovered += 1;
            } else {
                report.conflicts += 1;
            }
            return Ok(());
        }
        let live_valid = live.is_some() && self.live_entry_valid(key)?;
        match trust {
            ImportTrust::Trusted if live_valid => report.conflicts += 1,
            ImportTrust::Trusted
                if live.is_some() && self.verify_inputs(&entry.trace) != VerifyOutcome::Hit =>
            {
                report.conflicts += 1;
            }
            ImportTrust::Trusted => {
                report.blobs_published += self.publish_blobs(locked, bundle, &entry.pin.blobs)?;
                #[cfg(test)]
                self.test_event(crate::TestEvent::ImportBlobsStaged, &path);
                locked.publish(&path, entry.frame, live.is_some())?;
                report.entries_published += 1;
            }
            ImportTrust::Untrusted if live_valid => report.conflicts += 1,
            ImportTrust::Untrusted => {
                if self.stage_candidate(locked, bundle, entry)? {
                    report.staged += 1;
                } else {
                    report.unchanged += 1;
                }
            }
        }
        Ok(())
    }

    fn live_entry_valid(&self, key: Digest) -> Result<bool, ImportError> {
        let path = self.entry_path(Domain::ProbeV1, key);
        match self.read_entry::<Entry<Value>>(Domain::ProbeV1, key, &path, false)? {
            CacheRead::Found(entry) => Ok(self.entry_matches(&entry, blob_limit(), false)?),
            CacheRead::Missing | CacheRead::Invalid => Ok(false),
        }
    }

    fn missing_blobs(&self, blobs: &[Digest]) -> Result<Vec<Digest>, ImportError> {
        let mut missing = Vec::new();
        for &digest in blobs {
            if self.file_metadata(&self.blob_path(digest))?.is_none() {
                missing.push(digest);
            }
        }
        Ok(missing)
    }

    fn publish_blobs(
        &self,
        locked: &LockedStore<'_, '_>,
        bundle: &ProbeBundle,
        blobs: &[Digest],
    ) -> Result<usize, ImportError> {
        let mut published = 0;
        for &digest in blobs {
            let frame = bundle
                .blobs
                .get(&digest)
                .ok_or_else(|| invalid("A referenced blob is missing from the archive."))?;
            if locked.put_blob(digest, &self.blob_path(digest), frame, blob_limit())? {
                published += 1;
            }
        }
        Ok(published)
    }

    /// Write an entry and its blobs as candidates. Return whether anything changed.
    fn stage_candidate(
        &self,
        locked: &LockedStore<'_, '_>,
        bundle: &ProbeBundle,
        entry: &Checked<'_>,
    ) -> Result<bool, ImportError> {
        let mut changed = false;
        for &digest in &entry.pin.blobs {
            let frame = bundle
                .blobs
                .get(&digest)
                .ok_or_else(|| invalid("A referenced blob is missing from the archive."))?;
            let path = self.candidate_blob_path(digest);
            if read_frame(self, &path, MEMBER_BYTES)
                .map_err(remote_invalid)?
                .as_ref()
                != Some(frame)
            {
                locked.publish(&path, frame, true)?;
                changed = true;
            }
        }
        let path = self.candidate_entry_path(entry.pin.key);
        let current = read_frame(self, &path, frame::ENTRY_LIMIT).map_err(remote_invalid)?;
        if current.as_deref() != Some(entry.frame) {
            locked.publish(&path, entry.frame, true)?;
            changed = true;
        }
        Ok(changed)
    }

    /// Restore missing blobs of a live probe entry from a candidate with the same frame.
    ///
    /// The live entry is the pin. Its trace must match now. Each restored blob
    /// must match its digest. Return whether all referenced blobs are present.
    pub(crate) fn recover_from_candidate<T>(&self, key: Digest, entry: &Entry<T>) -> bool {
        self.try_recover(key, entry).unwrap_or(false)
    }

    fn try_recover<T>(&self, key: Digest, entry: &Entry<T>) -> Result<bool, ImportError> {
        if !self.writable() || entry.blobs.is_empty() {
            return Ok(false);
        }
        let candidate_path = self.candidate_entry_path(key);
        let Some(candidate) =
            read_frame(self, &candidate_path, frame::ENTRY_LIMIT).map_err(remote_invalid)?
        else {
            return Ok(false);
        };
        let live_path = self.entry_path(Domain::ProbeV1, key);
        let live = read_frame(self, &live_path, frame::ENTRY_LIMIT).map_err(remote_invalid)?;
        if live.as_deref() != Some(candidate.as_slice())
            || self.verify_inputs(&entry.trace) != VerifyOutcome::Hit
        {
            return Ok(false);
        }
        let missing = self.missing_blobs(&entry.blobs)?;
        let mut frames = Vec::new();
        for digest in missing {
            let Some(frame) = read_frame(self, &self.candidate_blob_path(digest), MEMBER_BYTES)
                .map_err(remote_invalid)?
            else {
                return Ok(false);
            };
            let mut budget = PROBE_BLOB_BYTES;
            if decode_blob(&frame, digest, &mut budget).is_err() {
                return Ok(false);
            }
            frames.push((digest, frame));
        }
        let locked = self.lock_cache(crate::PUBLISH_WAIT)?;
        for (digest, frame) in frames {
            locked.put_blob(digest, &self.blob_path(digest), &frame, blob_limit())?;
        }
        let _ = self
            .host
            .set_times(&candidate_path, None, Some(self.host.now()));
        Ok(true)
    }

    pub(crate) fn candidate_entry_path(&self, key: Digest) -> PathBuf {
        shard(self.config.root.join("candidates/entries"), key, "entry")
    }

    pub(crate) fn candidate_blob_path(&self, digest: Digest) -> PathBuf {
        shard(self.config.root.join("candidates/blobs"), digest, "blob")
    }

    pub(crate) fn candidate_locations(&self) -> [(PathBuf, &'static str); 2] {
        [
            (self.config.root.join("candidates/entries"), "entry"),
            (self.config.root.join("candidates/blobs"), "blob"),
        ]
    }
}

/// Check the complete archive content without store access.
///
/// The manifest must name only probe entries. The archive must hold exactly
/// the named entries and the union of their blobs. Each frame must match its
/// digest, record schema, key, and size limits.
fn check_bundle(bundle: &ProbeBundle) -> Result<Vec<Checked<'_>>, ImportError> {
    let manifest = CacheManifest::from_json(&bundle.manifest).map_err(remote_invalid)?;
    if manifest
        .entries()
        .iter()
        .any(|pin| pin.kind != CacheEntryKind::Probe)
    {
        return Err(invalid(
            "The archive contains entries other than probe entries.",
        ));
    }
    let keys: BTreeSet<Digest> = manifest.entries().iter().map(|pin| pin.key).collect();
    if !bundle.entries.keys().eq(keys.iter()) {
        return Err(invalid("The archive entries do not match its manifest."));
    }
    let referenced: BTreeSet<Digest> = manifest
        .entries()
        .iter()
        .flat_map(|pin| pin.blobs.iter().copied())
        .collect();
    if !bundle.blobs.keys().eq(referenced.iter()) {
        return Err(invalid("The archive blobs do not match its manifest."));
    }
    let mut sizes = BTreeMap::new();
    let mut decoded = 0usize;
    for (&digest, frame) in &bundle.blobs {
        let mut budget = PROBE_BLOB_BYTES;
        let size = decode_blob(frame, digest, &mut budget)
            .map_err(|error| invalid(format!("Blob {digest}: {error}")))?;
        decoded = decoded.saturating_add(size);
        if decoded > DECODED_BYTES {
            return Err(invalid("The archive blobs exceed the decoded size limit."));
        }
        sizes.insert(digest, size);
    }
    let mut checked = Vec::with_capacity(keys.len());
    for pin in manifest.entries() {
        let frame = bundle
            .entries
            .get(&pin.key)
            .ok_or_else(|| invalid("The archive entries do not match its manifest."))?;
        let entry = decode_entry::<Value>(CacheEntryKind::Probe, pin.key, frame, Some(pin))
            .map_err(|error| invalid(format!("Entry {}: {error}", pin.key)))?;
        let total = pin
            .blobs
            .iter()
            .map(|digest| sizes.get(digest).copied().unwrap_or_default())
            .fold(0usize, usize::saturating_add);
        if total > PROBE_BLOB_BYTES {
            return Err(invalid(format!(
                "Entry {}: its blobs exceed the probe size limit.",
                pin.key
            )));
        }
        checked.push(Checked {
            pin: pin.clone(),
            frame,
            trace: entry.trace,
        });
    }
    Ok(checked)
}

#[cfg(test)]
mod tests;
