// SPDX-License-Identifier: MIT OR Apache-2.0

//! Export and import of probe entries for portable cache archives.
//!
//! Export reads only probe entries and the blobs that they reference. It keeps
//! the archive inside [`ArchiveLimits`], the same limits that import applies.
//! Import checks a complete archive before it writes anything. A trusted import
//! publishes entries under the collection lock, with blobs before entries. An
//! untrusted import writes candidates outside the live probe namespace. A
//! candidate can only restore blobs for a live entry that has the same frame,
//! so a candidate can never establish its own pin.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_host::HostError;
use rcmake_json::{
    FieldError, FromJson, Value, deny_unknown, object, req, req_value, write_compact,
};

use crate::remote::{CacheEntryKind, CacheManifest, ManifestEntry, RemoteError};
use crate::remote::{decode_blob, decode_entry, read_frame};
use crate::{
    CacheRead, Digest, Domain, Entry, LockedStore, Store, VerifyOutcome, blob_limit, frame, io_at,
    path_from_json, shard,
};

const IMPORT_WAIT: Duration = Duration::from_secs(10);
const PROBE_BLOB_BYTES: usize = 16 * 1024 * 1024;
/// Most journal checks of one probe record. `MAX_OBSERVATIONS` in `rcmake-commands`
/// has the same value, and a parity test there keeps both checks in agreement.
const MAX_PROBE_CHECKS: usize = 16_384;
const BLOCK: u64 = 512;
/// Candidates that no live entry used within this time are removed by collection.
pub(crate) const CANDIDATE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// Size limits of a portable probe archive.
///
/// `rcmake cache export` and `rcmake cache import` use the same value. Export
/// keeps each archive inside these limits, so the import checks accept it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveLimits {
    /// Maximum size of the compressed archive file.
    pub archive_bytes: u64,
    /// Maximum size of the decompressed tar stream, with headers, padding, and end marker.
    pub tar_bytes: u64,
    /// Maximum size of one entry or blob member.
    pub member_bytes: u64,
    /// Maximum size of the manifest member.
    pub manifest_bytes: u64,
    /// Maximum combined size of all member data.
    pub total_bytes: u64,
    /// Maximum number of members, including the manifest.
    pub members: usize,
    /// Maximum combined decoded size of all blobs.
    pub decoded_bytes: u64,
    /// Maximum combined decoded size of all entry frames, as their headers declare it.
    ///
    /// A decoded entry holds the input trace that an import keeps until it publishes
    /// the entry. Export and import charge each entry against this one aggregate
    /// budget before they decompress it.
    pub entry_decoded_bytes: u64,
}

impl ArchiveLimits {
    /// The limits that `rcmake cache export` and `rcmake cache import` use.
    pub const DEFAULT: Self = {
        const MIB: u64 = 1024 * 1024;
        Self {
            archive_bytes: 256 * MIB,
            tar_bytes: 272 * MIB,
            member_bytes: 64 * MIB,
            manifest_bytes: 16 * MIB,
            total_bytes: 256 * MIB,
            members: 65_536,
            decoded_bytes: 1024 * MIB,
            entry_decoded_bytes: 512 * MIB,
        }
    };

    /// Largest tar stream that export writes.
    ///
    /// Entry and blob frames are compressed already, so gzip cannot make them
    /// smaller. The reserve covers the gzip framing of incompressible data.
    fn export_tar_bytes(&self) -> u64 {
        self.tar_bytes
            .min(self.archive_bytes - self.archive_bytes / 64)
    }
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Aggregate budget of decoded entry bytes for one export or import.
///
/// The budget reads the decoded size from the entry frame header, so a caller
/// charges an entry before it decompresses the entry.
#[derive(Clone, Copy, Debug)]
pub struct EntryBudget {
    left: u64,
}

impl EntryBudget {
    /// Start a budget of [`ArchiveLimits::entry_decoded_bytes`].
    #[must_use]
    pub const fn new(limits: &ArchiveLimits) -> Self {
        Self {
            left: limits.entry_decoded_bytes,
        }
    }

