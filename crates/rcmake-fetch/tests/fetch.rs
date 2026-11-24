//! Fetch tests with an in-memory host and simulated tools.

#![cfg(all(feature = "native", any(unix, windows)))]
#![forbid(unsafe_code)]
#![allow(clippy::panic)]

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

fn hash(algorithm: &str, algo: HashAlgo, bytes: &[u8]) -> Result<ContentHash, FetchError> {
    ContentHash::parse(&format!("{algorithm}={}", algo.hex_digest(bytes)))
}

#[track_caller]
fn fetch_error<T>(result: Result<T, FetchError>) -> FetchError {
    let Err(error) = result else {
        panic!("Expected a fetch error.");
    };
    error
}

#[test]
fn lock_round_trip_is_sorted_and_uses_version_one_json() -> Result<(), Box<dyn std::error::Error>> {
    let mut lock = LockFile::default();
    lock.insert(
        "Zlib",
        LockEntry::Url(UrlEntry::from_bytes(vec![URL.into()], b"data")),
    )?;
    lock.insert(
        "Fmt",
        LockEntry::Git(GitEntry {
            repository: REPO.into(),
            tag: Some("11.0.2".into()),
            commit: PIN.into(),
        }),
    )?;
    let json = lock.to_json()?;
    let text = std::str::from_utf8(&json)?;
    assert!(text.ends_with("\n"));
    let fmt = text.find("\"fmt\"").ok_or("Missing fmt entry.")?;
    let zlib = text.find("\"zlib\"").ok_or("Missing zlib entry.")?;
    assert!(fmt < zlib);
    assert_eq!(LockFile::parse(&json)?, lock);
    assert!(matches!(lock.get("FMT"), Some(LockEntry::Git(_))));
    assert!(matches!(lock.get("zlib"), Some(LockEntry::Url(_))));
    Ok(())
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
fn lock_modes_apply_and_concurrent_updates_merge() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let source = Path::new("/work");
    let path = source.join("rcmake-fetch.lock");
    let store = Store::new(&host, config())?;
    let cache = ArchiveCache::new(&host, &store);
    assert!(LockSession::open(&host, source, LockMode::Frozen).is_err());
    host.seed_file(&path, b"malformed")?;
    let mut off = LockSession::open(&host, source, LockMode::Off)?;
    assert!(LockSession::open(&host, source, LockMode::Auto).is_err());
    assert!(
        off.fetch_archive_with(&cache, "dep", URL, None, || Ok(b"old".to_vec()))?
            .1
            .is_none()
    );
    off.commit()?;
    host.remove_file(&path)?;

    let mut update = LockSession::open(&host, source, LockMode::Update)?;
    let mut concurrent = LockSession::open(&host, source, LockMode::Update)?;
    update.fetch_archive_with(&cache, "dep", URL, None, || Ok(b"new".to_vec()))?;
    concurrent.record(
        "git",
        LockEntry::Git(GitEntry {
            repository: REPO.into(),
            tag: Some("v1".into()),
            commit: PIN.into(),
        }),
    )?;
    update.commit()?;
    concurrent.commit()?;

    let mut frozen = LockSession::open(&host, source, LockMode::Frozen)?;
    assert_eq!(frozen.git_pin("GIT", REPO, Some("v1"))?, Some(PIN));
    assert!(frozen.git_pin("git", REPO, Some("v2")).is_err());
    let (archive, warning) =
        frozen.fetch_archive_with(&cache, "DEP", URL, None, || Err(FetchError::Offline))?;
    assert!(archive.cache_hit && warning.is_none());
    assert_eq!(archive.bytes, b"new");
    assert!(
        frozen
            .fetch_archive_with(&cache, "absent", URL, None, || Ok(Vec::new()))
            .is_err()
    );
    let empty = Store::new(&host, StoreConfig::new("/empty".into()))?;
    let empty = ArchiveCache::new(&host, &empty);
    assert!(matches!(
        frozen.fetch_archive_with(&empty, "dep", URL, None, || Ok(b"changed".to_vec())),
        Err(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::HashMismatch { .. })
    ));
    let mut auto = LockSession::open(&host, source, LockMode::Auto)?;
    assert_eq!(auto.git_pin("git", REPO, Some("v1"))?, None);
    let (archive, warning) =
        auto.fetch_archive_with(&cache, "dep", URL, None, || Ok(b"changed".to_vec()))?;
    assert_eq!(archive.bytes, b"changed");
    assert!(!archive.cache_hit);
    assert!(
        matches!(warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::HashMismatch { .. }))
    );
    let expected = hash("SHA256", HashAlgo::Sha256, b"changed")?;
    assert!(
        auto.fetch_archive_with(&empty, "dep", URL, Some(&expected), || Ok(
            b"changed".to_vec()
        ))?
        .1
        .is_none()
    );
    auto.commit()?;
    let frozen = LockSession::open(&host, source, LockMode::Frozen)?;
    assert_eq!(frozen.git_pin("git", REPO, Some("v1"))?, Some(PIN));
    Ok(())
}

