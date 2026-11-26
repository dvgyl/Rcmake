use super::*;
use rcmake_host::PureHost;

fn abs(path: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{path}"))
    } else {
        PathBuf::from(path)
    }
}

fn host() -> PureHost {
    PureHost::new(abs("/src"), abs("/bin/rcmake"))
}

fn site() -> HermeticSite {
    HermeticSite::new(&abs("/src/CMakeLists.txt"), 19)
}

#[test]
fn off_warn_and_strict_apply_to_each_observation() {
    let host = host();
    for (mode, expected) in [
        (HermeticMode::Off, AuditDecision::Allow),
        (HermeticMode::Warn, AuditDecision::Warn),
        (HermeticMode::Strict, AuditDecision::Deny),
    ] {
        let mut auditor = HermeticAuditor::new(mode, HermeticPolicy::from_host(&host, []));
        let site = site();
        let outcome = auditor.check_env(&site, b"TOKEN");
        assert_eq!(outcome.decision, expected);
        assert_eq!(outcome.diagnostic.is_some(), mode != HermeticMode::Off);
        let duplicate = auditor.check_env(&site, b"TOKEN");
        assert_eq!(duplicate.decision, expected);
        assert!(duplicate.diagnostic.is_none());
    }
}

#[test]
fn applies_environment_and_process_allowlists() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    host.env_set(b"RCMAKE_HERMETIC_ALLOW_ENV", Some(b"SDKROOT; CUSTOM"));
    let compiler = abs("/src/bin/cc");
    host.seed_file(&compiler, b"")?;
    host.set_permissions(&compiler, 0o755)?;
    let mut policy = HermeticPolicy::from_host(&host, []);
    assert!(policy.allow_program(&host, &compiler));
    for name in [
        b"PATH".as_slice(),
        b"HOME",
        b"USERPROFILE",
        b"TMP",
        b"SDKROOT",
        b"CUSTOM",
    ] {
        assert!(policy.allows_env(name));
    }
    let mut auditor = HermeticAuditor::new(HermeticMode::Strict, policy);
    let env = vec![
        ("PATH".into(), "bin".into()),
        ("TOKEN".into(), "secret".into()),
    ];
    assert_eq!(
        auditor.process_env(env),
        vec![("PATH".into(), "bin".into())]
    );
    assert_eq!(
        auditor.check_process(&host, &site(), &compiler).decision,
        AuditDecision::Allow
    );
    assert_eq!(
        auditor
            .check_process(&host, &site(), Path::new("cc"))
            .decision,
        AuditDecision::Deny
    );
    Ok(())
}

#[test]
fn checks_roots_by_component_and_blocks_symlink_escape() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    for path in [
        "/src/nested/input",
        "/src-other/input",
        "/sdk/include/input",
        "/outside/input",
    ] {
        host.seed_file(&abs(path), b"")?;
    }
    host.symlink(&abs("/outside"), &abs("/src/link"))?;
    let root = abs("/sdk");
    host.env_set(
        b"RCMAKE_HERMETIC_ALLOW_PATHS",
        Some(&rcmake_host::path::from_native(&root)),
    );
    let src = abs("/src");
    let policy = HermeticPolicy::from_host(&host, [src.as_path()]);
    for path in [
        "/src/nested/../nested/input",
        "/src/new/output",
        "/sdk/include/input",
    ] {
        assert!(
            policy
                .resolve_allowed_path(&host, &abs(path), None, PathAccess::Read)
                .is_some()
        );
    }
    for path in [
        "/src/../src-other/input",
        "/src-other/input",
        "/src/link/input",
    ] {
        assert!(
            policy
                .resolve_allowed_path(&host, &abs(path), None, PathAccess::Read)
                .is_none()
        );
    }
    Ok(())
}

