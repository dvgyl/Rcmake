use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use rcmake_host::{Host, PureHost};
use rcmake_json::ToJson;

use super::*;
use crate::remote::local_probe_pin;
use crate::{InputStamp, PublishOutcome, StoreConfig, TestEvent, TestHook};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn host() -> PureHost {
    let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
    PureHost::new(root.clone(), root.join("cmake"))
}

const LIMITS: ArchiveLimits = ArchiveLimits::DEFAULT;

/// A probe payload with the store fields. `output` names the probe.
fn payload(name: &str, artifact: Option<Digest>) -> Value {
    let artifact = artifact.map_or(Value::Null, |blob| {
        Value::object_from([
            ("blob", blob.to_json()),
            ("relative_path", "a.out".to_json()),
            ("mode", 0o755u32.to_json()),
        ])
    });
    Value::object_from([
        (
            "outcome",
            Value::object_from([
                ("max_start_depth", 0u32.to_json()),
                ("compiled", (!matches!(artifact, Value::Null)).to_json()),
                ("exit_code", 0i32.to_json()),
                ("output", name.to_json()),
                ("artifact", artifact),
                ("diagnostics", Value::Array(Vec::new())),
            ]),
        ),
        ("checks", Value::Array(Vec::new())),
    ])
}

fn name_of(payload: &Value) -> Option<String> {
    let outcome = object(payload)
        .ok()
        .and_then(|obj| req_value(obj, "outcome").ok())?;
    req::<String>(object(outcome).ok()?, "output").ok()
}

fn open<'a>(host: &'a PureHost, name: &str) -> Result<Store<'a>, HostError> {
    Store::new(host, StoreConfig::new(host.initial_cwd().join(name)))
}

/// Publish a probe entry whose trace depends on a compiler file.
fn probe(
    store: &Store<'_>,
    host: &PureHost,
    name: &str,
    output: &[u8],
) -> Result<Digest, Box<dyn std::error::Error>> {
    let compiler = host.initial_cwd().join("bin/cc");
    if host.metadata(&compiler, rcmake_host::Follow::No).is_err() {
        host.seed_file(&compiler, b"compiler v1")?;
    }
    let blob = store.put_blob(output)?;
    let key = Domain::ProbeV1.key(name.as_bytes());
    let entry = Entry {
        trace: vec![InputStamp::FileHash {
            path: compiler.clone(),
            digest: crate::file_digest(host, &compiler)?,
        }],
        blobs: vec![blob],
        payload: payload(name, Some(blob)),
    };
    assert_eq!(
        store.put_entry(Domain::ProbeV1, key, &entry)?,
        PublishOutcome::Published
    );
    Ok(key)
}

fn bundle(export: &ProbeExport) -> Result<ProbeBundle, RemoteError> {
    Ok(ProbeBundle {
        manifest: export.manifest.to_json()?,
        entries: export.entries.clone(),
        blobs: export.blobs.clone(),
    })
}

fn hit(store: &Store<'_>, key: Digest) -> Result<Option<String>, HostError> {
    Ok(store
        .get_entry::<Value>(Domain::ProbeV1, key)?
        .and_then(|entry| name_of(&entry.payload)))
}

fn with_hook<'a>(mut store: Store<'a>, hook: &'a TestHook<'a>) -> Store<'a> {
    store.test_hook = Some(hook);
    store
}