    /// Return the decoded size that an entry frame declares, or `None` for a frame
    /// without a valid entry header.
    #[must_use]
    pub fn declared(frame: &[u8]) -> Option<u64> {
        frame::payload_len(frame::ENTRY_MAGIC, frame).map(|len| len as u64)
    }

    /// Charge the declared decoded size of `frame`.
    ///
    /// # Errors
    ///
    /// Returns [`ImportError::Invalid`] for a frame without a valid entry header,
    /// or when the charge exceeds the remaining budget. A failed charge changes
    /// nothing.
    pub fn charge(&mut self, frame: &[u8]) -> Result<u64, ImportError> {
        let size = Self::declared(frame).ok_or_else(|| invalid("An entry frame is invalid."))?;
        self.left = self
            .left
            .checked_sub(size)
            .ok_or_else(|| invalid("The archive entries exceed the decoded size limit."))?;
        Ok(size)
    }
}

/// Size of one tar member with its header and padding.
fn tar_member(size: u64) -> u64 {
    BLOCK + size.div_ceil(BLOCK) * BLOCK
}

/// Counts of probe entries that an export left out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExportSkips {
    /// Entries whose recorded inputs no longer match this machine.
    pub stale: usize,
    /// Entries with an invalid frame, record, or probe payload.
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

impl ProbeBundle {
    /// Run the archive content checks of [`Store::import_probes`] without store access.
    ///
    /// # Errors
    ///
    /// Return [`ImportError::Invalid`] with the first problem found.
    pub fn check(&self, limits: &ArchiveLimits) -> Result<(), ImportError> {
        check_bundle(self, limits).map(|_| ())
    }
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
    /// Entries already present with the same frame and all valid blobs.
    pub unchanged: usize,
    /// Live entries with the same frame whose missing or corrupt blobs the import restored.
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
    /// A store operation failed after the import started.
    Store {
        /// The store error.
        error: HostError,
        /// Entries that the import completed before the error. Each one is complete.
        completed: ImportReport,
    },
}

impl fmt::Display for ImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(reason) => formatter.write_str(reason),
            Self::Store { error, .. } => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<HostError> for ImportError {
    fn from(error: HostError) -> Self {
        Self::Store {
            error,
            completed: ImportReport::default(),
        }
    }
}

fn invalid(reason: impl Into<String>) -> ImportError {
    ImportError::Invalid(reason.into())
}

fn remote_invalid(error: RemoteError) -> ImportError {
    invalid(error.to_string())
}

/// A store frame that cannot be read becomes a store error at `path`.
fn frame_error(path: &Path) -> impl FnOnce(RemoteError) -> HostError + '_ {
    move |error| {
        io_at(
            io::Error::new(ErrorKind::InvalidData, error.to_string()),
            path,
        )
    }
}

/// Check the complete persisted probe payload schema.
///
/// A probe payload is `{"outcome": {...}, "checks": [...]}`. The check follows
/// the typed records of `rcmake-commands` field by field: the outcome fields, the
/// diagnostics, the artifact reference, and each journal check with its digest.
/// The `output`, `message`, and `relative_path` templates must be present. Only a
/// compiled outcome can name an artifact, and its blob is then the only blob of
/// the entry. An outcome without an artifact has no blobs. A parity test in
/// `rcmake-commands` keeps this check in agreement with the typed records.
pub fn probe_payload_valid(entry: &Entry<Value>) -> bool {
    probe_artifact(entry).is_ok_and(|artifact| match artifact {
        Some(blob) => entry.blobs.as_slice() == [blob],
        None => entry.blobs.is_empty(),
    })
}

