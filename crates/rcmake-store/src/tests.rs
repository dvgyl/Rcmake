use std::{
    sync::{Mutex, mpsc},
    thread,
};

use rcmake_host::{FileTime, PureHost};

use super::*;

fn host() -> PureHost {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let path = PathBuf::from(root);
    PureHost::new(path.clone(), path.join("cmake"))
}

fn store(host: &PureHost) -> HostResult<Store<'_>> {
    Store::new(host, StoreConfig::new(host.initial_cwd().join("cache")))
}

fn entry(payload: &str) -> Entry<String> {
    Entry {
        trace: Vec::new(),
        blobs: Vec::new(),
        payload: payload.to_owned(),
    }
}

#[test]
fn remote_seed_imports_a_blob_and_reuses_it_on_retry() -> Result<(), Box<dyn std::error::Error>> {
    use crate::remote::{CacheEntryKind, RemoteError, export_blob, export_entry};

    let host = host();
    let source = store(&host)?;
    let destination = Store::new(
        &host,
        StoreConfig::new(host.initial_cwd().join("destination")),
    )?;
    let payload = b"probe output";
    let digest = source.put_blob(payload)?;
    let key = Domain::ProbeV1.key(b"probe inputs");
    let mut entry = entry("probe result");
    entry.blobs.push(digest);
    assert_eq!(
        source.put_entry(Domain::ProbeV1, key, &entry)?,
        PublishOutcome::Published
    );
    let (pin, frame) = export_entry(&source, CacheEntryKind::Probe, key)?
        .ok_or("The source must contain the probe entry.")?;
    let imported: Option<Entry<String>> = pin.seed(
        &destination,
        &frame,
        &entry.trace,
        |candidate| candidate == &entry,
        |digest, _| export_blob(&source, digest),
    )?;
    assert_eq!(imported.as_ref(), Some(&entry));
    assert_eq!(
        destination.get_blob(digest)?.as_deref(),
        Some(payload.as_slice())
    );
    assert_eq!(destination.get_entry(Domain::ProbeV1, key)?, imported);

    let retried: Option<Entry<String>> = pin.seed(
        &destination,
        &frame,
        &entry.trace,
        |candidate| candidate == &entry,
        |_, _| Err(RemoteError::Failure("Expected a local blob.")),
    )?;
    assert_eq!(retried, imported);
    Ok(())
}

#[test]
fn digest_hex_and_domains() -> Result<(), Box<dyn std::error::Error>> {
    let digest = Digest::hash(b"abc");
    let hex = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(digest.to_hex(), hex);
    assert_eq!(Digest::from_hex(&hex.to_ascii_uppercase()), Some(digest));
    assert_eq!(Digest::from_hex("é"), None);
    assert_eq!(
        serde_json::from_str::<Digest>(&serde_json::to_string(&digest)?)?,
        digest
    );
    let keys = Domain::ALL.map(|domain| domain.key(b"abc"));
    assert_eq!(
        keys.into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
    let mut a = KeyHasher::new(Domain::ProbeV1);
    a.field(1, b"a");
    a.field(2, b"bc");
    let mut b = KeyHasher::new(Domain::ProbeV1);
    b.field(1, b"ab");
    b.field(2, b"c");
    assert_ne!(a.finish(), b.finish());
    Ok(())
}

#[test]
fn blob_and_entry_round_trip() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let data = b"cached compiler output";
    let digest = store.put_blob(data)?;
    assert_eq!(store.get_blob(digest)?.as_deref(), Some(data.as_slice()));
    let destination = host.initial_cwd().join("restored");
    assert!(store.materialize_blob(digest, &destination, WriteMode::CreateNew)?);
    assert_eq!(
        host.read(&destination, ReadRange::All { limit: 128 })?,
        data
    );
    let domain = Domain::ProbeV1;
    let mut result = entry("compiled");
    result.blobs.push(digest);
    let key = domain.key(b"probe");
    store.put_entry(domain, key, &result)?;
    assert_eq!(store.get_entry(domain, key)?, Some(result));
    assert_eq!(store.get_entry::<String>(Domain::FetchV1, key)?, None);
    host.remove_file(&store.blob_path(digest))?;
    assert_eq!(store.get_entry::<String>(domain, key)?, None);
    Ok(())
}