#[test]
fn trusted_round_trip_restores_a_cold_store_and_misses_after_a_compiler_change() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let first = probe(&source, &host, "first", b"output one")?;
    let second = probe(&source, &host, "second", b"output two")?;
    let fetch = Domain::FetchV1.key(b"fetch");
    source.put_entry(
        Domain::FetchV1,
        fetch,
        &Entry {
            trace: Vec::new(),
            blobs: Vec::new(),
            payload: 1u32,
        },
    )?;
    let export = source.export_probes(&LIMITS)?;
    assert_eq!(export.entries.len(), 2);
    assert_eq!(export.blobs.len(), 2);
    assert_eq!(export.skipped, ExportSkips::default());

    let consumer = open(&host, "consumer")?;
    let report = consumer.import_probes(&bundle(&export)?, ImportTrust::Trusted, &LIMITS)?;
    assert_eq!((report.entries_published, report.blobs_published), (2, 2));
    assert_eq!(hit(&consumer, first)?.as_deref(), Some("first"));
    assert_eq!(hit(&consumer, second)?.as_deref(), Some("second"));
    assert!(consumer.get_entry::<u32>(Domain::FetchV1, fetch)?.is_none());

    let again = consumer.import_probes(&bundle(&export)?, ImportTrust::Trusted, &LIMITS)?;
    assert_eq!(
        again,
        ImportReport {
            unchanged: 2,
            ..ImportReport::default()
        }
    );

    host.write(
        &host.initial_cwd().join("bin/cc"),
        b"compiler v2",
        rcmake_host::WriteMode::Truncate,
    )?;
    assert_eq!(hit(&consumer, first)?, None);
    Ok(())
}

#[test]
fn export_skips_stale_entries_and_entries_with_missing_blobs() -> TestResult {
    let host = host();
    let store = open(&host, "cache")?;
    let kept = probe(&store, &host, "kept", b"kept")?;
    let lost = probe(&store, &host, "lost", b"lost")?;
    host.remove_file(&store.blob_path(Digest::hash(b"lost")))?;
    let stale_input = host.initial_cwd().join("input.h");
    host.seed_file(&stale_input, b"one")?;
    let stale = Domain::ProbeV1.key(b"stale");
    store.put_entry(
        Domain::ProbeV1,
        stale,
        &Entry {
            trace: vec![InputStamp::FileHash {
                path: stale_input.clone(),
                digest: crate::file_digest(&host, &stale_input)?,
            }],
            blobs: Vec::new(),
            payload: payload("stale", None),
        },
    )?;
    host.write(&stale_input, b"two", rcmake_host::WriteMode::Truncate)?;
    let export = store.export_probes(&LIMITS)?;
    assert_eq!(export.entries.keys().copied().collect::<Vec<_>>(), [kept]);
    assert_eq!(
        export.skipped,
        ExportSkips {
            stale: 1,
            missing_blob: 1,
            ..ExportSkips::default()
        }
    );
    assert!(!export.entries.contains_key(&lost));
    Ok(())
}

#[test]
fn untrusted_candidates_cannot_establish_a_pin_or_hit() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;

    let consumer = open(&host, "consumer")?;
    let report = consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    assert_eq!(
        report,
        ImportReport {
            staged: 1,
            ..ImportReport::default()
        }
    );
    assert_eq!(hit(&consumer, key)?, None);
    assert!(local_probe_pin::<Value>(&consumer, key)?.is_none());
    assert_eq!(consumer.stats()?.entries, 0);
    assert_eq!(consumer.stats()?.candidates, 2);

    let again = consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    assert_eq!(
        again,
        ImportReport {
            unchanged: 1,
            ..ImportReport::default()
        }
    );
    Ok(())
}

#[test]
fn a_candidate_restores_blobs_only_for_a_matching_local_pin() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;

    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    // A local measurement establishes the pin, then its blob is lost.
    assert_eq!(probe(&consumer, &host, "probe", b"artifact")?, key);
    host.remove_file(&consumer.blob_path(Digest::hash(b"artifact")))?;
    assert_eq!(hit(&consumer, key)?.as_deref(), Some("probe"));
    assert!(consumer.get_blob(Digest::hash(b"artifact"))?.is_some());

    // The same recovery runs at import time for an existing pin.
    let other = open(&host, "other")?;
    probe(&other, &host, "probe", b"artifact")?;
    host.remove_file(&other.blob_path(Digest::hash(b"artifact")))?;
    let report = other.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    assert_eq!(
        report,
        ImportReport {
            recovered: 1,
            blobs_published: 1,
            ..ImportReport::default()
        }
    );
    assert_eq!(hit(&other, key)?.as_deref(), Some("probe"));
    Ok(())
}

