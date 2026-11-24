//! Validate, restore, and publish compile probe outcomes.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_eval::ProbeCacheMode;
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, ReadRange, WriteMode, path};
use rcmake_store::{Digest, Domain, Entry, InputStamp, Store, StoreConfig, StoreMode};
use serde::{Deserialize, Serialize};

use super::journal::{ImpureReason, Journal, JournalChecks};
use super::plan::{
    ArtifactRef, ProbeDiagnostic, ProbeEligibility, ProbeOutcome, ProbeRecipe, ReuseScope,
    compute_probe_key, environment_fingerprint,
};
use super::template::{Templated, round_trip_verify};

const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_DIAGNOSTICS: usize = 16_384;
static NEXT_ARTIFACT: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize, Serialize)]
struct CachedProbe {
    outcome: ProbeOutcome,
    checks: JournalChecks,
}

/// Compile result for the sequential evaluator, from either execution or lookup.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ObservedOutcome {
    /// Whether the build succeeded.
    pub(crate) compiled: bool,
    /// Build exit status for the configure log.
    pub(crate) exit_code: i32,
    /// Exact compile output for this call.
    pub(crate) output: Vec<u8>,
    /// Artifact data for this call, when available.
    pub(crate) artifact: Option<Artifact>,
    /// Rendered diagnostic bytes in emission order.
    pub(crate) diagnostics: Vec<ProbeDiagnostic<Vec<u8>>>,
}

/// Artifact bytes and permissions, retained through application of a cache hit.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Artifact {
    /// Exact executable or archive bytes.
    pub(crate) bytes: Vec<u8>,
    /// Location relative to the scratch directory.
    pub(crate) relative_path: PathBuf,
    /// File permission bits.
    pub(crate) mode: u32,
}

impl Artifact {
    /// Restore the artifact to the same relative path in private scratch space.
    pub(crate) fn materialize(&self, host: &dyn Host, scratch: &Path) -> HostResult<PathBuf> {
        if !scratch.is_absolute()
            || scratch
                .components()
                .any(|part| part == Component::ParentDir)
            || !is_relative_artifact(&self.relative_path)
        {
            return Err(HostError::unsupported("Invalid scratch artifact path."));
        }
        let file = scratch.join(&self.relative_path);
        let mut parent = scratch.to_owned();
        if host.metadata(scratch, Follow::No)?.kind != FileKind::Dir {
            return Err(HostError::unsupported(
                "Expected a private scratch directory.",
            ));
        }
        if let Some(relative_parent) = self.relative_path.parent() {
            for part in relative_parent.components() {
                parent.push(part.as_os_str());
                ensure_directory(host, &parent)?;
            }
        }
        let id = NEXT_ARTIFACT.fetch_add(1, Ordering::Relaxed);
        let staged = parent.join(format!(".rcmake-artifact-{}-{id:x}", host.system().pid));
        host.write(&staged, &self.bytes, WriteMode::CreateNew)?;
        if let Err(error) = host
            .set_permissions(&staged, self.mode)
            .and_then(|()| host.rename(&staged, &file))
        {
            let _ = host.remove_file(&staged);
            return Err(error);
        }
        Ok(file)
    }

    /// Restore the artifact in private scratch space and copy it to the caller's destination.
    pub(crate) fn copy_to(
        &self,
        host: &dyn Host,
        scratch: &Path,
        destination: &Path,
    ) -> HostResult<()> {
        let file = self.materialize(host, scratch)?;
        super::copy_artifact(host, Some(&file), destination)
    }
}

/// Return whether the path can name a file inside private scratch space.
pub(crate) fn is_relative_artifact(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Cache events counted across all probes that share this cache.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProbeCacheStats {
    /// Valid cached outcomes.
    pub(crate) hits: u64,
    /// Lookups without a complete, valid outcome.
    pub(crate) misses: u64,
    /// Calls that could not use the cache.
    pub(crate) bypassed: u64,
    /// Publication attempts rejected by the purity check.
    pub(crate) impure: u64,
    /// Cache hits that differed from a fresh run in verify mode.
    pub(crate) verify_mismatches: u64,
    /// Number of publication errors from the store.
    pub(crate) store_errors: u64,
}

/// Reason the cache skipped a publication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkipReason {
    /// The cache policy or the recipe disallowed publication.
    Ineligible,
    /// At least one input changed or an effect could not be verified.
    Inputs(Option<ImpureReason>),
    /// The result exceeded the storage limit or could not be relocated safely.
    Outcome,
    /// Another store operation prevented publication.
    Busy,
    /// The job was cancelled before publication.
    Cancelled,
}

