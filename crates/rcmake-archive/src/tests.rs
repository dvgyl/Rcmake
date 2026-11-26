use std::error::Error;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use rcmake_host::{FileKind, FileTime, Follow, PureHost, WriteMode};
use tar::{Builder, EntryType, Header};
use zip::{ZipArchive, ZipWriter, extra_fields::ExtraField, write::SimpleFileOptions};

use super::*;

mod apple_double;
mod limits;
#[cfg(unix)]
mod oracle;
mod paths;

type TestResult = Result<(), Box<dyn Error>>;

const ALL_FORMATS: [ArchiveFormat; 9] = [
    ArchiveFormat::Tar,
    ArchiveFormat::TarGz,
    ArchiveFormat::TarBz2,
    ArchiveFormat::TarXz,
    ArchiveFormat::TarLzma,
    ArchiveFormat::TarZstd,
    ArchiveFormat::TarZ,
    ArchiveFormat::Zip,
    ArchiveFormat::ZipStored,
];

fn test_host() -> PureHost {
    PureHost::new(PathBuf::from("/source"), PathBuf::from("/bin/rcmake"))
}

fn create_options(format: ArchiveFormat) -> CreateOptions {
    CreateOptions {
        format,
        mtime: None,
        compression_level: Some(6),
        verbose: false,
    }
}

fn extract_options() -> ExtractOptions {
    ExtractOptions {
        destination: PathBuf::from("/extract"),
        ..Default::default()
    }
}

fn names(values: &[&str]) -> Vec<BString> {
    values.iter().copied().map(BString::from).collect()
}

fn absent(host: &PureHost, path: &str) -> bool {
    host.metadata(Path::new(path), Follow::No)
        .is_err_and(|error| error.kind == std::io::ErrorKind::NotFound)
}

fn seed_tree(host: &PureHost) -> TestResult {
    host.seed_file(Path::new("/source/tree/z.txt"), b"last")?;
    host.seed_file(Path::new("/source/tree/nested/a.txt"), b"nested")?;
    host.seed_file(Path::new("/source/tree/b.bin"), b"first")?;
    host.create_dir_all(Path::new("/source/tree/empty"))?;
    Ok(())
}

#[test]
fn round_trips_all_formats_in_byte_order() -> TestResult {
    for format in ALL_FORMATS {
        let host = test_host();
        seed_tree(&host)?;
        let create = create_options(format);
        let input = [PathBuf::from("tree")];
        let base = Path::new("/source");
        let archive = Path::new("/archive");
        let expected = names(&[
            "tree/",
            "tree/b.bin",
            "tree/empty/",
            "tree/nested/",
            "tree/nested/a.txt",
            "tree/z.txt",
        ]);
        assert_eq!(
            create_archive(
                &host,
                archive,
                base,
                &input,
                create.with_deterministic_order()
            )?,
            expected
        );
        let duplicate = Path::new("/duplicate");
        create_archive(
            &host,
            duplicate,
            base,
            &input,
            create.with_deterministic_order(),
        )?;
        assert_eq!(
            read_bytes(&host, archive, size_limit())?,
            read_bytes(&host, duplicate, size_limit())?
        );
        assert_eq!(
            list_command_archive(&host, archive, vec![], None)?,
            expected
        );
        assert_eq!(
            extract_archive(&host, archive, &extract_options())?,
            expected
        );
        assert_eq!(
            read_bytes(&host, Path::new("/extract/tree/nested/a.txt"), 16)?,
            b"nested"
        );
        assert_eq!(
            host.metadata(Path::new("/extract/tree/empty"), Follow::No)?
                .kind,
            FileKind::Dir
        );
    }
    Ok(())
}

