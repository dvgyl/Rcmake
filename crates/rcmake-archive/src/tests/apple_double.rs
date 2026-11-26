//! macOS AppleDouble (`._*`) and `__MACOSX/` rules of the CMake archive reader and writer.
//!
//! Each test runs the macOS rules and the rules for other platforms on every host.

use std::io::Read;

use super::*;

const ARCHIVE: &str = "/archive";

/// Append one ustar entry with the exact name bytes, without name normalization.
fn raw_entry(
    builder: &mut Builder<Vec<u8>>,
    name: &[u8],
    kind: EntryType,
    data: &[u8],
    link: Option<&[u8]>,
) -> std::io::Result<()> {
    let mut header = Header::new_ustar();
    let field = header
        .as_old_mut()
        .name
        .get_mut(..name.len())
        .ok_or_else(|| std::io::Error::other("The test entry name is too long."))?;
    field.copy_from_slice(name);
    header.set_entry_type(kind);
    header.set_mode(if kind == EntryType::Directory {
        0o755
    } else {
        0o644
    });
    header.set_mtime(0);
    header.set_size(data.len() as u64);
    if let Some(link) = link {
        header.set_link_name_literal(link)?;
    }
    header.set_cksum();
    builder.append(&header, data)
}

fn file(builder: &mut Builder<Vec<u8>>, name: &str) -> std::io::Result<()> {
    raw_entry(builder, name.as_bytes(), EntryType::Regular, b"x", None)
}

fn seed_tar(host: &PureHost, builder: Builder<Vec<u8>>) -> TestResult {
    host.write(
        Path::new(ARCHIVE),
        &builder.into_inner()?,
        WriteMode::Truncate,
    )?;
    Ok(())
}

fn run(host: &PureHost, options: &ExtractOptions, mac: bool) -> Result<Vec<BString>, ArchiveError> {
    extract_archive(
        host,
        Path::new(ARCHIVE),
        ExtractRequest::from(options).with_mac_extensions(mac),
    )
}

fn list(host: &PureHost, mac: bool) -> Result<Vec<BString>, ArchiveError> {
    run(
        host,
        &ExtractOptions {
            list_only: true,
            ..Default::default()
        },
        mac,
    )
}

#[test]
fn metadata_names_use_only_the_last_component() {
    for (name, expected) in [
        ("._", false),
        ("d/._", false),
        ("._/", true),
        ("._foo", true),
        ("._foo/", true),
        ("d/._x", true),
        ("._foo/bar", false),
        ("a._b", false),
        ("", false),
        ("/", false),
    ] {
        assert_eq!(is_mac_metadata_name(name.as_bytes()), expected, "{name}");
    }
}

#[test]
fn the_entry_after_metadata_is_always_kept() -> TestResult {
    let host = test_host();
    let mut builder = Builder::new(Vec::new());
    for name in ["._a", "._b", "._c", "d", "._e", "._f"] {
        file(&mut builder, name)?;
    }
    seed_tar(&host, builder)?;
    assert_eq!(
        list(&host, false)?,
        names(&["._a", "._b", "._c", "d", "._e", "._f"])
    );
    assert_eq!(
        run(&host, &extract_options(), true)?,
        names(&["._b", "d", "._f"])
    );
    for name in ["._a", "._c", "._e"] {
        assert!(absent(&host, &format!("/extract/{name}")), "{name}");
    }
    for name in ["._b", "d", "._f"] {
        assert_eq!(
            read_bytes(&host, &Path::new("/extract").join(name), 16)?,
            b"x"
        );
    }
    Ok(())
}

/// An entry name, its type, its link target, and `true` if the macOS rules hide it.
type Candidate<'a> = (&'a [u8], EntryType, Option<&'a [u8]>, bool);

