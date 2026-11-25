// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use rcmake_eval::{FetchLockMode, Interp, InterpConfig, RunMode, Services};
use rcmake_host::{Host, HostError, ProcessResult, PureHost, ReadRange, RunOutput, Stream, path};

use super::FetchRuntime;

type TestResult = Result<(), Box<dyn std::error::Error>>;

macro_rules! template {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/cmake-4.5/Modules/",
            $name
        ))
    };
}

fn absolute(name: &str) -> PathBuf {
    if cfg!(windows) {
        format!("C:/fetch-test/{name}").into()
    } else {
        format!("/fetch-test/{name}").into()
    }
}

fn host() -> Result<Arc<PureHost>, HostError> {
    let root = absolute("cmake");
    let host = Arc::new(PureHost::new(absolute("project"), absolute("bin/rcmake")));
    for (name, bytes) in [
        (
            "FetchContent.cmake",
            template!("FetchContent.cmake").as_slice(),
        ),
        (
            "ExternalProject/shared_internal_commands.cmake",
            template!("ExternalProject/shared_internal_commands.cmake").as_slice(),
        ),
        (
            "ExternalProject/RepositoryInfo.txt.in",
            template!("ExternalProject/RepositoryInfo.txt.in").as_slice(),
        ),
        (
            "ExternalProject/UpdateInfo.txt.in",
            template!("ExternalProject/UpdateInfo.txt.in").as_slice(),
        ),
        (
            "ExternalProject/PatchInfo.txt.in",
            template!("ExternalProject/PatchInfo.txt.in").as_slice(),
        ),
        (
            "ExternalProject/download.cmake.in",
            template!("ExternalProject/download.cmake.in").as_slice(),
        ),
        (
            "ExternalProject/verify.cmake.in",
            template!("ExternalProject/verify.cmake.in").as_slice(),
        ),
        (
            "ExternalProject/stepscript.cmake.in",
            template!("ExternalProject/stepscript.cmake.in").as_slice(),
        ),
    ] {
        host.seed_file(&root.join("Modules").join(name), bytes)?;
    }
    host.env_set(
        b"RCMAKE_CACHE_DIR",
        Some(&path::from_native(&absolute("cache"))),
    );
    Ok(host)
}

fn run(
    host: Arc<PureHost>,
    mode: FetchLockMode,
    script: &[u8],
) -> Result<i32, Box<dyn std::error::Error>> {
    evaluate(host, mode, RunMode::Script, script)
}

fn evaluate(
    host: Arc<PureHost>,
    mode: FetchLockMode,
    run_mode: RunMode,
    source: &[u8],
) -> Result<i32, Box<dyn std::error::Error>> {
    let mut services = Services::default();
    services.speed.fetch_lock = mode;
    let config = InterpConfig {
        services: Arc::new(services),
        host: host.clone(),
        mode: run_mode,
        cmake_root: absolute("cmake"),
        cmake_command: absolute("bin/rcmake"),
        cwd: absolute("project"),
        limits: Default::default(),
        diag: Default::default(),
        defines: Vec::new(),
        script_args: Vec::new(),
    };
    let project = absolute("project");
    let entry = project.join(if run_mode == RunMode::Script {
        "populate.cmake"
    } else {
        "CMakeLists.txt"
    });
    host.seed_file(&entry, source)?;
    let runtime =
        FetchRuntime::new(host, &project, &config.services).ok_or("missing fetch runtime")?;
    let mut interp = Interp::new(config, crate::register_all);
    runtime.install(&mut interp);
    let result = if run_mode == RunMode::Script {
        interp.run_script(&entry)
    } else {
        interp.run_configure(&project, &absolute("build"), &[])
    };
    runtime.finish(result.exit_code == 0)?;
    Ok(result.exit_code)
}

fn declaration(work: &Path, extra: &str) -> String {
    let work = path::from_native_cmake(work);
    format!(
        r#"
cmake_minimum_required(VERSION 4.5)
set(FETCHCONTENT_BASE_DIR "{work}")
include(FetchContent)
FetchContent_Populate(archive
  URL https://example.org/payload
  SOURCE_DIR "{work}/archive-src"
  BINARY_DIR "{work}/archive-build"
  DOWNLOAD_NO_EXTRACT TRUE {extra})
"#
    )
}