#[test]
fn concurrent_publish_preserves_winner_and_cleans_staging() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let domain = Domain::ProbeV1;
    let key = domain.key(b"same recipe");
    let barrier = std::sync::Barrier::new(2);
    thread::scope(|scope| -> HostResult<()> {
        let first = scope.spawn(|| {
            barrier.wait();
            store.put_blob(b"identical blob")?;
            store.put_entry(domain, key, &entry("first"))?;
            Ok::<_, HostError>(())
        });
        barrier.wait();
        store.put_blob(b"identical blob")?;
        store.put_entry(domain, key, &entry("second"))?;
        first
            .join()
            .map_err(|_| HostError::unsupported("Publisher thread failed."))?
    })?;
    let winner = store.get_entry::<String>(domain, key)?;
    assert!(
        winner
            .as_ref()
            .is_some_and(|entry| matches!(entry.payload.as_str(), "first" | "second"))
    );
    store.put_entry(domain, key, &entry("late"))?;
    assert_eq!(store.get_entry::<String>(domain, key)?, winner);
    assert_eq!(
        store.get_blob(Digest::hash(b"identical blob"))?.as_deref(),
        Some(b"identical blob".as_slice())
    );
    assert!(host.read_dir(&store.config.root.join("tmp"))?.is_empty());
    Ok(())
}

#[test]
fn invalid_frames_are_misses() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let domain = Domain::ConfigureV1;
    let key = domain.key(b"entry");
    store.put_entry(domain, key, &entry("result"))?;
    let path = store.entry_path(domain, key);
    let valid = host.read(&path, ReadRange::All { limit: 10_000 })?;
    let mut flipped = valid.clone();
    if let Some(byte) = flipped.last_mut() {
        *byte ^= 0x40;
    }
    let mut wrong_magic = valid.clone();
    if let Some(byte) = wrong_magic.first_mut() {
        *byte ^= 0x01;
    }
    let mut wrong_version = valid.clone();
    if let Some(byte) = wrong_version.get_mut(8) {
        *byte ^= 0x01;
    }
    let mut oversized = valid.clone();
    if let Some(len) = oversized.get_mut(9..17) {
        len.copy_from_slice(&((ENTRY_LIMIT + 1) as u64).to_le_bytes());
    }
    let footer = oversized.len().saturating_sub(32);
    if let Some((body, checksum)) = oversized.split_at_mut_checked(footer) {
        checksum.copy_from_slice(Digest::hash(body).as_bytes());
    }
    host.remove_file(&path)?;
    for bytes in [
        b"".as_slice(),
        valid
            .get(..valid.len().saturating_sub(1))
            .unwrap_or_default(),
        &flipped,
        &wrong_magic,
        &wrong_version,
        &oversized,
    ] {
        host.write(&path, bytes, WriteMode::CreateNew)?;
        assert_eq!(store.get_entry::<String>(domain, key)?, None);
        assert!(store.file_metadata(&path)?.is_none());
    }
    let wrong_key = domain.key(b"moved");
    let moved = store.entry_path(domain, wrong_key);
    store.ensure_dir(
        moved
            .parent()
            .ok_or_else(|| HostError::unsupported("Expected parent path."))?,
    )?;
    host.write(&moved, &valid, WriteMode::CreateNew)?;
    assert_eq!(store.get_entry::<String>(domain, wrong_key)?, None);
    Ok(())
}

#[test]
fn stat_negative_lookup_and_environment_invalidate_the_entry() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let root = host.initial_cwd().join("includes");
    let header = root.join("common.h");
    host.seed_file(&header, b"header")?;
    host.env_set(b"TOOLCHAIN", Some(b"system"));
    let mut result = entry("found");
    result.trace = vec![
        InputStamp::FileStat {
            path: header.clone(),
            stat: FastStat::read(&host, &header)?,
            digest: Some(file_digest(&host, &header)?),
        },
        InputStamp::NegativeLookup {
            root: root.clone(),
            relative: "other/opt.h".into(),
            deepest: root.clone(),
            listing: directory_digest(&host, &root)?,
        },
        InputStamp::EnvVar {
            name: b"TOOLCHAIN".to_vec(),
            digest: Some(Digest::hash(b"system")),
        },
    ];
    let domain = Domain::ProbeV1;
    let key = domain.key(b"probe");
    store.put_entry(domain, key, &result)?;
    assert_eq!(store.get_entry(domain, key)?, Some(result.clone()));
    host.env_set(b"TOOLCHAIN", None);
    assert_eq!(
        verify_trace(&host, &result.trace),
        VerifyOutcome::Miss(MissReason { stamp: 2 })
    );
    host.env_set(b"TOOLCHAIN", Some(b"system"));
    host.create_dir_all(&root.join("other"))?;
    assert_eq!(
        verify_trace(&host, &result.trace),
        VerifyOutcome::Miss(MissReason { stamp: 1 })
    );
    host.write(&header, b"changed", WriteMode::Truncate)?;
    assert_eq!(
        verify_trace(&host, &result.trace),
        VerifyOutcome::Miss(MissReason { stamp: 0 })
    );
    assert_eq!(store.get_entry::<String>(domain, key)?, None);
    Ok(())
}