/// Result of an optional cache write.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ProbePublishOutcome {
    /// The store has a valid result for this key.
    Published,
    /// The cache skipped the write.
    Skipped(SkipReason),
    /// An error occurred in the store. The probe result is still usable.
    StoreError(String),
}

/// Site and caller-visible results for a comparison in verify mode.
pub(crate) struct VerifyContext<'a> {
    /// CMake variable that receives the result.
    pub(crate) variable: &'a [u8],
    /// Call site file.
    pub(crate) file: &'a Path,
    /// One-based call site line.
    pub(crate) line: u32,
    /// Full probe key.
    pub(crate) key: Digest,
    /// Copy status from materializing the cached artifact, without writing the destination.
    pub(crate) cached_copy: Option<&'a [u8]>,
    /// Copy status from the single copy at the original call site.
    pub(crate) fresh_copy: Option<&'a [u8]>,
}

/// Fields that differ from a fresh result, with bounded diagnostic details.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct VerifyMismatchReport {
    /// CMake variable that receives the result.
    pub(crate) variable: String,
    /// Call site file.
    pub(crate) file: PathBuf,
    /// One-based call site line.
    pub(crate) line: u32,
    /// Full probe key.
    pub(crate) key: Digest,
    /// Details for each field that changed.
    pub(crate) differences: Vec<String>,
}

impl fmt::Display for VerifyMismatchReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Probe {} at {}:{} differs from cache entry {}.",
            self.variable,
            self.file.display(),
            self.line,
            self.key.to_hex()
        )?;
        for difference in &self.differences {
            writeln!(f, "  {difference}")?;
        }
        write!(
            f,
            "Rcmake used the fresh result. Use --no-probe-cache to bypass the cache. If you report this issue, include the probe key, Rcmake version, generator, and toolchain."
        )
    }
}

/// Compile probe cache policy and counters. The store owns atomic publication.
pub(crate) struct ProbeCache {
    mode: StoreMode,
    hits: AtomicU64,
    misses: AtomicU64,
    bypassed: AtomicU64,
    impure: AtomicU64,
    verify_mismatches: AtomicU64,
    store_errors: AtomicU64,
}

impl ProbeCache {
    /// Apply the evaluator's probe policy to store access.
    pub(crate) fn for_session(mode: ProbeCacheMode) -> Self {
        Self::new(match mode {
            ProbeCacheMode::Off => StoreMode::Off,
            ProbeCacheMode::ReadWrite => StoreMode::ReadWrite,
            ProbeCacheMode::ReadOnly => StoreMode::ReadOnly,
            ProbeCacheMode::Verify => StoreMode::Verify,
        })
    }

    /// Use the same mode as the backing store.
    pub(crate) fn new(mode: StoreMode) -> Self {
        Self {
            mode,
            hits: AtomicU64::default(),
            misses: AtomicU64::default(),
            bypassed: AtomicU64::default(),
            impure: AtomicU64::default(),
            verify_mismatches: AtomicU64::default(),
            store_errors: AtomicU64::default(),
        }
    }

    /// Return true when this cache can publish speculative results.
    pub(crate) fn writable(&self) -> bool {
        self.mode == StoreMode::ReadWrite
    }