#[test]
fn skips_only_paired_apple_double_files_during_directory_walks() -> TestResult {
    let host = test_host();
    let source = Path::new("/source");
    let archive = Path::new("/archive");
    let mut sidecar = b"\0\x05\x16\x07\0\x02\0\0".to_vec();
    sidecar.extend_from_slice(&[0; 16]);
    sidecar.extend_from_slice(&1_u16.to_be_bytes());
    for value in [9_u32, 38, 32] {
        sidecar.extend_from_slice(&value.to_be_bytes());
    }
    sidecar.resize(70, 0);
    for name in ["tree/._data", "tree/._unpaired"] {
        host.seed_file(&source.join(name), &sidecar)?;
    }
    for name in ["tree/data", "tree/notes", "tree/._notes", "tree/user-dir"] {
        host.seed_file(&source.join(name), b"file data")?;
    }
    host.create_dir_all(&source.join("tree/._user-dir"))?;
    let options = create_options(ArchiveFormat::Tar);
    assert_eq!(
        create_archive(
            &host,
            archive,
            source,
            &[PathBuf::from("tree")],
            options.with_deterministic_order().skipping_apple_double(),
        )?,
        names(&[
            "tree/",
            "tree/._notes",
            "tree/._unpaired",
            "tree/._user-dir/",
            "tree/data",
            "tree/notes",
            "tree/user-dir",
        ])
    );
    assert_eq!(
        create_archive(
            &host,
            archive,
            source,
            &[PathBuf::from("tree/._data")],
            &options,
        )?,
        names(&["tree/._data"])
    );
    let listed =
        create_command_archive(&host, archive, source, &[PathBuf::from("tree")], &options)?;
    assert!(listed.contains(&BString::from("tree/._data")));
    Ok(())
}

#[test]
fn seven_zip_lists_empty_entries_in_cmake_order() -> TestResult {
    let host = test_host();
    let source = Path::new("/source");
    let archive = Path::new("/archive");
    for dir in ["aaaa", "tree/empty", "z"] {
        host.create_dir_all(&source.join(dir))?;
    }
    for file in ["first-empty", "tree/last-empty"] {
        host.seed_file(&source.join(file), b"")?;
    }
    host.seed_file(&source.join("tree/data"), b"data")?;
    host.symlink(Path::new("data"), &source.join("tree/link"))?;
    let inputs = ["first-empty", "z", "aaaa", "tree"].map(PathBuf::from);
    let expected = names(&[
        "tree/data",
        "tree/link",
        "first-empty",
        "tree/last-empty",
        "tree/empty/",
        "tree/",
        "aaaa/",
        "z/",
    ]);
    for compression in [SevenZipCompression::Store, SevenZipCompression::Ppmd] {
        let options = CreateOptions {
            compression_level: Some(1),
            ..create_options(ArchiveFormat::SevenZip {
                compression,
                threads: 0,
            })
        };
        create_archive(&host, archive, source, &inputs, &options)?;
        assert_eq!(
            list_command_archive(&host, archive, vec![], None)?,
            expected
        );
        assert_eq!(
            extract_archive(&host, archive, &extract_options())?,
            expected
        );
        assert_eq!(
            read_bytes(&host, Path::new("/extract/tree/data"), 4)?,
            b"data"
        );
        assert_eq!(
            host.read_link(Path::new("/extract/tree/link"))?,
            Path::new("data")
        );
    }
    Ok(())
}

#[test]
fn parses_cmake_archive_mtime_values() {
    let host = test_host();
    for (value, expected) in [
        ("@0", Some(0)),
        ("@+1", Some(1)),
        ("1970-01-01UTC", Some(0)),
        ("2000-01-01 02:03:04 +0130", Some(946_686_784)),
        ("Tue, 14 Nov 2023 22:13:20 GMT", Some(1_700_000_000)),
        ("@-2", None),
        ("99", None),
        ("2400", None),
        ("1970-01-01T00:00:00Z", None),
        ("2000-01-01 02:03:04.5 UTC", None),
    ] {
        assert_eq!(
            parse_archive_mtime(&host, value.as_bytes()),
            expected,
            "{value}"
        );
    }
}

