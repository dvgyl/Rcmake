use std::io::ErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};

use rcmake_eval::{InterpConfig, RunMode};
use rcmake_host::{
    CommandSpec, Follow, HostResult, Input, Output, PureHost, ReadRange, RunRequest, path,
};

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
fn strict_denies_both_rename_operations_with_an_external_source() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let source = absolute("/private/data");
    let target = absolute("/build/moved");
    let host = runtime.host();
    assert_eq!(
        host.rename(&source, &target).unwrap_err().kind,
        ErrorKind::PermissionDenied
    );
    assert_eq!(
        host.rename_noreplace(&source, &target).unwrap_err().kind,
        ErrorKind::PermissionDenied
    );
    assert!(inner.metadata(&source, Follow::No).is_ok());
    assert!(inner.metadata(&target, Follow::No).is_err());
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
    let diagnostics = diagnostics.to_string();
    assert!(diagnostics.contains("CMakeLists.txt:3"), "{diagnostics}");
    assert!(diagnostics.contains("RCMAKE_HERMETIC_ENV"), "{diagnostics}");
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
        .audit_at(&site, |audit| audit.check_network(&site, url, false));
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
    let private_path = absolute("/private/data");
    let native = path::from_native(&private_path).to_string();
    let portable = path::from_native_cmake(&private_path).to_string();
    let escaped = native.replace('\\', "\\\\");
    for private in [
        "password",
        "token=hidden",
        "private env value",
        "private file content",
        &native,
        &portable,
        &escaped,
    ] {
        assert!(!report.contains(private));
    }
    Ok(())
}

#[test]
fn strict_allows_bundled_missing_metadata_and_withholds_bundled_env() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let data = absolute("/data");
    inner.create_dir_all(&data.join("Modules"))?;
    runtime.allow_root(&data);
    let bundled = HermeticSite::new(&data.join("Modules/CMakeDetermineSystem.cmake"), 42);
    let _guard = runtime.host.context.enter(bundled);
    let host = runtime.host();
    assert_eq!(
        host.metadata(&absolute("/does/not/exist"), Follow::Yes)
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    assert!(host.env_get(b"TOKEN").is_none());
    assert!(!runtime.denied());
    Ok(())
}


#[test]
fn strict_denies_undeclared_read_and_unapproved_process_from_bundled_module() -> HostResult<()> {
    let (inner, runtime) = setup(HermeticMode::Strict)?;
    let data = absolute("/data");
    inner.create_dir_all(&data.join("Modules"))?;
    runtime.allow_root(&data);
    let bundled = HermeticSite::new(&data.join("Modules/CMakeDetermineSystem.cmake"), 42);
    let _guard = runtime.host.context.enter(bundled);
    let read_err = runtime
        .host()
        .read(&absolute("/private/data"), ReadRange::All { limit: 100 })
        .unwrap_err();
    assert_eq!(read_err.kind, ErrorKind::PermissionDenied);
    assert!(runtime.denied());

    let (inner, runtime) = setup(HermeticMode::Strict)?;
    inner.create_dir_all(&data.join("Modules"))?;
    runtime.allow_root(&data);
    let unapproved = absolute("/bin/unapproved");
    inner.seed_file(&unapproved, b"")?;
    inner.set_permissions(&unapproved, 0o755)?;
    let bundled = HermeticSite::new(&data.join("Modules/CMakeDetermineSystem.cmake"), 43);
    let _guard = runtime.host.context.enter(bundled);
    let proc_err = runtime
        .host()
        .run_bounded(&request(inner.as_ref(), &[b"unapproved"]), 1024)
        .unwrap_err();
    assert_eq!(proc_err.kind, ErrorKind::PermissionDenied);
    assert!(runtime.denied());
    Ok(())
}