#[test]
fn file_hash_listing_and_missing_stamps_validate() -> HostResult<()> {
    let host = host();
    let root = host.initial_cwd().join("inputs");
    let file = root.join("data");
    let new = root.join("new");
    host.seed_file(&file, b"initial")?;
    let trace = [
        InputStamp::FileHash {
            path: file.clone(),
            digest: Digest::hash(b"initial"),
        },
        InputStamp::Missing { path: new.clone() },
        InputStamp::DirEntries {
            path: root.clone(),
            digest: directory_digest(&host, &root)?,
        },
    ];
    host.set_times(&file, None, Some(FileTime { secs: 10, nanos: 0 }))?;
    assert_eq!(verify_trace(&host, &trace), VerifyOutcome::Hit);
    host.create_dir_all(&root.join("subdir"))?;
    assert_eq!(
        verify_trace(&host, &trace),
        VerifyOutcome::Miss(MissReason { stamp: 2 })
    );
    host.seed_file(&new, b"")?;
    assert_eq!(
        verify_trace(&host, &trace),
        VerifyOutcome::Miss(MissReason { stamp: 1 })
    );
    host.remove_file(&file)?;
    assert_eq!(
        verify_trace(&host, &trace),
        VerifyOutcome::Miss(MissReason { stamp: 0 })
    );
    Ok(())
}

#[test]
fn collection_evicts_least_recently_used_entry() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let domain = Domain::ProbeV1;
    let first = domain.key(b"first");
    let second = domain.key(b"second");
    store.put_entry(domain, first, &entry("result"))?;
    host.sleep(Duration::from_secs(1));
    store.put_entry(domain, second, &entry("result"))?;
    host.sleep(Duration::from_secs(1));
    assert!(store.get_entry::<String>(domain, first)?.is_some());
    let total = host
        .metadata(&store.entry_path(domain, first), Follow::No)?
        .len
        + host
            .metadata(&store.entry_path(domain, second), Follow::No)?
            .len;
    store.config.max_bytes = total - 1;
    assert_eq!(store.gc()?.entries_removed, 1);
    assert!(store.get_entry::<String>(domain, first)?.is_some());
    assert_eq!(store.get_entry::<String>(domain, second)?, None);
    Ok(())
}

#[test]
fn collection_retains_grace_period_and_live_blob_references() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let live = store.put_blob(b"referenced")?;
    let orphan = store.put_blob(b"unreferenced")?;
    let domain = Domain::FetchV1;
    let key = domain.key(b"archive");
    let mut result = entry("archive");
    result.blobs.push(live);
    store.put_entry(domain, key, &result)?;
    assert_eq!(store.gc()?.blobs_removed, 0);
    host.sleep(store.config.blob_grace);
    assert_eq!(store.gc()?.blobs_removed, 1);
    assert!(store.get_blob(live)?.is_some());
    assert_eq!(store.get_blob(orphan)?, None);
    result.blobs.push(orphan);
    assert!(
        store
            .put_entry(domain, domain.key(b"new"), &result)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
    );
    Ok(())
}

#[test]
fn off_read_only_and_verify_modes() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let domain = Domain::ProbeV1;
    let key = domain.key(b"entry");
    let digest = store.put_blob(b"artifact")?;
    let mut result = entry("original");
    result.blobs.push(digest);
    store.put_entry(domain, key, &result)?;
    let path = store.entry_path(domain, key);
    let before = host.metadata(&path, Follow::No)?.mtime;
    host.sleep(Duration::from_secs(10));
    store.config.mode = StoreMode::ReadOnly;
    assert_eq!(store.get_entry(domain, key)?, Some(result.clone()));
    assert_eq!(host.metadata(&path, Follow::No)?.mtime, before);
    assert_eq!(
        store.put_entry(domain, domain.key(b"new"), &result)?,
        PublishOutcome::Disabled,
    );
    assert_eq!(store.get_entry::<String>(domain, domain.key(b"new"))?, None);
    assert_eq!(store.get_blob(store.put_blob(b"new")?)?, None);
    assert_eq!(store.gc()?, GcReport::default());
    store.config.mode = StoreMode::Verify;
    assert_eq!(store.get_entry(domain, key)?, Some(result));
    host.write(&store.blob_path(digest), b"corrupt", WriteMode::Truncate)?;
    assert_eq!(store.get_entry::<String>(domain, key)?, None);
    store.config.mode = StoreMode::Off;
    assert_eq!(store.get_entry::<String>(domain, key)?, None);
    assert_eq!(store.put_blob(b"artifact")?, digest);
    assert_eq!(store.get_blob(digest)?, None);
    assert_eq!(store.gc()?, GcReport::default());
    Ok(())
}

#[test]
fn cache_location_uses_host_environment() {
    let host = host();
    assert_eq!(StoreConfig::from_host(&host).mode, StoreMode::Off);
    host.env_set(b"RCMAKE_CACHE_DIR", Some(b"custom-cache"));
    host.env_set(b"RCMAKE_CACHE_MAX_SIZE", Some(b"1000"));
    let config = StoreConfig::from_host(&host);
    assert_eq!(config.root, host.initial_cwd().join("custom-cache"));
    assert_eq!(config.mode, StoreMode::ReadWrite);
    assert_eq!(config.max_bytes, 1000);
    assert_eq!(config.blob_grace, Duration::from_secs(3600));
}

