//! Fetch tests with an in-memory host and simulated tools.

#![cfg(all(feature = "native", any(unix, windows)))]
#![allow(clippy::panic, clippy::unwrap_used)]

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use rcmake_base::hash::HashAlgo;
use rcmake_fetch::{
    ArchiveCache, ContentHash, FetchError, FetchPool, GitEntry, GitMirror, LockEntry, LockFile,
    LockMode, LockSession, PrefetchSource, UrlEntry,
};
use rcmake_host::{Host, ProcessResult, PureHost, ReadRange, RunOutput};
use rcmake_store::{PublishOutcome, Store, StoreConfig};

const URL: &str = "https://example.invalid/archive";
const REPO: &str = "https://example.invalid/repo.git";
const PIN: &str = "0123456789abcdef0123456789abcdef01234567";

fn host() -> PureHost {
    PureHost::new("/work".into(), "/tools/rcmake".into())
}

fn config() -> StoreConfig {
    StoreConfig::new("/cache".into())
}

fn hash(algorithm: &str, algo: HashAlgo, bytes: &[u8]) -> ContentHash {
    ContentHash::parse(&format!("{algorithm}={}", algo.hex_digest(bytes))).unwrap()
}

#[test]
fn lock_round_trip_is_sorted_and_uses_version_one_json() {
    let mut lock = LockFile::default();
    lock.insert(
        "Zlib",
        LockEntry::Url(UrlEntry::from_bytes(vec![URL.into()], b"data")),
    )
    .unwrap();
    lock.insert(
        "Fmt",
        LockEntry::Git(GitEntry {
            repository: REPO.into(),
            tag: Some("11.0.2".into()),
            commit: PIN.into(),
        }),
    )
    .unwrap();
    let json = lock.to_json().unwrap();
    let text = std::str::from_utf8(&json).unwrap();
    assert!(text.ends_with("\n"));
    assert!(text.find("\"fmt\"").unwrap() < text.find("\"zlib\"").unwrap());
    assert_eq!(LockFile::parse(&json).unwrap(), lock);
    assert!(matches!(lock.get("FMT"), Some(LockEntry::Git(_))));
    assert!(matches!(lock.get("zlib"), Some(LockEntry::Url(_))));
}