#[test]
fn sibling_entrypoint_rejects_symlink_pointing_outside_stage() -> HostResult<()> {
    let source = absolute("/project");
    let build = absolute("/build");
    let stage = absolute("/stage/bin");
    let exe = stage.join("rcmake");
    let evil = absolute("/outside/evil");
    let host = Arc::new(PureHost::new(source.clone(), exe.clone()));
    host.seed_file(&source.join("CMakeLists.txt"), b"")?;
    host.create_dir_all(&build)?;
    host.seed_file(&exe, b"")?;
    host.set_permissions(&exe, 0o755)?;
    host.seed_file(&evil, b"")?;
    host.set_permissions(&evil, 0o755)?;
    host.symlink(&exe, &stage.join("cmake"))?;
    host.symlink(&evil, &stage.join("ctest"))?;
    host.env_set(b"PATH", Some(&path::from_native(&stage)));
    host.set_command_handler(|_, _, _| Ok(Default::default()))?;

    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;
    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();

    assert_eq!(
        guarded
            .metadata(&stage.join("cpack"), Follow::Yes)
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    assert!(!runtime.denied());

    guarded.run_bounded(&request(host.as_ref(), &[b"cmake"]), 1024)?;
    let err = guarded
        .run_bounded(&request(host.as_ref(), &[b"ctest"]), 1024)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::PermissionDenied);
    assert!(runtime.denied());
    Ok(())
}

#[test]
fn strict_denies_decoy_toolchain_binary_and_symlink_in_untrusted_path() -> HostResult<()> {
    let source = absolute("/project");
    let build = absolute("/build");
    let stage = absolute("/stage/bin");
    let exe = stage.join("rcmake");
    let untrusted = absolute("/tmp/untrusted");
    let decoy_cc = untrusted.join("cc");
    let outside_evil = absolute("/outside/evil");
    let decoy_clang = untrusted.join("clang");
    let explicit_tool = absolute("/opt/custom/mycc");

    let host = Arc::new(PureHost::new(source.clone(), exe.clone()));
    host.seed_file(&source.join("CMakeLists.txt"), b"")?;
    host.create_dir_all(&build)?;
    for binary in [&exe, &decoy_cc, &outside_evil, &explicit_tool] {
        host.seed_file(binary, b"")?;
        host.set_permissions(binary, 0o755)?;
    }
    host.symlink(&outside_evil, &decoy_clang)?;

    #[cfg(not(windows))]
    {
        let system_cc = PathBuf::from("/usr/bin/cc");
        host.seed_file(&system_cc, b"")?;
        host.set_permissions(&system_cc, 0o755)?;
    }

    let path_env = if cfg!(windows) {
        format!(
            "{};{}",
            path::from_native(&untrusted),
            path::from_native(&stage)
        )
    } else {
        format!("{}:/usr/bin", path::from_native(&untrusted))
    };
    host.env_set(b"PATH", Some(path_env.as_bytes()));
    host.env_set(
        b"RCMAKE_HERMETIC_ALLOW_PROGRAMS",
        Some(&path::from_native(&explicit_tool)),
    );
    host.set_command_handler(|_, _, _| Ok(Default::default()))?;

    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;

    // 1. Decoy cc and symlinked clang in an untrusted PATH dir are denied for both exec and read.
    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .run_bounded(&request(host.as_ref(), &[b"cc"]), 1024)
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());

    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .run_bounded(&request(host.as_ref(), &[b"clang"]), 1024)
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());

    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .read(&decoy_cc, ReadRange::All { limit: 100 })
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());

    // 2. Explicitly allowed tool and verified system tool paths still succeed.
    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    let explicit_arg = path::from_native(&explicit_tool);
    guarded.run_bounded(&request(host.as_ref(), &[explicit_arg.as_slice()]), 1024)?;
    #[cfg(not(windows))]
    {
        guarded.run_bounded(&request(host.as_ref(), &[b"/usr/bin/cc"]), 1024)?;
    }
    assert!(!runtime.denied());
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn strict_denies_symlink_in_trusted_dir_escaping_trusted_roots() -> HostResult<()> {
    let source = absolute("/project");
    let build = absolute("/build");
    let exe = absolute("/stage/bin/rcmake");
    let outside_evil = absolute("/tmp/outside/evil");
    let escaped_cc = PathBuf::from("/usr/bin/cc");

    let host = Arc::new(PureHost::new(source.clone(), exe.clone()));
    host.seed_file(&source.join("CMakeLists.txt"), b"")?;
    host.create_dir_all(&build)?;
    host.seed_file(&exe, b"")?;
    host.set_permissions(&exe, 0o755)?;
    host.seed_file(&outside_evil, b"evil")?;
    host.set_permissions(&outside_evil, 0o755)?;
    host.create_dir_all(Path::new("/usr/bin"))?;
    host.symlink(&outside_evil, &escaped_cc)?;
    host.env_set(b"PATH", Some(b"/usr/bin"));
    host.set_command_handler(|_, _, _| Ok(Default::default()))?;

    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;
    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .run_bounded(&request(host.as_ref(), &[b"/usr/bin/cc"]), 1024)
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());

    let runtime = HermeticRuntime::new(host.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .read(&escaped_cc, ReadRange::All { limit: 100 })
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());
    Ok(())
}