#[test]
fn stale_or_corrupt_entry_can_be_refreshed() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let domain = Domain::ConfigureV1;
    let key = domain.key(b"stable caller key");
    let mut original = entry("original");
    original.trace.push(InputStamp::EnvVar {
        name: b"TEST_INPUT".to_vec(),
        digest: None,
    });
    assert_eq!(
        store.put_entry(domain, key, &original)?,
        PublishOutcome::Published
    );
    host.env_set(b"TEST_INPUT", Some(b"new value"));
    assert_eq!(store.get_entry::<String>(domain, key)?, None);
    assert_eq!(
        store.put_entry(domain, key, &original)?,
        PublishOutcome::Stale
    );
    let fresh = entry("fresh");
    store.put_entry(domain, key, &fresh)?;
    assert_eq!(store.get_entry(domain, key)?, Some(fresh));
    host.write(&store.entry_path(domain, key), b"torn", WriteMode::Truncate)?;
    store.put_entry(domain, key, &entry("recovered"))?;
    assert_eq!(store.get_entry(domain, key)?, Some(entry("recovered")));
    Ok(())
}

#[test]
fn collection_accounts_for_shared_blobs_and_staging() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let domain = Domain::FetchV1;
    let mut items = Vec::new();
    let mut size = 0;
    let stale = store.config.root.join("tmp/stale");
    host.seed_file(&stale, &[0; 4096])?;
    for number in 0..5u8 {
        let digest = store.put_blob(&[number; 4096])?;
        let mut result = entry("archive");
        result.blobs = vec![digest, digest];
        let key = domain.key(&[number]);
        store.put_entry(domain, key, &result)?;
        size += host.metadata(&store.blob_path(digest), Follow::No)?.len;
        size += host
            .metadata(&store.entry_path(domain, key), Follow::No)?
            .len;
        items.push((key, digest));
        host.sleep(Duration::from_secs(1));
    }

    host.sleep(Duration::from_secs(3600));
    let active = store.config.root.join("tmp/active");
    host.seed_file(&active, &[0; 32])?;
    store.config.max_bytes = size + 31;
    let report = store.gc()?;
    assert_eq!(report.staging_removed, 1);
    assert_eq!(report.entries_removed, 2);
    assert_eq!(report.blobs_removed, 2);
    assert!(store.file_metadata(&stale)?.is_none());
    assert!(store.file_metadata(&active)?.is_some());
    for (index, (key, digest)) in items.into_iter().enumerate() {
        assert_eq!(
            store.get_entry::<String>(domain, key)?.is_some(),
            index >= 2
        );
        assert_eq!(store.get_blob(digest)?.is_some(), index >= 2);
    }
    Ok(())
}

#[test]
fn corrupt_blobs_are_removed_and_republished() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let digest = store.put_blob(b"artifact")?;
    let path = store.blob_path(digest);
    let wrong = frame::encode(BLOB_MAGIC, b"wrong digest", blob_limit())
        .map_err(|error| io_at(error, &path))?;
    host.write(&path, &wrong, WriteMode::Truncate)?;
    store.config.mode = StoreMode::Verify;
    assert_eq!(store.get_blob(digest)?, None);
    assert!(store.file_metadata(&path)?.is_some());
    store.config.mode = StoreMode::ReadWrite;
    assert_eq!(store.get_blob(digest)?, None);
    assert!(store.file_metadata(&path)?.is_none());

    host.write(&path, b"", WriteMode::CreateNew)?;
    assert_eq!(store.put_blob(b"artifact")?, digest);
    assert_eq!(
        store.get_blob(digest)?.as_deref(),
        Some(b"artifact".as_slice())
    );
    host.sleep(store.config.blob_grace);
    store.put_blob(b"artifact")?;
    assert_eq!(store.gc()?.blobs_removed, 0);
    Ok(())
}