/// Return the artifact blob of a probe outcome, or `None` if it keeps no artifact.
fn probe_artifact(entry: &Entry<Value>) -> Result<Option<Digest>, FieldError> {
    let payload = object(&entry.payload)?;
    deny_unknown(payload, &["outcome", "checks"])?;
    probe_checks(req_value(payload, "checks")?)?;
    let outcome = object(req_value(payload, "outcome")?)?;
    deny_unknown(
        outcome,
        &[
            "max_start_depth",
            "compiled",
            "exit_code",
            "output",
            "artifact",
            "diagnostics",
        ],
    )?;
    req::<u32>(outcome, "max_start_depth")?;
    req::<i32>(outcome, "exit_code")?;
    req_value(outcome, "output")?;
    for diagnostic in rcmake_json::array(req_value(outcome, "diagnostics")?)? {
        let diagnostic = object(diagnostic)?;
        deny_unknown(diagnostic, &["stream", "message"])?;
        match rcmake_json::str(req_value(diagnostic, "stream")?)? {
            "Stdout" | "Stderr" => {}
            other => {
                return Err(FieldError::invalid(format!(
                    "unknown diagnostic stream '{other}'"
                )));
            }
        }
        req_value(diagnostic, "message")?;
    }
    let compiled: bool = req(outcome, "compiled")?;
    let artifact = match req_value(outcome, "artifact")? {
        Value::Null => None,
        value => {
            let artifact = object(value)?;
            deny_unknown(artifact, &["blob", "relative_path", "mode"])?;
            req_value(artifact, "relative_path")?;
            req::<u32>(artifact, "mode")?;
            Some(req::<Digest>(artifact, "blob")?)
        }
    };
    if artifact.is_some() && !compiled {
        return Err(FieldError::invalid(
            "only a compiled outcome has an artifact",
        ));
    }
    Ok(artifact)
}

/// Return the two items of a 2-element array.
fn pair(value: &Value) -> Result<(&Value, &Value), FieldError> {
    match rcmake_json::array(value)? {
        [first, second] => Ok((first, second)),
        items => Err(FieldError::invalid(format!(
            "expected 2-element array, found {}",
            items.len()
        ))),
    }
}

/// Check the journal checks of a probe: a bounded array of `[check, digest]` pairs.
fn probe_checks(value: &Value) -> Result<(), FieldError> {
    let items = rcmake_json::array(value)?;
    if items.len() > MAX_PROBE_CHECKS {
        return Err(FieldError::invalid(format!(
            "journal check count {} exceeds limit of {MAX_PROBE_CHECKS}",
            items.len()
        )));
    }
    for item in items {
        let (check, digest) = pair(item)?;
        probe_check(check)?;
        Digest::from_json(digest)?;
    }
    Ok(())
}

/// Check one journal check: a unit variant name, or an object with one variant key.
fn probe_check(value: &Value) -> Result<(), FieldError> {
    let obj = match value {
        Value::String(_) => {
            return match rcmake_json::str(value)? {
                "Environment" | "Executable" | "WorkingDirectory" | "System" => Ok(()),
                other => Err(FieldError::invalid(format!(
                    "unknown journal check '{other}'"
                ))),
            };
        }
        Value::Object(obj) => obj,
        _ => return Err(FieldError::expected("string or object", value)),
    };
    let mut fields = obj.iter();
    let (Some((tag, payload)), None) = (fields.next(), fields.next()) else {
        return Err(FieldError::invalid(
            "expected single-key journal check object",
        ));
    };
    match tag.as_slice() {
        b"Metadata" => {
            let (path, follow) = pair(payload)?;
            path_from_json(path)?;
            rcmake_json::bool(follow)?;
        }
        b"Slice" => {
            let (path, range) = pair(payload)?;
            path_from_json(path)?;
            let range = object(range)?;
            deny_unknown(range, &["offset", "len"])?;
            req::<u64>(range, "offset")?;
            req::<Option<u64>>(range, "len")?;
        }
        b"Chunks"
        | b"DirectoryOrder"
        | b"OrderedDirectoryMissing"
        | b"FollowMissing"
        | b"Link"
        | b"Canonical" => {
            path_from_json(payload)?;
        }
        b"Request" => {
            u32::from_json(payload)?;
        }
        other => {
            return Err(FieldError::invalid(format!(
                "unknown journal check '{}'",
                String::from_utf8_lossy(other)
            )));
        }
    }
    Ok(())
}