struct CountingReadHost {
    inner: Arc<PureHost>,
    reads: Arc<AtomicUsize>,
}

impl Host for CountingReadHost {
    fn metadata(&self, path: &Path, follow: Follow) -> HostResult<rcmake_host::Metadata> {
        self.inner.metadata(path, follow)
    }

    fn read(&self, path: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(path, range)
    }

    fn read_chunks(&self, path: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_chunks(path, consume)
    }

    fn write(&self, path: &Path, data: &[u8], mode: rcmake_host::WriteMode) -> HostResult<()> {
        self.inner.write(path, data, mode)
    }

    fn read_dir(&self, path: &Path) -> HostResult<Vec<rcmake_host::DirEntry>> {
        self.inner.read_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> HostResult<()> {
        self.inner.create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_dir(path)
    }

    fn remove_dir_all(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.inner.rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<std::time::Duration>,
    ) -> HostResult<Box<dyn rcmake_host::FileLockGuard>> {
        self.inner.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: rcmake_host::CopyOptions) -> HostResult<()> {
        self.inner.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.inner.symlink(target, link)
    }

    fn hard_link(&self, source: &Path, destination: &Path) -> HostResult<()> {
        self.inner.hard_link(source, destination)
    }

    fn read_link(&self, path: &Path) -> HostResult<PathBuf> {
        self.inner.read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> HostResult<PathBuf> {
        self.inner.canonicalize(path)
    }

    fn set_permissions(&self, path: &Path, mode: u32) -> HostResult<()> {
        self.inner.set_permissions(path, mode)
    }

    fn set_times(
        &self,
        path: &Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
    ) -> HostResult<()> {
        self.inner.set_times(path, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        path: &Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
    ) -> HostResult<()> {
        self.inner.set_symlink_times(path, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<rcmake_base::BString> {
        self.inner.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.inner.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(rcmake_base::BString, rcmake_base::BString)> {
        self.inner.env_all()
    }

    fn supports_process_execution(&self) -> bool {
        self.inner.supports_process_execution()
    }

    fn registry_host_view(&self) -> Option<rcmake_host::RegistryView> {
        self.inner.registry_host_view()
    }

    fn registry_value(
        &self,
        root: rcmake_host::RegistryRoot,
        subkey: &[u8],
        name: Option<&[u8]>,
        view: rcmake_host::RegistryView,
    ) -> HostResult<Option<rcmake_base::BString>> {
        self.inner.registry_value(root, subkey, name, view)
    }

    fn open_read(&self, p: &Path) -> HostResult<Box<dyn rcmake_host::ReadSeek + '_>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.open_read(p)
    }

    fn scratch_file(&self) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.inner.scratch_file()
    }

    fn scratch_file_in(&self, dir: &Path) -> HostResult<Box<dyn rcmake_host::ScratchFile + '_>> {
        self.inner.scratch_file_in(dir)
    }

    fn write_from(
        &self,
        p: &Path,
        mode: rcmake_host::WriteMode,
        input: &mut dyn std::io::Read,
        len: u64,
    ) -> HostResult<()> {
        self.inner.write_from(p, mode, input, len)
    }

    fn run_controlled(
        &self,
        req: &RunRequest,
        graceful: Option<rcmake_host::GracefulTimeout>,
        hard_limit: Option<std::time::Duration>,
        update: &mut rcmake_host::DeadlineUpdate<'_>,
    ) -> HostResult<rcmake_host::ControlledOutput> {
        self.inner.run_controlled(req, graceful, hard_limit, update)
    }

    fn run(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<rcmake_host::RunOutput> {
        self.inner.run(request, on_output)
    }

    fn run_with_timeout_signal(
        &self,
        request: &RunRequest,
        timeout: rcmake_host::GracefulTimeout,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<rcmake_host::RunOutput> {
        self.inner
            .run_with_timeout_signal(request, timeout, on_output)
    }

    fn run_bounded(
        &self,
        request: &RunRequest,
        limit: usize,
    ) -> HostResult<rcmake_host::RunOutput> {
        self.inner.run_bounded(request, limit)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.inner.write_out(stream, bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn terminal_width(&self, stream: Stream) -> Option<usize> {
        self.inner.terminal_width(stream)
    }

    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> std::time::Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> jiff::tz::TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        self.inner.system()
    }
}

#[test]
fn rich_violation_reporter_uses_source_map_and_never_reads_disk() -> HostResult<()> {
    let source = absolute("/project");
    let build = absolute("/build");
    let tool = absolute("/bin/approved");
    let lists = source.join("CMakeLists.txt");
    let unparsed_secret = absolute("/private/unparsed_secret.txt");

    let inner = Arc::new(PureHost::new(source.clone(), tool.clone()));
    inner.seed_file(
        &lists,
        b"cmake_minimum_required(VERSION 4.5)\nproject(Audit NONE)\nset(ORIG_SNIPPET \"$ENV{TOKEN}\")\n",
    )?;
    inner.seed_file(&unparsed_secret, b"UNPARSED_SECRET_LINE\n")?;
    inner.create_dir_all(&build)?;
    inner.seed_file(&tool, b"")?;
    inner.set_permissions(&tool, 0o755)?;
    inner.env_set(b"TOKEN", Some(b"private env value"));

    let reads = Arc::new(AtomicUsize::new(0));
    let counting = Arc::new(CountingReadHost {
        inner: Arc::clone(&inner),
        reads: Arc::clone(&reads),
    });

    // 1. Warn mode: parse CMakeLists.txt, overwrite it on disk before finish(), and prove
    //    finish() renders the parsed SourceMap snippet with zero host.read calls.
    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Warn;
    let runtime = HermeticRuntime::new(counting.clone(), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let mut interp = Interp::new(
        InterpConfig {
            host: runtime.host(),
            mode: RunMode::Configure,
            cmake_root: source.join("data"),
            cmake_command: tool.clone(),
            cwd: source.clone(),
            limits: Default::default(),
            diag: Default::default(),
            services: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    interp.add_session_data(RichOptions::default());
    runtime.install(&mut interp);
    let outcome = interp.run_configure(&source, &build, &[]);
    assert_eq!(outcome.exit_code, 0);

    // Replace CMakeLists.txt on disk after parsing to simulate a TOCTOU swap.
    inner.seed_file(&lists, b"line1\nline2\nDISK_SECRET_REPLACED\n")?;
    reads.store(0, Ordering::Relaxed);
    runtime.finish();
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "violation reporter must never reopen files through the inner host"
    );
    let stderr = inner.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.contains("RCMAKE_HERMETIC_ENV"), "{stderr}");
    assert!(stderr.contains("ORIG_SNIPPET"), "{stderr}");
    assert!(!stderr.contains("DISK_SECRET_REPLACED"), "{stderr}");

    // 2. Strict mode with a violation site NOT in SourceMap: omits snippet cleanly with 0 reads.
    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;
    let runtime = HermeticRuntime::new(counting, &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    runtime.host.state().rich = Some(RichOptions::default());
    reads.store(0, Ordering::Relaxed);
    let _guard = runtime
        .host
        .context
        .enter(HermeticSite::new(&unparsed_secret, 1));
    assert!(runtime.host().env_get(b"TOKEN").is_none());
    assert!(runtime.denied());
    assert_eq!(
        reads.load(Ordering::Relaxed),
        0,
        "missing SourceMap entry must omit snippet without reading disk"
    );
    let stderr = inner.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.contains("unparsed_secret.txt"), "{stderr}");
    assert!(!stderr.contains("UNPARSED_SECRET_LINE"), "{stderr}");
    Ok(())
}

struct SysnameHost {
    inner: Arc<PureHost>,
    sysname: &'static str,
}

impl Host for SysnameHost {
    fn metadata(&self, path: &Path, follow: Follow) -> HostResult<rcmake_host::Metadata> {
        self.inner.metadata(path, follow)
    }

    fn read(&self, path: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        self.inner.read(path, range)
    }

    fn read_chunks(&self, path: &Path, consume: &mut dyn FnMut(&[u8])) -> HostResult<()> {
        self.inner.read_chunks(path, consume)
    }

    fn write(&self, path: &Path, data: &[u8], mode: rcmake_host::WriteMode) -> HostResult<()> {
        self.inner.write(path, data, mode)
    }

    fn read_dir(&self, path: &Path) -> HostResult<Vec<rcmake_host::DirEntry>> {
        self.inner.read_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> HostResult<()> {
        self.inner.create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_dir(path)
    }

    fn remove_dir_all(&self, path: &Path) -> HostResult<()> {
        self.inner.remove_dir_all(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.inner.rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> HostResult<()> {
        self.inner.rename_noreplace(from, to)
    }

    fn lock_file(
        &self,
        path: &Path,
        timeout: Option<std::time::Duration>,
    ) -> HostResult<Box<dyn rcmake_host::FileLockGuard>> {
        self.inner.lock_file(path, timeout)
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: rcmake_host::CopyOptions) -> HostResult<()> {
        self.inner.copy_file(from, to, opts)
    }

    fn symlink(&self, target: &Path, link: &Path) -> HostResult<()> {
        self.inner.symlink(target, link)
    }

    fn hard_link(&self, source: &Path, destination: &Path) -> HostResult<()> {
        self.inner.hard_link(source, destination)
    }

    fn read_link(&self, path: &Path) -> HostResult<PathBuf> {
        self.inner.read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> HostResult<PathBuf> {
        self.inner.canonicalize(path)
    }

    fn set_permissions(&self, path: &Path, mode: u32) -> HostResult<()> {
        self.inner.set_permissions(path, mode)
    }

    fn set_times(
        &self,
        path: &Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
    ) -> HostResult<()> {
        self.inner.set_times(path, atime, mtime)
    }

    fn set_symlink_times(
        &self,
        path: &Path,
        atime: Option<rcmake_host::FileTime>,
        mtime: Option<rcmake_host::FileTime>,
    ) -> HostResult<()> {
        self.inner.set_symlink_times(path, atime, mtime)
    }

    fn env_get(&self, key: &[u8]) -> Option<rcmake_base::BString> {
        self.inner.env_get(key)
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        self.inner.env_set(key, value);
    }

    fn env_all(&self) -> Vec<(rcmake_base::BString, rcmake_base::BString)> {
        self.inner.env_all()
    }

    fn supports_process_execution(&self) -> bool {
        self.inner.supports_process_execution()
    }

    fn run(
        &self,
        request: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<rcmake_host::RunOutput> {
        self.inner.run(request, on_output)
    }

    fn run_bounded(
        &self,
        request: &RunRequest,
        limit: usize,
    ) -> HostResult<rcmake_host::RunOutput> {
        self.inner.run_bounded(request, limit)
    }

    fn write_out(&self, stream: Stream, bytes: &[u8]) {
        self.inner.write_out(stream, bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.inner.read_stdin(limit)
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.inner.is_terminal(stream)
    }

    fn now(&self) -> rcmake_host::FileTime {
        self.inner.now()
    }

    fn monotonic(&self) -> std::time::Duration {
        self.inner.monotonic()
    }

    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration);
    }

    fn time_zone(&self) -> jiff::tz::TimeZone {
        self.inner.time_zone()
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.inner.current_exe()
    }

    fn initial_cwd(&self) -> PathBuf {
        self.inner.initial_cwd()
    }

    fn system(&self) -> rcmake_host::SystemInfo {
        let mut info = self.inner.system();
        info.sysname = rcmake_base::BString::from(self.sysname);
        info
    }
}

#[test]
fn strict_windows_never_trusts_systemroot_from_environment() -> HostResult<()> {
    let source = absolute("/project");
    let build = absolute("/build");
    let exe = absolute("/stage/bin/rcmake.exe");
    let fake_win = absolute("/attacker/windows");
    let fake_ninja = fake_win.join("System32/ninja.exe");
    let explicit_tool = absolute("/approved/bin/cl.exe");

    let inner = Arc::new(PureHost::new(source.clone(), exe.clone()));
    inner.seed_file(&source.join("CMakeLists.txt"), b"")?;
    inner.create_dir_all(&build)?;
    for file in [&exe, &fake_ninja, &explicit_tool] {
        inner.seed_file(file, b"")?;
        inner.set_permissions(file, 0o755)?;
    }
    inner.env_set(b"SYSTEMROOT", Some(&path::from_native(&fake_win)));
    inner.env_set(
        b"RCMAKE_HERMETIC_ALLOW_PROGRAMS",
        Some(&path::from_native(&explicit_tool)),
    );
    inner.set_command_handler(|_, _, _| Ok(Default::default()))?;

    let win_host: Arc<dyn Host> = Arc::new(SysnameHost {
        inner: Arc::clone(&inner),
        sysname: "Windows",
    });
    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;

    let runtime = HermeticRuntime::new(Arc::clone(&win_host), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    let fake_arg = path::from_native(&fake_ninja);
    assert_eq!(
        guarded
            .run_bounded(&request(win_host.as_ref(), &[fake_arg.as_slice()]), 1024)
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());

    let runtime = HermeticRuntime::new(Arc::clone(&win_host), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .read(&fake_ninja, ReadRange::All { limit: 100 })
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());

    let runtime = HermeticRuntime::new(Arc::clone(&win_host), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    let explicit_arg = path::from_native(&explicit_tool);
    guarded.run_bounded(
        &request(win_host.as_ref(), &[explicit_arg.as_slice()]),
        1024,
    )?;
    assert!(!runtime.denied());
    Ok(())
}

#[cfg(not(windows))]
#[test]
fn strict_darwin_rejects_symlink_into_case_only_lookalike_directory() -> HostResult<()> {
    let source = absolute("/project");
    let build = absolute("/build");
    let exe = absolute("/stage/bin/rcmake");
    let lookalike = PathBuf::from("/USR/bin/cc");
    let link_in_usr = PathBuf::from("/usr/bin/cc");

    let inner = Arc::new(PureHost::new(source.clone(), exe.clone()));
    inner.seed_file(&source.join("CMakeLists.txt"), b"")?;
    inner.create_dir_all(&build)?;
    inner.seed_file(&exe, b"")?;
    inner.set_permissions(&exe, 0o755)?;
    inner.seed_file(&lookalike, b"")?;
    inner.set_permissions(&lookalike, 0o755)?;
    inner.create_dir_all(Path::new("/usr/bin"))?;
    inner.symlink(&lookalike, &link_in_usr)?;
    inner.env_set(b"PATH", Some(b"/usr/bin"));
    inner.set_command_handler(|_, _, _| Ok(Default::default()))?;

    let darwin_host: Arc<dyn Host> = Arc::new(SysnameHost {
        inner,
        sysname: "Darwin",
    });
    let mut services = Services::default();
    services.speed.hermetic = HermeticMode::Strict;
    let runtime = HermeticRuntime::new(Arc::clone(&darwin_host), &source, &build, &services)
        .ok_or_else(|| HostError::unsupported("Cannot create the audit runtime."))?;
    let guarded = runtime.host();
    assert_eq!(
        guarded
            .run_bounded(&request(darwin_host.as_ref(), &[b"/usr/bin/cc"]), 1024)
            .unwrap_err()
            .kind,
        ErrorKind::PermissionDenied
    );
    assert!(runtime.denied());
    Ok(())
}