#[test]
fn archive_cache_checks_non_sha256_aliases_and_offline_misses()
-> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let store = Store::new(&host, config())?;
    let cache = ArchiveCache::new(&host, &store);
    let expected = hash("MD5", HashAlgo::Md5, b"archive")?;
    assert!(cache.lookup(URL, &expected)?.is_none());
    assert!(matches!(
        cache.fetch_with(URL, Some(&expected), || Ok(b"wrong".to_vec())),
        Err(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::HashMismatch { .. })
    ));
    assert!(cache.lookup(URL, &expected)?.is_none());
    let miss = cache.fetch_with(URL, Some(&expected), || Ok(b"archive".to_vec()))?;
    assert!(!miss.cache_hit);
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    let hit = cache.fetch_with("https://mirror.invalid/archive", Some(&expected), || {
        panic!("cache hit must use the store")
    })?;
    assert!(hit.cache_hit);
    assert_eq!(hit.bytes, b"archive");
    let error =
        fetch_error(cache.fetch_with(URL, None, || panic!("offline miss must not download")));
    assert!(error.to_string().contains("RCMAKE_FETCH_OFFLINE=1"));
    assert!(error.to_string().contains("URL_HASH"));
    let other = hash("SHA256", HashAlgo::Sha256, b"other")?;
    let error = fetch_error(cache.fetch_with(URL, Some(&other), || {
        panic!("offline miss must not download")
    }));
    assert!(error.to_string().contains("SHA256="));
    Ok(())
}

#[test]
fn git_offline_miss_does_not_run_a_process() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    host.set_command_handler(|_, _, _| panic!("offline miss must not run Git"))?;
    let mirror = GitMirror::new(&host, Path::new("/cache"))?;
    assert!(matches!(mirror.prefetch(REPO, PIN), Err(FetchError::Git {
        repository, commit, path, ..
    }) if repository == REPO && commit.as_deref() == Some(PIN) && path.starts_with("/cache/git")));
    assert!(mirror.prefetch(REPO, "main").is_err());
    Ok(())
}

#[test]
fn pinned_git_mirror_reuses_local_commit() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    host.set_command_handler(move |host, req, _| {
        let Some(invocation) = req.pipeline.first() else {
            panic!("Missing Git command.");
        };
        let git = &invocation.args;
        let stdout = if git.first().is_some_and(|arg| arg == "clone") {
            observed.fetch_add(1, Ordering::SeqCst);
            let Some(destination) = git.last().and_then(|arg| std::str::from_utf8(arg).ok()) else {
                panic!("Missing UTF-8 Git destination.");
            };
            let destination = PathBuf::from(destination);
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
    })?;
    let mirrors = GitMirror::new(&host, Path::new("/cache"))?;
    let first = mirrors.prefetch(REPO, PIN)?;
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    assert_eq!(mirrors.prefetch(REPO, PIN)?, first);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn pool_deduplicates_pins_and_populates_store() -> Result<(), Box<dyn std::error::Error>> {
    let host = Arc::new(host());
    host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"0"));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    host.set_command_handler(move |_, req, _| {
        assert!(
            req.pipeline
                .first()
                .is_some_and(|cmd| cmd.program == "curl")
        );
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: b"archive".to_vec(),
            ..RunOutput::default()
        })
    })?;
    let pool = FetchPool::new(host.clone(), config())?;
    let hash = hash("SHA256", HashAlgo::Sha256, b"archive")?;
    let source = PrefetchSource::Url {
        url: URL.into(),
        hash: hash.clone(),
    };
    let first = pool.submit(source.clone())?;
    let second = pool.submit(source.clone())?;
    first.wait()?;
    second.wait()?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let store = Store::new(host.as_ref(), config())?;
    assert_eq!(
        ArchiveCache::new(host.as_ref(), &store).lookup(URL, &hash)?,
        Some(b"archive".to_vec())
    );
    let digest = rcmake_store::Digest::hash(b"archive").to_hex();
    let path = config()
        .root
        .join("blobs")
        .join(digest.get(..2).ok_or("Missing digest prefix.")?)
        .join(format!("{digest}.blob"));
    host.remove_file(&path)?;
    pool.submit(source)?.wait()?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        pool.submit(PrefetchSource::Git {
            repository: REPO.into(),
            commit: "main".into()
        })
        .is_err()
    );
    Ok(())
}