#[test]
fn file_stamps_detect_same_time_replacements_and_allow_touch() -> HostResult<()> {
    let host = host();
    let path = host.initial_cwd().join("source");
    let replacement = host.initial_cwd().join("replacement");
    let resolution = Duration::from_secs(2);
    host.seed_file(&path, b"old")?;
    assert!(matches!(
        InputStamp::record_file(&host, path.clone(), resolution)?,
        InputStamp::FileHash { .. }
    ));
    host.sleep(Duration::from_secs(10));
    let stamp = InputStamp::record_file(&host, path.clone(), resolution)?;
    assert!(matches!(stamp, InputStamp::FileStat { .. }));
    let time = host.now();
    host.set_times(&path, None, Some(time))?;
    assert_eq!(
        verify_trace(&host, std::slice::from_ref(&stamp)),
        VerifyOutcome::Hit
    );
    host.seed_file(&replacement, b"new")?;
    host.set_times(&replacement, None, Some(time))?;
    let stamp = InputStamp::record_file(&host, path.clone(), Duration::ZERO)?;
    let before = FastStat::read(&host, &path)?;
    host.rename(&replacement, &path)?;
    let after = FastStat::read(&host, &path)?;
    assert_eq!(
        (before.len, before.mtime_secs, before.mtime_nanos),
        (after.len, after.mtime_secs, after.mtime_nanos)
    );
    assert_ne!(before.ino, after.ino);
    assert_eq!(
        verify_trace(&host, &[stamp]),
        VerifyOutcome::Miss(MissReason { stamp: 0 })
    );
    Ok(())
}

#[test]
fn unsupported_rename_uses_fallback_and_preserves_existing_file() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let temp = store.config.root.join("tmp/staged");
    let output = store.config.root.join("final");
    host.seed_file(&temp, b"first")?;
    let locked = store.lock_cache(Duration::ZERO)?;
    locked.rename_staged(&temp, &output, || {
        Err(HostError::unsupported("Unsupported by test host."))
    })?;
    assert!(store.file_metadata(&temp)?.is_none());
    assert!(
        locked
            .publish(&output, b"second", false)
            .is_err_and(|error| error.kind == ErrorKind::AlreadyExists)
    );
    assert_eq!(host.read(&output, ReadRange::All { limit: 32 })?, b"first");
    host.create_dir_all(&store.config.root.join("blocked/child"))?;
    assert!(
        locked
            .publish(&store.config.root.join("blocked"), b"unused", true)
            .is_err()
    );
    assert!(host.read_dir(&store.config.root.join("tmp"))?.is_empty());
    Ok(())
}

#[test]
fn entries_with_blobs_skip_publication_while_collection_holds_lock() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let mut result = entry("archive");
    result.blobs.push(store.put_blob(b"blob")?);
    let domain = Domain::FetchV1;
    let key = domain.key(b"dependency");
    let lock = host.lock_file(&store.gc_lock_path()?, Some(Duration::ZERO))?;
    assert_eq!(
        store.put_entry(domain, key, &result)?,
        PublishOutcome::SkippedBusy
    );
    assert_eq!(store.get_entry::<String>(domain, key)?, None);
    drop(lock);
    assert_eq!(
        store.put_entry(domain, key, &result)?,
        PublishOutcome::Published
    );
    assert_eq!(store.get_entry(domain, key)?, Some(result));
    Ok(())
}

#[test]
fn stat_stamps_check_same_inode_rewrites_and_reject_old_records() -> HostResult<()> {
    let host = host();
    let path = host.initial_cwd().join("source");
    host.seed_file(&path, b"set(VALUE old)")?;
    let stamp = InputStamp::record_file(&host, path.clone(), Duration::ZERO)?;
    assert!(matches!(stamp, InputStamp::FileStat { .. }));
    let stat = host.metadata(&path, Follow::Yes)?;
    assert_eq!(
        verify_trace(&host, std::slice::from_ref(&stamp)),
        VerifyOutcome::Hit
    );
    let old_record = serde_json::json!({
        "FileStat": { "path": path, "stat": FastStat::from(&stat) },
    });
    let legacy = serde_json::from_value(old_record).map_err(|error| io_at(error.into(), &path))?;
    assert_eq!(
        verify_trace(&host, &[legacy]),
        VerifyOutcome::Miss(MissReason { stamp: 0 })
    );
    host.sleep(Duration::from_secs(1));
    host.write(&path, b"set(VALUE new)", WriteMode::Truncate)?;
    host.set_times(&path, None, Some(stat.mtime))?;
    assert_eq!(FastStat::read(&host, &path)?, FastStat::from(&stat));
    assert_eq!(
        verify_trace(&host, &[stamp]),
        VerifyOutcome::Miss(MissReason { stamp: 0 })
    );
    Ok(())
}