#[test]
fn every_entry_type_with_a_metadata_name_is_hidden() -> TestResult {
    let host = test_host();
    let mut builder = Builder::new(Vec::new());
    let candidates: [Candidate<'_>; 10] = [
        (b"._", EntryType::Regular, None, false),
        (b"d/._", EntryType::Regular, None, false),
        (b"._foo/", EntryType::Regular, None, true),
        (b"d/._x", EntryType::Regular, None, true),
        (b"._d/", EntryType::Directory, None, true),
        (b"._l", EntryType::Symlink, Some(b"keep0"), true),
        (b"._h", EntryType::Link, Some(b"keep0"), true),
        (b"._z", EntryType::Continuous, None, true),
        (b"._foo/bar", EntryType::Regular, None, false),
        (b"a._b", EntryType::Regular, None, false),
    ];
    let mut all = Vec::new();
    let mut shown = Vec::new();
    for (index, (name, kind, link, hidden)) in candidates.into_iter().enumerate() {
        let data: &[u8] = if kind == EntryType::Regular || kind == EntryType::Continuous {
            b"x"
        } else {
            b""
        };
        raw_entry(&mut builder, name, kind, data, link)?;
        let keep = format!("keep{index}");
        file(&mut builder, &keep)?;
        all.extend([BString::from(name), BString::from(keep.as_str())]);
        if !hidden {
            shown.push(BString::from(name));
        }
        shown.push(BString::from(keep));
    }
    seed_tar(&host, builder)?;
    assert_eq!(list(&host, false)?, all);
    assert_eq!(list(&host, true)?, shown);
    Ok(())
}

#[test]
fn the_resolved_pax_path_decides() -> TestResult {
    let host = test_host();
    let mut builder = Builder::new(Vec::new());
    builder.append_pax_extensions([("path", b"._foo".as_slice())])?;
    file(&mut builder, "foo1")?;
    file(&mut builder, "keep")?;
    builder.append_pax_extensions([("path", b"realname".as_slice())])?;
    file(&mut builder, "._foo")?;
    seed_tar(&host, builder)?;
    assert_eq!(list(&host, false)?, names(&["._foo", "keep", "realname"]));
    assert_eq!(list(&host, true)?, names(&["keep", "realname"]));
    Ok(())
}

#[test]
fn metadata_paths_are_never_checked() -> TestResult {
    let host = test_host();
    let mut builder = Builder::new(Vec::new());
    file(&mut builder, "../._evil")?;
    file(&mut builder, "foo")?;
    file(&mut builder, "/abs/._x")?;
    file(&mut builder, "bar")?;
    seed_tar(&host, builder)?;
    assert!(run(&host, &extract_options(), false).is_err());
    assert!(absent(&host, "/extract"));
    assert_eq!(
        run(&host, &extract_options(), true)?,
        names(&["foo", "bar"])
    );
    assert_eq!(read_bytes(&host, Path::new("/extract/foo"), 16)?, b"x");
    assert_eq!(read_bytes(&host, Path::new("/extract/bar"), 16)?, b"x");
    assert!(absent(&host, "/evil") && absent(&host, "/._evil") && absent(&host, "/abs"));
    Ok(())
}

#[test]
fn member_patterns_never_match_metadata() -> TestResult {
    let host = test_host();
    let mut builder = Builder::new(Vec::new());
    file(&mut builder, "._foo")?;
    file(&mut builder, "foo")?;
    seed_tar(&host, builder)?;
    let options = ExtractOptions {
        list_only: true,
        patterns: names(&["._foo"]),
        ..Default::default()
    };
    assert_eq!(run(&host, &options, false)?, names(&["._foo"]));
    assert_eq!(
        run(&host, &options, true).map_err(|error| error.message),
        Err("._foo: Not found in archive".to_string())
    );
    Ok(())
}