#[test]
fn reports_cmake_compression_level_limits() {
    let host = test_host();
    let archive = Path::new("/archive");
    for (format, max) in [
        (ArchiveFormat::ZipCompressed(ZipCompression::Zstd), 9),
        (ArchiveFormat::ZipCompressed(ZipCompression::Ppmd), 9),
        (ArchiveFormat::GnuTarCompressed(TarCompression::Zstd), 19),
        (
            ArchiveFormat::SevenZip {
                compression: SevenZipCompression::Zstd,
                threads: 0,
            },
            19,
        ),
    ] {
        let level = max + 1;
        let options = CreateOptions {
            compression_level: Some(level),
            ..create_options(format)
        };
        let result = create_archive(&host, archive, Path::new("/source"), &[], &options);
        assert!(result.is_err_and(|error| error.message
            == format!("Compression level must be between 0 and {max}. Got {level}")));
        assert!(absent(&host, "/archive"));
    }
}

#[test]
fn restores_modification_times_and_permissions() -> TestResult {
    let time = FileTime {
        secs: 1_707_871_601,
        nanos: 123_456_700,
    };
    for format in ALL_FORMATS {
        let host = test_host();
        let source = Path::new("/source/bin/run");
        let directory = Path::new("/source/bin");
        host.seed_file(source, b"program")?;
        for (path, mode) in [(source, 0o6751), (directory, 0o1750)] {
            host.set_permissions(path, mode)?;
            host.set_times(path, None, Some(time))?;
        }

        let archive = Path::new("/archive");
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[directory.to_owned()],
            &create_options(format),
        )?;
        extract_archive(&host, archive, &extract_options())?;
        for (path, mode) in [("/extract/bin/run", 0o751), ("/extract/bin", 0o750)] {
            let metadata = host.metadata(Path::new(path), Follow::No)?;
            assert_eq!(metadata.mtime, time);
            assert_eq!(metadata.mode, mode);
        }
    }
    Ok(())
}

#[test]
fn overrides_mtime_and_supports_touch() -> TestResult {
    for format in ALL_FORMATS {
        let host = test_host();
        host.seed_file(Path::new("/source/input"), b"input")?;
        let mut create = create_options(format);
        create.mtime = Some(1);
        let archive = Path::new("/archive");
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("input")],
            &create,
        )?;
        extract_archive(&host, archive, &extract_options())?;
        let saved_time = host
            .metadata(Path::new("/extract/input"), Follow::No)?
            .mtime;
        assert_eq!(saved_time, timestamp::seconds(1));

        extract_archive(
            &host,
            archive,
            &ExtractOptions {
                touch: true,
                ..extract_options()
            },
        )?;
        assert!(
            host.metadata(Path::new("/extract/input"), Follow::No)?
                .mtime
                > saved_time
        );
    }
    Ok(())
}

#[test]
fn zip_records_whole_seconds_for_readers_without_ntfs_timestamps() -> TestResult {
    let host = test_host();
    let source = Path::new("/source/input");
    let time = FileTime {
        secs: 1_707_871_601,
        nanos: 123_456_700,
    };
    host.seed_file(source, b"input")?;
    host.set_times(source, None, Some(time))?;
    let archive = Path::new("/archive");
    create_archive(
        &host,
        archive,
        Path::new("/source"),
        &[PathBuf::from("input")],
        &create_options(ArchiveFormat::Zip),
    )?;
    let mut zip = ZipArchive::new(Cursor::new(read_bytes(&host, archive, size_limit())?))?;
    let entry = zip.by_name("input")?;
    let secs = u32::try_from(time.secs)?;
    assert!(entry.extra_data_fields().any(|field| {
        matches!(field, ExtraField::ExtendedTimestamp(time) if time.mod_time() == Some(secs))
    }));
    Ok(())
}

#[test]
fn preserves_links_and_relative_targets() -> TestResult {
    for format in ALL_FORMATS {
        let host = test_host();
        host.seed_file(Path::new("/source/tree/top"), b"target")?;
        host.create_dir_all(Path::new("/source/tree/sub"))?;
        host.symlink(Path::new("../top"), Path::new("/source/tree/sub/link"))?;
        let archive = Path::new("/archive");
        let entries = create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("tree")],
            &create_options(format),
        )?;
        assert_eq!(
            entries,
            names(&["tree/", "tree/sub/", "tree/sub/link", "tree/top"])
        );
        extract_archive(&host, archive, &extract_options())?;
        let link = Path::new("/extract/tree/sub/link");
        assert_eq!(host.metadata(link, Follow::No)?.kind, FileKind::Symlink);
        assert_eq!(host.read_link(link)?, Path::new("../top"));
        assert_eq!(read_bytes(&host, link, 16)?, b"target");
    }
    Ok(())
}