#[test]
fn rejects_malformed_duplicate_unsupported_and_oversized_lockfiles() {
    let git = format!(r#"{{"kind":"git","repository":"{REPO}","commit":"{PIN}"}}"#);
    for bytes in [
        b"\xff".to_vec(),
        b"[]".to_vec(),
        br#"{"version":2,"entries":{}}"#.to_vec(),
        br#"{"version":1,"entries":{},"entries":{}}"#.to_vec(),
        br#"{"version":1,"entries":{}} {}"#.to_vec(),
        br#"{"version":1,"entries":{"x":{"kind":"url","urls":[],"sha256":"x","size":0}}}"#.to_vec(),
        format!(r#"{{"version":1,"entries":{{"x":{git},"X":{git}}}}}"#).into_bytes(),
        format!(r#"{{"version":1,"entries":{{"x":{{"kind":"git","repository":"{REPO}","commit":"{PIN}","extra":1}}}}}}"#).into_bytes(),
        vec![b' '; 4 * 1024 * 1024 + 1],
    ] {
        assert!(LockFile::parse(&bytes).is_err());
    }
    let entries = (0..=10_000)
        .map(|id| format!(r#""repo{id}":{git}"#))
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        LockFile::parse(format!(r#"{{"version":1,"entries":{{{entries}}}}}"#).as_bytes()).is_err()
    );
}

#[test]
fn lock_modes_apply_and_concurrent_updates_merge() {
    let host = host();
    let source = Path::new("/work");
    let path = source.join("rcmake-fetch.lock");
    let store = Store::new(&host, config()).unwrap();
    let cache = ArchiveCache::new(&host, &store);
    assert!(LockSession::open(&host, source, LockMode::Frozen).is_err());
    host.seed_file(&path, b"malformed").unwrap();
    let mut off = LockSession::open(&host, source, LockMode::Off).unwrap();
    assert!(LockSession::open(&host, source, LockMode::Auto).is_err());
    assert!(
        off.fetch_archive_with(&cache, "dep", URL, None, || Ok(b"old".to_vec()))
            .unwrap()
            .1
            .is_none()
    );
    off.commit().unwrap();
    host.remove_file(&path).unwrap();

    let mut update = LockSession::open(&host, source, LockMode::Update).unwrap();
    let mut concurrent = LockSession::open(&host, source, LockMode::Update).unwrap();
    update
        .fetch_archive_with(&cache, "dep", URL, None, || Ok(b"new".to_vec()))
        .unwrap();
    concurrent
        .record(
            "git",
            LockEntry::Git(GitEntry {
                repository: REPO.into(),
                tag: Some("v1".into()),
                commit: PIN.into(),
            }),
        )
        .unwrap();
    update.commit().unwrap();
    concurrent.commit().unwrap();

    let mut frozen = LockSession::open(&host, source, LockMode::Frozen).unwrap();
    assert_eq!(frozen.git_pin("GIT", REPO, Some("v1")).unwrap(), Some(PIN));
    assert!(frozen.git_pin("git", REPO, Some("v2")).is_err());
    let (archive, warning) = frozen
        .fetch_archive_with(&cache, "DEP", URL, None, || Err(FetchError::Offline))
        .unwrap();
    assert!(archive.cache_hit && warning.is_none());
    assert_eq!(archive.bytes, b"new");
    assert!(
        frozen
            .fetch_archive_with(&cache, "absent", URL, None, || Ok(Vec::new()))
            .is_err()
    );
    let empty = Store::new(&host, StoreConfig::new("/empty".into())).unwrap();
    let empty = ArchiveCache::new(&host, &empty);
    assert!(matches!(
        frozen.fetch_archive_with(&empty, "dep", URL, None, || Ok(b"changed".to_vec())),
        Err(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::HashMismatch { .. })
    ));
    let mut auto = LockSession::open(&host, source, LockMode::Auto).unwrap();
    assert_eq!(auto.git_pin("git", REPO, Some("v1")).unwrap(), None);
    let (archive, warning) = auto
        .fetch_archive_with(&cache, "dep", URL, None, || Ok(b"changed".to_vec()))
        .unwrap();
    assert_eq!(archive.bytes, b"changed");
    assert!(!archive.cache_hit);
    assert!(
        matches!(warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::HashMismatch { .. }))
    );
    let expected = hash("SHA256", HashAlgo::Sha256, b"changed");
    assert!(
        auto.fetch_archive_with(&empty, "dep", URL, Some(&expected), || Ok(
            b"changed".to_vec()
        ))
        .unwrap()
        .1
        .is_none()
    );
    auto.commit().unwrap();
    let frozen = LockSession::open(&host, source, LockMode::Frozen).unwrap();
    assert_eq!(frozen.git_pin("git", REPO, Some("v1")).unwrap(), Some(PIN));
}

#[test]
fn archive_cache_checks_non_sha256_aliases_and_offline_misses() {
    let host = host();
    let store = Store::new(&host, config()).unwrap();
    let cache = ArchiveCache::new(&host, &store);
    let expected = hash("MD5", HashAlgo::Md5, b"archive");
    assert!(cache.lookup(URL, &expected).unwrap().is_none());
    assert!(matches!(
        cache.fetch_with(URL, Some(&expected), || Ok(b"wrong".to_vec())),
        Err(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::HashMismatch { .. })
    ));
    assert!(cache.lookup(URL, &expected).unwrap().is_none());
    let miss = cache
        .fetch_with(URL, Some(&expected), || Ok(b"archive".to_vec()))
        .unwrap();
    assert!(!miss.cache_hit);
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    let hit = cache
        .fetch_with("https://mirror.invalid/archive", Some(&expected), || {
            panic!("cache hit must use the store")
        })
        .unwrap();
    assert!(hit.cache_hit);
    assert_eq!(hit.bytes, b"archive");
    let error = cache
        .fetch_with(URL, None, || panic!("offline miss must not download"))
        .unwrap_err();
    assert!(error.to_string().contains("RCMAKE_FETCH_OFFLINE=1"));
    assert!(error.to_string().contains("URL_HASH"));
    let other = hash("SHA256", HashAlgo::Sha256, b"other");
    let error = cache
        .fetch_with(URL, Some(&other), || {
            panic!("offline miss must not download")
        })
        .unwrap_err();
    assert!(error.to_string().contains("SHA256="));
}

#[test]
fn git_offline_miss_does_not_run_a_process() {
    let host = host();
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    host.set_command_handler(|_, _, _| panic!("offline miss must not run Git"))
        .unwrap();
    let mirror = GitMirror::new(&host, Path::new("/cache")).unwrap();
    assert!(matches!(mirror.prefetch(REPO, PIN), Err(FetchError::Git {
        repository, commit, path, ..
    }) if repository == REPO && commit.as_deref() == Some(PIN) && path.starts_with("/cache/git")));
    assert!(mirror.prefetch(REPO, "main").is_err());
}

#[test]
fn pinned_git_mirror_reuses_local_commit() {
    let host = host();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    host.set_command_handler(move |host, req, _| {
        let git = &req.pipeline.first().unwrap().args;
        let stdout = if git.first().is_some_and(|arg| arg == "clone") {
            observed.fetch_add(1, Ordering::SeqCst);
            let destination = PathBuf::from(std::str::from_utf8(git.last().unwrap()).unwrap());
            seed_bare(host, &destination)?;
            Vec::new()
        } else if git.iter().any(|arg| arg == "--is-bare-repository") {
            b"true\n".to_vec()
        } else if git.iter().any(|arg| arg == "--verify") {
            PIN.as_bytes().to_vec()
        } else if git.iter().any(|arg| arg == "cat-file") {
            b"tree\n".to_vec()
        } else {
            Vec::new()
        };
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout,
            ..RunOutput::default()
        })
    })
    .unwrap();
    let mirrors = GitMirror::new(&host, Path::new("/cache")).unwrap();
    let first = mirrors.prefetch(REPO, PIN).unwrap();
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    assert_eq!(mirrors.prefetch(REPO, PIN).unwrap(), first);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn pool_deduplicates_pins_and_populates_store() {
    let host = Arc::new(host());
    host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"0"));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    host.set_command_handler(move |_, req, _| {
        assert_eq!(req.pipeline.first().unwrap().program, "curl");
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: b"archive".to_vec(),
            ..RunOutput::default()
        })
    })
    .unwrap();
    let pool = FetchPool::new(host.clone(), config()).unwrap();
    let hash = hash("SHA256", HashAlgo::Sha256, b"archive");
    let source = PrefetchSource::Url {
        url: URL.into(),
        hash: hash.clone(),
    };
    let first = pool.submit(source.clone()).unwrap();
    let second = pool.submit(source.clone()).unwrap();
    first.wait().unwrap();
    second.wait().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let store = Store::new(host.as_ref(), config()).unwrap();
    assert_eq!(
        ArchiveCache::new(host.as_ref(), &store)
            .lookup(URL, &hash)
            .unwrap(),
        Some(b"archive".to_vec())
    );
    let digest = rcmake_store::Digest::hash(b"archive").to_hex();
    let path = config()
        .root
        .join("blobs")
        .join(digest.get(..2).unwrap())
        .join(format!("{digest}.blob"));
    host.remove_file(&path).unwrap();
    pool.submit(source).unwrap().wait().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        pool.submit(PrefetchSource::Git {
            repository: REPO.into(),
            commit: "main".into()
        })
        .is_err()
    );
}