    /// Look up a reusable result. Store errors and invalid data give a miss.
    ///
    /// Use the probe's filesystem view and a host that reads current inputs.
    /// Keep the verified snapshot stable through use. In verify mode,
    /// the caller executes the probe and applies the fresh result.
    pub(crate) fn lookup(
        &self,
        host: &dyn Host,
        config: &StoreConfig,
        recipe: &ProbeRecipe,
    ) -> Option<ObservedOutcome> {
        if self.mode == StoreMode::Off || recipe.eligibility != ProbeEligibility::Eligible {
            self.bypassed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let outcome = self.lookup_with_mode(host, config, recipe, self.mode);
        let counter = if outcome.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        outcome
    }

    /// Look up an outcome without changing the cache counters or its access time.
    pub(crate) fn lookup_uncounted(
        &self,
        host: &dyn Host,
        config: &StoreConfig,
        recipe: &ProbeRecipe,
    ) -> Option<ObservedOutcome> {
        let mode = if self.mode == StoreMode::Verify {
            StoreMode::Verify
        } else {
            StoreMode::ReadOnly
        };
        self.lookup_with_mode(host, config, recipe, mode)
    }

    fn lookup_with_mode(
        &self,
        host: &dyn Host,
        config: &StoreConfig,
        recipe: &ProbeRecipe,
        mode: StoreMode,
    ) -> Option<ObservedOutcome> {
        if self.mode == StoreMode::Off
            || config.mode == StoreMode::Off
            || recipe.eligibility != ProbeEligibility::Eligible
            || (recipe.try_run && recipe.scope != ReuseScope::ExactBindings)
            || recipe.environment != environment_fingerprint(&host.env_all())
        {
            return None;
        }
        let mut config = config.clone();
        config.mode = match (config.mode, mode) {
            (StoreMode::ReadWrite, mode) | (_, mode @ StoreMode::Verify) => mode,
            (mode, _) => mode,
        };
        let store = Store::new(host, config).ok()?;
        read_outcome(host, &store, recipe)
    }

    /// Publish a verified foreground result. Treat cache failures as optional.
    pub(crate) fn publish(
        &self,
        host: &dyn Host,
        config: &StoreConfig,
        recipe: &ProbeRecipe,
        outcome: &ObservedOutcome,
        journal: &Journal,
    ) -> ProbePublishOutcome {
        let status = self.publish_uncounted(host, config, recipe, outcome, journal, || false);
        match &status {
            ProbePublishOutcome::Skipped(SkipReason::Inputs(_)) => {
                self.impure.fetch_add(1, Ordering::Relaxed);
            }
            ProbePublishOutcome::Skipped(SkipReason::Outcome) => {
                self.bypassed.fetch_add(1, Ordering::Relaxed);
            }
            ProbePublishOutcome::StoreError(_) => {
                self.store_errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        status
    }

    /// Publish for a worker without changing foreground counters.
    pub(crate) fn publish_uncounted(
        &self,
        host: &dyn Host,
        config: &StoreConfig,
        recipe: &ProbeRecipe,
        outcome: &ObservedOutcome,
        journal: &Journal,
        cancelled: impl Fn() -> bool,
    ) -> ProbePublishOutcome {
        let publish = || -> HostResult<ProbePublishOutcome> {
            if !self.writable()
                || config.mode != StoreMode::ReadWrite
                || recipe.eligibility != ProbeEligibility::Eligible
                || (recipe.try_run && recipe.scope != ReuseScope::ExactBindings)
            {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Ineligible));
            }
            let inputs_changed =
                || ProbePublishOutcome::Skipped(SkipReason::Inputs(journal.impure()));
            let Some((trace, checks)) = journal.verified_inputs(host) else {
                return Ok(inputs_changed());
            };
            if cancelled() {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Cancelled));
            }
            if recipe.environment != environment_fingerprint(&host.env_all()) {
                return Ok(inputs_changed());
            }
            let bindings = recipe.bindings.clone().with_canonical(host);
            let Some(payload) = template_outcome(recipe, &bindings, outcome) else {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Outcome));
            };
            if !entry_fits(trace, checks, &payload) {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Outcome));
            }
            let key = compute_probe_key(recipe);
            let store = Store::new(host, config.clone())?;
            let markers = config.root.join("probe-generations");
            ensure_directory(host, &config.root)?;
            ensure_directory(host, &markers)?;
            let [shard, ..] = key.as_bytes();
            let marker = markers.join(format!("{shard:02x}"));
            // A bounded generation table lets Store replace a stale result
            // without using Store's private file layout. A changed supplemental
            // input invalidates this shard's entries until the next publication.
            let lock = marker.with_extension("lock");
            check_regular_or_missing(host, &lock)?;
            let _guard = match host.lock_file(&lock, Some(Duration::ZERO)) {
                Ok(guard) => guard,
                Err(error) if error.kind == ErrorKind::TimedOut => {
                    return Ok(ProbePublishOutcome::Skipped(SkipReason::Busy));
                }
                Err(error) => return Err(error),
            };
            let existing = store.get_entry::<CachedProbe>(Domain::ProbeV1, key)?;
            if let Some(entry) = &existing
                && entry.payload.checks.verify(host)
                && entry
                    .blobs
                    .iter()
                    .all(|digest| store.get_blob(*digest).is_ok_and(|blob| blob.is_some()))
            {
                return Ok(ProbePublishOutcome::Published);
            }
            check_regular_or_missing(host, &marker)?;
            let mut generation = match host.read(&marker, ReadRange::All { limit: 32 }) {
                Ok(bytes) => bytes,
                Err(error) if error.kind == ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error),
            };
            let blobs = match &outcome.artifact {
                Some(artifact) => vec![store.put_blob(&artifact.bytes)?],
                None => Vec::new(),
            };
            if cancelled() {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Cancelled));
            }
            if existing.is_some() || generation.len() != 32 {
                generation = Digest::hash(&generation).as_bytes().to_vec();
                let id = NEXT_ARTIFACT.fetch_add(1, Ordering::Relaxed);
                let staged = markers.join(format!(".generation-{}-{id:x}", host.system().pid));
                host.write(&staged, &generation, WriteMode::CreateNew)?;
                if let Err(error) = host.rename(&staged, &marker) {
                    let _ = host.remove_file(&staged);
                    return Err(error);
                }
            }
            let mut trace = trace.clone();
            trace.push(InputStamp::FileHash {
                path: marker,
                digest: Digest::hash(&generation),
            });
            if !entry_fits(&trace, checks, &payload) {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Outcome));
            }
            let entry = Entry {
                trace,
                blobs,
                payload: CachedProbe {
                    outcome: payload,
                    checks: checks.clone(),
                },
            };
            if recipe.environment != environment_fingerprint(&host.env_all())
                || journal.verified_inputs(host).is_none()
            {
                return Ok(inputs_changed());
            }
            if cancelled() {
                return Ok(ProbePublishOutcome::Skipped(SkipReason::Cancelled));
            }
            match store.put_entry(Domain::ProbeV1, key, &entry) {
                Ok(_) => {
                    let visible = store.get_entry::<CachedProbe>(Domain::ProbeV1, key)?;
                    if visible.is_some_and(|entry| entry.payload.checks.verify(host)) {
                        Ok(ProbePublishOutcome::Published)
                    } else {
                        Ok(ProbePublishOutcome::Skipped(SkipReason::Busy))
                    }
                }
                Err(error) => Err(error),
            }
        };
        publish().unwrap_or_else(|error| ProbePublishOutcome::StoreError(error.to_string()))
    }

    /// Compare a hit with a fresh result in verify mode.
    ///
    /// Apply the fresh result at the original call site even when they match.
    pub(crate) fn verify_mode(
        &self,
        site: VerifyContext<'_>,
        cached: &ObservedOutcome,
        fresh: &ObservedOutcome,
    ) -> Option<VerifyMismatchReport> {
        if self.mode != StoreMode::Verify {
            return None;
        }
        let mut differences = Vec::new();
        if cached.compiled != fresh.compiled {
            differences.push(format!(
                "compiled changed from {} to {}.",
                cached.compiled, fresh.compiled
            ));
        }
        if cached.exit_code != fresh.exit_code {
            differences.push(format!(
                "exit_code changed from {} to {}.",
                cached.exit_code, fresh.exit_code
            ));
        }
        let before = normalized_output(&cached.output);
        let after = normalized_output(&fresh.output);
        if before != after {
            let offset = before
                .iter()
                .zip(&after)
                .take_while(|(left, right)| left == right)
                .count();
            differences.push(format!(
                "output differs at byte {} after normalization ({} cached bytes, {} fresh bytes).",
                offset,
                before.len(),
                after.len()
            ));
        }
        if cached.artifact != fresh.artifact {
            let describe = |artifact: Option<&Artifact>| {
                artifact.map_or_else(
                    || "absent".to_owned(),
                    |artifact| {
                        format!(
                            "{} ({} bytes, mode {:o}, {})",
                            Digest::hash(&artifact.bytes).to_hex(),
                            artifact.bytes.len(),
                            artifact.mode,
                            artifact.relative_path.display()
                        )
                    },
                )
            };
            differences.push(format!(
                "artifact changed from {} to {}.",
                describe(cached.artifact.as_ref()),
                describe(fresh.artifact.as_ref())
            ));
        }
        if site.cached_copy != site.fresh_copy {
            differences.push(format!(
                "COPY_FILE status changed from {:?} to {:?}.",
                site.cached_copy.map(short_preview),
                site.fresh_copy.map(short_preview)
            ));
        }
        if !cached
            .diagnostics
            .iter()
            .map(|diag| (diag.stream, normalized_output(&diag.message)))
            .eq(fresh
                .diagnostics
                .iter()
                .map(|diag| (diag.stream, normalized_output(&diag.message))))
        {
            differences.push(format!(
                "diagnostics differ ({} cached messages, {} fresh messages).",
                cached.diagnostics.len(),
                fresh.diagnostics.len()
            ));
        }
        if differences.is_empty() {
            return None;
        }
        self.verify_mismatches.fetch_add(1, Ordering::Relaxed);
        Some(VerifyMismatchReport {
            variable: short_preview(site.variable),
            file: site.file.to_owned(),
            line: site.line,
            key: site.key,
            differences,
        })
    }

    /// Read the current cache counters.
    pub(crate) fn stats(&self) -> ProbeCacheStats {
        ProbeCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            bypassed: self.bypassed.load(Ordering::Relaxed),
            impure: self.impure.load(Ordering::Relaxed),
            verify_mismatches: self.verify_mismatches.load(Ordering::Relaxed),
            store_errors: self.store_errors.load(Ordering::Relaxed),
        }
    }
}

