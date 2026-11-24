//! Verify Git commands against a real local repository.

#![cfg(all(feature = "native", any(unix, windows)))]
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_fetch::{FetchError, GitMirror, classify_git_clone};
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunRequest,
    WriteMode, path::from_native,
};

use rcmake_store::Digest;

struct Scratch<'a> {
    host: &'a dyn Host,
    root: PathBuf,
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.root);
    }
}

fn git(host: &dyn Host, cwd: &Path, args: &[&str]) -> String {
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "git".into(),
                    args: args.iter().map(|arg| (*arg).into()).collect(),
                }],
                cwd: cwd.to_owned(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(10)),
                tee: false,
            },
            1024 * 1024,
        )
        .unwrap();
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0)],
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn mirror_clones_updates_and_checks_out_old_pins() {
    let host = NativeHost::new().unwrap();
    let time = host.now();
    let root = PathBuf::from(format!(
        "/tmp/rcmake-fetch-test-{}-{}",
        time.secs, time.nanos
    ));
    host.create_dir_all(&root).unwrap();
    let scratch = Scratch { host: &host, root };
    let root = scratch.root.as_path();
    let upstream = root.join("upstream");
    host.create_dir_all(&upstream).unwrap();
    git(&host, &upstream, &["init", "-q", "--initial-branch=main"]);
    git(
        &host,
        &upstream,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "First",
            "--allow-empty",
        ],
    );
    let first = git(&host, &upstream, &["rev-parse", "HEAD"]);
    let mirrors = GitMirror::new(&host, &root.join("cache")).unwrap();
    let repository = upstream.to_str().unwrap();
    mirrors.prefetch(repository, &first).unwrap();
    git(&host, &upstream, &["switch", "-qc", "stable"]);

    git(
        &host,
        &upstream,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "Second",
            "--allow-empty",
            "--amend",
        ],
    );
    git(&host, &upstream, &["branch", "-f", "main", "HEAD"]);
    let second = git(&host, &upstream, &["rev-parse", "HEAD"]);
    let template_args = vec![
        "clone".into(),
        "--no-checkout".into(),
        "--depth".into(),
        "1".into(),
        "--origin".into(),
        "upstream".into(),
        from_native(&upstream),
        "latest".into(),
    ];
    let clone = classify_git_clone(&template_args).unwrap();
    mirrors.clone_template(&clone, root, None).unwrap();
    let latest = root.join("latest");
    assert_eq!(mirrors.head(&latest).unwrap(), second);
    assert_eq!(
        git(&host, &latest, &["symbolic-ref", "--short", "HEAD"]),
        "stable"
    );
    let config = host
        .read(&latest.join(".git/config"), ReadRange::All { limit: 4096 })
        .unwrap();
    assert!(String::from_utf8(config).unwrap().contains(repository));

    let frozen_args = vec![
        "clone".into(),
        "--no-checkout".into(),
        "--depth".into(),
        "1".into(),
        from_native(&upstream),
        "frozen".into(),
    ];
    let frozen_clone = classify_git_clone(&frozen_args).unwrap();
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    mirrors
        .clone_template(&frozen_clone, root, Some(&first))
        .unwrap();
    let frozen = root.join("frozen");
    mirrors.checkout_commit(&frozen, &first).unwrap();
    assert_eq!(mirrors.head(&frozen).unwrap(), first);
    let source = root.join("pinned");
    mirrors.checkout(repository, &first, &source).unwrap();
    assert_eq!(mirrors.head(&source).unwrap(), first);
}