#[test]
fn store_write_errors_leave_verified_downloads_usable_in_each_lock_mode() {
    let host = host();
    let bad_root = Path::new("/work/cache-file");
    host.seed_file(bad_root, b"not a directory").unwrap();
    let store = Store::new(&host, StoreConfig::new(bad_root.into())).unwrap();
    let cache = ArchiveCache::new(&host, &store);
    let expected = hash("SHA256", HashAlgo::Sha256, b"valid");
    let download = || Ok(b"valid".to_vec());
    let result = cache.fetch_with(URL, Some(&expected), download).unwrap();
    assert_eq!(result.bytes, b"valid");
    assert!(
        matches!(result.cache_warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. }))
    );
    let error = cache
        .fetch_with(URL, Some(&expected), || Ok(b"invalid".to_vec()))
        .unwrap_err();
    assert!(
        matches!(&error, FetchError::Archive { cause, .. }
            if matches!(cause.as_ref(), FetchError::CacheAndDownload { cache, download }
                if matches!(cache.as_ref(), FetchError::CacheRead { path, .. } if path == bad_root)
                    && matches!(download.as_ref(), FetchError::HashMismatch { .. }))),
        "{error}"
    );

    let mut update = LockSession::open(&host, Path::new("/work"), LockMode::Update).unwrap();
    let (result, warning) = update
        .fetch_archive_with(&cache, "dep", URL, None, download)
        .unwrap();
    assert!(warning.is_none());
    assert!(
        matches!(result.cache_warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. }))
    );
    update.commit().unwrap();

    let mut frozen = LockSession::open(&host, Path::new("/work"), LockMode::Frozen).unwrap();
    let (result, warning) = frozen
        .fetch_archive_with(&cache, "dep", URL, None, download)
        .unwrap();
    assert_eq!(result.bytes, b"valid");
    assert!(warning.is_none());
    assert!(
        matches!(result.cache_warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. }))
    );
}