#[test]
fn blob_and_aliases_report_busy_disabled_and_present() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let domain = Domain::FetchV1;
    let aliases = [domain.key(b"digest alias"), domain.key(b"URL alias")];
    let data = b"archive";
    let digest = Digest::hash(data);
    let lock = store.lock_cache(Duration::ZERO)?;
    assert_eq!(
        store.put_blob_with_aliases(data, domain, &aliases)?,
        (digest, PublishOutcome::SkippedBusy)
    );
    assert_eq!(store.get_blob(digest)?, None);
    for key in aliases {
        assert_eq!(store.get_entry::<Digest>(domain, key)?, None);
    }
    drop(lock);
    store.config.mode = StoreMode::ReadOnly;
    assert_eq!(
        store.put_blob_with_aliases(data, domain, &aliases)?,
        (digest, PublishOutcome::Disabled)
    );
    assert_eq!(store.get_blob(digest)?, None);
    store.config.mode = StoreMode::ReadWrite;
    assert_eq!(
        store.put_blob_with_aliases(data, domain, &aliases)?,
        (digest, PublishOutcome::Published)
    );
    assert_eq!(
        store.put_blob_with_aliases(data, domain, &aliases)?,
        (digest, PublishOutcome::AlreadyPresent)
    );
    assert_eq!(store.get_blob(digest)?.as_deref(), Some(data.as_slice()));
    for key in aliases {
        assert_eq!(
            store.get_entry(domain, key)?,
            Some(Entry {
                trace: Vec::new(),
                blobs: vec![digest],
                payload: digest
            }),
        );
    }
    Ok(())
}

#[test]
fn bounded_blob_lookup_leaves_an_oversized_blob_in_the_store() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let data = vec![b'a'; 32 * 1024];
    let domain = Domain::FetchV1;
    let alias = domain.key(b"bounded lookup");
    let (digest, outcome) = store.put_blob_with_aliases(&data, domain, &[alias])?;
    assert_eq!(outcome, PublishOutcome::Published);
    let limit = 1024;
    let stat = host.metadata(&store.blob_path(digest), Follow::No)?;
    assert!(stat.len < frame::disk_limit(limit));
    store.config.mode = StoreMode::Verify;
    assert_eq!(
        store.get_entry_with_blob_limit::<Digest>(domain, alias, limit)?,
        None
    );
    assert!(store.get_entry::<Digest>(domain, alias)?.is_some());
    assert_eq!(store.get_blob_with_limit(digest, limit)?, None);
    assert_eq!(store.get_blob_with_diagnostics(digest, limit)?, None);
    assert_eq!(
        store.get_entry_with_diagnostics::<Digest>(domain, alias, limit)?,
        None
    );
    assert_eq!(store.get_blob_with_limit(digest, data.len())?, Some(data));
    Ok(())
}

#[test]
fn diagnostic_lookup_reports_the_blocked_cache_directory() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let digest = Digest::hash(b"archive");
    let domain = Domain::FetchV1;
    let key = domain.key(b"archive");
    assert_eq!(store.get_blob_with_diagnostics(digest, 1024)?, None);
    assert_eq!(
        store.get_entry_with_diagnostics::<Digest>(domain, key, 1024)?,
        None
    );
    let blocked = store.root().join("blobs");
    host.seed_file(&blocked, b"file instead of directory")?;
    assert_eq!(store.get_blob(digest)?, None);
    assert!(
        store
            .get_blob_with_diagnostics(digest, 1024)
            .is_err_and(|error| {
                error.kind == ErrorKind::NotADirectory
                    && error.path.as_deref() == Some(blocked.as_path())
            })
    );
    let blocked = store.root().join("entries").join(domain.as_str());
    host.seed_file(&blocked, b"file instead of directory")?;
    assert_eq!(store.get_entry::<Digest>(domain, key)?, None);
    assert!(
        store
            .get_entry_with_diagnostics::<Digest>(domain, key, 1024)
            .is_err_and(|error| {
                error.kind == ErrorKind::NotADirectory
                    && error.path.as_deref() == Some(blocked.as_path())
            })
    );
    Ok(())
}

#[test]
fn diagnostic_lookup_reports_invalid_frames_and_respects_store_mode() -> HostResult<()> {
    let host = host();
    let mut store = store(&host)?;
    let digest = Digest::hash(b"archive");
    let blob = store.blob_path(digest);
    let domain = Domain::FetchV1;
    let key = domain.key(b"archive");
    let entry = store.entry_path(domain, key);
    host.seed_file(&blob, b"corrupt blob")?;
    host.seed_file(&entry, b"corrupt entry")?;
    for mode in [StoreMode::ReadOnly, StoreMode::ReadWrite] {
        store.config.mode = mode;
        assert!(
            store
                .get_blob_with_diagnostics(digest, 1024)
                .is_err_and(|error| {
                    error.kind == ErrorKind::InvalidData
                        && error.path.as_deref() == Some(blob.as_path())
                })
        );
        assert!(
            store
                .get_entry_with_diagnostics::<Digest>(domain, key, 1024)
                .is_err_and(|error| {
                    error.kind == ErrorKind::InvalidData
                        && error.path.as_deref() == Some(entry.as_path())
                })
        );
        assert_eq!(
            store.file_metadata(&blob)?.is_some(),
            mode == StoreMode::ReadOnly
        );
        assert_eq!(
            store.file_metadata(&entry)?.is_some(),
            mode == StoreMode::ReadOnly
        );
    }
    Ok(())
}

