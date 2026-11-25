use std::io::ErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};

use rcmake_eval::{InterpConfig, RunMode};
use rcmake_host::{CommandSpec, HostResult, Input, Output, PureHost, ReadRange, RunRequest, path};

use super::*;

fn absolute(path: &str) -> PathBuf {
    if cfg!(windows) {
        format!("C:{path}").into()
    } else {
        path.into()
    }
}

fn setup(mode: HermeticMode) -> HostResult<(Arc<PureHost>, HermeticRuntime)> {
    let source = absolute("/project");
    let build = absolute("/build");
    let tool = absolute("/bin/approved");
    let host = Arc::new(PureHost::new(source.clone(), tool.clone()));
    host.seed_file(&source.join("CMakeLists.txt"), b"set(READY 1)\n")?;
    host.seed_file(&absolute("/private/data"), b"private file content")?;
    host.create_dir_all(&build)?;
    host.seed_file(&tool, b"")?;
    host.set_permissions(&tool, 0o755)?;
    host.env_set(b"PATH", Some(&path::from_native(&absolute("/bin"))));
    host.env_set(b"TOKEN", Some(b"private env value"));
    let mut services = Services::default();
    services.speed.hermetic = mode;
    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    Ok((host, runtime))
}

fn request(host: &dyn Host, programs: &[&[u8]]) -> RunRequest {
    RunRequest {
        pipeline: programs
            .iter()
            .map(|&program| CommandSpec {
                program: program.into(),
                args: vec!["private process argument".into()],
            })
            .collect(),
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: None,
        tee: false,
    }
}

#[test]
fn strict_denies_a_symlink_escape_before_reading() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let path = absolute("/project/link");
    inner.symlink(&absolute("/private/data"), &path)?;
    let host = runtime.host();
    let error = host.read(&path, ReadRange::All { limit: 100 }).unwrap_err();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(runtime.denied());
    Ok(())
}

#[test]
fn strict_checks_every_stage_before_starting_a_pipeline() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let count = Arc::new(AtomicUsize::new(0));
    let runs = Arc::clone(&count);
    inner.set_command_handler(move |_, request, _| {
        runs.fetch_add(1, Ordering::Relaxed);
        let [command] = request.pipeline.as_slice() else {
            return Err(HostError::unsupported("Expected one approved process."));
        };
        assert_eq!(
            command.program,
            path::from_native(&absolute("/bin/approved"))
        );
        assert!(!request.env.iter().any(|(key, _)| key == b"TOKEN"));
        Ok(Default::default())
    })?;
    let host = runtime.host();
    host.run_bounded(&request(inner.as_ref(), &[b"approved"]), 4096)?;
    let pipeline = request(inner.as_ref(), &[b"approved", b"unapproved"]);
    let error = host.run(&pipeline, &mut |_, _| {}).unwrap_err();
    assert_eq!(error.kind, ErrorKind::PermissionDenied);
    assert!(runtime.denied());
    assert_eq!(count.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn strict_denies_an_unpinned_upload_through_an_approved_curl_alias() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    inner.symlink(&absolute("/bin/approved"), &absolute("/bin/curl"))?;
    let mut request = request(inner.as_ref(), &[b"curl"]);
    let [command] = request.pipeline.as_mut_slice() else {
        return Err(HostError::unsupported("Expected one upload process."));
    };
    command.args = vec![
        "--upload-file".into(),
        "CMakeLists.txt".into(),
        "--url".into(),
        "https://example.org/input".into(),
    ];
    let error = runtime.host().run_bounded(&request, 1024).unwrap_err();
    assert_eq!(error.os_message, "RCMAKE_HERMETIC_NETWORK");
    assert!(runtime.denied());
    Ok(())
}