#[test]
fn offline_blocks_direct_transfers_before_starting_a_process() -> TestResult {
    let host = host()?;
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    let count = Arc::new(AtomicUsize::new(0));
    let runs = Arc::clone(&count);
    host.set_command_handler(move |_, _, _| {
        runs.fetch_add(1, Ordering::Relaxed);
        Ok(Default::default())
    })?;
    for command in [
        "file(DOWNLOAD https://example.org/payload output)",
        "file(UPLOAD input https://example.org/payload)",
        "execute_process(COMMAND git clone https://example.org/repo)",
        "execute_process(COMMAND git -C checkout fetch origin)",
        "execute_process(COMMAND git ls-remote origin)",
        "execute_process(COMMAND git submodule update --init)",
    ] {
        assert_ne!(
            run(Arc::clone(&host), FetchLockMode::Off, command.as_bytes())?,
            0
        );
    }
    assert_eq!(count.load(Ordering::Relaxed), 0);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Offline fetch blocks remote transfers")
    );
    Ok(())
}

#[test]
fn frozen_blocks_an_unmatched_external_project_transfer_via_a_symlink() -> TestResult {
    let host = host()?;
    let module = absolute("cmake/Modules/ExternalProject/custom.cmake");
    host.seed_file(
        &module,
        b"file(DOWNLOAD https://example.org/payload output)\n",
    )?;
    let alias = absolute("project/custom.cmake");
    host.symlink(&module, &alias)?;
    let command = format!("include([[{}]])", path::from_native_cmake(&alias));
    assert_ne!(
        run(Arc::clone(&host), FetchLockMode::Frozen, command.as_bytes())?,
        0
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Frozen fetch blocks transfers")
    );
    Ok(())
}