#[test]
fn keeps_untruncated_paths_for_deduplication() -> Result<(), &'static str> {
    let host = host();
    let mut auditor =
        HermeticAuditor::new(HermeticMode::Strict, HermeticPolicy::from_host(&host, []));
    let prefix = "a".repeat(200);
    let path = abs(&format!("/{prefix}/{prefix}/CMakeLists#dev?.txt"));
    let site = HermeticSite::new(&path, 12);
    assert_eq!(site.file(), path.as_path());
    assert!(site.to_string().ends_with("CMakeLists#dev?.txt:12"));
    for suffix in ["#one", "?two"] {
        let input = abs(&format!("/{prefix}/{prefix}/{suffix}"));
        let violation = auditor
            .check_path(&host, &site, &input)
            .diagnostic
            .ok_or("Missing diagnostic.")?;
        assert_eq!(violation.site, site);
    }
    assert_eq!(auditor.records().len(), 2);
    Ok(())
}

#[test]
fn reports_invalid_entries_and_combines_env_names() -> Result<(), &'static str> {
    let host = host();
    host.env_set(b"RCMAKE_HERMETIC_ALLOW_ENV", Some(b"OK;BAD=private"));
    host.env_set(b"RCMAKE_HERMETIC_ALLOW_PATHS", Some(b"relative;another"));
    let policy = HermeticPolicy::from_host(&host, []);
    assert_eq!(policy.warnings().len(), 3);
    assert!(
        policy
            .warnings()
            .iter()
            .all(|warning| !warning.contains("private"))
    );
    let mut auditor = HermeticAuditor::new(HermeticMode::Warn, policy);
    for name in [b"B", b"A", b"C", b"B"] {
        auditor.check_env(&site(), name);
    }
    let summary = auditor
        .take_omission_summary()
        .ok_or("Missing environment rule.")?;
    assert_eq!(summary.to_string(), "RCMAKE_HERMETIC_ALLOW_ENV=A;B;C");
    assert!(auditor.take_omission_summary().is_none());
    assert_eq!(display_text("test\u{202e}"), "test<U+202E>");
    Ok(())
}

#[test]
fn bounds_records_and_formats_clickable_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let mut auditor =
        HermeticAuditor::new(HermeticMode::Strict, HermeticPolicy::from_host(&host, []))
            .with_record_limit(2);
    let site = site();
    let record = auditor
        .check_env(&site, b"FOO")
        .diagnostic
        .ok_or("missing diagnostic")?;
    assert!(record.to_string().starts_with(&format!("{site}: ")));
    assert!(record.hint().contains("-DFOO=..."));
    assert!(record.hint().contains("${FOO}"));
    let url = "https://user:password@example.org/path?token=secret";
    assert_eq!(
        auditor.check_network(&site, url, true).decision,
        AuditDecision::Allow
    );
    let denied = auditor.check_fetch_network(&site, url, false, "widget");
    assert_eq!(denied.decision, AuditDecision::Deny);
    let record = denied.diagnostic.ok_or("missing network diagnostic")?;
    assert_eq!(record.value, "https://example.org");
    assert!(record.hint().contains("--fetch-lock=update"));
    assert!(auditor.check_env(&site, b"FOO").diagnostic.is_none());
    assert_eq!(
        auditor.check_env(&site, b"BAR").decision,
        AuditDecision::Deny
    );
    assert_eq!(
        auditor.summary(),
        AuditSummary {
            recorded: 2,
            omitted: 1,
            user: 2,
            bundled: 0,
            omitted_user: 1,
            omitted_bundled: 0,
            allow_env: None,
        }
    );
    assert_eq!(auditor.records().len(), 2);
    assert!(auditor.take_omission_summary().is_some());
    assert!(auditor.take_omission_summary().is_none());
    Ok(())
}

