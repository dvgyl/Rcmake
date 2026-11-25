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
                .resolve_allowed_path(&host, &abs(path), None)
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
                .resolve_allowed_path(&host, &abs(path), None)
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
            allow_env: None,
        }
    );
    assert_eq!(auditor.records().len(), 2);
    assert!(auditor.take_omission_summary().is_some());
    assert!(auditor.take_omission_summary().is_none());
    Ok(())
}