#[test]
fn existing_valid_local_entries_win() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"remote artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;

    let consumer = open(&host, "consumer")?;
    let blob = consumer.put_blob(b"local artifact")?;
    let local = Entry {
        trace: Vec::new(),
        blobs: vec![blob],
        payload: payload("local", Some(blob)),
    };
    consumer.put_entry(Domain::ProbeV1, key, &local)?;
    for trust in [ImportTrust::Trusted, ImportTrust::Untrusted] {
        let report = consumer.import_probes(&bundle, trust, &LIMITS)?;
        assert_eq!(
            report,
            ImportReport {
                conflicts: 1,
                ..ImportReport::default()
            }
        );
        assert_eq!(hit(&consumer, key)?.as_deref(), Some("local"));
    }
    Ok(())
}

#[test]
fn malformed_archives_are_rejected_without_changes() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let good = bundle(&source.export_probes(&LIMITS)?)?;
    let blob = Digest::hash(b"artifact");

    let mut wrong_digest = good.clone();
    let manifest = String::from_utf8(good.manifest.clone())?;
    let frame_sha = Digest::hash(good.entries.get(&key).ok_or("entry")?).to_hex();
    wrong_digest.manifest = manifest.replace(&frame_sha, &"0".repeat(64)).into_bytes();
    let mut missing_blob = good.clone();
    missing_blob.blobs.clear();
    let mut extra_blob = good.clone();
    extra_blob.blobs.insert(Digest::hash(b"extra"), Vec::new());
    let mut corrupt_blob = good.clone();
    if let Some(last) = corrupt_blob
        .blobs
        .get_mut(&blob)
        .and_then(|frame| frame.last_mut())
    {
        *last ^= 1;
    }
    let mut fetch = good.clone();
    fetch.manifest = manifest.replace("\"probe\"", "\"fetch\"").into_bytes();
    let mut bad_json = good.clone();
    bad_json.manifest = b"{".to_vec();

    let consumer = open(&host, "consumer")?;
    for (bundle, needle) in [
        (wrong_digest, "digest"),
        (missing_blob, "blobs do not match"),
        (extra_blob, "blobs do not match"),
        (corrupt_blob, "Blob"),
        (fetch, "other than probe"),
        (bad_json, "manifest"),
    ] {
        for trust in [ImportTrust::Trusted, ImportTrust::Untrusted] {
            match consumer.import_probes(&bundle, trust, &LIMITS) {
                Err(ImportError::Invalid(reason)) => assert!(reason.contains(needle), "{reason}"),
                other => panic!("expected {needle:?} rejection, got {other:?}"),
            }
        }
    }
    assert!(
        host.metadata(consumer.root(), rcmake_host::Follow::No)
            .is_err()
    );
    Ok(())
}

#[test]
fn collection_cannot_sweep_staged_blobs_before_activation() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;

    let root = host.initial_cwd().join("consumer");
    let mut config = StoreConfig::new(root.clone());
    config.blob_grace = Duration::ZERO;
    config.max_bytes = 0;
    let collector = Store::new(&host, config)?;
    let swept = AtomicUsize::new(0);
    let hook = |event: TestEvent, _: &Path| {
        if event == TestEvent::ImportBlobsStaged {
            let report = collector.gc().unwrap_or_default();
            swept.fetch_add(report.blobs_removed + 1, Ordering::Relaxed);
        }
    };
    let consumer = with_hook(open(&host, "consumer")?, &hook);
    let report = consumer.import_probes(&bundle, ImportTrust::Trusted, &LIMITS)?;
    assert_eq!(report.entries_published, 1);
    assert_eq!(
        swept.load(Ordering::Relaxed),
        1,
        "collection ran and removed nothing"
    );
    assert_eq!(hit(&consumer, key)?.as_deref(), Some("probe"));
    Ok(())
}