#[test]
fn lists_and_filters_without_creating_a_destination() -> TestResult {
    let host = test_host();
    seed_tree(&host)?;
    let archive = Path::new("/archive");
    create_archive(
        &host,
        archive,
        Path::new("/source"),
        &[PathBuf::from("./tree")],
        &create_options(ArchiveFormat::TarGz),
    )?;
    let list = ExtractOptions {
        list_only: true,
        patterns: names(&["*.txt", "./tree/[e]mpty"]),
        verbose: true,
        ..extract_options()
    };
    assert_eq!(
        extract_archive(&host, archive, &list)?,
        names(&["./tree/empty/", "./tree/nested/a.txt", "./tree/z.txt"])
    );
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        BString::from("./tree/empty/\n./tree/nested/a.txt\n./tree/z.txt\n")
    );
    assert!(absent(&host, "/extract"));

    let extract = ExtractOptions {
        patterns: names(&["tree/nested"]),
        ..extract_options()
    };
    assert_eq!(
        extract_archive(&host, archive, &extract)?,
        names(&["./tree/nested/", "./tree/nested/a.txt"])
    );
    assert!(absent(&host, "/extract/tree/b.bin"));
    assert!(absent(&host, "/extract/tree/z.txt"));

    let missing = ExtractOptions {
        patterns: names(&["missing"]),
        list_only: true,
        ..Default::default()
    };
    assert!(extract_archive(&host, archive, &missing).is_err());
    Ok(())
}

#[test]
fn creates_missing_parents_for_a_selected_file() -> TestResult {
    let host = test_host();
    seed_tree(&host)?;
    let archive = Path::new("/archive");
    create_archive(
        &host,
        archive,
        Path::new("/source"),
        &[PathBuf::from("tree")],
        &create_options(ArchiveFormat::Tar),
    )?;
    let extract = ExtractOptions {
        patterns: names(&["tree/nested/a.txt"]),
        ..extract_options()
    };
    assert_eq!(
        extract_archive(&host, archive, &extract)?,
        names(&["tree/nested/a.txt"])
    );
    assert_eq!(
        read_bytes(&host, Path::new("/extract/tree/nested/a.txt"), 16)?,
        b"nested"
    );
    Ok(())
}

fn add_test_tar_file(builder: &mut Builder<Vec<u8>>, name: &str) -> std::io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    header.set_mtime(0);
    builder.append_data(&mut header, name, b"x".as_slice())
}

fn add_test_tar_link(
    builder: &mut Builder<Vec<u8>>,
    name: &str,
    kind: EntryType,
    target: &[u8],
) -> std::io::Result<()> {
    let mut header = Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(0);
    header.set_mode(0o755);
    header.set_mtime(123);
    header.set_link_name_literal(target)?;
    builder.append_data(&mut header, name, b"".as_slice())
}

#[test]
fn skips_tar_fifos_and_devices() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    for (name, kind) in [
        ("pipe", EntryType::Fifo),
        ("char", EntryType::Char),
        ("block", EntryType::Block),
    ] {
        let mut header = Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(0);
        builder.append_data(&mut header, name, b"".as_slice())?;
    }
    add_test_tar_file(&mut builder, "file")?;
    host.seed_file(archive, &builder.into_inner()?)?;
    extract_archive(&host, archive, &extract_options())?;
    for path in ["/extract/pipe", "/extract/char", "/extract/block"] {
        assert!(absent(&host, path));
    }
    assert_eq!(read_bytes(&host, Path::new("/extract/file"), 16)?, b"x");
    Ok(())
}

#[test]
fn rejects_incomplete_compression_headers() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for magic in [
        b"\xfd7zXZ\0".as_slice(),
        b"\x28\xb5\x2f\xfd",
        b"BZh",
        b"\x1f\x9d",
        b"\x1f\x9d\xff",
        b"\x1f\x9d\x90\x01",
        b"\x1f\x8b",
    ] {
        host.write(archive, magic, WriteMode::Truncate)?;
        assert!(extract_archive(&host, archive, &extract_options()).is_err());
        assert!(absent(&host, "/extract"));
    }
    Ok(())
}