/// One checked archive entry.
struct Checked<'a> {
    pin: ManifestEntry,
    frame: &'a [u8],
    trace: crate::Trace,
}

/// Running totals of an export against [`ArchiveLimits`].
struct ExportBudget<'a> {
    limits: &'a ArchiveLimits,
    /// Tar stream size without the manifest member.
    tar: u64,
    /// Member data size without the manifest.
    data: u64,
    /// Exact compact manifest JSON size.
    manifest: u64,
    members: usize,
    decoded: u64,
    entries: EntryBudget,
}

impl<'a> ExportBudget<'a> {
    fn new(limits: &'a ArchiveLimits) -> Self {
        let empty = CacheManifest::default()
            .to_json()
            .map_or(0, |json| json.len() as u64);
        Self {
            limits,
            tar: 2 * BLOCK,
            data: 0,
            manifest: empty,
            members: 1,
            decoded: 0,
            entries: EntryBudget::new(limits),
        }
    }

    /// Add one entry with its new blobs. Return `false` and change nothing if it does not fit.
    ///
    /// `frame` is the entry frame. Its declared decoded size counts against the
    /// entry budget that import applies.
    fn admit(&mut self, pin: &ManifestEntry, frame: &[u8], blobs: &[u64], decoded: u64) -> bool {
        let entry = frame.len() as u64;
        let mut entries = self.entries;
        if entries.charge(frame).is_err() {
            return false;
        }
        // Each record after the first adds one comma to the manifest array.
        let record = write_compact(&rcmake_json::ToJson::to_json(pin)).len() as u64
            + u64::from(self.members > 1);
        let manifest = self.manifest + record;
        let tar = blobs
            .iter()
            .fold(self.tar + tar_member(entry), |total, &blob| {
                total + tar_member(blob)
            });
        let data = blobs
            .iter()
            .fold(self.data + entry, |total, &blob| total + blob);
        let members = self.members + 1 + blobs.len();
        let decoded_total = self.decoded + decoded;
        let limits = self.limits;
        let fits = entry <= limits.member_bytes
            && blobs.iter().all(|&blob| blob <= limits.member_bytes)
            && manifest <= limits.manifest_bytes
            && tar + tar_member(manifest) <= limits.export_tar_bytes()
            && data + manifest <= limits.total_bytes
            && members <= limits.members
            && decoded_total <= limits.decoded_bytes;
        if fits {
            (self.tar, self.data, self.manifest) = (tar, data, manifest);
            (self.members, self.decoded) = (members, decoded_total);
            self.entries = entries;
        }
        fits
    }
}