#[test]
fn competing_imports_publish_each_entry_once() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let keys = [
        probe(&source, &host, "one", b"one")?,
        probe(&source, &host, "two", b"two")?,
    ];
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let reports = thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    open(&host, "consumer")
                        .map_err(ImportError::from)
                        .and_then(|store| {
                            store.import_probes(&bundle, ImportTrust::Trusted, &LIMITS)
                        })
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().map_err(|_| "thread panicked"))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut published = 0;
    let mut unchanged = 0;
    for report in reports {
        let report = report?;
        published += report.entries_published;
        unchanged += report.unchanged;
    }
    assert_eq!((published, unchanged), (2, 2));
    let consumer = open(&host, "consumer")?;
    for key in keys {
        assert!(hit(&consumer, key)?.is_some());
    }
    Ok(())
}

#[test]
fn interrupted_imports_leave_only_complete_entries_and_resume() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let keys = [
        probe(&source, &host, "one", b"one")?,
        probe(&source, &host, "two", b"two")?,
    ];
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let sorted: BTreeSet<Digest> = keys.into_iter().collect();
    let (first, second) = (*sorted.first().ok_or("key")?, *sorted.last().ok_or("key")?);

    for (event, stop_at) in [
        (TestEvent::ImportBlobsStaged, 1),
        (TestEvent::ImportStep, 2),
    ] {
        let name = format!("consumer-{stop_at}");
        let seen = AtomicUsize::new(0);
        let hook = |current: TestEvent, _: &Path| {
            if current == event && seen.fetch_add(1, Ordering::Relaxed) + 1 == stop_at {
                panic!("simulated interruption");
            }
        };
        let consumer = with_hook(open(&host, &name)?, &hook);
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            consumer.import_probes(&bundle, ImportTrust::Trusted, &LIMITS)
        }));
        assert!(interrupted.is_err());
        let fresh = open(&host, &name)?;
        if stop_at == 1 {
            // The first entry stopped after its blob and before its activation.
            assert_eq!(hit(&fresh, first)?, None);
            assert!(
                fresh.get_blob(Digest::hash(b"one"))?.is_some()
                    || fresh.get_blob(Digest::hash(b"two"))?.is_some()
            );
        } else {
            assert!(hit(&fresh, first)?.is_some());
            assert_eq!(hit(&fresh, second)?, None);
        }
        let resumed = fresh.import_probes(&bundle, ImportTrust::Trusted, &LIMITS)?;
        assert_eq!(resumed.entries_published + resumed.unchanged, 2);
        assert!(hit(&fresh, first)?.is_some() && hit(&fresh, second)?.is_some());
    }
    Ok(())
}

#[test]
fn collection_expires_old_candidates_and_clear_removes_them() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    assert_eq!(consumer.gc()?.staging_removed, 0);
    host.sleep(CANDIDATE_TTL + Duration::from_secs(1));
    assert_eq!(consumer.gc()?.staging_removed, 2);
    consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    consumer.clear()?;
    assert_eq!(consumer.stats()?.candidates, 0);
    Ok(())
}

/// Replace the bytes of a stored blob file, so its content no longer matches its digest.
fn corrupt(host: &PureHost, path: &Path) -> TestResult {
    let mut bytes = host.read(path, rcmake_host::ReadRange::All { limit: 1 << 20 })?;
    if let Some(last) = bytes.last_mut() {
        *last ^= 1;
    }
    host.write(path, &bytes, rcmake_host::WriteMode::Truncate)?;
    Ok(())
}