#[test]
fn creates_tar_hard_links() -> TestResult {
    for format in ALL_FORMATS
        .into_iter()
        .filter(|format| !matches!(format, ArchiveFormat::Zip | ArchiveFormat::ZipStored))
    {
        let host = test_host();
        host.seed_file(Path::new("/source/tree/a"), b"shared")?;
        host.hard_link(Path::new("/source/tree/a"), Path::new("/source/tree/b"))?;
        let archive = Path::new("/archive");
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("tree")],
            &create_options(format),
        )?;
        extract_archive(&host, archive, &extract_options())?;
        let first = host.metadata(Path::new("/extract/tree/a"), Follow::No)?;
        let second = host.metadata(Path::new("/extract/tree/b"), Follow::No)?;
        assert_eq!(first, second);
        assert_eq!(first.nlink, 2);
    }
    Ok(())
}

#[test]
fn rejects_truncated_tar_payload_before_writing() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "valid")?;
    add_test_tar_file(&mut builder, "truncated")?;
    let mut bytes = builder.into_inner()?;
    bytes.truncate(1536);
    host.seed_file(archive, &bytes)?;
    assert!(extract_archive(&host, archive, &extract_options()).is_err());
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn rejects_truncated_compression_trailers_before_writing() -> TestResult {
    for format in [
        ArchiveFormat::TarGz,
        ArchiveFormat::TarBz2,
        ArchiveFormat::TarXz,
        ArchiveFormat::TarZstd,
    ] {
        let host = test_host();
        let archive = Path::new("/archive");
        host.seed_file(Path::new("/source/input"), b"input")?;
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("input")],
            &create_options(format),
        )?;
        let mut bytes = read_bytes(&host, archive, size_limit())?;
        bytes.pop();
        host.write(archive, &bytes, WriteMode::Truncate)?;
        assert!(
            extract_archive(&host, archive, &extract_options()).is_err(),
            "{format:?}"
        );
        assert!(absent(&host, "/extract"));
    }
    Ok(())
}

#[test]
fn handles_empty_archives() -> TestResult {
    for format in ALL_FORMATS {
        let host = test_host();
        let archive = Path::new("/archive");
        assert!(
            create_archive(
                &host,
                archive,
                Path::new("/source"),
                &[],
                &create_options(format)
            )?
            .is_empty()
        );
        assert!(extract_archive(&host, archive, &extract_options())?.is_empty());
    }
    Ok(())
}

#[test]
fn creates_repeated_zip_members_with_their_own_payloads() -> TestResult {
    for format in [
        ArchiveFormat::Zip,
        ArchiveFormat::ZipStored,
        ArchiveFormat::ZipCompressed(ZipCompression::Bzip2),
    ] {
        let host = test_host();
        let archive = Path::new("/archive");
        let input = PathBuf::from("file");
        host.seed_file(Path::new("/source/file"), b"contents")?;
        let expected = names(&["file", "file"]);
        assert_eq!(
            create_command_archive(
                &host,
                archive,
                Path::new("/source"),
                &[input.clone(), input],
                &create_options(format),
            )?,
            expected
        );
        assert_eq!(
            list_command_archive(&host, archive, Vec::new(), None)?,
            expected
        );
        assert_eq!(
            extract_archive(&host, archive, &extract_options())?,
            expected
        );
        assert_eq!(
            read_bytes(&host, Path::new("/extract/file"), 16)?,
            b"contents"
        );
    }
    Ok(())
}