#[test]
fn truncated_and_oversized_metadata_match_cmake() -> TestResult {
    const LIMIT: usize = 16 * 1024 * 1024;
    let host = test_host();

    let mut builder = Builder::new(Vec::new());
    raw_entry(&mut builder, b"._foo", EntryType::Regular, &[0; 4096], None)?;
    let mut bytes = builder.into_inner()?;
    bytes.truncate(512 + 100);
    host.write(Path::new(ARCHIVE), &bytes, WriteMode::Truncate)?;
    assert_eq!(
        list(&host, true).map_err(|error| error.message),
        Err("Problem with archive_read_next_header(): Truncated archive detected while reading macOS metadata".to_string())
    );

    let mut header = Header::new_ustar();
    header.set_path("._big")?;
    header.set_size(LIMIT as u64 + 1);
    header.set_mode(0o644);
    header.set_cksum();
    let mut bytes = header.as_bytes().to_vec();
    bytes.resize(4096, 0);
    host.write(Path::new(ARCHIVE), &bytes, WriteMode::Truncate)?;
    assert_eq!(
        list(&host, true).map_err(|error| error.message),
        Err("Problem with archive_read_next_header(): Oversized AppleDouble extension has size 16777217 > 16777216".to_string())
    );

    let mut builder = Builder::new(Vec::new());
    raw_entry(
        &mut builder,
        b"._big",
        EntryType::Regular,
        &vec![0; LIMIT],
        None,
    )?;
    file(&mut builder, "foo")?;
    seed_tar(&host, builder)?;
    assert_eq!(list(&host, true)?, names(&["foo"]));
    Ok(())
}

#[test]
fn zip_hides_only_macosx_resource_entries() -> TestResult {
    let host = test_host();
    let cases: [(&[&str], &[&str]); 3] = [
        (
            &["foo", "__MACOSX/._foo", "._bar", "bar", "d/._baz"],
            &["foo", "._bar", "bar", "d/._baz"],
        ),
        (
            &[
                "__MACOSX/",
                "__MACOSX/sub/",
                "__MACOSX/sub/plain.txt",
                "__MACOSX/d/._orphan",
                "__MACOSX/._top",
                "__MACOSXfoo/._x",
                "a/__MACOSX/._y",
                "keep.txt",
            ],
            &[
                "__MACOSX/sub/plain.txt",
                "__MACOSXfoo/._x",
                "a/__MACOSX/._y",
                "keep.txt",
            ],
        ),
        (
            &["__MACOSX/._", "__MACOSX/.x", "keep"],
            &["__MACOSX/.x", "keep"],
        ),
    ];
    for (entries, shown) in cases {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for entry in entries {
            if let Some(directory) = entry.strip_suffix('/') {
                writer.add_directory(directory, SimpleFileOptions::default())?;
            } else {
                writer.start_file(*entry, SimpleFileOptions::default())?;
                writer.write_all(b"x")?;
            }
        }
        host.write(
            Path::new(ARCHIVE),
            &writer.finish()?.into_inner(),
            WriteMode::Truncate,
        )?;
        assert_eq!(list(&host, false)?, names(entries));
        assert_eq!(list(&host, true)?, names(shown));
    }
    Ok(())
}

#[test]
fn pax_tar_writes_a_stub_that_keeps_real_metadata_names() -> TestResult {
    let host = test_host();
    host.seed_file(Path::new("/source/d/._foo"), b"real sidecar")?;
    host.seed_file(Path::new("/source/d/foo"), b"payload")?;
    let input = [PathBuf::from("d")];
    for format in [ArchiveFormat::Tar, ArchiveFormat::GnuTar] {
        for mac in [false, true] {
            let options = create_options(format);
            let created = create_archive(
                &host,
                Path::new(ARCHIVE),
                Path::new("/source"),
                &input,
                options.with_deterministic_order().with_mac_extensions(mac),
            )?;
            assert_eq!(created, names(&["d/", "d/._foo", "d/foo"]));
            let bytes = read_bytes(&host, Path::new(ARCHIVE), 1024 * 1024)?;
            let mut raw = Vec::new();
            for entry in tar::Archive::new(bytes.as_slice()).entries()? {
                let mut entry = entry?;
                let name = entry.path_bytes().into_owned();
                let mut data = Vec::new();
                entry.read_to_end(&mut data)?;
                raw.push((BString::from(name), data));
            }
            let stub = mac && format == ArchiveFormat::Tar;
            let raw_names: Vec<_> = raw.iter().map(|(name, _)| name.clone()).collect();
            if stub {
                assert_eq!(raw_names, names(&["d/", "d/._._foo", "d/._foo", "d/foo"]));
                let mut expected = b"\0\x05\x16\x07\0\x02\0\0Mac OS X        \0\x02".to_vec();
                for value in [9_u32, 50, 32, 2, 82, 0] {
                    expected.extend_from_slice(&value.to_be_bytes());
                }
                expected.resize(82, 0);
                assert_eq!(
                    raw.get(1).map(|(_, data)| data.as_slice()),
                    Some(expected.as_slice())
                );
            } else {
                assert_eq!(raw_names, created);
            }
            let _ = host.remove_dir_all(Path::new("/extract"));
            let extracted = run(&host, &extract_options(), true)?;
            if stub {
                assert_eq!(extracted, created);
                assert_eq!(
                    read_bytes(&host, Path::new("/extract/d/._foo"), 64)?,
                    b"real sidecar"
                );
            } else {
                // Without a stub, the macOS reader reads the real `._foo` as metadata.
                assert_eq!(extracted, names(&["d/", "d/foo"]));
            }
            assert_eq!(
                read_bytes(&host, Path::new("/extract/d/foo"), 64)?,
                b"payload"
            );
        }
    }
    Ok(())
}