#[test]
fn import_repairs_a_corrupt_blob_of_a_matching_live_entry() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let blob = Digest::hash(b"artifact");

    for trust in [ImportTrust::Trusted, ImportTrust::Untrusted] {
        let name = format!("consumer-{trust:?}");
        let consumer = open(&host, &name)?;
        probe(&consumer, &host, "probe", b"artifact")?;
        corrupt(&host, &consumer.blob_path(blob))?;
        let report = consumer.import_probes(&bundle, trust, &LIMITS)?;
        assert_eq!(
            report,
            ImportReport {
                recovered: 1,
                blobs_published: 1,
                ..ImportReport::default()
            },
            "{trust:?}"
        );
        assert_eq!(consumer.get_blob(blob)?.as_deref(), Some(&b"artifact"[..]));
        assert_eq!(hit(&consumer, key)?.as_deref(), Some("probe"));
    }
    Ok(())
}

#[test]
fn candidate_recovery_repairs_a_corrupt_blob_after_a_locked_recheck() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let blob = Digest::hash(b"artifact");
    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    probe(&consumer, &host, "probe", b"artifact")?;

    // A corrupt blob is invalid data. The lookup removes it and restores it from the candidate.
    corrupt(&host, &consumer.blob_path(blob))?;
    assert!(consumer.recover_from_candidate(key, &live_entry(&consumer, key)?));
    assert_eq!(consumer.get_blob(blob)?.as_deref(), Some(&b"artifact"[..]));

    // The trace is checked again under the lock, so a changed compiler restores nothing.
    let entry = live_entry(&consumer, key)?;
    host.remove_file(&consumer.blob_path(blob))?;
    host.write(
        &host.initial_cwd().join("bin/cc"),
        b"compiler v2",
        rcmake_host::WriteMode::Truncate,
    )?;
    assert!(!consumer.recover_from_candidate(key, &entry));
    assert!(consumer.get_blob(blob)?.is_none());
    Ok(())
}

fn live_entry(store: &Store<'_>, key: Digest) -> Result<Entry<Value>, Box<dyn std::error::Error>> {
    let path = store.entry_path(Domain::ProbeV1, key);
    match store.read_entry::<Entry<Value>>(Domain::ProbeV1, key, &path, false)? {
        CacheRead::Found(entry) => Ok(entry),
        _ => Err("live entry".into()),
    }
}

#[test]
fn a_caller_check_runs_before_candidate_recovery() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let key = probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let blob = Digest::hash(b"artifact");
    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    probe(&consumer, &host, "probe", b"artifact")?;
    host.remove_file(&consumer.blob_path(blob))?;

    let reject = |_: &Entry<Value>| false;
    assert!(
        consumer
            .get_entry_accepted::<Value>(Domain::ProbeV1, key, &reject)?
            .is_none()
    );
    assert!(
        consumer.get_blob(blob)?.is_none(),
        "a rejected entry restores nothing"
    );
    let accept = |_: &Entry<Value>| true;
    assert!(
        consumer
            .get_entry_accepted::<Value>(Domain::ProbeV1, key, &accept)?
            .is_some()
    );
    assert!(consumer.get_blob(blob)?.is_some());
    Ok(())
}

#[test]
fn import_checks_the_typed_probe_payload_and_artifact_role() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let blob = source.put_blob(b"artifact")?;
    let key = Domain::ProbeV1.key(b"wrong role");
    // A failed outcome that still references a blob has the wrong artifact role.
    source.put_entry(
        Domain::ProbeV1,
        key,
        &Entry {
            trace: Vec::new(),
            blobs: vec![blob],
            payload: payload("wrong role", None),
        },
    )?;
    let export = source.export_probes(&LIMITS)?;
    assert!(export.entries.is_empty());
    assert_eq!(export.skipped.invalid, 1);

    // An archive that carries the malformed frame is rejected before any write.
    let (pin, frame) =
        crate::remote::export_entry(&source, CacheEntryKind::Probe, key)?.ok_or("entry")?;
    let mut manifest = CacheManifest::default();
    manifest.insert(pin)?;
    let forged = ProbeBundle {
        manifest: manifest.to_json()?,
        entries: BTreeMap::from([(key, frame)]),
        blobs: BTreeMap::from([(
            blob,
            crate::remote::export_blob(&source, blob)?.ok_or("blob")?,
        )]),
    };
    let consumer = open(&host, "consumer")?;
    match consumer.import_probes(&forged, ImportTrust::Trusted, &LIMITS) {
        Err(ImportError::Invalid(reason)) => assert!(reason.contains("probe payload"), "{reason}"),
        other => panic!("expected a payload rejection, got {other:?}"),
    }

    // A malformed live frame is not a valid local entry, so it does not block a usable restore.
    let good = probe(&source, &host, "good", b"good")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let good_blob = consumer.put_blob(b"good")?;
    consumer.put_entry(
        Domain::ProbeV1,
        good,
        &Entry {
            trace: Vec::new(),
            blobs: vec![good_blob],
            payload: payload("good", None),
        },
    )?;
    let report = consumer.import_probes(&bundle, ImportTrust::Trusted, &LIMITS)?;
    assert_eq!(report.entries_published, 1);
    assert_eq!(hit(&consumer, good)?.as_deref(), Some("good"));
    Ok(())
}