fn ensure_directory(host: &dyn Host, path: &Path) -> HostResult<()> {
    match host.metadata(path, Follow::No) {
        Ok(meta) if meta.kind == FileKind::Dir => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound => host.create_dir_all(path),
        Ok(_) => Err(HostError::unsupported("Expected a cache directory.")),
        Err(error) => Err(error),
    }
}

fn check_regular_or_missing(host: &dyn Host, path: &Path) -> HostResult<()> {
    match host.metadata(path, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File && meta.nlink <= 1 => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(HostError::unsupported("Expected a regular cache file.")),
        Err(error) => Err(error),
    }
}

fn short_preview(bytes: &[u8]) -> String {
    let prefix = bytes.get(..256).unwrap_or(bytes);
    String::from_utf8_lossy(prefix).into_owned()
}

fn normalized_output(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|&byte| byte == b'\n') {
        // Only normalize known build-tool elapsed time records.
        if line.starts_with(b"Total time: ") || line.starts_with(b"Elapsed time: ") {
            let time = line
                .iter()
                .position(|&byte| byte == b':')
                .and_then(|index| line.get(index + 1..))
                .map(<[u8]>::trim_ascii);
            if time.is_some_and(|time| {
                !time.is_empty()
                    && time.iter().all(|byte| {
                        byte.is_ascii_digit() || matches!(byte, b'.' | b' ' | b's' | b'm')
                    })
            }) {
                continue;
            }
        }
        for (index, word) in line.split_inclusive(u8::is_ascii_whitespace).enumerate() {
            let file = word.trim_ascii_end();
            let name = file
                .rsplit(|byte| matches!(byte, b'/' | b'\\'))
                .next()
                .unwrap_or(file);
            let temp_path = [
                b"/tmp/".as_slice(),
                b"/private/tmp/",
                b"/var/folders/",
                b"/private/var/folders/",
            ]
            .iter()
            .any(|prefix| file.starts_with(prefix));
            let temporary_object = name
                .strip_suffix(b".o")
                .and_then(|name| {
                    name.strip_prefix(b"cc-")
                        .or_else(|| name.strip_prefix(b"cc"))
                })
                .is_some_and(|id| {
                    (6..=16).contains(&id.len()) && id.iter().all(u8::is_ascii_alphanumeric)
                });
            if temp_path && temporary_object {
                output.extend_from_slice(b"<temporary object>");
                output.extend_from_slice(word.get(file.len()..).unwrap_or_default());
            } else if index == 0 && word == b"\r" {
                continue;
            } else {
                output.extend_from_slice(word);
            }
        }
    }
    output
}