#[test]
fn allows_symlinked_modules_and_templates_without_allowing_outside()
-> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    host.seed_file(
        &abs("/repo/modules/Modules/CMakeDetermineSystem.cmake"),
        b"",
    )?;
    host.seed_file(&abs("/repo/modules/Templates/Test.in"), b"")?;
    host.seed_file(&abs("/outside/secret.txt"), b"")?;
    host.create_dir_all(&abs("/stage/share/cmake-4.5"))?;
    host.symlink(
        &abs("/repo/modules/Modules"),
        &abs("/stage/share/cmake-4.5/Modules"),
    )?;
    host.symlink(
        &abs("/repo/modules/Templates"),
        &abs("/stage/share/cmake-4.5/Templates"),
    )?;

    let stage_root = abs("/stage/share/cmake-4.5");
    let mut policy = HermeticPolicy::from_host(&host, [stage_root.as_path()]);
    for child in ["Modules", "Templates"] {
        assert!(policy.allow_root(&host, &stage_root.join(child)));
    }
    assert!(
        policy
            .resolve_allowed_path(
                &host,
                &abs("/stage/share/cmake-4.5/Modules/CMakeDetermineSystem.cmake"),
                None,
                PathAccess::Read,
            )
            .is_some()
    );
    assert!(
        policy
            .resolve_allowed_path(
                &host,
                &abs("/stage/share/cmake-4.5/Templates/Test.in"),
                None,
                PathAccess::Read,
            )
            .is_some()
    );
    assert!(
        policy
            .resolve_allowed_path(
                &host,
                &abs("/stage/share/cmake-4.5/../../../outside/secret.txt"),
                None,
                PathAccess::Read,
            )
            .is_none()
    );
    Ok(())
}

#[test]
fn keeps_user_records_first_even_after_hundreds_of_bundled_records() {
    let host = host();
    let modules = abs("/data/Modules");
    let mut policy = HermeticPolicy::from_host(&host, []);
    policy.set_bundled_roots(&host, [modules.as_path()]);
    let mut auditor = HermeticAuditor::new(HermeticMode::Warn, policy);
    let bundled_site = HermeticSite::new(&modules.join("CMakeDetermineSystem.cmake"), 10);
    for i in 0..300 {
        let p = abs(&format!("/external/probe_{i}"));
        let _ = auditor.check_path(&host, &bundled_site, &p);
    }
    let user_site = site();
    let _ = auditor.check_path(&host, &user_site, &abs("/etc/hosts"));
    let records: Vec<_> = auditor.records().collect();
    assert_eq!(records.len(), BUNDLED_RECORD_LIMIT + 1);
    let first = records.first().expect("user record should be retained");
    assert!(!first.bundled);
    assert_eq!(first.site, user_site);
    assert!(first.value.contains("hosts"));
}

#[test]
fn startup_site_omits_line_zero() {
    assert_eq!(HermeticSite::startup().to_string(), "<rcmake startup>");
    assert_eq!(
        HermeticSite::new(&abs("/src/CMakeLists.txt"), 0).to_string(),
        abs("/src/CMakeLists.txt").to_string_lossy()
    );
}

#[test]
fn strict_denies_bundled_read_and_process_violations() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let modules = abs("/data/Modules");
    let secret = abs("/private/secret.txt");
    let unapproved = abs("/bin/evil");
    host.create_dir_all(&modules)?;
    host.seed_file(&secret, b"secret")?;
    host.seed_file(&unapproved, b"")?;
    host.set_permissions(&unapproved, 0o755)?;

    let mut policy = HermeticPolicy::from_host(&host, [modules.as_path()]);
    policy.set_bundled_roots(&host, [modules.as_path()]);
    let mut auditor = HermeticAuditor::new(HermeticMode::Strict, policy);
    let bundled_site = HermeticSite::new(&modules.join("CMakeDetermineSystem.cmake"), 14);

    let (path_outcome, resolved) = auditor.check_read_path(&host, &bundled_site, &secret, None);
    assert_eq!(path_outcome.decision, AuditDecision::Deny);
    assert!(resolved.is_none());

    let proc_outcome = auditor.check_process(&host, &bundled_site, &unapproved);
    assert_eq!(proc_outcome.decision, AuditDecision::Deny);
    Ok(())
}