#[test]
fn a_failed_write_reports_the_entries_completed_before_it() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    let keys = [
        probe(&source, &host, "one", b"one")?,
        probe(&source, &host, "two", b"two")?,
    ];
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let sorted: BTreeSet<Digest> = keys.into_iter().collect();
    let (first, second) = (*sorted.first().ok_or("key")?, *sorted.last().ok_or("key")?);
    let steps = AtomicUsize::new(0);
    let hook = |event: TestEvent, path: &Path| {
        if event == TestEvent::ImportStep && steps.fetch_add(1, Ordering::Relaxed) == 1 {
            // A file where the entry directory belongs makes the entry write fail.
            if let Some(parent) = path.parent() {
                let _ = host.seed_file(parent, b"blocked");
            }
        }
    };
    let consumer = with_hook(open(&host, "consumer")?, &hook);
    match consumer.import_probes(&bundle, ImportTrust::Trusted, &LIMITS) {
        Err(ImportError::Store { completed, .. }) => assert_eq!(
            completed,
            ImportReport {
                entries_published: 1,
                blobs_published: 1,
                ..ImportReport::default()
            }
        ),
        other => panic!("expected a store error, got {other:?}"),
    }
    let consumer = open(&host, "consumer")?;
    assert!(hit(&consumer, first)?.is_some());
    assert_eq!(hit(&consumer, second)?, None);
    Ok(())
}

#[test]
fn export_stays_inside_the_import_limits() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    for name in ["one", "two", "three", "four"] {
        probe(&source, &host, name, name.as_bytes())?;
    }
    let full = source.export_probes(&LIMITS)?;
    assert_eq!(full.entries.len(), 4);
    let one = bundle(&full)?;
    let entry = one.entries.values().map(Vec::len).max().unwrap_or_default() as u64;
    let blob = one.blobs.values().map(Vec::len).max().unwrap_or_default() as u64;

    let tight = [
        ArchiveLimits {
            members: 5,
            ..LIMITS
        },
        ArchiveLimits {
            member_bytes: entry.min(blob) - 1,
            ..LIMITS
        },
        ArchiveLimits {
            total_bytes: one.manifest.len() as u64 + 2 * (entry + blob),
            ..LIMITS
        },
        ArchiveLimits {
            decoded_bytes: 7,
            ..LIMITS
        },
        ArchiveLimits {
            entry_decoded_bytes: 1,
            ..LIMITS
        },
        ArchiveLimits {
            manifest_bytes: one.manifest.len() as u64 / 2,
            ..LIMITS
        },
    ];
    for limits in tight {
        let export = source.export_probes(&limits)?;
        assert!(export.entries.len() < 4, "{limits:?}");
        assert_eq!(
            export.skipped.over_budget,
            4 - export.entries.len(),
            "{limits:?}"
        );
        bundle(&export)?.check(&limits)?;
    }
    assert!(one.check(&tight[0]).is_err());
    match one.check(&ArchiveLimits {
        entry_decoded_bytes: 1,
        ..LIMITS
    }) {
        Err(ImportError::Invalid(reason)) => {
            assert!(reason.contains("decoded size limit"), "{reason}");
        }
        other => panic!("expected an entry budget rejection, got {other:?}"),
    }
    Ok(())
}