#[test]
fn store_write_errors_leave_verified_downloads_usable_in_each_lock_mode()
-> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let bad_root = Path::new("/work/cache-file");
    host.seed_file(bad_root, b"not a directory")?;
    let store = Store::new(&host, StoreConfig::new(bad_root.into()))?;
    let cache = ArchiveCache::new(&host, &store);
    let expected = hash("SHA256", HashAlgo::Sha256, b"valid")?;
    let download = || Ok(b"valid".to_vec());
    let result = cache.fetch_with(URL, Some(&expected), download)?;
    assert_eq!(result.bytes, b"valid");
    assert!(
        matches!(result.cache_warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. }))
    );
    let error = fetch_error(cache.fetch_with(URL, Some(&expected), || Ok(b"invalid".to_vec())));
    assert!(
        matches!(&error, FetchError::Archive { cause, .. }
            if matches!(cause.as_ref(), FetchError::CacheAndDownload { cache, download }
                if matches!(cache.as_ref(), FetchError::CacheRead { path, .. } if path == bad_root)
                    && matches!(download.as_ref(), FetchError::HashMismatch { .. }))),
        "{error}"
    );

    let mut update = LockSession::open(&host, Path::new("/work"), LockMode::Update)?;
    let (result, warning) = update.fetch_archive_with(&cache, "dep", URL, None, download)?;
    assert!(warning.is_none());
    assert!(
        matches!(result.cache_warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. }))
    );
    update.commit()?;

    let mut frozen = LockSession::open(&host, Path::new("/work"), LockMode::Frozen)?;
    let (result, warning) = frozen.fetch_archive_with(&cache, "dep", URL, None, download)?;
    assert_eq!(result.bytes, b"valid");
    assert!(warning.is_none());
    assert!(
        matches!(result.cache_warning, Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. }))
    );
    Ok(())
}

#[test]
fn curl_and_git_default_to_no_timeout_and_accept_a_caller_timeout()
-> Result<(), Box<dyn std::error::Error>> {
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
        let Some(invocation) = req.pipeline.first() else {
            panic!("Missing command.");
        };
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
            let Some(destination) = invocation
                .args
                .last()
                .and_then(|arg| std::str::from_utf8(arg).ok())
            else {
                panic!("Missing UTF-8 Git destination.");
            };
            let destination = PathBuf::from(destination);
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
    })?;

    let store = Store::new(&host, config())?;
    ArchiveCache::new(&host, &store).fetch(URL, None)?;
    ArchiveCache::new(&host, &store)
        .with_timeout(timeout)
        .fetch(URL, None)?;
    GitMirror::new(&host, Path::new("/cache/default"))?.prefetch(REPO, PIN)?;
    GitMirror::new(&host, Path::new("/cache/custom"))?
        .with_timeout(timeout)
        .prefetch(REPO, PIN)?;
    assert_eq!(calls.load(Ordering::SeqCst), 12);
    Ok(())
}