#[test]
fn curl_and_git_default_to_no_timeout_and_accept_a_caller_timeout() {
    use std::time::Duration;

    let host = host();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let timeout = Duration::from_secs(600);
    for (key, value) in [
        ("GIT_SSH_COMMAND", "ssh -F /keys/config"),
        ("GIT_SSL_CAINFO", "/certs/company.pem"),
        ("GIT_SSL_CAPATH", "/certs"),
        ("GIT_ASKPASS", "/bin/askpass"),
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "http.proxy"),
        ("GIT_CONFIG_VALUE_0", "http://proxy.test"),
        ("GIT_DIR", "/other/git"),
        ("GIT_WORK_TREE", "/other"),
        ("GIT_INDEX_FILE", "/other/index"),
    ] {
        host.env_set(key.as_bytes(), Some(value.as_bytes()));
    }
    host.set_command_handler(move |host, req, _| {
        let invocation = req.pipeline.first().unwrap();
        let env = |key: &str| {
            req.env
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_slice())
        };
        let call = observed.fetch_add(1, Ordering::SeqCst);
        let expected_timeout = if call == 0 || (2..7).contains(&call) {
            None
        } else {
            Some(timeout)
        };
        assert_eq!(req.timeout, expected_timeout);
        let stdout = if invocation.program == "curl" {
            b"download".to_vec()
        } else if invocation.args.iter().any(|arg| arg == "--mirror") {
            assert_eq!(
                env("GIT_SSH_COMMAND"),
                Some(b"ssh -F /keys/config".as_slice())
            );
            assert_eq!(
                env("GIT_SSL_CAINFO"),
                Some(b"/certs/company.pem".as_slice())
            );
            assert_eq!(env("GIT_SSL_CAPATH"), Some(b"/certs".as_slice()));
            assert_eq!(env("GIT_ASKPASS"), Some(b"/bin/askpass".as_slice()));
            assert_eq!(env("GIT_CONFIG_KEY_0"), Some(b"http.proxy".as_slice()));
            assert_eq!(
                env("GIT_CONFIG_VALUE_0"),
                Some(b"http://proxy.test".as_slice())
            );
            assert_eq!(env("GIT_CONFIG_COUNT"), Some(b"1".as_slice()));
            assert_eq!(
                env("GIT_ALLOW_PROTOCOL"),
                Some(b"file:git:http:https:ssh".as_slice())
            );
            let destination =
                PathBuf::from(std::str::from_utf8(invocation.args.last().unwrap()).unwrap());
            seed_bare(host, &destination)?;
            Vec::new()
        } else {
            assert!(env("GIT_SSH_COMMAND").is_none());
            assert!(env("GIT_CONFIG_COUNT").is_none());
            if invocation
                .args
                .iter()
                .any(|arg| arg == "--is-bare-repository")
            {
                b"true\n".to_vec()
            } else if invocation.args.iter().any(|arg| arg == "--verify") {
                PIN.as_bytes().to_vec()
            } else {
                b"tree\n".to_vec()
            }
        };
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            if invocation.program == "git" {
                assert!(env(key).is_none());
            }
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout,
            ..RunOutput::default()
        })
    })
    .unwrap();

    let store = Store::new(&host, config()).unwrap();
    ArchiveCache::new(&host, &store).fetch(URL, None).unwrap();
    ArchiveCache::new(&host, &store)
        .with_timeout(timeout)
        .fetch(URL, None)
        .unwrap();
    GitMirror::new(&host, Path::new("/cache/default"))
        .unwrap()
        .prefetch(REPO, PIN)
        .unwrap();
    GitMirror::new(&host, Path::new("/cache/custom"))
        .unwrap()
        .with_timeout(timeout)
        .prefetch(REPO, PIN)
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 12);
}