fn entry_fits(trace: &[InputStamp], checks: &JournalChecks, outcome: &ProbeOutcome) -> bool {
    let path_size = |path: &Path| path.as_os_str().as_encoded_bytes().len().saturating_mul(6);
    let bytes = trace
        .iter()
        .fold(checks.encoded_size_bound(), |bytes, stamp| {
            let keys = match stamp {
                InputStamp::FileStat { path, .. }
                | InputStamp::FileHash { path, .. }
                | InputStamp::DirEntries { path, .. }
                | InputStamp::Missing { path } => path_size(path),
                InputStamp::EnvVar { name, .. } => name.len().saturating_mul(4),
                InputStamp::NegativeLookup {
                    root,
                    relative,
                    deepest,
                    ..
                } => path_size(root)
                    .saturating_add(path_size(relative))
                    .saturating_add(path_size(deepest)),
            };
            bytes.saturating_add(512).saturating_add(keys)
        });
    let bytes = bytes
        .saturating_add(8192)
        .saturating_add(outcome.output.encoded_size_bound());
    let bytes = outcome.diagnostics.iter().fold(bytes, |bytes, diagnostic| {
        bytes
            .saturating_add(64)
            .saturating_add(diagnostic.message.encoded_size_bound())
    });
    let bytes = outcome.artifact.as_ref().map_or(bytes, |artifact| {
        bytes
            .saturating_add(512)
            .saturating_add(artifact.relative_path.encoded_size_bound())
    });
    bytes <= MAX_ENTRY_BYTES
}

