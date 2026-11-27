use std::error::Error;
use std::io::Write;

use flate2::{Compression, write::GzEncoder};
use tar::{Builder, EntryType, Header};

use super::*;

type TestResult = Result<(), Box<dyn Error>>;

const KEY: [u8; 32] = [0xab; 32];
const BLOB: [u8; 32] = [0x01; 32];

fn sample() -> Result<Vec<u8>, ArchiveError> {
    write_portable(&[
        (PortableName::Manifest, b"{}\n".as_slice()),
        (PortableName::Entry(KEY), b"entry frame".as_slice()),
        (PortableName::Blob(BLOB), b"blob frame".as_slice()),
    ])
}

fn gzip(tar: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(tar)?;
    Ok(encoder.finish()?)
}

/// Build a tar stream with the `tar` crate. Each item is a name, type, and data.
fn foreign_tar(members: &[(&str, EntryType, &[u8])]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut builder = Builder::new(Vec::new());
    for (name, kind, data) in members {
        let mut header = Header::new_ustar();
        header.set_entry_type(*kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name("manifest.json")?;
        }
        header.as_old_mut().name.fill(0);
        header
            .as_old_mut()
            .name
            .get_mut(..name.len())
            .ok_or("name too long")?
            .copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, *data)?;
    }
    Ok(builder.into_inner()?)
}

fn rejects(archive: &[u8], needle: &str) {
    match read_portable(archive, &PortableLimits::default()) {
        Ok(members) => panic!("expected rejection containing {needle:?}, got {members:?}"),
        Err(error) => assert!(
            error.message.contains(needle),
            "{:?} does not contain {needle:?}",
            error.message
        ),
    }
}

#[test]
fn round_trip_is_deterministic_and_ordered() -> TestResult {
    let first = sample()?;
    assert_eq!(first, sample()?);
    let members = read_portable(&first, &PortableLimits::default())?;
    let names: Vec<_> = members.iter().map(|member| member.name).collect();
    assert_eq!(
        names,
        [
            PortableName::Manifest,
            PortableName::Entry(KEY),
            PortableName::Blob(BLOB)
        ]
    );
    assert_eq!(
        members.get(1).map(|m| m.data.as_slice()),
        Some(&b"entry frame"[..])
    );
    Ok(())
}

#[test]
fn foreign_ustar_regular_files_are_accepted() -> TestResult {
    let tar = foreign_tar(&[
        ("manifest.json", EntryType::Regular, b"{}"),
        (
            &PortableName::Blob(BLOB).to_name(),
            EntryType::Regular,
            b"x",
        ),
    ])?;
    assert_eq!(
        read_portable(&gzip(&tar)?, &PortableLimits::default())?.len(),
        2
    );
    Ok(())
}

#[test]
fn writer_rejects_unsorted_or_duplicate_names() {
    let blob = PortableName::Blob(BLOB);
    for members in [
        vec![(blob, b"".as_slice())],
        vec![
            (PortableName::Manifest, b"".as_slice()),
            (blob, b""),
            (blob, b""),
        ],
        vec![
            (PortableName::Manifest, b"".as_slice()),
            (blob, b""),
            (PortableName::Entry(KEY), b""),
        ],
    ] {
        assert!(write_portable(&members).is_err());
    }
}

#[test]
fn names_require_exact_lowercase_hex() {
    let upper = format!("blobs/{}", "AB".repeat(32));
    for name in [
        "entries/".to_owned(),
        upper,
        format!("blobs/{}", "a".repeat(63)),
        format!("blobs/../{}", "a".repeat(61)),
        format!("./blobs/{}", "a".repeat(64)),
        "Manifest.json".to_owned(),
    ] {
        assert_eq!(PortableName::parse(name.as_bytes()), None, "{name}");
    }
    assert_eq!(
        PortableName::parse(PortableName::Entry(KEY).to_name().as_bytes()),
        Some(PortableName::Entry(KEY))
    );
}

#[test]
fn links_devices_and_directories_are_rejected() -> TestResult {
    for kind in [
        EntryType::Symlink,
        EntryType::Link,
        EntryType::Char,
        EntryType::Block,
        EntryType::Fifo,
        EntryType::Directory,
        EntryType::GNUSparse,
    ] {
        let tar = foreign_tar(&[
            ("manifest.json", EntryType::Regular, b"{}"),
            (&PortableName::Blob(BLOB).to_name(), kind, b""),
        ])?;
        rejects(&gzip(&tar)?, "not a regular file");
    }
    Ok(())
}

#[test]
fn extension_headers_that_rename_members_are_rejected() -> TestResult {
    let blob = PortableName::Blob(BLOB).to_name();
    let pax = format!("{} path={blob}\n", blob.len() + 7);
    for (kind, data) in [
        (EntryType::XHeader, pax.as_bytes()),
        (EntryType::XGlobalHeader, pax.as_bytes()),
        (EntryType::GNULongName, blob.as_bytes()),
    ] {
        let tar = foreign_tar(&[
            ("manifest.json", EntryType::Regular, b"{}"),
            ("entries/x", kind, data),
            ("blobs/short", EntryType::Regular, b""),
        ])?;
        rejects(&gzip(&tar)?, "not a regular file");
    }
    Ok(())
}