#[test]
fn probe_payload_check_types_each_journal_check() {
    let with_checks = |checks: Value| {
        let mut payload = payload("probe", None);
        if let Value::Object(fields) = &mut payload {
            fields.insert("checks".into(), checks);
        }
        probe_payload_valid(&Entry {
            trace: Vec::new(),
            blobs: Vec::new(),
            payload,
        })
    };
    let digest = Digest::from_bytes([7; 32]).to_json();
    let pair = |check: Value| Value::array_from([check, digest.clone()]);
    let valid = [
        Value::string("Environment"),
        Value::object_from([(
            "Metadata",
            Value::array_from([Value::string("/a"), Value::Bool(true)]),
        )]),
        Value::object_from([(
            "Slice",
            Value::array_from([
                Value::string("/a"),
                Value::object_from([("offset", 0u64.to_json()), ("len", Value::Null)]),
            ]),
        )]),
        Value::object_from([("Canonical", Value::string("/a"))]),
        Value::object_from([("Request", 3u32.to_json())]),
    ];
    assert!(with_checks(Value::array_from(valid.into_iter().map(pair))));
    let invalid = [
        Value::string("Unknown"),
        Value::object_from([("Link", 1u32.to_json())]),
        Value::object_from([("Request", Value::string("x"))]),
        Value::object_from([(
            "Slice",
            Value::array_from([
                Value::string("/a"),
                Value::object_from([("offset", 0u64.to_json()), ("extra", Value::Null)]),
            ]),
        )]),
        Value::object_from([
            ("Chunks", Value::string("/a")),
            ("Link", Value::string("/b")),
        ]),
    ];
    for check in invalid {
        assert!(
            !with_checks(Value::array_from([pair(check.clone())])),
            "{check:?}"
        );
    }
    assert!(!with_checks(Value::array_from([Value::array_from([
        Value::string("System")
    ])])));
    assert!(!with_checks(Value::array_from([Value::array_from([
        Value::string("System"),
        Value::string("not a digest"),
    ])])));
    assert!(!with_checks(Value::object()));
    let too_many = Value::array_from(
        std::iter::repeat_with(|| pair(Value::string("System"))).take(MAX_PROBE_CHECKS + 1),
    );
    assert!(!with_checks(too_many));
}

#[test]
fn collection_counts_candidates_toward_the_size_cap() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    for name in ["one", "two", "three"] {
        probe(&source, &host, name, name.as_bytes())?;
    }
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let root = host.initial_cwd().join("consumer");
    let mut config = StoreConfig::new(root);
    config.max_bytes = 1;
    let consumer = Store::new(&host, config)?;
    let report = consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    assert_eq!(report.staged, 3);
    assert_eq!(consumer.stats()?.candidates, 6);
    let collected = consumer.gc()?;
    assert_eq!(collected.staging_removed, 6);
    assert_eq!(consumer.stats()?.candidates, 0);
    Ok(())
}

#[test]
fn refreshing_a_candidate_keeps_its_blobs_for_the_same_period() -> TestResult {
    let host = host();
    let source = open(&host, "producer")?;
    probe(&source, &host, "probe", b"artifact")?;
    let bundle = bundle(&source.export_probes(&LIMITS)?)?;
    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    host.sleep(CANDIDATE_TTL - Duration::from_secs(60));
    let again = consumer.import_probes(&bundle, ImportTrust::Untrusted, &LIMITS)?;
    assert_eq!(again.unchanged, 1);
    host.sleep(Duration::from_secs(120));
    assert_eq!(consumer.gc()?.staging_removed, 0);
    assert_eq!(consumer.stats()?.candidates, 2);
    Ok(())
}