#[test]
fn prefetch_propagates_the_timeout_and_reports_cache_warnings()
-> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    let host = Arc::new(host());
    let path = Path::new("/work/cache-file");
    host.seed_file(path, b"not a directory")?;
    host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"1"));
    let timeout = Duration::from_secs(420);
    host.set_command_handler(move |_, req, _| {
        assert_eq!(req.timeout, Some(timeout));
        Ok(RunOutput {
            stdout: b"download".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let pool = FetchPool::new_with_timeout(host, StoreConfig::new(path.into()), Some(timeout))?;
    let ticket = pool.submit(PrefetchSource::Url {
        url: URL.into(),
        hash: hash("SHA256", HashAlgo::Sha256, b"download")?,
    })?;
    assert!(matches!(
        ticket.wait_with_warning()?,
        Some(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Host(_) | FetchError::CacheRead { .. })
    ));
    Ok(())
}

fn seed_bare(host: &PureHost, path: &Path) -> rcmake_host::HostResult<()> {
    host.create_dir_all(&path.join("objects"))?;
    host.seed_file(&path.join("HEAD"), b"ref: refs/heads/main\n")
}

#[test]
fn archive_lock_preserves_mirrors_when_the_primary_fails() -> Result<(), Box<dyn std::error::Error>>
{
    let host = host();
    let store = Store::new(&host, config())?;
    let empty = Store::new(&host, StoreConfig::new("/empty".into()))?;
    let cache = ArchiveCache::new(&host, &store);
    let cold = ArchiveCache::new(&host, &empty);
    let source_dir = Path::new("/work");
    let mirror = "https://mirror.invalid/archive";
    let mirrors = [URL, mirror];
    let archive = b"archive from mirror";
    let mut update = LockSession::open(&host, source_dir, LockMode::Update)?;
    let mut attempts = Vec::new();
    let (download, warning) =
        update.fetch_archive_urls_with(&cache, "dep", &mirrors, None, |url| {
            attempts.push(url.to_owned());
            if url == URL {
                Err(FetchError::Offline)
            } else {
                Ok(archive.to_vec())
            }
        })?;
    assert!(download.offline_seeded && warning.is_none());
    assert_eq!(attempts, mirrors.map(str::to_owned));
    update.commit()?;

    let file = host.read(
        &source_dir.join("rcmake-fetch.lock"),
        ReadRange::All { limit: 4096 },
    )?;
    let lock = LockFile::parse(&file)?;
    assert!(matches!(lock.get("dep"), Some(LockEntry::Url(entry)) if entry.urls == mirrors));
    let mut frozen = LockSession::open(&host, source_dir, LockMode::Frozen)?;
    let (warm, _) =
        frozen.fetch_archive_urls_with(&cache.cache_only(), "dep", &mirrors, None, |_| {
            panic!("Frozen warm fetch must use the content pin.")
        })?;
    assert!(warm.cache_hit);
    let (fetched, _) = frozen.fetch_archive_urls_with(&cold, "dep", &mirrors, None, |url| {
        if url == URL {
            Ok(b"stale mirror".to_vec())
        } else {
            Ok(archive.to_vec())
        }
    })?;
    assert!(!fetched.cache_hit && fetched.offline_seeded);
    assert_eq!(fetched.bytes, archive);
    assert!(matches!(
        frozen.fetch_archive_urls_with(&cold, "dep", &[mirror, URL], None, |_| panic!(
            "Check the declaration before using a cache or transport."
        )),
        Err(FetchError::Lock { .. })
    ));
    Ok(())
}

#[test]
fn concurrent_lock_updates_agree_or_preserve_the_first_file()
-> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let source = Path::new("/work");
    let path = source.join("rcmake-fetch.lock");
    let make_entry = |bytes: &[u8]| LockEntry::Url(UrlEntry::from_bytes(vec![URL.into()], bytes));
    let session = || LockSession::open(&host, source, LockMode::Update);
    let mut first = session()?;
    let mut identical = session()?;
    let mut conflict = session()?;
    first.record("dependency", make_entry(b"first"))?;
    identical.record("Dependency", make_entry(b"first"))?;
    conflict.record("dependency", make_entry(b"second"))?;
    conflict.record("disjoint", make_entry(b"third"))?;
    first.commit()?;
    let before = host.read(&path, ReadRange::All { limit: 4096 })?;
    identical.commit()?;
    assert!(
        matches!(conflict.commit(), Err(FetchError::LockConflict { dependency, .. })
        if dependency == "dependency")
    );
    assert_eq!(host.read(&path, ReadRange::All { limit: 4096 })?, before);
    Ok(())
}

#[test]
fn fetch_errors_and_lockfiles_remove_credentials() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let store = Store::new(&host, config())?;
    let cache = ArchiveCache::new(&host, &store);
    let url = "https://user:secret@example.invalid/archive";
    let source = Path::new("/work");
    let mut update = LockSession::open(&host, source, LockMode::Update)?;
    update.fetch_archive_with(&cache, "dep", url, None, || Ok(b"archive".to_vec()))?;
    update.commit()?;
    let lock = host.read(
        &source.join("rcmake-fetch.lock"),
        ReadRange::All { limit: 4096 },
    )?;
    let lock = String::from_utf8(lock)?;
    assert!(lock.contains(URL));
    assert!(!lock.contains("user") && !lock.contains("secret"));
    let mut frozen = LockSession::open(&host, source, LockMode::Frozen)?;
    let expected = hash("SHA512", HashAlgo::Sha512, b"another archive")?;
    let message =
        fetch_error(
            frozen.fetch_archive_with(&cache, "dep", url, Some(&expected), || {
                panic!("Check both hashes on a warm hit.")
            }),
        )
        .to_string();
    for label in ["SHA512", "dep", URL, "rcmake-fetch.lock", "URL_HASH"] {
        assert!(message.contains(label));
    }
    assert!(!message.contains("secret"));

    let url = "https://user:secret@example.invalid/archive?token=credential";
    let message = fetch_error(cache.cache_only().fetch_with(url, None, || {
        panic!("Cache-only operations must skip network access.")
    }))
    .to_string();
    assert!(message.contains("cache-only mode"));
    assert!(!message.contains("RCMAKE_FETCH_OFFLINE"));
    for secret in ["user", "secret", "token", "credential"] {
        assert!(!message.contains(secret));
    }
    Ok(())
}

#[test]
fn busy_alias_publication_does_not_confirm_the_prefetch() -> Result<(), Box<dyn std::error::Error>>
{
    use std::time::Duration;
    let host = Arc::new(host());
    let config = config();
    host.create_dir_all(&config.root)?;
    let guard = host.lock_file(&config.root.join("gc.lock"), Some(Duration::ZERO))?;
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            stdout: b"archive".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let pool = FetchPool::new(host.clone(), config)?;
    let source = PrefetchSource::Url {
        url: URL.into(),
        hash: hash("SHA512", HashAlgo::Sha512, b"archive")?,
    };
    assert!(matches!(
        pool.submit(source.clone())?.wait(),
        Err(FetchError::Archive { cause, .. }) if matches!(*cause, FetchError::Publication {
            outcome: PublishOutcome::SkippedBusy,
        })
    ));
    drop(guard);
    pool.submit(source)?.wait()?;
    Ok(())
}

#[test]
fn prefetch_shutdown_releases_tickets_even_if_the_host_ignores_timeouts()
-> Result<(), Box<dyn std::error::Error>> {
    use std::{sync::mpsc, time::Duration};
    let host = Arc::new(host());
    host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"1"));
    let (entered, started) = mpsc::channel();
    let (release, allowed) = mpsc::channel();
    let allowed = std::sync::Mutex::new(allowed);
    host.set_command_handler(move |_, req, _| {
        assert_eq!(req.timeout, Some(Duration::from_secs(15)));
        assert!(entered.send(()).is_ok());
        let Ok(allowed) = allowed.lock() else {
            panic!("Cannot lock the release channel.");
        };
        assert!(allowed.recv_timeout(Duration::from_secs(5)).is_ok());
        Ok(RunOutput {
            results: vec![ProcessResult::TimedOut],
            ..RunOutput::default()
        })
    })?;
    let pool = FetchPool::new(host, config())?;
    let first = pool.submit(PrefetchSource::Url {
        url: URL.into(),
        hash: hash("SHA256", HashAlgo::Sha256, b"one")?,
    })?;
    started.recv_timeout(Duration::from_secs(5))?;
    let queued = pool.submit(PrefetchSource::Url {
        url: URL.into(),
        hash: hash("SHA256", HashAlgo::Sha256, b"two")?,
    })?;
    let (done, stopped) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        drop(pool);
        done.send(())
    });
    let shutdown_result = stopped.recv_timeout(Duration::from_secs(3));
    release.send(())?;
    shutdown
        .join()
        .map_err(|_| "The shutdown thread panicked.")??;
    assert!(shutdown_result.is_ok());
    assert!(
        fetch_error(first.wait())
            .to_string()
            .contains("shutdown time limit")
    );
    assert!(fetch_error(queued.wait()).to_string().contains("cancelled"));
    Ok(())
}