#[test]
fn allow_program_in_roots_rejects_symlinks_escaping_trusted_roots()
-> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let trusted_dir = abs("/trusted/bin");
    let trusted_tool = trusted_dir.join("cc");
    let outside_tool = abs("/outside/evil");
    let escape_link = trusted_dir.join("clang");
    host.seed_file(&trusted_tool, b"")?;
    host.set_permissions(&trusted_tool, 0o755)?;
    host.seed_file(&outside_tool, b"")?;
    host.set_permissions(&outside_tool, 0o755)?;
    host.symlink(&outside_tool, &escape_link)?;

    let mut policy = HermeticPolicy::from_host(&host, []);
    assert!(policy.allow_program_in_roots(&host, &trusted_tool, [trusted_dir.as_path()]));
    assert!(!policy.allow_program_in_roots(&host, &escape_link, [trusted_dir.as_path()]));
    assert!(!policy.allow_program_in_roots(&host, &outside_tool, [trusted_dir.as_path()]));

    let mut auditor = HermeticAuditor::new(HermeticMode::Strict, policy);
    assert_eq!(
        auditor
            .check_process(&host, &site(), &trusted_tool)
            .decision,
        AuditDecision::Allow
    );
    assert_eq!(
        auditor.check_process(&host, &site(), &escape_link).decision,
        AuditDecision::Deny
    );
    assert_eq!(
        auditor
            .check_process(&host, &site(), &outside_tool)
            .decision,
        AuditDecision::Deny
    );
    Ok(())
}