#[test]
fn prefetch_propagates_the_timeout_and_reports_cache_warnings() {
    use std::time::Duration;

    let host = Arc::new(host());
    let path = Path::new("/work/cache-file");
    host.seed_file(path, b"not a directory").unwrap();
    host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"1"));
    let timeout = Duration::from_secs(420);
    host.set_command_handler(move |_, req, _| {
        assert_eq!(req.timeout, Some(timeout));
        Ok(RunOutput {
            stdout: b"download".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    let pool =
        FetchPool::new_with_timeout(host, StoreConfig::new(path.into()), Some(timeout)).unwrap();
    let ticket = pool
        .submit(PrefetchSource::Url {
            url: URL.into(),
            hash: hash("SHA256", HashAlgo::Sha256, b"download"),
        })
        .unwrap();
    assert!(matches!(
        ticket.wait_with_warning().unwrap(),
        Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. })
    ));
}

fn seed_bare(host: &PureHost, path: &Path) -> rcmake_host::HostResult<()> {
    host.create_dir_all(&path.join("objects"))?;
    host.seed_file(&path.join("HEAD"), b"ref: refs/heads/main\n")
}

#[test]
fn archive_lock_preserves_mirrors_when_the_primary_fails() {
    let host = host();
    let store = Store::new(&host, config()).unwrap();
    let empty = Store::new(&host, StoreConfig::new("/empty".into())).unwrap();
    let cache = ArchiveCache::new(&host, &store);
    let cold = ArchiveCache::new(&host, &empty);
    let source_dir = Path::new("/work");
    let mirrors = [URL, "https://mirror.invalid/archive"];
    let archive = b"archive from mirror";
    let mut update = LockSession::open(&host, source_dir, LockMode::Update).unwrap();
    let mut attempts = Vec::new();
    let (download, warning) = update
        .fetch_archive_urls_with(&cache, "dep", &mirrors, None, |url| {
            attempts.push(url.to_owned());
            if url == URL {
                Err(FetchError::Offline)
            } else {
                Ok(archive.to_vec())
            }
        })
        .unwrap();
    assert!(download.offline_seeded && warning.is_none());
    assert_eq!(attempts, mirrors.map(str::to_owned));
    update.commit().unwrap();

    let file = host
        .read(
            &source_dir.join("rcmake-fetch.lock"),
            ReadRange::All { limit: 4096 },
        )
        .unwrap();
    let lock = LockFile::parse(&file).unwrap();
    assert!(matches!(lock.get("dep"), Some(LockEntry::Url(entry)) if entry.urls == mirrors));
    let mut frozen = LockSession::open(&host, source_dir, LockMode::Frozen).unwrap();
    let (warm, _) = frozen
        .fetch_archive_urls_with(&cache.cache_only(), "dep", &mirrors, None, |_| {
            panic!("Frozen warm fetch must use the content pin.")
        })
        .unwrap();
    assert!(warm.cache_hit);
    let (fetched, _) = frozen
        .fetch_archive_urls_with(&cold, "dep", &mirrors, None, |url| {
            if url == URL {
                Ok(b"stale mirror".to_vec())
            } else {
                Ok(archive.to_vec())
            }
        })
        .unwrap();
    assert!(!fetched.cache_hit && fetched.offline_seeded);
    assert_eq!(fetched.bytes, archive);
    assert!(matches!(
        frozen.fetch_archive_urls_with(&cold, "dep", &[mirrors[1], URL], None, |_| panic!(
            "Check the declaration before using a cache or transport."
        )),
        Err(FetchError::Lock { .. })
    ));
}

#[test]
fn concurrent_lock_updates_agree_or_preserve_the_first_file() {
    let host = host();
    let source = Path::new("/work");
    let path = source.join("rcmake-fetch.lock");
    let make_entry = |bytes: &[u8]| LockEntry::Url(UrlEntry::from_bytes(vec![URL.into()], bytes));
    let session = || LockSession::open(&host, source, LockMode::Update).unwrap();
    let mut first = session();
    let mut identical = session();
    let mut conflict = session();
    first.record("dependency", make_entry(b"first")).unwrap();
    identical
        .record("Dependency", make_entry(b"first"))
        .unwrap();
    conflict
        .record("dependency", make_entry(b"second"))
        .unwrap();
    conflict.record("disjoint", make_entry(b"third")).unwrap();
    first.commit().unwrap();
    let before = host.read(&path, ReadRange::All { limit: 4096 }).unwrap();
    identical.commit().unwrap();
    assert!(
        matches!(conflict.commit(), Err(FetchError::LockConflict { dependency, .. })
        if dependency == "dependency")
    );
    assert_eq!(
        host.read(&path, ReadRange::All { limit: 4096 }).unwrap(),
        before
    );
}

#[test]
fn fetch_errors_and_lockfiles_remove_credentials() {
    let host = host();
    let store = Store::new(&host, config()).unwrap();
    let cache = ArchiveCache::new(&host, &store);
    let url = "https://user:secret@example.invalid/archive";
    let source = Path::new("/work");
    let mut update = LockSession::open(&host, source, LockMode::Update).unwrap();
    update
        .fetch_archive_with(&cache, "dep", url, None, || Ok(b"archive".to_vec()))
        .unwrap();
    update.commit().unwrap();
    let lock = host
        .read(
            &source.join("rcmake-fetch.lock"),
            ReadRange::All { limit: 4096 },
        )
        .unwrap();
    let lock = String::from_utf8(lock).unwrap();
    assert!(lock.contains(URL));
    assert!(!lock.contains("user") && !lock.contains("secret"));
    let mut frozen = LockSession::open(&host, source, LockMode::Frozen).unwrap();
    let expected = hash("SHA512", HashAlgo::Sha512, b"another archive");
    let message = frozen
        .fetch_archive_with(&cache, "dep", url, Some(&expected), || {
            panic!("Check both hashes on a warm hit.")
        })
        .unwrap_err()
        .to_string();
    for label in ["SHA512", "dep", URL, "rcmake-fetch.lock", "URL_HASH"] {
        assert!(message.contains(label));
    }
    assert!(!message.contains("secret"));

    let url = "https://user:secret@example.invalid/archive?token=credential";
    let message = cache
        .cache_only()
        .fetch_with(url, None, || {
            panic!("Cache-only operations must skip network access.")
        })
        .unwrap_err()
        .to_string();
    assert!(message.contains("cache-only mode"));
    assert!(!message.contains("RCMAKE_FETCH_OFFLINE"));
    for secret in ["user", "secret", "token", "credential"] {
        assert!(!message.contains(secret));
    }
}

#[test]
fn busy_alias_publication_does_not_confirm_the_prefetch() {
    use std::time::Duration;
    let host = Arc::new(host());
    let config = config();
    host.create_dir_all(&config.root).unwrap();
    let guard = host
        .lock_file(&config.root.join("gc.lock"), Some(Duration::ZERO))
        .unwrap();
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            stdout: b"archive".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    let pool = FetchPool::new(host.clone(), config).unwrap();
    let source = PrefetchSource::Url {
        url: URL.into(),
        hash: hash("SHA512", HashAlgo::Sha512, b"archive"),
    };
    assert!(matches!(
        pool.submit(source.clone()).unwrap().wait(),
        Err(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Publication {
            outcome: PublishOutcome::SkippedBusy,
        })
    ));
    drop(guard);
    pool.submit(source).unwrap().wait().unwrap();
}

#[test]
fn prefetch_shutdown_releases_tickets_even_if_the_host_ignores_timeouts() {
    use std::{sync::mpsc, time::Duration};
    let host = Arc::new(host());
    host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"1"));
    let (entered, started) = mpsc::channel();
    let (release, allowed) = mpsc::channel();
    let allowed = std::sync::Mutex::new(allowed);
    host.set_command_handler(move |_, req, _| {
        assert_eq!(req.timeout, Some(Duration::from_secs(15)));
        entered.send(()).unwrap();
        allowed
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        Ok(RunOutput {
            results: vec![ProcessResult::TimedOut],
            ..RunOutput::default()
        })
    })
    .unwrap();
    let pool = FetchPool::new(host, config()).unwrap();
    let first = pool
        .submit(PrefetchSource::Url {
            url: URL.into(),
            hash: hash("SHA256", HashAlgo::Sha256, b"one"),
        })
        .unwrap();
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    let queued = pool
        .submit(PrefetchSource::Url {
            url: URL.into(),
            hash: hash("SHA256", HashAlgo::Sha256, b"two"),
        })
        .unwrap();
    let (done, stopped) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        drop(pool);
        done.send(()).unwrap();
    });
    let shutdown_result = stopped.recv_timeout(Duration::from_secs(3));
    release.send(()).unwrap();
    shutdown.join().unwrap();
    assert!(shutdown_result.is_ok());
    assert!(
        first
            .wait()
            .unwrap_err()
            .to_string()
            .contains("shutdown time limit")
    );
    assert!(queued.wait().unwrap_err().to_string().contains("cancelled"));
}