#[test]
fn contiguous_tar_entries_are_files_or_slash_terminated_directories() -> TestResult {
    for mac in [false, true] {
        let host = test_host();
        let mut builder = Builder::new(Vec::new());
        raw_entry(&mut builder, b"file", EntryType::Continuous, b"data", None)?;
        raw_entry(&mut builder, b"foo/", EntryType::Continuous, b"", None)?;
        file(&mut builder, "foo/child")?;
        seed_tar(&host, builder)?;
        let details = list_archive_details(
            &host,
            Path::new(ARCHIVE),
            &ExtractOptions::default(),
            &|_| String::from("time"),
        )?;
        let kinds: Vec<_> = details.iter().map(|line| line.first().copied()).collect();
        assert_eq!(kinds, [Some(b'-'), Some(b'd'), Some(b'-')], "{details:?}");
        assert_eq!(
            run(&host, &extract_options(), mac)?,
            names(&["file", "foo/", "foo/child"])
        );
        assert_eq!(read_bytes(&host, Path::new("/extract/file"), 16)?, b"data");
        assert_eq!(
            host.metadata(Path::new("/extract/file"), Follow::No)?.kind,
            FileKind::File
        );
        assert_eq!(
            host.metadata(Path::new("/extract/foo"), Follow::No)?.kind,
            FileKind::Dir
        );
        assert_eq!(
            read_bytes(&host, Path::new("/extract/foo/child"), 16)?,
            b"x"
        );
    }
    Ok(())
}

/// One ZIP member for [`zip_bytes`].
struct Member<'a> {
    stored: &'a str,
    unicode: Option<&'a str>,
    method: u16,
    data: &'a [u8],
}

fn member(stored: &str) -> Member<'_> {
    Member {
        stored,
        unicode: None,
        method: 0,
        data: b"x",
    }
}

/// Write a ZIP file with exact stored names, extra fields, and methods.
fn zip_bytes(members: &[Member<'_>]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = Vec::new();
    let mut central = Vec::new();
    for member in members {
        let mut extra = Vec::new();
        if let Some(unicode) = member.unicode {
            extra.extend_from_slice(&0x7075_u16.to_le_bytes());
            extra.extend_from_slice(&u16::try_from(5 + unicode.len())?.to_le_bytes());
            extra.push(1);
            extra.extend_from_slice(&crc32fast::hash(member.stored.as_bytes()).to_le_bytes());
            extra.extend_from_slice(unicode.as_bytes());
        }
        let name = member.stored.as_bytes();
        let size = u32::try_from(member.data.len())?;
        let crc = crc32fast::hash(member.data);
        let offset = u32::try_from(bytes.len())?;
        let mut fields = Vec::new();
        for word in [0_u16, member.method, 0, 0x21] {
            fields.extend_from_slice(&word.to_le_bytes());
        }
        for value in [crc, size, size] {
            fields.extend_from_slice(&value.to_le_bytes());
        }
        fields.extend_from_slice(&u16::try_from(name.len())?.to_le_bytes());
        fields.extend_from_slice(&u16::try_from(extra.len())?.to_le_bytes());
        bytes.extend_from_slice(b"PK\x03\x04\x14\0");
        bytes.extend_from_slice(&fields);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&extra);
        bytes.extend_from_slice(member.data);
        central.extend_from_slice(b"PK\x01\x02\x14\0\x14\0");
        central.extend_from_slice(&fields);
        // Comment length, disk, internal attributes, and external attributes.
        central.extend_from_slice(&[0; 10]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
        central.extend_from_slice(&extra);
    }
    let count = u16::try_from(members.len())?;
    let central_offset = u32::try_from(bytes.len())?;
    let central_size = u32::try_from(central.len())?;
    bytes.extend_from_slice(&central);
    bytes.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&central_size.to_le_bytes());
    bytes.extend_from_slice(&central_offset.to_le_bytes());
    bytes.extend_from_slice(&[0; 2]);
    Ok(bytes)
}