#[test]
fn fallback_rename_keeps_first_concurrent_entry() -> HostResult<()> {
    let host = host();
    let contender = store(&host)?;
    contender.rename_fallback.store(true, Ordering::Relaxed);
    let domain = Domain::ProbeV1;
    let key = domain.key(b"concurrent fallback");
    let outcome = run_paused(
        &host,
        &contender.entry_path(domain, key),
        TestEvent::FallbackRename,
        |first| {
            first.rename_fallback.store(true, Ordering::Relaxed);
            first.put_entry(domain, key, &entry("first"))
        },
        || {
            assert_eq!(
                contender.put_entry(domain, key, &entry("second"))?,
                PublishOutcome::SkippedBusy
            );
            Ok(())
        },
    )?;
    assert_eq!(outcome, PublishOutcome::Published);
    assert_eq!(
        contender.put_entry(domain, key, &entry("second"))?,
        PublishOutcome::AlreadyPresent
    );
    assert_eq!(contender.get_entry(domain, key)?, Some(entry("first")));
    Ok(())
}

#[test]
fn corruption_cleanup_blocks_repair_between_check_and_remove() -> HostResult<()> {
    let host = host();
    let repairer = store(&host)?;
    let bytes = b"verified";
    let digest = Digest::hash(bytes);
    let path = repairer.blob_path(digest);
    host.seed_file(&path, b"corrupt")?;
    let result = run_paused(
        &host,
        &path,
        TestEvent::BeforeRemove,
        |reader| reader.get_blob(digest),
        || {
            let error = repairer
                .put_blob(bytes)
                .err()
                .ok_or_else(|| HostError::unsupported("A repair passed a held cleanup lock."))?;
            assert_eq!(error.kind, ErrorKind::TimedOut);
            assert!(repairer.file_metadata(&path)?.is_some());
            assert_eq!(repairer.gc()?, GcReport::default());
            Ok(())
        },
    )?;
    assert_eq!(result, None);
    assert!(repairer.file_metadata(&path)?.is_none());
    repairer.put_blob(bytes)?;
    assert_eq!(
        repairer.get_blob(digest)?.as_deref(),
        Some(bytes.as_slice())
    );
    Ok(())
}

#[test]
fn corruption_cleanup_rechecks_an_object_repaired_before_the_lock() -> HostResult<()> {
    let host = host();
    let repairer = store(&host)?;
    let bytes = b"verified";
    let digest = Digest::hash(bytes);
    let path = repairer.blob_path(digest);
    host.seed_file(&path, b"corrupt")?;
    let result = run_paused(
        &host,
        &path,
        TestEvent::InvalidRead,
        |reader| reader.get_blob(digest),
        || {
            repairer.put_blob(bytes)?;
            Ok(())
        },
    )?;
    assert_eq!(result.as_deref(), Some(bytes.as_slice()));
    assert_eq!(
        repairer.get_blob(digest)?.as_deref(),
        Some(bytes.as_slice())
    );
    Ok(())
}