#[test]
fn updates_the_lock_and_populates_an_offline_archive() -> TestResult {
    let host = host()?;
    let launches = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&launches);
    host.set_command_handler(move |host, request, _| {
        count.fetch_add(1, Ordering::Relaxed);
        let [curl] = request.pipeline.as_slice() else {
            return Err(HostError::unsupported("Expected one download process."));
        };
        assert_eq!(curl.program, "curl");
        let destination = curl
            .args
            .windows(2)
            .find_map(|args| match args {
                [flag, file] if flag == "--output" => Some(file),
                _ => None,
            })
            .ok_or_else(|| HostError::unsupported("Missing download destination."))?;
        host.seed_file(&path::to_native(destination), b"abc")?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let pin = "URL_HASH SHA256=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(
        run(
            host.clone(),
            FetchLockMode::Update,
            declaration(&absolute("cold"), pin).as_bytes()
        )?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let lock = host.read(
        &absolute("project/rcmake-fetch.lock"),
        ReadRange::All { limit: 8192 },
    )?;
    assert!(String::from_utf8_lossy(&lock).contains("https://example.org/payload"));
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    assert_eq!(
        run(
            host.clone(),
            FetchLockMode::Frozen,
            declaration(&absolute("warm"), pin).as_bytes()
        )?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(launches.load(Ordering::Relaxed), 1);
    assert_eq!(
        host.read(
            &absolute("warm/archive-src/payload"),
            ReadRange::All { limit: 3 }
        )?,
        b"abc"
    );
    Ok(())
}

#[test]
fn offline_population_rejects_an_archive_without_a_pin() -> TestResult {
    let host = host()?;
    let launches = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&launches);
    host.set_command_handler(move |_, _, _| {
        count.fetch_add(1, Ordering::Relaxed);
        Err(HostError::unsupported("Unexpected process."))
    })?;
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    assert_ne!(
        run(
            host.clone(),
            FetchLockMode::Off,
            declaration(&absolute("offline"), "").as_bytes()
        )?,
        0
    );
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(
        diagnostic_words(&stderr).contains("Set URL_HASH"),
        "{stderr}"
    );
    assert_eq!(launches.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn offline_population_rejects_a_custom_transfer_before_it_starts() -> TestResult {
    let host = host()?;
    let launches = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&launches);
    host.set_command_handler(move |_, _, _| {
        count.fetch_add(1, Ordering::Relaxed);
        Err(HostError::unsupported("Unexpected process."))
    })?;
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    let script = declaration(
        &absolute("custom"),
        "DOWNLOAD_COMMAND curl https://example.org/unmatched",
    );
    assert_ne!(run(host.clone(), FetchLockMode::Off, script.as_bytes())?, 0);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(
        diagnostic_words(&stderr).contains("This transfer cannot enforce a fetch pin"),
        "{stderr}"
    );
    assert_eq!(launches.load(Ordering::Relaxed), 0);
    Ok(())
}

fn diagnostic_words(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn offline_local_archive_uses_verified_bytes_when_the_origin_is_missing() -> TestResult {
    let host = host()?;
    let origin = absolute("origin/payload");
    host.seed_file(&origin, b"abc")?;
    host.set_command_handler(|_, _, _| Err(HostError::unsupported("Unexpected process.")))?;
    // `ExternalProject` removes the `file://` prefix and uses the rest as the path.
    // A Windows path therefore gives `file://C:/...`, not `file:///C:/...`.
    let url = format!("file://{}", path::from_native_cmake(&origin));
    let pin = "URL_HASH SHA256=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let script =
        |dir: &str| declaration(&absolute(dir), pin).replace("https://example.org/payload", &url);
    assert_eq!(
        run(host.clone(), FetchLockMode::Auto, script("cold").as_bytes())?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    host.remove_file(&origin)?;
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
    assert_eq!(
        run(host.clone(), FetchLockMode::Auto, script("warm").as_bytes())?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(
        host.read(
            &absolute("warm/archive-src/payload"),
            ReadRange::All { limit: 3 }
        )?,
        b"abc"
    );
    Ok(())
}

#[test]
fn frozen_archive_checks_warm_stamp_hash_skip_and_disconnected_reuse() -> TestResult {
    use rcmake_fetch::{ArchiveCache, ContentHash, LockEntry, LockFile, UrlEntry};
    use rcmake_store::{Store, StoreConfig};

    let host = host()?;
    let url = "https://example.org/payload";
    let entry = UrlEntry::from_bytes(vec![url.into()], b"abc");
    let hash = ContentHash::sha256(entry.sha256);
    let mut lock = LockFile::default();
    lock.insert("archive", LockEntry::Url(entry.clone()))?;
    let lock_path = absolute("project/rcmake-fetch.lock");
    let alternate = absolute("project/alternate.lock");
    host.seed_file(&lock_path, &lock.to_json()?)?;
    let store = Store::new(host.as_ref(), StoreConfig::from_host(host.as_ref()))?;
    ArchiveCache::new(host.as_ref(), &store).insert(url, &hash, b"abc")?;
    host.set_command_handler(|_, _, _| Err(HostError::unsupported("Unexpected process.")))?;
    host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));

    let work = absolute("warm");
    let declared = format!("URL_HASH SHA256={}", entry.sha256);
    let script = format!(
        "cmake_minimum_required(VERSION 4.5)\nproject(Frozen NONE)\n{}\nFetchContent_MakeAvailable(archive)\nfile(WRITE \"{}/used\" yes)\n",
        declaration(&work, &declared).replace(
            "FetchContent_Populate(archive\n",
            "FetchContent_Declare(archive\n"
        ),
        path::from_native_cmake(&work),
    );
    let run_frozen = |script: &str| {
        evaluate(
            host.clone(),
            FetchLockMode::Frozen,
            RunMode::Configure,
            script.as_bytes(),
        )
    };
    for _ in 0..2 {
        assert_eq!(
            run_frozen(&script)?,
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
    }
    let stamp = absolute("build/CMakeFiles/fc-stamp/archive/download.stamp");
    let receipt = stamp.with_added_extension("rcmake");
    let retained = work.join("archive-tmp/payload");
    let copied = work.join("archive-src/payload");
    host.remove_file(&receipt)?;
    host.seed_file(&copied, b"old source")?;
    assert_eq!(
        run_frozen(&script)?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(host.read(&copied, ReadRange::All { limit: 16 })?, b"abc");

    let disconnected = script.replace(
        "FetchContent_MakeAvailable",
        "set(FETCHCONTENT_FULLY_DISCONNECTED TRUE)\nFetchContent_MakeAvailable",
    );
    assert_eq!(
        run_frozen(&disconnected)?,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );

    host.remove_file(&work.join("used"))?;
    host.seed_file(&alternate, &LockFile::default().to_json()?)?;
    host.env_set(
        b"RCMAKE_FETCH_LOCK_FILE",
        Some(&path::from_native(&alternate)),
    );
    assert_ne!(run_frozen(&script)?, 0);
    assert!(
        host.metadata(&work.join("used"), rcmake_host::Follow::Yes)
            .is_err()
    );
    host.env_set(b"RCMAKE_FETCH_LOCK_FILE", None);

    host.remove_file(&lock_path)?;
    assert_ne!(run_frozen(&script)?, 0);
    for mut changed in [UrlEntry::from_bytes(vec![url.into()], b"def"), entry] {
        changed.size += 1;
        let mut lock = LockFile::default();
        lock.insert("archive", LockEntry::Url(changed))?;
        host.seed_file(&lock_path, &lock.to_json()?)?;
        assert_ne!(run_frozen(&script)?, 0);
    }
    host.remove_file(&stamp)?;
    assert_ne!(run_frozen(&script)?, 0);
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(
        diagnostic_words(&stderr).contains("matches the declared hash but differs from the lock"),
        "{stderr}"
    );
    assert_eq!(host.read(&retained, ReadRange::All { limit: 16 })?, b"abc");
    assert_ne!(run_frozen(&disconnected)?, 0);
    assert!(
        host.metadata(&work.join("used"), rcmake_host::Follow::Yes)
            .is_err()
    );
    Ok(())
}