#[test]
fn unknown_escaping_and_duplicate_names_are_rejected() -> TestResult {
    let blob = PortableName::Blob(BLOB).to_name();
    for (name, needle) in [
        ("../manifest.json", "unexpected member"),
        ("/manifest.json", "unexpected member"),
        ("lockfile", "unexpected member"),
        (blob.as_str(), "out of order"),
    ] {
        let tar = foreign_tar(&[
            ("manifest.json", EntryType::Regular, b"{}"),
            (&PortableName::Entry(KEY).to_name(), EntryType::Regular, b""),
            (blob.as_str(), EntryType::Regular, b""),
            (name, EntryType::Regular, b""),
        ])?;
        if name == blob {
            rejects(&gzip(&tar)?, "repeats member");
        } else {
            rejects(&gzip(&tar)?, needle);
        }
    }
    let unordered = foreign_tar(&[
        ("manifest.json", EntryType::Regular, b"{}"),
        (blob.as_str(), EntryType::Regular, b""),
        (&PortableName::Entry(KEY).to_name(), EntryType::Regular, b""),
    ])?;
    rejects(&gzip(&unordered)?, "out of order");
    let headless = foreign_tar(&[(blob.as_str(), EntryType::Regular, b"")])?;
    rejects(&gzip(&headless)?, "out of order");
    Ok(())
}

#[test]
fn trailing_data_and_truncation_are_rejected() -> TestResult {
    let archive = sample()?;
    let mut doubled = archive.clone();
    doubled.extend_from_slice(&archive);
    rejects(&doubled, "after its gzip stream");
    let mut garbage = archive.clone();
    garbage.push(0);
    rejects(&garbage, "after its gzip stream");
    rejects(&archive[..archive.len() - 9], "");

    let mut tar = foreign_tar(&[("manifest.json", EntryType::Regular, b"{}")])?;
    tar.extend_from_slice(&[0; 1024]);
    tar.extend_from_slice(&foreign_tar(&[(
        "manifest.json",
        EntryType::Regular,
        b"{}",
    )])?);
    rejects(&gzip(&tar)?, "after its tar end marker");

    let mut short = foreign_tar(&[("manifest.json", EntryType::Regular, b"{}")])?;
    short.truncate(700);
    rejects(&gzip(&short)?, "truncated");
    rejects(&gzip(&[])?, "truncated");
    Ok(())
}

#[test]
fn corrupt_headers_are_rejected() -> TestResult {
    let mut tar = foreign_tar(&[("manifest.json", EntryType::Regular, b"{}")])?;
    tar[20] ^= 1;
    rejects(&gzip(&tar)?, "checksum");

    let mut gnu = Header::new_gnu();
    gnu.set_path("manifest.json")?;
    gnu.set_size(0);
    gnu.set_cksum();
    let mut builder = Builder::new(Vec::new());
    builder.append(&gnu, &b""[..])?;
    rejects(&gzip(&builder.into_inner()?)?, "POSIX ustar");

    let mut huge = foreign_tar(&[("manifest.json", EntryType::Regular, b"")])?;
    huge[124] = 0x80;
    let mut header = [0u8; BLOCK];
    header.copy_from_slice(&huge[..BLOCK]);
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(b)
            }
        })
        .sum();
    huge[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    rejects(&gzip(&huge)?, "size field");
    Ok(())
}

#[test]
fn limits_apply_before_allocation() -> TestResult {
    let archive = sample()?;
    let strict = |change: fn(&mut PortableLimits)| {
        let mut limits = PortableLimits::default();
        change(&mut limits);
        read_portable(&archive, &limits)
    };
    assert!(strict(|l| l.archive_bytes = 10).is_err());
    assert!(strict(|l| l.tar_bytes = 1024).is_err());
    assert!(strict(|l| l.member_bytes = 4).is_err());
    assert!(strict(|l| l.manifest_bytes = 1).is_err());
    assert!(strict(|l| l.total_bytes = 10).is_err());
    assert!(strict(|l| l.members = 2).is_err());
    assert!(strict(|_| {}).is_ok());

    // A small archive that inflates to a large declared member stops at the tar limit.
    let bomb = write_portable(&[
        (PortableName::Manifest, b"{}".as_slice()),
        (
            PortableName::Blob(BLOB),
            vec![0u8; 4 * 1024 * 1024].as_slice(),
        ),
    ])?;
    assert!(bomb.len() < 64 * 1024);
    let limits = PortableLimits {
        tar_bytes: 1024 * 1024,
        ..PortableLimits::default()
    };
    let error = read_portable(&bomb, &limits)
        .err()
        .ok_or("expected a limit error")?;
    assert!(error.message.contains("size limit"), "{}", error.message);
    Ok(())
}
