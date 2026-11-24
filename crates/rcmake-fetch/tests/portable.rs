//! Fetch command and option checks with default features.

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_fetch::{LockMode, classify_git_checkout, classify_git_clone};
use rcmake_host::{Host, PureHost};

const REPO: &str = "https://example.test/fixture.git";

fn args(values: &[&str]) -> Vec<BString> {
    values.iter().map(|value| (*value).into()).collect()
}

#[test]
fn git_classifier_tracks_vendored_template_and_rejects_other_shapes()
-> Result<(), Box<dyn std::error::Error>> {
    let template =
        include_str!("../../../modules/cmake-4.5/Modules/ExternalProject/gitclone.cmake.in");
    let render = |line: &str| {
        line.replace("@git_clone_options@", "--no-checkout --depth 1 --no-single-branch --progress --config advice.detachedHead=false --origin origin2 -c http.sslVerify=true")
            .replace("\"@git_repository@\"", REPO).replace("\"@src_name@\"", "source")
            .replace("\"@git_tag@\"", "v1").replace("@git_checkout_explicit--@", "--")
            .split_whitespace().map(BString::from).collect::<Vec<_>>()
    };
    let clone = render(
        template
            .lines()
            .find(|line| line.contains("clone @git_clone_options@"))
            .ok_or("Missing Git command in the CMake template.")?,
    );
    let clone = classify_git_clone(&clone).ok_or("Invalid Git clone arguments.")?;
    assert_eq!(
        (clone.repository, clone.directory, clone.origin),
        (REPO, "source", "origin2")
    );
    let checkout = render(
        template
            .lines()
            .find(|line| line.contains("checkout \"@git_tag@\""))
            .ok_or("Missing Git command in the CMake template.")?,
    );
    assert_eq!(classify_git_checkout(&checkout), Some("v1"));
    assert_eq!(
        classify_git_checkout(&args(&["checkout", "v1"])),
        Some("v1")
    );
    assert!(
        classify_git_clone(&args(&[
            "-c",
            "http.sslVersion=tlsv1.2",
            "clone",
            REPO,
            "dir"
        ]))
        .is_some()
    );
    for invocation in [
        ["clone", "--mirror", REPO, "dir"],
        ["clone", "--shared", REPO, "dir"],
        ["fetch", "origin", REPO, "dir"],
    ] {
        assert!(classify_git_clone(&args(&invocation)).is_none());
    }
    assert!(classify_git_checkout(&args(&["checkout", "--", "some-file"])).is_none());
    assert!(classify_git_checkout(&args(&["checkout", "v1", "--", "some-file"])).is_none());
    Ok(())
}

#[test]
fn classifies_the_argv_emitted_by_fetchcontent_4_5_1() -> Result<(), Box<dyn std::error::Error>> {
    // Captured with GIT_TRACE during FetchContent_MakeAvailable(fixture).
    let argv = args(&[
        "clone",
        "--no-checkout",
        "--depth",
        "1",
        "--no-single-branch",
        "--progress",
        "--config",
        "advice.detachedHead=false",
        "--origin",
        "origin2",
        "-c",
        "http.sslVerify=true",
        REPO,
        "fixture-src",
    ]);
    let clone = classify_git_clone(&argv).ok_or("Invalid Git clone arguments.")?;
    assert_eq!(
        (clone.repository, clone.directory, clone.origin),
        (REPO, "fixture-src", "origin2")
    );
    Ok(())
}

#[test]
fn lock_mode_reads_the_session_environment_key() {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let host = PureHost::new(root.into(), format!("{root}/cmake").into());
    assert_eq!(LockMode::from_host(&host), Ok(LockMode::Auto));
    host.env_set(b"RCMAKE_FETCH_LOCK_MODE", Some(b"off"));
    host.env_set(b"RCMAKE_FETCH_LOCK", Some(b"frozen"));
    assert_eq!(LockMode::from_host(&host), Ok(LockMode::Frozen));
    host.env_set(b"RCMAKE_FETCH_LOCK", Some(b"unknown"));
    assert!(LockMode::from_host(&host).is_err());
}
