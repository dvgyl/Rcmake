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
use rcmake_host::{Host, ProcessResult, PureHost, RunOutput};
use rcmake_store::{Store, StoreConfig};

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
        Err(FetchError::HashMismatch { .. })
    ));
    let mut auto = LockSession::open(&host, source, LockMode::Auto).unwrap();
    assert_eq!(auto.git_pin("git", REPO, Some("v1")).unwrap(), None);
    let (archive, warning) = auto
        .fetch_archive_with(&cache, "dep", URL, None, || Ok(b"changed".to_vec()))
        .unwrap();
    assert_eq!(archive.bytes, b"changed");
    assert!(!archive.cache_hit);
    assert!(matches!(warning, Some(FetchError::HashMismatch { .. })));
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
        Err(FetchError::HashMismatch { .. })
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
    assert_eq!(
        cache.fetch_with(URL, None, || panic!("offline miss must not download")),
        Err(FetchError::Offline)
    );
    let other = hash("SHA256", HashAlgo::Sha256, b"other");
    assert_eq!(
        cache.fetch_with(URL, Some(&other), || panic!(
            "offline miss must not download"
        )),
        Err(FetchError::Offline)
    );
}

#[test]
fn git_offline_miss_does_not_run_a_process() {
    let host = host();
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    host.set_command_handler(|_, _, _| panic!("offline miss must not run Git"))
        .unwrap();
    let mirror = GitMirror::new(&host, Path::new("/cache")).unwrap();
    assert_eq!(mirror.prefetch(REPO, PIN), Err(FetchError::Offline));
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
            host.create_dir_all(&destination)?;
            Vec::new()
        } else if git.iter().any(|arg| arg == "cat-file") {
            b"commit\n".to_vec()
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
    let second = pool.submit(source).unwrap();
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
    assert!(matches!(result.cache_warning, Some(FetchError::Host(_))));
    assert!(matches!(
        cache.fetch_with(URL, Some(&expected), || Ok(b"invalid".to_vec())),
        Err(FetchError::HashMismatch { .. })
    ));

    let mut update = LockSession::open(&host, Path::new("/work"), LockMode::Update).unwrap();
    let (result, warning) = update
        .fetch_archive_with(&cache, "dep", URL, None, download)
        .unwrap();
    assert!(warning.is_none());
    assert!(matches!(result.cache_warning, Some(FetchError::Host(_))));
    update.commit().unwrap();

    let mut frozen = LockSession::open(&host, Path::new("/work"), LockMode::Frozen).unwrap();
    let (result, warning) = frozen
        .fetch_archive_with(&cache, "dep", URL, None, download)
        .unwrap();
    assert_eq!(result.bytes, b"valid");
    assert!(warning.is_none());
    assert!(matches!(result.cache_warning, Some(FetchError::Host(_))));
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
        let expected_timeout = if call == 0 || (2..6).contains(&call) {
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
            host.create_dir_all(&destination)?;
            Vec::new()
        } else {
            assert!(env("GIT_SSH_COMMAND").is_none());
            assert!(env("GIT_CONFIG_COUNT").is_none());
            b"commit\n".to_vec()
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
    assert_eq!(calls.load(Ordering::SeqCst), 10);
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
        Some(FetchError::Host(_))
    ));
}