#[test]
fn prefetch_publishes_a_large_archive_within_the_byte_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let host = Arc::new(host());
    let bytes = vec![42u8; 16 * 1024 * 1024];
    let expected = hash("SHA512", HashAlgo::Sha512, &bytes)?;
    host.set_command_handler(move |_, req, _| {
        let Some(invocation) = req.pipeline.first() else {
            panic!("Missing curl command.");
        };
        let args = &invocation.args;
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
    })?;
    let pool = FetchPool::new(host.clone(), config())?;
    pool.submit(PrefetchSource::Url {
        url: URL.into(),
        hash: expected.clone(),
    })?
    .wait()?;
    let store = Store::new(host.as_ref(), config())?;
    let archive = ArchiveCache::new(host.as_ref(), &store)
        .cache_only()
        .fetch(URL, Some(&expected))?;
    assert!(archive.offline_seeded && archive.cache_hit);
    assert_eq!(archive.bytes.len(), 16 * 1024 * 1024);
    Ok(())
}

#[test]
fn archive_offline_error_names_the_corrupt_cache_object() -> Result<(), Box<dyn std::error::Error>>
{
    use rcmake_host::WriteMode;
    use rcmake_store::Digest;

    let host = host();
    let config = config();
    let store = Store::new(&host, config.clone())?;
    let cache = ArchiveCache::new(&host, &store).cache_only();
    let bytes = b"the expected archive";
    let digest = Digest::hash(bytes);
    let hash = ContentHash::sha256(digest);
    let hex = digest.to_hex();
    cache.insert(URL, &hash, bytes)?;
    let object = config
        .root
        .join("blobs")
        .join(hex.get(..2).ok_or("Missing digest prefix.")?)
        .join(format!("{hex}.blob"));
    host.write(&object, b"invalid frame", WriteMode::Truncate)?;
    let error = fetch_error(cache.fetch_with(URL, Some(&hash), || {
        panic!("Offline mode must not download.")
    }));
    assert!(
        error
            .to_string()
            .contains(object.to_str().ok_or("The cache path is not UTF-8.")?)
    );
    assert!(matches!(error, FetchError::Archive { cause, .. }
        if matches!(cause.as_ref(), FetchError::CacheAndDownload { cache, .. }
            if matches!(cache.as_ref(), FetchError::CacheRead { path, .. } if path == &object))));
    Ok(())
}

#[test]
fn legacy_lockfiles_load_and_remove_secrets_on_update() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let dir = Path::new("/work");
    let path = dir.join("rcmake-fetch.lock");
    let mut legacy = LockFile::default();
    let entry = LockEntry::Url(UrlEntry::from_bytes(vec![URL.into()], b"archive"));
    legacy.insert("dependency", entry.clone())?;
    let raw = String::from_utf8(legacy.to_json()?)?.replace(
        URL,
        "https://legacy:secret@example.invalid/archive?token=hidden",
    );
    host.seed_file(&path, raw.as_bytes())?;
    LockSession::open(&host, dir, LockMode::Auto)?;
    let mut update = LockSession::open(&host, dir, LockMode::Update)?;
    update.record("another-dependency", entry)?;
    update.commit()?;
    let bytes = host.read(&path, ReadRange::All { limit: 4096 })?;
    let updated = String::from_utf8(bytes)?;
    for secret in ["legacy", "secret", "token", "hidden"] {
        assert!(!updated.contains(secret));
    }
    Ok(())
}