fn seed_zip(host: &PureHost, members: &[Member<'_>]) -> TestResult {
    host.write(
        Path::new(ARCHIVE),
        &zip_bytes(members)?,
        WriteMode::Truncate,
    )?;
    Ok(())
}

#[test]
fn zip_metadata_uses_the_stored_name() -> TestResult {
    let host = test_host();
    seed_zip(
        &host,
        &[
            Member {
                unicode: Some("__MACOSX/._raw-name"),
                ..member("raw-name")
            },
            Member {
                unicode: Some("visible"),
                ..member("__MACOSX/._old")
            },
            member("keep"),
        ],
    )?;
    assert_eq!(
        list(&host, false)?,
        names(&["__MACOSX/._raw-name", "visible", "keep"])
    );
    assert_eq!(list(&host, true)?, names(&["__MACOSX/._raw-name", "keep"]));
    Ok(())
}

#[test]
fn zip_metadata_parents_follow_central_directory_order() -> TestResult {
    let host = test_host();
    let forward = ["__MACOSX/._top", "__MACOSX/._top/real.txt"];
    seed_zip(&host, &forward.map(member))?;
    assert_eq!(list(&host, true)?, names(&forward));
    let reverse = ["__MACOSX/._top/real.txt", "__MACOSX/._top"];
    seed_zip(&host, &reverse.map(member))?;
    assert_eq!(list(&host, true)?, names(&["__MACOSX/._top/real.txt"]));
    // The walk to higher parents stops at the first parent that is not metadata.
    let gap = ["__MACOSX/._a", "__MACOSX/._a/b/c.txt"];
    seed_zip(&host, &gap.map(member))?;
    assert_eq!(list(&host, true)?, names(&["__MACOSX/._a/b/c.txt"]));
    Ok(())
}

#[test]
fn zip_reads_paired_metadata_before_it_discards_it() -> TestResult {
    let host = test_host();
    let fork = |method: u16, data: &'static [u8]| Member {
        method,
        data,
        ..member("__MACOSX/d/._keep")
    };
    let cases: [(Member<'_>, Option<&str>); 4] = [
        (fork(0, b"stored metadata"), None),
        (
            fork(12, b"x"),
            Some(
                "Problem with archive_read_next_header(): Unsupported ZIP compression method (bzip)",
            ),
        ),
        (
            fork(8, b"\xff\xff\xff\xff"),
            Some("Problem with archive_read_next_header(): ZIP decompression failed (-3)"),
        ),
        (
            Member {
                stored: "__MACOSX/d/._other",
                ..fork(12, b"x")
            },
            None,
        ),
    ];
    for (sidecar, error) in cases {
        seed_zip(&host, &[sidecar, member("d/keep")])?;
        assert_eq!(list(&host, false)?.len(), 2);
        let _ = host.remove_dir_all(Path::new("/extract"));
        for result in [list(&host, true), run(&host, &extract_options(), true)] {
            match error {
                Some(message) => {
                    assert_eq!(result.map_err(|error| error.message), Err(message.into()));
                }
                None => assert_eq!(result?, names(&["d/keep"])),
            }
        }
    }
    Ok(())
}