impl Store<'_> {
    /// Read the verified probe entries and their blobs for an export.
    ///
    /// The read takes no lock, so collection can run at the same time. An
    /// entry is exported only if its frame, its probe payload, its current
    /// input trace, and all of its blobs are valid when they are read, and
    /// only if the archive stays inside `limits`. Other entries are counted
    /// in [`ProbeExport::skipped`].
    pub fn export_probes(&self, limits: &ArchiveLimits) -> Result<ProbeExport, HostError> {
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
        let mut budget = ExportBudget::new(limits);
        let mut decoded_sizes = BTreeMap::new();
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
            // Check the declared decoded size before the decompression.
            if EntryBudget::new(limits).charge(&encoded).is_err() {
                export.skipped.over_budget += 1;
                continue;
            }
            let Ok(entry) = decode_entry::<Value>(CacheEntryKind::Probe, key, &encoded, Some(&pin))
            else {
                export.skipped.invalid += 1;
                continue;
            };
            if !probe_payload_valid(&entry) {
                export.skipped.invalid += 1;
                continue;
            }
            if self.verify_inputs(&entry.trace) != VerifyOutcome::Hit {
                export.skipped.stale += 1;
                continue;
            }
            let mut blobs = Vec::new();
            let mut complete = true;
            let mut entry_decoded = 0usize;
            for &digest in &pin.blobs {
                if let Some(&size) = decoded_sizes.get(&digest) {
                    entry_decoded = entry_decoded.saturating_add(size);
                    continue;
                }
                let mut left = PROBE_BLOB_BYTES;
                let read = read_frame(self, &self.blob_path(digest), PROBE_BLOB_BYTES)
                    .ok()
                    .flatten()
                    .and_then(|frame| {
                        let size = decode_blob(&frame, digest, &mut left).ok()?;
                        Some((frame, size))
                    });
                let Some((frame, size)) = read else {
                    complete = false;
                    break;
                };
                entry_decoded = entry_decoded.saturating_add(size);
                blobs.push((digest, frame, size));
            }
            if !complete {
                export.skipped.missing_blob += 1;
                continue;
            }
            let new_decoded = blobs
                .iter()
                .fold(0u64, |total, (_, _, size)| total + *size as u64);
            let sizes: Vec<u64> = blobs
                .iter()
                .map(|(_, frame, _)| frame.len() as u64)
                .collect();
            if entry_decoded > PROBE_BLOB_BYTES
                || !budget.admit(&pin, &encoded, &sizes, new_decoded)
                || export.manifest.insert(pin).is_err()
            {
                export.skipped.over_budget += 1;
                continue;
            }
            export.entries.insert(key, encoded);
            for (digest, frame, size) in blobs {
                decoded_sizes.insert(digest, size);
                export.blobs.insert(digest, frame);
            }
        }
        Ok(export)
    }

    /// Check a complete archive, then admit its probe entries.
    ///
    /// Every check runs before the first write. A failed check returns
    /// [`ImportError::Invalid`] and leaves the store unchanged. Writes run under
    /// the collection lock, with blobs before the entry that references them.
    /// If a write fails, [`ImportError::Store`] counts the entries completed
    /// before it. Each completed entry is whole, and a repeated import finishes
    /// the rest. A valid local entry is never replaced.
    pub fn import_probes(
        &self,
        bundle: &ProbeBundle,
        trust: ImportTrust,
        limits: &ArchiveLimits,
    ) -> Result<ImportReport, ImportError> {
        let checked = check_bundle(bundle, limits)?;
        if !self.writable() {
            return Err(HostError::unsupported("The probe cache is not writable.").into());
        }
        let locked = self.lock_cache(IMPORT_WAIT)?;
        let mut report = ImportReport::default();
        for entry in &checked {
            #[cfg(test)]
            self.test_event(
                crate::TestEvent::ImportStep,
                &self.entry_path(Domain::ProbeV1, entry.pin.key),
            );
            let mut step = report;
            match self.admit(&locked, bundle, entry, trust, &mut step) {
                Ok(()) => report = step,
                Err(error) => {
                    return Err(ImportError::Store {
                        error,
                        completed: report,
                    });
                }
            }
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
    ) -> Result<(), HostError> {
        let key = entry.pin.key;
        let path = self.entry_path(Domain::ProbeV1, key);
        let live = read_frame(self, &path, frame::ENTRY_LIMIT).map_err(frame_error(&path))?;
        if let Some(live) = &live
            && live.as_slice() == entry.frame
        {
            // The live entry is an independent pin with the same frame.
            let unusable = self.unusable_blobs(&entry.pin.blobs)?;
            if unusable.is_empty() {
                report.unchanged += 1;
            } else if trust == ImportTrust::Trusted
                || self.verify_inputs(&entry.trace) == VerifyOutcome::Hit
            {
                report.blobs_published += self.publish_blobs(locked, bundle, &unusable)?;
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

    /// Return whether the live entry at `key` is a usable probe entry.
    ///
    /// It needs a valid probe payload, a matching input trace, and blobs with
    /// valid content. Call it under the collection lock.
    fn live_entry_valid(&self, key: Digest) -> Result<bool, HostError> {
        let path = self.entry_path(Domain::ProbeV1, key);
        match self.read_entry::<Entry<Value>>(Domain::ProbeV1, key, &path, false)? {
            CacheRead::Found(entry) => Ok(probe_payload_valid(&entry)
                && self.verify_inputs(&entry.trace) == VerifyOutcome::Hit
                && self.unusable_blobs(&entry.blobs)?.is_empty()),
            CacheRead::Missing | CacheRead::Invalid => Ok(false),
        }
    }

    /// Return the blobs whose files are missing or do not match their digests.
    ///
    /// The check reads each blob. Call it under the collection lock, so the
    /// result stays valid until the lock is released.
    fn unusable_blobs(&self, blobs: &[Digest]) -> Result<Vec<Digest>, HostError> {
        let mut unusable = Vec::new();
        for &digest in blobs {
            let read = self.read_blob(digest, &self.blob_path(digest), blob_limit(), false)?;
            if !matches!(read, CacheRead::Found(_)) {
                unusable.push(digest);
            }
        }
        Ok(unusable)
    }

    fn publish_blobs(
        &self,
        locked: &LockedStore<'_, '_>,
        bundle: &ProbeBundle,
        blobs: &[Digest],
    ) -> Result<usize, HostError> {
        let mut published = 0;
        for &digest in blobs {
            let path = self.blob_path(digest);
            let frame = bundle.blobs.get(&digest).ok_or_else(|| {
                io_at(
                    io::Error::new(
                        ErrorKind::InvalidData,
                        "A referenced blob is missing from the archive.",
                    ),
                    &path,
                )
            })?;
            if locked.put_blob(digest, &path, frame, blob_limit())? {
                published += 1;
            }
        }
        Ok(published)
    }

    /// Write an entry and its blobs as candidates. Return whether anything changed.
    ///
    /// An unchanged candidate gets a new time, with its blobs, so collection
    /// keeps them for the same period.
    fn stage_candidate(
        &self,
        locked: &LockedStore<'_, '_>,
        bundle: &ProbeBundle,
        entry: &Checked<'_>,
    ) -> Result<bool, HostError> {
        let mut changed = false;
        for &digest in &entry.pin.blobs {
            let path = self.candidate_blob_path(digest);
            let frame = bundle.blobs.get(&digest).ok_or_else(|| {
                io_at(
                    io::Error::new(
                        ErrorKind::InvalidData,
                        "A referenced blob is missing from the archive.",
                    ),
                    &path,
                )
            })?;
            if read_frame(self, &path, PROBE_BLOB_BYTES)
                .map_err(frame_error(&path))?
                .as_ref()
                != Some(frame)
            {
                locked.publish(&path, frame, true)?;
                changed = true;
            }
        }
        let path = self.candidate_entry_path(entry.pin.key);
        let current = read_frame(self, &path, frame::ENTRY_LIMIT).map_err(frame_error(&path))?;
        if current.as_deref() != Some(entry.frame) {
            locked.publish(&path, entry.frame, true)?;
            changed = true;
        }
        if !changed {
            self.touch_candidate(entry.pin.key, &entry.pin.blobs);
        }
        Ok(changed)
    }

    /// Give a candidate entry and its candidate blobs the current time.
    fn touch_candidate(&self, key: Digest, blobs: &[Digest]) {
        let now = self.host.now();
        let paths = blobs
            .iter()
            .map(|&digest| self.candidate_blob_path(digest))
            .chain([self.candidate_entry_path(key)]);
        for path in paths {
            let _ = self.host.set_times(&path, None, Some(now));
        }
    }

    /// Restore missing blobs of a live probe entry from a candidate with the same frame.
    ///
    /// The live entry is the pin. Under the collection lock, the live frame must
    /// still equal the candidate and describe `entry`, its probe payload must be
    /// valid, and its trace must match now. Each restored blob must match its
    /// digest. Return whether all referenced blobs are usable.
    pub(crate) fn recover_from_candidate<T>(&self, key: Digest, entry: &Entry<T>) -> bool {
        self.try_recover(key, entry).unwrap_or(false)
    }

    fn try_recover<T>(&self, key: Digest, entry: &Entry<T>) -> Result<bool, HostError> {
        let candidate_path = self.candidate_entry_path(key);
        if !self.writable()
            || entry.blobs.is_empty()
            || self.file_metadata(&candidate_path)?.is_none()
        {
            return Ok(false);
        }
        let locked = self.lock_cache(crate::PUBLISH_WAIT)?;
        let Ok(Some(candidate)) = read_frame(self, &candidate_path, frame::ENTRY_LIMIT) else {
            return Ok(false);
        };
        let live_path = self.entry_path(Domain::ProbeV1, key);
        let Ok(Some(live)) = read_frame(self, &live_path, frame::ENTRY_LIMIT) else {
            return Ok(false);
        };
        let pinned = (live == candidate)
            .then(|| decode_entry::<Value>(CacheEntryKind::Probe, key, &live, None).ok())
            .flatten();
        let Some(pinned) = pinned else {
            return Ok(false);
        };
        if pinned.trace != entry.trace
            || pinned.blobs != entry.blobs
            || !probe_payload_valid(&pinned)
            || self.verify_inputs(&pinned.trace) != VerifyOutcome::Hit
        {
            return Ok(false);
        }
        let mut frames = Vec::new();
        for digest in self.unusable_blobs(&pinned.blobs)? {
            let path = self.candidate_blob_path(digest);
            let Ok(Some(frame)) = read_frame(self, &path, PROBE_BLOB_BYTES) else {
                return Ok(false);
            };
            let mut budget = PROBE_BLOB_BYTES;
            if decode_blob(&frame, digest, &mut budget).is_err() {
                return Ok(false);
            }
            frames.push((digest, frame));
        }
        for (digest, frame) in frames {
            locked.put_blob(digest, &self.blob_path(digest), &frame, blob_limit())?;
        }
        self.touch_candidate(key, &pinned.blobs);
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
/// digest, record schema, probe payload, key, and the size limits.
fn check_bundle<'a>(
    bundle: &'a ProbeBundle,
    limits: &ArchiveLimits,
) -> Result<Vec<Checked<'a>>, ImportError> {
    if bundle.manifest.len() as u64 > limits.manifest_bytes {
        return Err(invalid("The archive manifest exceeds the size limit."));
    }
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
    let frames = bundle.entries.values().chain(bundle.blobs.values());
    let data = frames
        .clone()
        .fold(bundle.manifest.len() as u64, |total, frame| {
            total.saturating_add(frame.len() as u64)
        });
    if 1 + bundle.entries.len() + bundle.blobs.len() > limits.members
        || data > limits.total_bytes
        || frames
            .clone()
            .any(|frame| frame.len() as u64 > limits.member_bytes)
    {
        return Err(invalid("The archive members exceed the size limits."));
    }
    // Charge every entry before the first decompression, so that the decoded
    // entries and the traces that the import keeps stay inside one budget.
    let mut entry_budget = EntryBudget::new(limits);
    for frame in bundle.entries.values() {
        entry_budget.charge(frame)?;
    }
    let mut sizes = BTreeMap::new();
    let mut decoded = 0u64;
    for (&digest, frame) in &bundle.blobs {
        let mut budget = PROBE_BLOB_BYTES;
        let size = decode_blob(frame, digest, &mut budget)
            .map_err(|error| invalid(format!("Blob {digest}: {error}")))?;
        decoded = decoded.saturating_add(size as u64);
        if decoded > limits.decoded_bytes {
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
        if !probe_payload_valid(&entry) {
            return Err(invalid(format!(
                "Entry {}: the probe payload or its artifact is invalid.",
                pin.key
            )));
        }
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