fn read_outcome(
    host: &dyn Host,
    store: &Store<'_>,
    recipe: &ProbeRecipe,
) -> Option<ObservedOutcome> {
    let entry = store
        .get_entry::<CachedProbe>(Domain::ProbeV1, compute_probe_key(recipe))
        .ok()??;
    if !entry.payload.checks.verify(host) {
        return None;
    }
    let bindings = recipe.bindings.clone().with_canonical(host);
    let outcome = entry.payload.outcome;
    if !entry_fits(&entry.trace, &entry.payload.checks, &outcome) {
        return None;
    }
    let artifact = match (&outcome.artifact, entry.blobs.as_slice()) {
        (None, []) => None,
        (Some(artifact), [blob]) if outcome.compiled && artifact.blob == *blob => {
            let bytes = store.get_blob(*blob).ok()??;
            if bytes.len() > MAX_ARTIFACT_BYTES {
                return None;
            }
            let relative_path = path::to_native(&artifact.relative_path.render(&bindings)?);
            if !is_relative_artifact(&relative_path) {
                return None;
            }
            Some(Artifact {
                bytes,
                relative_path,
                mode: artifact.mode,
            })
        }
        _ => return None,
    };
    if outcome.diagnostics.len() > MAX_DIAGNOSTICS {
        return None;
    }
    let output = outcome.output.render(&bindings)?;
    let mut remaining = MAX_ARTIFACT_BYTES.checked_sub(output.len())?;
    let mut diagnostics = Vec::with_capacity(outcome.diagnostics.len());
    for diagnostic in outcome.diagnostics {
        let message = diagnostic.message.render_bounded(&bindings, remaining)?;
        remaining -= message.len();
        diagnostics.push(ProbeDiagnostic {
            stream: diagnostic.stream,
            message,
        });
    }
    Some(ObservedOutcome {
        compiled: outcome.compiled,
        exit_code: outcome.exit_code,
        output,
        artifact,
        diagnostics,
    })
}

fn template_outcome(
    recipe: &ProbeRecipe,
    bindings: &super::template::PathBindings,
    outcome: &ObservedOutcome,
) -> Option<ProbeOutcome> {
    if outcome.diagnostics.len() > MAX_DIAGNOSTICS
        || outcome
            .diagnostics
            .iter()
            .fold(outcome.output.len(), |bytes, diagnostic| {
                bytes.saturating_add(diagnostic.message.len())
            })
            > MAX_ARTIFACT_BYTES
    {
        return None;
    }
    if recipe.scope == ReuseScope::VerifiedRelocation && !bindings.can_scan_aliases() {
        return None;
    }
    let capture = |bytes: &[u8]| {
        let template = match recipe.scope {
            ReuseScope::ExactBindings => Templated::literal(bytes)?,
            ReuseScope::VerifiedRelocation => {
                let template = Templated::capture(bytes, bindings)?;
                if template.has_leak(bindings) {
                    return None;
                }
                template
            }
        };
        round_trip_verify(&template, bindings, bytes).then_some(template)
    };
    let output = capture(&outcome.output)?;
    let diagnostics = outcome
        .diagnostics
        .iter()
        .map(|diagnostic| {
            Some(ProbeDiagnostic {
                stream: diagnostic.stream,
                message: capture(&diagnostic.message)?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let artifact = match &outcome.artifact {
        None => None,
        Some(artifact)
            if outcome.compiled
                && artifact.bytes.len() <= MAX_ARTIFACT_BYTES
                && is_relative_artifact(&artifact.relative_path)
                && (recipe.scope == ReuseScope::ExactBindings
                    || !bindings.has_leak(&artifact.bytes)) =>
        {
            Some(ArtifactRef {
                blob: Digest::hash(&artifact.bytes),
                relative_path: capture(&path::from_native(&artifact.relative_path))?,
                mode: artifact.mode,
            })
        }
        Some(_) => return None,
    };
    Some(ProbeOutcome {
        compiled: outcome.compiled,
        exit_code: outcome.exit_code,
        output,
        artifact,
        diagnostics,
    })
}

#[cfg(all(test, unix))]
#[path = "cache_tests.rs"]
/// Test fixtures shared by the cache and the worker pool.
pub(in super::super) mod tests;