#[test]
fn prefetch_publishes_a_large_archive_within_the_byte_budget() {
    let host = Arc::new(host());
    let bytes = vec![42u8; 16 * 1024 * 1024];
    let expected = hash("SHA512", HashAlgo::Sha512, &bytes);
    host.set_command_handler(move |_, req, _| {
        let args = &req.pipeline.first().unwrap().args;
        assert!(
            args.windows(2)
                .any(|pair| pair.first().is_some_and(|v| v == "--max-filesize")
                    && pair.last().is_some_and(|v| v == "33554432"))
        );
        Ok(RunOutput {
            stdout: bytes.clone(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    let pool = FetchPool::new(host.clone(), config()).unwrap();
    pool.submit(PrefetchSource::Url {
        url: URL.into(),
        hash: expected.clone(),
    })
    .unwrap()
    .wait()
    .unwrap();
    let store = Store::new(host.as_ref(), config()).unwrap();
    let archive = ArchiveCache::new(host.as_ref(), &store)
        .cache_only()
        .fetch(URL, Some(&expected))
        .unwrap();
    assert!(archive.offline_seeded && archive.cache_hit);
    assert_eq!(archive.bytes.len(), 16 * 1024 * 1024);
}

#[test]
fn archive_offline_error_names_the_corrupt_cache_object() {
    use rcmake_host::WriteMode;
    use rcmake_store::Digest;

    let host = host();
    let config = config();
    let store = Store::new(&host, config.clone()).unwrap();
    let cache = ArchiveCache::new(&host, &store).cache_only();
    let bytes = b"the expected archive";
    let digest = Digest::hash(bytes);
    let hash = ContentHash::sha256(digest);
    let hex = digest.to_hex();
    cache.insert(URL, &hash, bytes).unwrap();
    let object = config
        .root
        .join("blobs")
        .join(hex.get(..2).unwrap())
        .join(format!("{hex}.blob"));
    host.write(&object, b"invalid frame", WriteMode::Truncate)
        .unwrap();
    let error = cache
        .fetch_with(URL, Some(&hash), || {
            panic!("Offline mode must not download.")
        })
        .unwrap_err();
    assert!(error.to_string().contains(object.to_str().unwrap()));
    assert!(matches!(error, FetchError::Archive { cause, .. }
        if matches!(cause.as_ref(), FetchError::CacheAndDownload { cache, .. }
            if matches!(cache.as_ref(), FetchError::CacheRead { path, .. } if path == &object))));
}

#[test]
fn legacy_lockfiles_load_and_remove_secrets_on_update() {
    let host = host();
    let dir = Path::new("/work");
    let path = dir.join("rcmake-fetch.lock");
    let mut legacy = LockFile::default();
    let entry = LockEntry::Url(UrlEntry::from_bytes(vec![URL.into()], b"archive"));
    legacy.insert("dependency", entry.clone()).unwrap();
    let raw = String::from_utf8(legacy.to_json().unwrap())
        .unwrap()
        .replace(
            URL,
            "https://legacy:secret@example.invalid/archive?token=hidden",
        );
    host.seed_file(&path, raw.as_bytes()).unwrap();
    LockSession::open(&host, dir, LockMode::Auto).unwrap();
    let mut update = LockSession::open(&host, dir, LockMode::Update).unwrap();
    update.record("another-dependency", entry).unwrap();
    update.commit().unwrap();
    let bytes = host.read(&path, ReadRange::All { limit: 4096 }).unwrap();
    let updated = String::from_utf8(bytes).unwrap();
    for secret in ["legacy", "secret", "token", "hidden"] {
        assert!(!updated.contains(secret));
    }
}