#[test]
fn strict_stops_configure_when_a_condition_ignores_an_environment_denial() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let source = absolute("/project");
    let build = absolute("/build");
    let script = b"cmake_minimum_required(VERSION 4.5)\nproject(Audit NONE)\nif(DEFINED ENV{TOKEN})\nendif()\nfile(WRITE ${CMAKE_BINARY_DIR}/continued.txt no)\n";
    inner.seed_file(&source.join("CMakeLists.txt"), script)?;
    let mut interp = Interp::new(
        InterpConfig {
            host: runtime.host(),
            mode: RunMode::Configure,
            cmake_root: source.join("data"),
            cmake_command: absolute("/bin/approved"),
            cwd: source.clone(),
            limits: Default::default(),
            diag: Default::default(),
            services: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    runtime.install(&mut interp);
    let outcome = interp.run_configure(&source, &build, &[]);
    let diagnostics = inner.stream_bytes(Stream::Stderr);
    assert_eq!(outcome.exit_code, 1, "{diagnostics}");
    assert!(runtime.denied(), "{diagnostics}");
    assert!(
        inner
            .metadata(&build.join("continued.txt"), rcmake_host::Follow::Yes)
            .is_err()
    );
    assert!(
        diagnostics
            .to_string()
            .contains("CMakeLists.txt:3: RCMAKE_HERMETIC_ENV"),
        "{diagnostics}"
    );
    Ok(())
}

#[test]
fn git_cache_scope_only_permits_the_selected_cache_during_internal_calls() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let cache_path = absolute("/shared-cache/git/HEAD");
    inner.seed_file(&cache_path, b"head")?;
    inner.set_command_handler(|_, request, _| {
        assert!(
            request
                .env
                .iter()
                .any(|(name, value)| name == b"GIT_ALLOW_PROTOCOL" && value == b"file")
        );
        Ok(Default::default())
    })?;
    let host = runtime.host();
    {
        let _scope = runtime
            .host
            .context
            .enter_fetch_cache(absolute("/shared-cache"));
        assert_eq!(
            host.read(&cache_path, ReadRange::All { limit: 100 })?,
            b"head"
        );
        let mut request = request(inner.as_ref(), &[b"approved"]);
        request
            .env
            .push(("GIT_ALLOW_PROTOCOL".into(), "file".into()));
        host.run_bounded(&request, 4096)?;
    }
    assert_eq!(
        host.read(&cache_path, ReadRange::All { limit: 100 })
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    Ok(())
}

#[test]
fn warn_substitutes_environment_values_and_writes_a_redacted_report() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Warn)?;
    let host = runtime.host();
    let path = absolute("/private/data");
    let site = HermeticSite::new(&absolute("/project/CMakeLists.txt"), 17);
    let _site = runtime.host.context.enter(site.clone());
    assert!(host.env_get(b"TOKEN").is_none());
    assert!(host.env_get(b"UNDEFINED").is_none());
    assert_eq!(host.env_get(b"SOURCE_DATE_EPOCH"), Some("0".into()));
    assert_eq!(
        host.read(&path, ReadRange::All { limit: 100 })?,
        b"private file content"
    );
    let url = "https://user:password@example.org/private?token=hidden";
    runtime
        .host
        .audit(|audit| audit.check_network(&site, url, false));
    assert!(!runtime.denied());
    runtime.finish();
    let read_report = || {
        inner.read(
            &absolute("/build/CMakeFiles/rcmake/inputs.v1.json"),
            ReadRange::All { limit: 8192 },
        )
    };
    let report = read_report()?;
    runtime.finish();
    assert_eq!(report, read_report()?);
    let report = String::from_utf8_lossy(&report);
    for expected in [
        "RCMAKE_HERMETIC_ENV",
        "RCMAKE_HERMETIC_NETWORK",
        "https://example.org",
        "{source}/CMakeLists.txt",
        "\"line\":17",
    ] {
        assert!(report.contains(expected));
    }
    for private in [
        "password",
        "token=hidden",
        "private env value",
        "private file content",
        "/private/data",
    ] {
        assert!(!report.contains(private));
    }
    Ok(())
}