#[test]
fn command_creation_stores_separate_tar_members_and_whole_seconds() -> TestResult {
    for format in [
        ArchiveFormat::Tar,
        ArchiveFormat::GnuTar,
        ArchiveFormat::Zip,
    ] {
        let host = test_host();
        let archive = Path::new("/archive");
        let source = Path::new("/source/a");
        host.seed_file(source, b"data")?;
        let secs = 1_707_871_601;
        host.set_times(
            source,
            None,
            Some(FileTime {
                secs,
                nanos: 123_456_700,
            }),
        )?;
        host.hard_link(source, Path::new("/source/b"))?;
        create_command_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("a"), PathBuf::from("b")],
            &create_options(format),
        )?;
        extract_archive(&host, archive, &extract_options())?;
        let a = host.metadata(Path::new("/extract/a"), Follow::No)?;
        let b = host.metadata(Path::new("/extract/b"), Follow::No)?;
        assert_eq!(a.mtime, timestamp::seconds(secs));
        assert_eq!(b.mtime, a.mtime);
        assert_ne!((a.dev, a.ino), (b.dev, b.ino));
    }
    Ok(())
}

#[test]
fn zip_dos_time_uses_local_time() -> TestResult {
    let zone = jiff::tz::Offset::from_seconds(3600)?.to_time_zone();
    let dos = zip::DateTime::from_date_and_time(2000, 1, 1, 1, 0, 0)?;
    assert_eq!(
        timestamp::dos_to_unix(dos, &zone),
        Some(timestamp::seconds(946_684_800))
    );
    Ok(())
}

#[test]
fn zip_ppmd_reports_the_cmake_diagnostic() {
    let result = create_command_archive(
        &test_host(),
        Path::new("/archive"),
        Path::new("/source"),
        &[],
        &create_options(ArchiveFormat::ZipCompressed(ZipCompression::Ppmd)),
    );
    assert!(result.is_err_and(|error| error.message == "PPMd is not supported for zip"));
}

#[test]
fn raw_archive_writes_only_the_file_data_through_each_filter() -> TestResult {
    use std::io::Read;

    let host = PureHost::default();
    let base = host.initial_cwd();
    let output = base.join("out");
    let payload = b"raw contents\0\xff";
    host.seed_file(&base.join("input"), payload)?;
    for filter in [
        None,
        Some(TarCompression::Zstd),
        Some(TarCompression::Gzip),
        Some(TarCompression::Xz),
        Some(TarCompression::Bzip2),
    ] {
        create_archive(
            &host,
            &output,
            &base,
            &["input".into()],
            &create_options(ArchiveFormat::Raw(filter)),
        )?;
        let encoded = host.read(&output, ReadRange::All { limit: 1024 })?;
        assert_eq!(crate::codec::detect(&encoded), filter);
        if let Some(filter) = filter {
            let mut decoded = Vec::new();
            crate::codec::decoder(filter, encoded.as_slice())?.read_to_end(&mut decoded)?;
            assert_eq!(decoded, payload);
        } else {
            assert_eq!(encoded, payload);
        }
    }
    Ok(())
}

fn ar_member(name: &str, payload: &[u8]) -> Vec<u8> {
    let header = format!(
        "{name:16}{:12}{:6}{:6}{:8}{:10}`\n",
        2,
        0,
        0,
        "100640",
        payload.len()
    );
    let mut member = [header.as_bytes(), payload].concat();
    if !payload.len().is_multiple_of(2) {
        member.push(b'\n');
    }
    member
}

#[test]
fn ar_lists_and_extracts_members_with_short_gnu_and_bsd_names() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut data = b"!<arch>\n".to_vec();
    data.extend(ar_member("short/", b"odd"));
    data.extend(ar_member("//", b"long-gnu-filename.txt/\n"));
    data.extend(ar_member("/0", b"GNU"));
    data.extend(ar_member("#1/24", b"long-bsd-filename.txt\0\0\0BSD"));
    host.seed_file(archive, &data)?;
    assert_eq!(
        list_command_archive(&host, archive, Vec::new(), None)?,
        names(&[
            "short",
            "//",
            "long-gnu-filename.txt",
            "long-bsd-filename.txt"
        ])
    );
    let opts = ExtractOptions {
        patterns: names(&["long-*"]),
        ..extract_options()
    };
    assert_eq!(
        extract_archive(&host, archive, &opts)?,
        names(&["long-gnu-filename.txt", "long-bsd-filename.txt"])
    );
    assert!(absent(&host, "/extract/short"));
    for (name, bytes) in [
        ("long-gnu-filename.txt", b"GNU"),
        ("long-bsd-filename.txt", b"BSD"),
    ] {
        let path = Path::new("/extract").join(name);
        assert_eq!(read_bytes(&host, &path, 16)?, bytes);
        let meta = host.metadata(&path, Follow::No)?;
        assert_eq!(meta.mode & 0o7777, 0o640);
        assert_eq!(meta.mtime, FileTime { secs: 2, nanos: 0 });
    }
    Ok(())
}

