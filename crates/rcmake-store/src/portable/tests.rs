use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use rcmake_host::{Host, PureHost};

use super::*;
use crate::remote::local_probe_pin;
use crate::{InputStamp, PublishOutcome, StoreConfig, TestEvent, TestHook};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn host() -> PureHost {
    let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
    PureHost::new(root.clone(), root.join("cmake"))
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
        payload: name.to_owned(),
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
        .get_entry::<String>(Domain::ProbeV1, key)?
        .map(|entry| entry.payload))
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
    let export = source.export_probes()?;
    assert_eq!(export.entries.len(), 2);
    assert_eq!(export.blobs.len(), 2);
    assert_eq!(export.skipped, ExportSkips::default());

    let consumer = open(&host, "consumer")?;
    let report = consumer.import_probes(&bundle(&export)?, ImportTrust::Trusted)?;
    assert_eq!((report.entries_published, report.blobs_published), (2, 2));
    assert_eq!(hit(&consumer, first)?.as_deref(), Some("first"));
    assert_eq!(hit(&consumer, second)?.as_deref(), Some("second"));
    assert!(consumer.get_entry::<u32>(Domain::FetchV1, fetch)?.is_none());

    let again = consumer.import_probes(&bundle(&export)?, ImportTrust::Trusted)?;
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
            payload: "stale".to_owned(),
        },
    )?;
    host.write(&stale_input, b"two", rcmake_host::WriteMode::Truncate)?;
    let export = store.export_probes()?;
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
    let bundle = bundle(&source.export_probes()?)?;

    let consumer = open(&host, "consumer")?;
    let report = consumer.import_probes(&bundle, ImportTrust::Untrusted)?;
    assert_eq!(
        report,
        ImportReport {
            staged: 1,
            ..ImportReport::default()
        }
    );
    assert_eq!(hit(&consumer, key)?, None);
    assert!(local_probe_pin::<String>(&consumer, key)?.is_none());
    assert_eq!(consumer.stats()?.entries, 0);
    assert_eq!(consumer.stats()?.candidates, 2);

    let again = consumer.import_probes(&bundle, ImportTrust::Untrusted)?;
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
    let bundle = bundle(&source.export_probes()?)?;

    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted)?;
    // A local measurement establishes the pin, then its blob is lost.
    assert_eq!(probe(&consumer, &host, "probe", b"artifact")?, key);
    host.remove_file(&consumer.blob_path(Digest::hash(b"artifact")))?;
    assert_eq!(hit(&consumer, key)?.as_deref(), Some("probe"));
    assert!(consumer.get_blob(Digest::hash(b"artifact"))?.is_some());

    // The same recovery runs at import time for an existing pin.
    let other = open(&host, "other")?;
    probe(&other, &host, "probe", b"artifact")?;
    host.remove_file(&other.blob_path(Digest::hash(b"artifact")))?;
    let report = other.import_probes(&bundle, ImportTrust::Untrusted)?;
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
    let bundle = bundle(&source.export_probes()?)?;

    let consumer = open(&host, "consumer")?;
    let blob = consumer.put_blob(b"local artifact")?;
    let local = Entry {
        trace: Vec::new(),
        blobs: vec![blob],
        payload: "local".to_owned(),
    };
    consumer.put_entry(Domain::ProbeV1, key, &local)?;
    for trust in [ImportTrust::Trusted, ImportTrust::Untrusted] {
        let report = consumer.import_probes(&bundle, trust)?;
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
    let good = bundle(&source.export_probes()?)?;
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
            match consumer.import_probes(&bundle, trust) {
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
    let bundle = bundle(&source.export_probes()?)?;

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
    let report = consumer.import_probes(&bundle, ImportTrust::Trusted)?;
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
    let bundle = bundle(&source.export_probes()?)?;
    let reports = thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    open(&host, "consumer")
                        .map_err(ImportError::from)
                        .and_then(|store| store.import_probes(&bundle, ImportTrust::Trusted))
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
    let bundle = bundle(&source.export_probes()?)?;
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
            consumer.import_probes(&bundle, ImportTrust::Trusted)
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
        let resumed = fresh.import_probes(&bundle, ImportTrust::Trusted)?;
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
    let bundle = bundle(&source.export_probes()?)?;
    let consumer = open(&host, "consumer")?;
    consumer.import_probes(&bundle, ImportTrust::Untrusted)?;
    assert_eq!(consumer.gc()?.staging_removed, 0);
    host.sleep(CANDIDATE_TTL + Duration::from_secs(1));
    assert_eq!(consumer.gc()?.staging_removed, 2);
    consumer.import_probes(&bundle, ImportTrust::Untrusted)?;
    consumer.clear()?;
    assert_eq!(consumer.stats()?.candidates, 0);
    Ok(())
}
