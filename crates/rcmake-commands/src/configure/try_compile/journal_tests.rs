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