#[test]
fn ar_rejects_unsafe_names_and_truncated_members_before_extraction() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for invalid in [
        ar_member("#1/10", b"../outsidebad"),
        ar_member("second/", b"truncated")[..62].to_vec(),
    ] {
        let data = [
            b"!<arch>\n".as_slice(),
            &ar_member("valid/", b"data"),
            &invalid,
        ]
        .concat();
        host.seed_file(archive, &data)?;
        assert!(extract_archive(&host, archive, &extract_options()).is_err());
        assert!(absent(&host, "/extract/valid"));
        assert!(absent(&host, "/outside"));
    }
    Ok(())
}

#[test]
fn compressed_ar_archives_list_and_extract_within_bounds() -> TestResult {
    use std::io::Write;

    let mut plain = b"!<arch>\n".to_vec();
    plain.extend(ar_member("alpha.txt/", b"hello"));
    plain.extend(ar_member("beta.txt/", b"world!"));

    for filter in [
        TarCompression::Gzip,
        TarCompression::Bzip2,
        TarCompression::Xz,
        TarCompression::Zstd,
    ] {
        let mut encoder = crate::codec::Encoder::new(Some(filter), None, 1)?;
        encoder.write_all(&plain)?;
        let encoded = encoder.finish()?;

        let host = test_host();
        let archive = Path::new("/archive.ar.compressed");
        host.seed_file(archive, &encoded)?;

        assert_eq!(
            list_command_archive(&host, archive, Vec::new(), None)?,
            names(&["alpha.txt", "beta.txt"])
        );
        assert_eq!(
            extract_archive(&host, archive, &extract_options())?,
            names(&["alpha.txt", "beta.txt"])
        );
        assert_eq!(
            read_bytes(&host, Path::new("/extract/alpha.txt"), 16)?,
            b"hello"
        );
        assert_eq!(
            read_bytes(&host, Path::new("/extract/beta.txt"), 16)?,
            b"world!"
        );

        // Decompression stays bounded by the output limit.
        let bounded_opts = extract_options();
        let bounded_err = crate::extract::run_with_output_limit(
            &host,
            archive,
            ExtractRequest {
                options: &bounded_opts,
                confined_links: false,
                mac_extensions: false,
            },
            crate::extract::Listing::Names,
            1,
        )
        .unwrap_err();
        assert!(
            bounded_err.message.contains("size limit"),
            "{}",
            bounded_err.message
        );
    }

    // External filter (grzip) producing an ar payload also routes to the ar reader.
    let host = test_host();
    let archive = Path::new("/archive.ar.grz");
    host.seed_file(archive, b"GRZipII\0payload")?;
    let grzip = Path::new("/bin/grzip");
    host.seed_file(grzip, b"")?;
    host.set_permissions(grzip, 0o755)?;
    host.env_set(b"PATH", Some(b"/bin"));
    let plain_copy = plain.clone();
    host.set_command_handler(move |_, _, _| {
        Ok(rcmake_host::RunOutput {
            stdout: plain_copy.clone(),
            stderr: Vec::new(),
            results: vec![rcmake_host::ProcessResult::Exited(0)],
        })
    })?;
    assert_eq!(
        extract_archive(&host, archive, &extract_options())?,
        names(&["alpha.txt", "beta.txt"])
    );
    assert_eq!(
        read_bytes(&host, Path::new("/extract/alpha.txt"), 16)?,
        b"hello"
    );
    assert_eq!(
        read_bytes(&host, Path::new("/extract/beta.txt"), 16)?,
        b"world!"
    );
    Ok(())
}