#[test]
fn allow_program_in_roots_rejects_case_only_lookalike_directory()
-> Result<(), Box<dyn std::error::Error>> {
    struct DarwinHost(PureHost);
    impl Host for DarwinHost {
        fn metadata(
            &self,
            path: &Path,
            follow: Follow,
        ) -> rcmake_host::HostResult<rcmake_host::Metadata> {
            self.0.metadata(path, follow)
        }
        fn read(
            &self,
            path: &Path,
            range: rcmake_host::ReadRange,
        ) -> rcmake_host::HostResult<Vec<u8>> {
            self.0.read(path, range)
        }
        fn read_chunks(
            &self,
            path: &Path,
            consume: &mut dyn FnMut(&[u8]),
        ) -> rcmake_host::HostResult<()> {
            self.0.read_chunks(path, consume)
        }
        fn write(
            &self,
            path: &Path,
            data: &[u8],
            mode: rcmake_host::WriteMode,
        ) -> rcmake_host::HostResult<()> {
            self.0.write(path, data, mode)
        }
        fn read_dir(&self, path: &Path) -> rcmake_host::HostResult<Vec<rcmake_host::DirEntry>> {
            self.0.read_dir(path)
        }
        fn create_dir_all(&self, path: &Path) -> rcmake_host::HostResult<()> {
            self.0.create_dir_all(path)
        }
        fn remove_file(&self, path: &Path) -> rcmake_host::HostResult<()> {
            self.0.remove_file(path)
        }
        fn remove_dir(&self, path: &Path) -> rcmake_host::HostResult<()> {
            self.0.remove_dir(path)
        }
        fn remove_dir_all(&self, path: &Path) -> rcmake_host::HostResult<()> {
            self.0.remove_dir_all(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> rcmake_host::HostResult<()> {
            self.0.rename(from, to)
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> rcmake_host::HostResult<()> {
            self.0.rename_noreplace(from, to)
        }
        fn lock_file(
            &self,
            path: &Path,
            timeout: Option<std::time::Duration>,
        ) -> rcmake_host::HostResult<Box<dyn rcmake_host::FileLockGuard>> {
            self.0.lock_file(path, timeout)
        }
        fn copy_file(
            &self,
            from: &Path,
            to: &Path,
            opts: rcmake_host::CopyOptions,
        ) -> rcmake_host::HostResult<()> {
            self.0.copy_file(from, to, opts)
        }
        fn symlink(&self, target: &Path, link: &Path) -> rcmake_host::HostResult<()> {
            self.0.symlink(target, link)
        }
        fn hard_link(&self, source: &Path, destination: &Path) -> rcmake_host::HostResult<()> {
            self.0.hard_link(source, destination)
        }
        fn read_link(&self, path: &Path) -> rcmake_host::HostResult<PathBuf> {
            self.0.read_link(path)
        }
        fn canonicalize(&self, path: &Path) -> rcmake_host::HostResult<PathBuf> {
            self.0.canonicalize(path)
        }
        fn set_permissions(&self, path: &Path, mode: u32) -> rcmake_host::HostResult<()> {
            self.0.set_permissions(path, mode)
        }
        fn set_times(
            &self,
            path: &Path,
            atime: Option<rcmake_host::FileTime>,
            mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()> {
            self.0.set_times(path, atime, mtime)
        }
        fn set_symlink_times(
            &self,
            path: &Path,
            atime: Option<rcmake_host::FileTime>,
            mtime: Option<rcmake_host::FileTime>,
        ) -> rcmake_host::HostResult<()> {
            self.0.set_symlink_times(path, atime, mtime)
        }
        fn env_get(&self, key: &[u8]) -> Option<BString> {
            self.0.env_get(key)
        }
        fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
            self.0.env_set(key, value);
        }
        fn env_all(&self) -> Vec<(BString, BString)> {
            self.0.env_all()
        }
        fn run(
            &self,
            request: &rcmake_host::RunRequest,
            on_output: &mut dyn FnMut(Stream, &[u8]),
        ) -> rcmake_host::HostResult<rcmake_host::RunOutput> {
            self.0.run(request, on_output)
        }
        fn write_out(&self, stream: Stream, bytes: &[u8]) {
            self.0.write_out(stream, bytes);
        }
        fn read_stdin(&self, limit: u64) -> rcmake_host::HostResult<Vec<u8>> {
            self.0.read_stdin(limit)
        }
        fn is_terminal(&self, stream: Stream) -> bool {
            self.0.is_terminal(stream)
        }
        fn now(&self) -> rcmake_host::FileTime {
            self.0.now()
        }
        fn monotonic(&self) -> std::time::Duration {
            self.0.monotonic()
        }
        fn sleep(&self, duration: std::time::Duration) {
            self.0.sleep(duration);
        }
        fn time_zone(&self) -> rcmake_host::TimeZone {
            self.0.time_zone()
        }
        fn current_exe(&self) -> rcmake_host::HostResult<PathBuf> {
            self.0.current_exe()
        }
        fn initial_cwd(&self) -> PathBuf {
            self.0.initial_cwd()
        }
        fn system(&self) -> rcmake_host::SystemInfo {
            let mut info = self.0.system();
            info.sysname = BString::from("Darwin");
            info
        }
    }

    let inner = host();
    let trusted_dir = abs("/usr/bin");
    let trusted_cc = trusted_dir.join("cc");
    let lookalike_cc = abs("/USR/bin/cc");
    inner.seed_file(&trusted_cc, b"")?;
    inner.set_permissions(&trusted_cc, 0o755)?;
    inner.seed_file(&lookalike_cc, b"")?;
    inner.set_permissions(&lookalike_cc, 0o755)?;
    let darwin = DarwinHost(inner);

    let mut policy = HermeticPolicy::from_host(&darwin, []);
    assert!(policy.allow_program_in_roots(&darwin, &trusted_cc, [trusted_dir.as_path()]));
    assert!(!policy.allow_program_in_roots(&darwin, &lookalike_cc, [trusted_dir.as_path()]));

    let mut auditor = HermeticAuditor::new(HermeticMode::Strict, policy);
    assert_eq!(
        auditor
            .check_process(&darwin, &site(), &trusted_cc)
            .decision,
        AuditDecision::Allow
    );
    assert_eq!(
        auditor
            .check_process(&darwin, &site(), &lookalike_cc)
            .decision,
        AuditDecision::Deny
    );
    Ok(())
}
