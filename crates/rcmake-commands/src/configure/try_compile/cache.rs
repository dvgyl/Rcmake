//! Validate, restore, and publish compile probe outcomes.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_host::{CopyOptions, Host, HostError, HostResult, WriteMode, path};
use rcmake_store::{Domain, Entry, Store, StoreMode};
use serde::{Deserialize, Serialize};

use super::journal::{Journal, JournalChecks};
use super::plan::{
    ArtifactRef, ProbeDiagnostic, ProbeEligibility, ProbeOutcome, ProbeRecipe, ReuseScope,
    compute_probe_key,
};
use super::template::{Templated, round_trip_verify};

const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;

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
        if !scratch.is_absolute() || !is_relative_artifact(&self.relative_path) {
            return Err(HostError::unsupported("Invalid scratch artifact path."));
        }
        let file = scratch.join(&self.relative_path);
        if let Some(parent) = file.parent() {
            host.create_dir_all(parent)?;
        }
        host.write(&file, &self.bytes, WriteMode::CreateNew)?;
        if let Err(error) = host.set_permissions(&file, self.mode) {
            let _ = host.remove_file(&file);
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
        if let Some(parent) = destination.parent() {
            host.create_dir_all(parent)?;
        }
        host.copy_file(
            &file,
            destination,
            CopyOptions {
                preserve_permissions: true,
                preserve_mtime: false,
            },
        )
    }
}

fn is_relative_artifact(path: &Path) -> bool {
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
}

/// Compile probe cache policy and counters. The store owns atomic publication.
pub(crate) struct ProbeCache {
    mode: StoreMode,
    hits: AtomicU64,
    misses: AtomicU64,
    bypassed: AtomicU64,
    impure: AtomicU64,
    verify_mismatches: AtomicU64,
}

impl ProbeCache {
    /// Use the same mode as the backing store.
    pub(crate) fn new(mode: StoreMode) -> Self {
        Self {
            mode,
            hits: AtomicU64::default(),
            misses: AtomicU64::default(),
            bypassed: AtomicU64::default(),
            impure: AtomicU64::default(),
            verify_mismatches: AtomicU64::default(),
        }
    }

    /// Return true when this cache can publish speculative results.
    pub(crate) fn writable(&self) -> bool {
        self.mode == StoreMode::ReadWrite
    }

