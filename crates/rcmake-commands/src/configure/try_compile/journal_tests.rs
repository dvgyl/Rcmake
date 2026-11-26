use super::*;
use rcmake_host::PureHost;

#[test]
fn keeps_exact_environment_and_validates_a_read() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    let header = Path::new("/work/input.h");
    host.create_dir_all(scratch)?;
    host.seed_file(header, b"old")?;
    host.env_set(b"LC_MESSAGES", Some(b"C"));
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    assert_eq!(journal.read(header, ReadRange::All { limit: 32 })?, b"old");
    journal.env_get(b"LC_MESSAGES");
    journal.env_all();
    let journal = journal.finish();
    assert_eq!(journal.full_environment(), Some(host.env_all().as_slice()));
    assert_eq!(
        journal.environment().get(b"LC_MESSAGES".as_slice()),
        Some(&Some("C".into()))
    );
    assert!(journal.verified_inputs(&host).is_some());
    host.env_set(b"LC_MESSAGES", Some(b"en_US"));
    assert!(journal.verified_inputs(&host).is_none());
    Ok(())
}

#[test]
fn observes_external_writes_and_blocks_them_in_speculation() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    let outside = Path::new("/work/outside");
    host.create_dir_all(scratch)?;
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    journal.write(outside, b"first", WriteMode::Truncate)?;
    assert_eq!(journal.finish().impure(), Some(ImpureReason::ExternalWrite));

    host.symlink(outside, &scratch.join("alias"))?;
    let journal = JournalHost::new(&host, scratch, JournalMode::Speculate, None)?;
    assert!(
        journal
            .write(&scratch.join("alias"), b"second", WriteMode::Truncate)
            .is_err()
    );
    assert_eq!(host.read(outside, ReadRange::All { limit: 32 })?, b"first");
    assert_eq!(journal.finish().impure(), Some(ImpureReason::ExternalWrite));
    Ok(())
}

#[test]
fn ignores_internal_time_and_private_link_reads() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    let link = scratch.join("link");
    host.create_dir_all(scratch)?;
    host.symlink(Path::new("/unused"), &link)?;
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    assert_eq!(journal.read_link(&link)?, Path::new("/unused"));
    journal.monotonic();
    journal.system();
    let recorded = journal.finish();
    host.remove_file(&link)?;
    assert!(recorded.verified_inputs(&host).is_some());
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    journal.now();
    assert_eq!(
        journal.finish().impure(),
        Some(ImpureReason::NondeterministicInput)
    );
    Ok(())
}

#[test]
fn logs_distinct_impurity_sites_and_preserves_the_first_reason() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    host.create_dir_all(scratch)?;
    let journal =
        JournalHost::new(&host, scratch, JournalMode::Observe, None)?.with_impurity_sites(true);
    journal.guard_effect(ImpureReason::ExternalWrite)?;
    journal.guard_effect(ImpureReason::SharedState)?;
    let journal = journal.finish();
    assert_eq!(journal.impure(), Some(ImpureReason::ExternalWrite));
    let [(first, write), (second, shared)] = journal.impurities() else {
        panic!("Expected two impurity sites.");
    };
    assert_eq!(*first, ImpureReason::ExternalWrite);
    assert_eq!(*second, ImpureReason::SharedState);
    assert_eq!(write.file(), file!());
    assert_eq!(shared.line(), write.line() + 1);
    Ok(())
}

#[test]
fn process_environment_keeps_unknown_variables_and_scalar_reads() -> HostResult<()> {
    let host = PureHost::default();
    let scratch = Path::new("/work/scratch");
    host.create_dir_all(scratch)?;
    host.env_set(b"PWD", Some(b"/first"));
    host.env_set(b"SHLVL", Some(b"1"));
    host.env_set(b"GITHUB_RUN_ID", Some(b"123"));
    host.env_set(b"CPATH", Some(b"/include"));
    host.env_set(b"COLUMNS", Some(b"80"));
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    let mut projected = journal.env_all();
    super::super::plan::process_environment(&mut projected);
    assert_eq!(
        projected,
        vec![
            ("COLUMNS".into(), "80".into()),
            ("CPATH".into(), "/include".into())
        ]
    );
    let process_journal = journal.finish();
    let journal = JournalHost::new(&host, scratch, JournalMode::Observe, None)?;
    assert_eq!(
        journal.env_get(b"PWD").as_deref().map(Vec::as_slice),
        Some(b"/first".as_slice())
    );
    let scalar_journal = journal.finish();
    host.env_set(b"PWD", Some(b"/second"));
    host.env_set(b"SHLVL", Some(b"2"));
    host.env_set(b"GITHUB_RUN_ID", Some(b"456"));
    assert!(process_journal.verified_inputs(&host).is_some());
    assert!(scalar_journal.verified_inputs(&host).is_none());
    host.env_set(b"CPATH", Some(b"/other"));
    assert!(process_journal.verified_inputs(&host).is_none());
    Ok(())
}

#[test]
fn journal_checks_json_round_trips_all_variants() -> Result<(), Box<dyn std::error::Error>> {
    use rcmake_json::{ReadOptions, parse, write_compact};

    let digest = Digest::from_bytes([0x55; 32]);
    let checks = JournalChecks(vec![
        (Check::Metadata(PathBuf::from("/work/a.h"), true), digest),
        (
            Check::Slice(
                PathBuf::from("/work/b.h"),
                Slice {
                    offset: 4,
                    len: Some(16),
                },
            ),
            digest,
        ),
        (
            Check::Slice(
                PathBuf::from("/work/c.h"),
                Slice {
                    offset: 0,
                    len: None,
                },
            ),
            digest,
        ),
        (Check::Chunks(PathBuf::from("/work/d.h")), digest),
        (Check::DirectoryOrder(PathBuf::from("/work/dir")), digest),
        (
            Check::OrderedDirectoryMissing(PathBuf::from("/work/missing")),
            digest,
        ),
        (
            Check::FollowMissing(PathBuf::from("/work/dangling")),
            digest,
        ),
        (Check::Link(PathBuf::from("/work/link")), digest),
        (Check::Canonical(PathBuf::from("/work/real")), digest),
        (Check::Environment, digest),
        (Check::Executable, digest),
        (Check::WorkingDirectory, digest),
        (Check::System, digest),
        (Check::Request(3), digest),
    ]);
    let encoded = write_compact(&checks.to_json());
    let parsed = parse(
        encoded.as_slice(),
        &ReadOptions {
            allow_comments: false,
            dup_keys_last_wins: false,
        },
    )?;
    assert_eq!(JournalChecks::from_json(&parsed)?, checks);
    assert!(
        JournalChecks::from_json(&Value::array_from([Value::array_from([
            Value::string("UnknownVariant"),
            digest.to_json(),
        ])]))
        .is_err()
    );
    Ok(())
}
