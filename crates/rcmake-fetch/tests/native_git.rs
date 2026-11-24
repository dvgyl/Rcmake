//! Verify Git commands against a real local repository.

#![cfg(all(feature = "native", any(unix, windows)))]
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_fetch::{GitMirror, classify_git_clone};
use rcmake_host::{
    CommandSpec, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunRequest,
    path::from_native,
};

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