    /// Look up a reusable result. Store errors and invalid data give a miss.
    ///
    /// The caller keeps the input snapshot stable through use. In verify mode,
    /// the caller executes the probe and applies the fresh result.
    pub(crate) fn lookup(
        &self,
        host: &dyn Host,
        store: &Store<'_>,
        recipe: &ProbeRecipe,
    ) -> Option<ObservedOutcome> {
        if self.mode == StoreMode::Off || recipe.eligibility != ProbeEligibility::Eligible {
            self.bypassed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let outcome = read_outcome(host, store, recipe);
        let counter = if outcome.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        outcome
    }

    /// In read-write mode, publish a result with validated inputs and exact output.
    pub(crate) fn publish(
        &self,
        host: &dyn Host,
        store: &Store<'_>,
        recipe: &ProbeRecipe,
        outcome: &ObservedOutcome,
        journal: &Journal,
    ) -> HostResult<()> {
        if !self.writable() || recipe.eligibility != ProbeEligibility::Eligible {
            return Ok(());
        }
        let Some((trace, checks)) = journal.verified_inputs(host) else {
            self.impure.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let Some(outcome) = template_outcome(store, recipe, outcome)? else {
            self.bypassed.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        let blobs = outcome
            .artifact
            .iter()
            .map(|artifact| artifact.blob)
            .collect();
        store.put_entry(
            Domain::ProbeV1,
            compute_probe_key(recipe),
            &Entry {
                trace: trace.clone(),
                blobs,
                payload: CachedProbe {
                    outcome,
                    checks: checks.clone(),
                },
            },
        )
    }

    /// Compare a hit with a fresh result when the store is in verify mode.
    ///
    /// Apply the fresh result at the original call site even when they match.
    pub(crate) fn verify_mode(&self, cached: &ObservedOutcome, fresh: &ObservedOutcome) -> bool {
        if self.mode != StoreMode::Verify {
            return true;
        }
        let matches = cached == fresh;
        if !matches {
            self.verify_mismatches.fetch_add(1, Ordering::Relaxed);
        }
        matches
    }

    /// Read the current cache counters.
    pub(crate) fn stats(&self) -> ProbeCacheStats {
        ProbeCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            bypassed: self.bypassed.load(Ordering::Relaxed),
            impure: self.impure.load(Ordering::Relaxed),
            verify_mismatches: self.verify_mismatches.load(Ordering::Relaxed),
        }
    }
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
    let outcome = entry.payload.outcome;
    let artifact = match (&outcome.artifact, entry.blobs.as_slice()) {
        (None, []) => None,
        (Some(artifact), [blob]) if outcome.compiled && artifact.blob == *blob => {
            let bytes = store.get_blob(*blob).ok()??;
            if bytes.len() > MAX_ARTIFACT_BYTES {
                return None;
            }
            let relative_path = path::to_native(&artifact.relative_path.render(&recipe.bindings)?);
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
    Some(ObservedOutcome {
        compiled: outcome.compiled,
        exit_code: outcome.exit_code,
        output: outcome.output.render(&recipe.bindings)?,
        artifact,
        diagnostics: outcome
            .diagnostics
            .into_iter()
            .map(|diagnostic| {
                Some(ProbeDiagnostic {
                    stream: diagnostic.stream,
                    message: diagnostic.message.render(&recipe.bindings)?,
                })
            })
            .collect::<Option<Vec<_>>>()?,
    })
}

fn template_outcome(
    store: &Store<'_>,
    recipe: &ProbeRecipe,
    outcome: &ObservedOutcome,
) -> HostResult<Option<ProbeOutcome>> {
    let capture = |bytes: &[u8]| {
        let template = match recipe.scope {
            ReuseScope::ExactBindings => Templated::literal(bytes)?,
            ReuseScope::VerifiedRelocation => Templated::capture(bytes, &recipe.bindings)?,
        };
        round_trip_verify(&template, &recipe.bindings, bytes).then_some(template)
    };
    let Some(output) = capture(&outcome.output) else {
        return Ok(None);
    };
    let Some(diagnostics) = outcome
        .diagnostics
        .iter()
        .map(|diagnostic| {
            Some(ProbeDiagnostic {
                stream: diagnostic.stream,
                message: capture(&diagnostic.message)?,
            })
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let artifact = match &outcome.artifact {
        None => None,
        Some(artifact)
            if outcome.compiled
                && artifact.bytes.len() <= MAX_ARTIFACT_BYTES
                && is_relative_artifact(&artifact.relative_path) =>
        {
            let Some(relative_path) = capture(&path::from_native(&artifact.relative_path)) else {
                return Ok(None);
            };
            Some(ArtifactRef {
                blob: store.put_blob(&artifact.bytes)?,
                relative_path,
                mode: artifact.mode,
            })
        }
        Some(_) => return Ok(None),
    };
    Ok(Some(ProbeOutcome {
        compiled: outcome.compiled,
        exit_code: outcome.exit_code,
        output,
        artifact,
        diagnostics,
    }))
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use std::time::Duration;

    use rcmake_host::{
        CommandSpec, Follow, Input, Output, ProcessResult, PureHost, ReadRange, RunOutput,
        RunRequest,
    };
    use rcmake_store::{Digest, InputStamp, StoreConfig, file_digest};

    use super::super::journal::{ImpureReason, JournalHost, JournalMode};
    use super::super::plan::{Admission, DiagnosticStream, RcmakeBuildId, environment_fingerprint};
    use super::super::template::PathBindings;
    use super::*;

    /// Create an eligible recipe for a test probe.
    pub(crate) fn recipe(host: &dyn Host, root: &Path) -> HostResult<ProbeRecipe> {
        let eligibility = Admission {
            store_mode: StoreMode::ReadWrite,
            debug_try_compile: false,
            trace: false,
            profiling: false,
            sources_signature: true,
            private_scratch: true,
            verified_execution: true,
        }
        .eligibility();
        let digest = Digest::hash(b"test");
        let bindings = PathBindings::new(
            &root.join("scratch"),
            &root.join("build"),
            &root.join("source"),
            Path::new("/work/cmake"),
        )
        .and_then(|bindings| bindings.with_target(b"cmTC_1"))
        .ok_or_else(|| HostError::unsupported("Missing path bindings."))?;
        Ok(ProbeRecipe {
            eligibility,
            rcmake: RcmakeBuildId(digest),
            cmake_root: digest,
            host: digest,
            generator: digest,
            files: BTreeMap::new(),
            tools: BTreeMap::new(),
            flags: Vec::new(),
            definitions: Vec::new(),
            link_libraries: Vec::new(),
            target: "arm64".into(),
            sysroot: "/sdk".into(),
            seed: digest,
            imports: digest,
            environment: environment_fingerprint(&host.env_all()),
            scope: ReuseScope::VerifiedRelocation,
            bindings,
        })
    }

    /// Return an example successful test probe.
    pub(crate) fn success() -> ObservedOutcome {
        ObservedOutcome {
            compiled: true,
            exit_code: 0,
            output: b"/work/scratch/cmTC_1".to_vec(),
            artifact: Some(Artifact {
                bytes: b"test executable".to_vec(),
                relative_path: "bin/cmTC_1".into(),
                mode: 0o755,
            }),
            diagnostics: vec![ProbeDiagnostic {
                stream: DiagnosticStream::Stderr,
                message: b"/work/source/probe.c: warning".to_vec(),
            }],
        }
    }

    #[test]
    fn verifies_inputs_and_materializes_the_copy_file_blob() -> HostResult<()> {
        let host = PureHost::default();
        let store = Store::new(&host, StoreConfig::new("/cache".into()))?;
        let cache = ProbeCache::new(StoreMode::ReadWrite);
        let scratch = Path::new("/work/scratch");
        let header = Path::new("/work/header.h");
        let compiler = Path::new("/work/compiler");
        host.create_dir_all(scratch)?;
        host.seed_file(header, b"old")?;
        host.seed_file(compiler, b"cc1")?;
        host.env_set(b"LANG", Some(b"C"));
        let mut original = recipe(&host, Path::new("/work"))?;
        original
            .tools
            .insert("cc".into(), file_digest(&host, compiler)?);
        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
        journal.record_dependency(header)?;
        journal.record_dependency(compiler)?;
        journal.env_get(b"LANG");
        let journal = journal.finish();
        assert!(cache.lookup(&host, &store, &original).is_none());
        cache.publish(&host, &store, &original, &success(), &journal)?;

        let mut relocated = recipe(&host, Path::new("/other"))?;
        relocated.tools = original.tools.clone();
        let hit = cache
            .lookup(&host, &store, &relocated)
            .ok_or_else(|| HostError::unsupported("Expected a probe cache hit."))?;
        assert_eq!(hit.output, b"/other/scratch/cmTC_1");
        assert_eq!(
            hit.diagnostics.first().map(|diag| diag.message.as_slice()),
            Some(b"/other/source/probe.c: warning".as_slice())
        );
        let artifact = hit
            .artifact
            .ok_or_else(|| HostError::unsupported("Missing probe artifact."))?;
        assert_eq!(artifact.relative_path, Path::new("bin/cmTC_1"));
        let destination = Path::new("/work/copied/test");
        artifact.copy_to(&host, scratch, destination)?;
        assert_eq!(
            host.read(destination, ReadRange::All { limit: 32 })?,
            artifact.bytes
        );
        assert_eq!(host.metadata(destination, Follow::Yes)?.mode & 0o777, 0o755);

        host.env_set(b"LANG", Some(b"fr_FR"));
        assert!(cache.lookup(&host, &store, &original).is_none());
        host.env_set(b"LANG", Some(b"C"));
        host.seed_file(header, b"new")?;
        assert!(cache.lookup(&host, &store, &original).is_none());
        host.seed_file(header, b"old")?;
        host.seed_file(compiler, b"cc2")?;
        assert!(cache.lookup(&host, &store, &original).is_none());
        assert_eq!(
            cache.stats(),
            ProbeCacheStats {
                hits: 1,
                misses: 4,
                ..ProbeCacheStats::default()
            }
        );
        Ok(())
    }

    #[test]
    fn enforces_bypass_purity_and_verify_mode() -> HostResult<()> {
        let host = PureHost::default();
        let store = Store::new(&host, StoreConfig::new("/cache".into()))?;
        let scratch = Path::new("/work/scratch");
        host.create_dir_all(scratch)?;
        let mut recipe = recipe(&host, Path::new("/work"))?;
        recipe.scope = ReuseScope::ExactBindings;
        let cache = ProbeCache::new(StoreMode::ReadWrite);
        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
        journal.env_set(b"LC_ALL", Some(b"C"));
        cache.publish(&host, &store, &recipe, &success(), &journal.finish())?;
        assert!(cache.lookup(&host, &store, &recipe).is_none());
        assert_eq!(cache.stats().impure, 1);

        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?.finish();
        cache.publish(&host, &store, &recipe, &success(), &journal)?;
        assert_eq!(cache.lookup(&host, &store, &recipe), Some(success()));
        recipe.eligibility = ProbeEligibility::Bypass(super::super::plan::BypassReason::Trace);
        assert!(cache.lookup(&host, &store, &recipe).is_none());
        assert_eq!(cache.stats().bypassed, 1);

        let verify = ProbeCache::new(StoreMode::Verify);
        let mut fresh = success();
        fresh.output.push(b'!');
        assert!(!verify.verify_mode(&success(), &fresh));
        assert_eq!(verify.stats().verify_mismatches, 1);
        Ok(())
    }

    #[test]
    fn keys_preserve_argument_and_environment_boundaries() -> HostResult<()> {
        let host = PureHost::default();
        let mut original = recipe(&host, Path::new("/work"))?;
        let initial_key = compute_probe_key(&original);
        host.env_set(b"LC_ALL", Some(b""));
        let with_empty_locale = recipe(&host, Path::new("/work"))?;
        assert_ne!(compute_probe_key(&with_empty_locale), initial_key);

        original.flags = vec!["-Idir".into(), "-DSET".into()];
        let ordered_key = compute_probe_key(&original);
        original.flags.reverse();
        assert_ne!(compute_probe_key(&original), ordered_key);
        original.flags = vec!["-Idir-DSET".into()];
        assert_ne!(compute_probe_key(&original), ordered_key);
        Ok(())
    }
    #[test]
    fn rejects_unverified_process_before_it_starts() -> HostResult<()> {
        let host = PureHost::default();
        let scratch = Path::new("/work/scratch");
        host.create_dir_all(scratch)?;
        let journal = JournalHost::new(&host, scratch, JournalMode::Speculate, None)?;
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/work/compiler".into(),
                args: Vec::new(),
            }],
            cwd: scratch.to_owned(),
            env: Vec::new(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(1)),
            tee: false,
        };
        assert!(journal.run(&request, &mut |_, _| {}).is_err());
        let journal = journal.finish();
        assert_eq!(journal.processes(), &[request]);
        assert_eq!(journal.impure(), Some(ImpureReason::UnverifiedProcess));
        Ok(())
    }

    #[test]
    fn certifies_the_exact_process_and_rechecks_its_executable() -> HostResult<()> {
        let host = PureHost::default();
        let scratch = Path::new("/work/scratch");
        let compiler = Path::new("/work/compiler");
        host.create_dir_all(scratch)?;
        host.seed_file(compiler, b"compiler-v1")?;
        host.set_command_handler(|_, _, _| {
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(1)],
                stderr: b"invalid option".to_vec(),
                ..RunOutput::default()
            })
        })?;
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/work/compiler".into(),
                args: vec!["-invalid".into()],
            }],
            cwd: scratch.to_owned(),
            env: vec![("LC_ALL".into(), "C".into())],
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(1)),
            tee: false,
        };
        let verifier = |observed: &RunRequest| (observed == &request).then(Vec::new);
        let journal = JournalHost::new(&host, scratch, JournalMode::Speculate, Some(&verifier))?;
        let result = journal.run(&request, &mut |_, _| {})?;
        assert_eq!(result.results, [ProcessResult::Exited(1)]);
        let journal = journal.finish();
        assert_eq!(journal.processes(), &[request]);
        assert!(journal.verified_inputs(&host).is_some());
        host.seed_file(compiler, b"compiler-v2")?;
        assert!(journal.verified_inputs(&host).is_none());
        Ok(())
    }

    #[test]
    fn missing_nested_header_invalidates_when_a_shadow_appears() -> HostResult<()> {
        let host = PureHost::default();
        let scratch = Path::new("/work/scratch");
        let root = Path::new("/work/include");
        let relative = Path::new("sub/header.h");
        host.create_dir_all(scratch)?;
        host.create_dir_all(&root.join("sub"))?;
        let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
        assert!(!journal.record_search(root, relative)?);
        let journal = journal.finish();
        assert!(matches!(
            journal
                .verified_inputs(&host)
                .map(|(trace, _)| trace.as_slice()),
            Some([InputStamp::NegativeLookup { .. }])
        ));
        host.seed_file(&root.join(relative), b"shadow")?;
        assert!(journal.verified_inputs(&host).is_none());
        Ok(())
    }
}