fn run_paused<T: Send>(
    host: &PureHost,
    path: &Path,
    event: TestEvent,
    operation: impl FnOnce(&Store<'_>) -> HostResult<T> + Send,
    interleave: impl FnOnce() -> HostResult<()>,
) -> HostResult<T> {
    const TEST_WAIT: Duration = Duration::from_secs(5);
    let (pause, paused) = mpsc::sync_channel(1);
    let (resume, resumed) = mpsc::sync_channel(1);
    let resumed = Mutex::new(resumed);
    let hook = |actual_event, actual_path: &Path| {
        if actual_event == event && actual_path == path {
            let _ = pause.try_send(());
            if let Ok(resumed) = resumed.lock() {
                let _ = resumed.recv_timeout(TEST_WAIT);
            }
        }
    };
    let mut hooked = store(host)?;
    hooked.test_hook = Some(&hook);
    thread::scope(|scope| {
        let worker = scope.spawn(|| operation(&hooked));
        let result = paused
            .recv_timeout(TEST_WAIT)
            .map_err(|_| HostError::unsupported("The test did not reach the pause point."))
            .and_then(|()| interleave());
        let _ = resume.try_send(());
        result?;
        worker
            .join()
            .map_err(|_| HostError::unsupported("Store test thread failed."))?
    })
}

#[test]
fn checked_publication_replaces_only_the_stale_key() -> HostResult<()> {
    let host = host();
    let store = store(&host)?;
    let mut first = [0xA5; 32];
    let second = Digest::from_bytes(first);
    first[31] = 0x5A;
    let first = Digest::from_bytes(first);
    for key in [first, second] {
        assert_eq!(
            store.put_entry(Domain::ProbeV1, key, &entry("old"))?,
            PublishOutcome::Published
        );
    }
    host.seed_file(&store.root().join("probe-generations/a5"), &[0; 33])?;
    assert_eq!(
        store.put_entry_checked(
            Domain::ProbeV1,
            first,
            &entry("new"),
            |_, _| false,
            || false
        )?,
        PublishOutcome::Stale
    );
    assert_eq!(
        store.put_entry_checked(Domain::ProbeV1, first, &entry("new"), |_, _| false, || true)?,
        PublishOutcome::Published
    );
    let read = |key| -> HostResult<_> {
        Ok(store
            .get_entry::<String>(Domain::ProbeV1, key)?
            .map(|entry| entry.payload))
    };
    assert_eq!(read(first)?.as_deref(), Some("new"));
    assert_eq!(read(second)?.as_deref(), Some("old"));
    Ok(())
}

#[test]
fn memo_detects_hard_link_writes_with_restored_modification_time() -> HostResult<()> {
    let host = host();
    let file = host.initial_cwd().join("header.h");
    let alias = host.initial_cwd().join("alias.h");
    host.seed_file(&file, b"old")?;
    host.hard_link(&file, &alias)?;
    host.sleep(Duration::from_secs(3));
    let memo = TraceMemo::default();
    let old = memo.file_digest(&host, &file)?;
    let mtime = host.metadata(&file, Follow::Yes)?.mtime;
    host.write(&alias, b"new", WriteMode::Truncate)?;
    host.set_times(&file, None, Some(mtime))?;
    host.sleep(Duration::from_secs(3));
    assert_ne!(old, memo.file_digest(&host, &file)?);
    Ok(())
}

#[test]
fn sdk_listing_checks_linked_directories_and_dangling_targets() -> HostResult<()> {
    let host = host();
    let sdk = host.initial_cwd().join("sdk");
    let headers = host.initial_cwd().join("headers");
    let missing = host.initial_cwd().join("missing");
    host.create_dir_all(&sdk)?;
    host.create_dir_all(&headers.join("nested"))?;
    host.symlink(&headers, &sdk.join("include"))?;
    host.symlink(&sdk, &headers.join("back"))?;
    host.symlink(&missing, &sdk.join("optional"))?;
    let before = sdk_tree_listing_digest(&host, &sdk)?;
    host.seed_file(&headers.join("nested/extra.h"), b"new")?;
    let inserted = sdk_tree_listing_digest(&host, &sdk)?;
    assert_ne!(before, inserted);
    host.seed_file(&missing, b"new")?;
    assert_ne!(inserted, sdk_tree_listing_digest(&host, &sdk)?);
    Ok(())
}

#[test]
fn memo_hashes_again_when_the_host_cannot_verify_change_time() -> HostResult<()> {
    let base = host();
    let path = base.initial_cwd().join("header.h");
    base.seed_file(&path, b"old")?;
    let host = rcmake_host::OverlayHost::new(base);
    host.write(&path, b"old", WriteMode::Truncate)?;
    let before = host.metadata(&path, Follow::Yes)?;
    assert_eq!(before.ctime, None);
    let memo = TraceMemo::default();
    let first = memo.file_digest(&host, &path)?;
    host.write(&path, b"new", WriteMode::Truncate)?;
    host.set_times(&path, None, Some(before.mtime))?;
    let after = host.metadata(&path, Follow::Yes)?;
    assert_eq!(FastStat::read(&host, &path)?.ino, before.ino);
    assert_eq!(after.mtime, before.mtime);
    assert_eq!(after.ctime, None);
    assert_ne!(first, memo.file_digest(&host, &path)?);
    Ok(())
}

#[test]
fn sdk_memo_rechecks_the_graph_after_a_host_write() -> HostResult<()> {
    let base: std::sync::Arc<dyn Host> = std::sync::Arc::new(host());
    let host = std::sync::Arc::new(rcmake_host::MemoHost::from_shared(base.clone()));
    let sdk = host.initial_cwd().join("sdk");
    host.create_dir_all(&sdk)?;
    let memo = TraceMemo::with_generation(host.clone());
    let old = memo.sdk_digest(base.as_ref(), &sdk)?;
    let scratch = host.initial_cwd().join("build/CMakeFiles/CMakeScratch");
    host.create_dir_all(&scratch)?;
    host.write(&scratch.join("probe.c"), b"int x;", WriteMode::CreateNew)?;
    assert_eq!(old, memo.sdk_digest(base.as_ref(), &sdk)?);
    assert_eq!(memo.sdk_graph_hits(), 1);
    host.write(&sdk.join("new.h"), b"header", WriteMode::CreateNew)?;
    let fresh = memo.sdk_digest(base.as_ref(), &sdk)?;
    assert_ne!(old, fresh);
    assert_eq!(fresh, sdk_tree_listing_digest(base.as_ref(), &sdk)?);
    assert_eq!(memo.sdk_graph_hits(), 1);
    assert!(memo.sdk_digest(base.as_ref(), &sdk.join("absent")).is_err());
    assert_eq!(memo.sdk_coverage_failures(), 1);
    Ok(())
}