#[test]
fn damaged_mirror_repairs_online_and_can_be_used_as_an_immutable_snapshot() {
    let host = NativeHost::new().unwrap();
    let time = host.now();
    let root = PathBuf::from(format!(
        "/tmp/rcmake-fetch-repair-{}-{}",
        time.secs, time.nanos
    ));
    host.create_dir_all(&root).unwrap();
    let scratch = Scratch { host: &host, root };
    let root = scratch.root.as_path();
    let cache = root.join("cache");
    let upstream = root.join("upstream");
    let disposable = root.join("disposable");
    for repo in [&upstream, &disposable] {
        host.create_dir_all(repo).unwrap();
        git(&host, repo, &["init", "-q", "--initial-branch=main"]);
        host.write(
            &repo.join("fixture.txt"),
            b"tracked content\n",
            WriteMode::CreateNew,
        )
        .unwrap();
        git(&host, repo, &["add", "--", "fixture.txt"]);
        git(
            &host,
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "Initial commit",
                "--allow-empty",
            ],
        );
    }
    let pin = git(&host, &upstream, &["rev-parse", "HEAD"]);
    let repo = upstream.to_str().unwrap();
    let digest = Digest::hash(repo.as_bytes());
    let mirror_path = cache.join("git").join(format!("{digest}.git"));
    host.create_dir_all(&mirror_path).unwrap();

    let mirror = GitMirror::new(&host, &cache)
        .unwrap()
        .with_timeout(Duration::from_secs(10));
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    let error = mirror.prefetch(repo, &pin).unwrap_err();
    assert!(matches!(error, FetchError::Git { path, commit, cause, .. }
        if path == mirror_path && commit.as_deref() == Some(pin.as_str())
            && *cause == FetchError::DamagedGitMirror));
    host.env_set(b"RCMAKE_FETCH_OFFLINE", None);
    assert_eq!(mirror.prefetch(repo, &pin).unwrap(), mirror_path);

    let tree = git(&host, &upstream, &["rev-parse", "HEAD^{tree}"]);
    let tree_object = mirror_path
        .join("objects")
        .join(&tree[..2])
        .join(&tree[2..]);
    host.remove_file(&tree_object).unwrap();
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    assert!(
        matches!(mirror.prefetch(repo, &pin), Err(FetchError::Git { cause, .. })
        if cause.as_ref() == &FetchError::DamagedGitMirror)
    );
    host.env_set(b"RCMAKE_FETCH_OFFLINE", None);
    assert_eq!(mirror.prefetch(repo, &pin).unwrap(), mirror_path);

    let template = [
        "clone".into(),
        "--no-checkout".into(),
        from_native(&disposable),
        "unlocked".into(),
    ];
    let clone = classify_git_clone(&template).unwrap();
    mirror.clone_template(&clone, root, None).unwrap();
    let unpinned_digest = Digest::hash(&from_native(&disposable));
    let unpinned = cache.join("git").join(format!("{unpinned_digest}.git"));
    let guard = host
        .lock_file(&unpinned.with_added_extension("lock"), Some(Duration::ZERO))
        .unwrap();
    host.env_set(b"RCMAKE_FETCH_GIT_MAX_SIZE", Some(b"0"));
    let busy = mirror.collect().unwrap();
    assert_eq!(busy.busy_skipped, 1);
    assert_eq!(busy.mirrors_removed, 0);
    drop(guard);
    let report = mirror.collect().unwrap();
    assert_eq!(report.mirrors_removed, 1);
    assert_eq!(report.pinned_retained, 1);
    assert!(report.bytes_before > report.bytes_after && report.bytes_after > 0);
    assert!(host.metadata(&unpinned, Follow::No).is_err());
    let lock = mirror_path.with_added_extension("lock");
    host.remove_file(&lock).unwrap();
    let snapshot = mirror.read_only_cache();
    assert_eq!(snapshot.prefetch(repo, &pin).unwrap(), mirror_path);
    let checkout = root.join("snapshot-checkout");
    snapshot.checkout(repo, &pin, &checkout).unwrap();
    assert_eq!(snapshot.head(&checkout).unwrap(), pin);
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    host.env_set(b"RCMAKE_FETCH_GIT_READ_ONLY", Some(b"1"));
    assert_eq!(
        GitMirror::new(&host, &cache)
            .unwrap()
            .prefetch(repo, &pin)
            .unwrap(),
        mirror_path
    );
    assert!(host.metadata(&lock, Follow::No).is_err());
}
