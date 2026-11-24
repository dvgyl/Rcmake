use std::error::Error;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use rcmake_host::{FileKind, FileTime, Follow, PureHost, WriteMode};
use tar::{Builder, EntryType, Header};
use zip::{ZipArchive, ZipWriter, extra_fields::ExtraField, write::SimpleFileOptions};

use super::*;

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
            create_archive(&host, archive, base, &input, &create)?,
            expected
        );
        let duplicate = Path::new("/duplicate");
        create_archive(&host, duplicate, base, &input, &create)?;
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
    let mut sidecar = vec![0; 70];
    sidecar[..8].copy_from_slice(b"\0\x05\x16\x07\0\x02\0\0");
    sidecar[24..26].copy_from_slice(&1_u16.to_be_bytes());
    sidecar[26..30].copy_from_slice(&9_u32.to_be_bytes());
    sidecar[30..34].copy_from_slice(&38_u32.to_be_bytes());
    sidecar[34..38].copy_from_slice(&32_u32.to_be_bytes());
    for name in ["tree/._data", "tree/._unpaired"] {
        host.seed_file(&source.join(name), &sidecar)?;
    }
    for name in ["tree/data", "tree/notes", "tree/._notes", "tree/user-dir"] {
        host.seed_file(&source.join(name), b"file data")?;
    }
    host.create_dir_all(&source.join("tree/._user-dir"))?;
    let options = create_options(ArchiveFormat::Tar);
    assert_eq!(
        create_archive(&host, archive, source, &[PathBuf::from("tree")], &options)?,
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

#[test]
fn rejects_unsafe_tar_entry_paths_before_writing() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for name in [b"tree/../marker".as_slice(), b"/marker", b"bad\0name"] {
        let mut builder = Builder::new(Vec::new());
        add_test_tar_file(&mut builder, "valid")?;
        builder.append_pax_extensions([("path", name)])?;
        add_test_tar_file(&mut builder, "placeholder")?;
        host.write(archive, &builder.into_inner()?, WriteMode::Truncate)?;
        assert!(extract_archive(&host, archive, &extract_options()).is_err());
        assert!(absent(&host, "/extract"));
        assert!(absent(&host, "/marker"));
        assert!(
            extract_archive(
                &host,
                archive,
                &ExtractOptions {
                    list_only: true,
                    patterns: names(&["valid"]),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
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
    builder.append_data(&mut header, name, &[][..])
}

#[test]
fn rejects_unsafe_zip_entry_paths() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for name in [
        "a/../marker",
        "/marker",
        "bad\0name",
        "a\\..\\marker",
        "C:/marker",
    ] {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer.start_file(name, SimpleFileOptions::default())?;
        writer.write_all(b"test")?;
        host.write(archive, &writer.finish()?.into_inner(), WriteMode::Truncate)?;
        assert!(extract_archive(&host, archive, &extract_options()).is_err());
        assert!(absent(&host, "/extract"));
        assert!(absent(&host, "/marker"));
    }
    Ok(())
}

#[test]
fn rejects_unsafe_windows_entry_components() {
    for name in [
        b"NUL.txt".as_slice(),
        b"cOn .txt",
        b"lPt3",
        b"COM\xc2\xb2.dat",
        b"target:stream",
        b".. ",
        b"directory.",
    ] {
        assert!(unsafe_windows_name(name), "{name:?}");
    }
    for name in [b"nul-file".as_slice(), b"lpt10", b"normal file", b".hidden"] {
        assert!(!unsafe_windows_name(name), "{name:?}");
    }
}

#[test]
fn caps_sparse_tar_size_even_when_the_entry_is_excluded() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "selected")?;
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::GNUSparse);
    header.set_size(0);
    let size = size_limit() + 1;
    let gnu = header.as_gnu_mut().ok_or("Expected a GNU tar header")?;
    gnu.set_real_size(size);
    let block = gnu.sparse.first_mut().ok_or("Expected a sparse block")?;
    block.set_offset(size);
    block.set_length(0);
    builder.append_data(&mut header, "sparse", &[][..])?;
    host.seed_file(archive, &builder.into_inner()?)?;
    let options = ExtractOptions {
        patterns: names(&["selected"]),
        ..extract_options()
    };
    assert!(
        extract_archive(&host, archive, &options)
            .is_err_and(|error| error.message == "Archive exceeds the size limit")
    );
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn caps_zip_size_even_when_the_entry_is_excluded() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("selected", SimpleFileOptions::default())?;
    writer.write_all(b"x")?;
    writer.start_file("oversized", SimpleFileOptions::default())?;
    writer.write_all(b"x")?;
    let mut bytes = writer.finish()?.into_inner();
    let mut zip = ZipArchive::new(Cursor::new(&bytes))?;
    let offset = usize::try_from(zip.by_index_raw(1)?.central_header_start())? + 24;
    drop(zip);
    let oversized = u32::try_from(size_limit() + 1)?;
    bytes[offset..offset + 4].copy_from_slice(&oversized.to_le_bytes());
    host.seed_file(archive, &bytes)?;
    let options = ExtractOptions {
        patterns: names(&["selected"]),
        ..extract_options()
    };
    assert!(
        extract_archive(&host, archive, &options)
            .is_err_and(|error| error.message == "Archive exceeds the size limit")
    );
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn replaces_leaf_links_without_changing_their_targets() -> TestResult {
    let host = test_host();
    host.seed_file(Path::new("/source/tree/file"), b"archive")?;
    host.seed_file(Path::new("/outside/file"), b"outside")?;
    let archive = Path::new("/archive");
    let create = create_options(ArchiveFormat::Tar);
    create_archive(
        &host,
        archive,
        Path::new("/source"),
        &[PathBuf::from("tree/file")],
        &create,
    )?;

    host.create_dir_all(Path::new("/extract"))?;
    host.symlink(Path::new("/outside"), Path::new("/extract/tree"))?;
    assert!(extract_archive(&host, archive, &extract_options()).is_err());
    host.remove_file(Path::new("/extract/tree"))?;
    host.create_dir_all(Path::new("/extract/tree"))?;
    let file = Path::new("/extract/tree/file");
    let outside = Path::new("/outside/file");
    host.symlink(outside, file)?;
    extract_archive(&host, archive, &extract_options())?;
    assert_eq!(host.metadata(file, Follow::No)?.kind, FileKind::File);
    assert_eq!(read_bytes(&host, file, 16)?, b"archive");

    host.remove_file(file)?;
    host.hard_link(outside, file)?;
    extract_archive(&host, archive, &extract_options())?;
    assert_eq!(read_bytes(&host, file, 16)?, b"archive");
    assert_eq!(read_bytes(&host, outside, 16)?, b"outside");
    assert_eq!(host.metadata(outside, Follow::No)?.nlink, 1);
    Ok(())
}

#[test]
fn round_trips_absolute_and_parent_symlink_targets_twice() -> TestResult {
    for format in ALL_FORMATS {
        let host = test_host();
        let archive = Path::new("/archive");
        let links = [
            ("absolute", Path::new("/etc/hosts")),
            ("relative", Path::new("../outside")),
        ];
        for (name, target) in links {
            host.symlink(target, &Path::new("/source").join(name))?;
        }
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("absolute"), PathBuf::from("relative")],
            &create_options(format),
        )?;
        for _ in 0..2 {
            extract_archive(&host, archive, &extract_options())?;
            for (name, target) in links {
                assert_eq!(host.read_link(&Path::new("/extract").join(name))?, target);
            }
        }
    }
    Ok(())
}

#[test]
fn rejects_writes_through_an_extracted_symlink() -> TestResult {
    let host = test_host();
    host.seed_file(Path::new("/outside/file"), b"outside")?;
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_link(&mut builder, "link", EntryType::Symlink, b"/outside")?;
    add_test_tar_file(&mut builder, "link/file")?;
    host.seed_file(archive, &builder.into_inner()?)?;
    assert!(extract_archive(&host, archive, &extract_options()).is_err());
    assert_eq!(
        host.read_link(Path::new("/extract/link"))?,
        Path::new("/outside")
    );
    assert_eq!(
        read_bytes(&host, Path::new("/outside/file"), 16)?,
        b"outside"
    );
    Ok(())
}

#[test]
fn tar_directories_cannot_be_replaced_by_symlinks() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "tree/first")?;
    add_test_tar_link(&mut builder, "tree", EntryType::Symlink, b"/outside")?;
    add_test_tar_file(&mut builder, "tree/second")?;
    host.seed_file(archive, &builder.into_inner()?)?;
    let result = extract_archive(&host, archive, &extract_options());
    assert!(result.is_err_and(|error| error.message
        == format!(
            "Cannot extract over this filesystem entry: {}",
            Path::new("/extract").join("tree").display()
        )));
    assert!(absent(&host, "/outside/second"));
    assert_eq!(
        read_bytes(&host, Path::new("/extract/tree/first"), 1)?,
        b"x"
    );
    Ok(())
}

#[test]
fn chunked_tar_extraction_can_replace_the_archive_it_reads() -> TestResult {
    let host = test_host();
    let archive = Path::new("/extract/archive.tar");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "archive.tar")?;
    let contents = vec![b'z'; 300 * 1024];
    let mut header = Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    builder.append_data(&mut header, "large", contents.as_slice())?;
    host.seed_file(archive, &builder.into_inner()?)?;
    extract_archive(&host, archive, &extract_options())?;
    assert_eq!(read_bytes(&host, archive, 1)?, b"x");
    assert_eq!(
        read_bytes(&host, Path::new("/extract/large"), size_limit())?,
        contents
    );
    Ok(())
}

#[test]
fn extracts_tar_hard_links_twice() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "nested/file")?;
    add_test_tar_link(&mut builder, "link", EntryType::Link, b"./nested/file")?;
    host.seed_file(archive, &builder.into_inner()?)?;
    for _ in 0..2 {
        assert_eq!(
            extract_archive(&host, archive, &extract_options())?,
            names(&["nested/file", "link"])
        );
        let file = host.metadata(Path::new("/extract/nested/file"), Follow::No)?;
        let link = Path::new("/extract/link");
        assert_eq!(host.metadata(link, Follow::No)?, file);
        assert_eq!(file.nlink, 2);
        assert_eq!(file.mode, 0o644);
        assert_eq!(file.mtime, timestamp::seconds(0));
        assert_eq!(read_bytes(&host, link, 16)?, b"x");
    }
    Ok(())
}

#[test]
fn rejects_unsafe_tar_hard_link_targets_before_writing() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for target in [b"../outside".as_slice(), b"/outside", b"./link", b"."] {
        let mut builder = Builder::new(Vec::new());
        add_test_tar_file(&mut builder, "valid")?;
        add_test_tar_link(&mut builder, "link", EntryType::Link, target)?;
        host.write(archive, &builder.into_inner()?, WriteMode::Truncate)?;
        assert!(extract_archive(&host, archive, &extract_options()).is_err());
        assert!(absent(&host, "/extract"));
    }
    Ok(())
}

#[test]
fn rejects_hard_link_targets_through_symlinks() -> TestResult {
    for target in [b"parent/file".as_slice(), b"leaf"] {
        let host = test_host();
        host.seed_file(Path::new("/outside/file"), b"outside")?;
        let archive = Path::new("/archive");
        let mut builder = Builder::new(Vec::new());
        add_test_tar_link(&mut builder, "parent", EntryType::Symlink, b"/outside")?;
        add_test_tar_link(&mut builder, "leaf", EntryType::Symlink, b"/outside/file")?;
        add_test_tar_link(&mut builder, "link", EntryType::Link, target)?;
        host.seed_file(archive, &builder.into_inner()?)?;
        assert!(extract_archive(&host, archive, &extract_options()).is_err());
        assert!(absent(&host, "/extract/link"));
    }
    Ok(())
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
        builder.append_data(&mut header, name, &[][..])?;
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

#[cfg(unix)]
#[test]
fn preserves_tar_names_that_are_not_utf8() -> TestResult {
    let host = test_host();
    let relative = to_native(b"tree/a\xff");
    host.seed_file(&Path::new("/source").join(&relative), b"data")?;
    let archive = Path::new("/archive");
    let expected = vec![
        BString::from("tree/"),
        BString::from(b"tree/a\xff".as_slice()),
    ];
    assert_eq!(
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("tree")],
            &create_options(ArchiveFormat::Tar)
        )?,
        expected
    );
    assert_eq!(
        extract_archive(&host, archive, &extract_options())?,
        expected
    );
    assert_eq!(
        read_bytes(&host, &Path::new("/extract").join(relative), 16)?,
        b"data"
    );
    Ok(())
}

#[cfg(unix)]
mod oracle {
    use std::time::Duration;

    use rcmake_host::{CommandSpec, Input, NativeHost, Output, ProcessResult, RunRequest};

    use super::*;

    struct TempDir<'a> {
        host: &'a NativeHost,
        path: PathBuf,
    }

    impl Drop for TempDir<'_> {
        fn drop(&mut self) {
            let _ = self.host.remove_dir_all(&self.path);
        }
    }

    fn run(
        host: &NativeHost,
        cwd: &Path,
        program: BString,
        arguments: &[&str],
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program,
                args: names(arguments),
            }],
            cwd: cwd.to_owned(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(15)),
            tee: false,
        };
        let result = host.run(&request, &mut |_, _| {})?;
        assert_eq!(
            result.results,
            [ProcessResult::Exited(0)],
            "{:?}: {}",
            request.pipeline,
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(result.stdout)
    }

    #[test]
    #[ignore = "Set RCMAKE_ARCHIVE_INTEROP_CMAKE to the CMake executable"]
    fn interops_with_cmake_7zip_archives() -> TestResult {
        let host = NativeHost::new()?;
        let cmake = host
            .env_get(b"RCMAKE_ARCHIVE_INTEROP_CMAKE")
            .ok_or("Set RCMAKE_ARCHIVE_INTEROP_CMAKE to the CMake executable")?;
        let directory = run(&host, &host.initial_cwd(), BString::from("mktemp"), &["-d"])?;
        let temporary = TempDir {
            host: &host,
            path: to_native(directory.strip_suffix(b"\n").unwrap_or(&directory)),
        };
        let source = temporary.path.join("source");
        let destination = temporary.path.join("destination");
        let archive = temporary.path.join("archive.7z");
        host.create_dir_all(&source.join("data/empty"))?;
        host.create_dir_all(&destination)?;
        let payload = b"sample\n".repeat(4096);
        let input = source.join("data/file");
        host.write(&input, &payload, WriteMode::CreateNew)?;
        host.set_permissions(&input, 0o751)?;
        host.symlink(Path::new("file"), &source.join("data/link"))?;
        let archive_arg = archive.to_str().ok_or("Invalid archive path")?;
        for (compression, method) in [
            (SevenZipCompression::Store, "store"),
            (SevenZipCompression::Lzma, "lzma"),
            (SevenZipCompression::Lzma2, "lzma2"),
            (SevenZipCompression::Deflate, "deflate"),
            (SevenZipCompression::Bzip2, "bzip2"),
            (SevenZipCompression::Ppmd, "ppmd"),
            (SevenZipCompression::Zstd, "zstd"),
        ] {
            let mut options = create_options(ArchiveFormat::SevenZip {
                compression,
                threads: 2,
            });
            options.mtime = Some(1_700_000_000);
            create_archive(&host, &archive, &source, &[PathBuf::from("data")], &options)?;
            run(
                &host,
                &destination,
                cmake.clone(),
                &["-E", "tar", "xf", archive_arg],
            )?;
            let output = destination.join("data/file");
            assert_eq!(
                read_bytes(&host, &output, size_limit())?,
                payload,
                "{method}"
            );
            assert_eq!(
                host.read_link(&destination.join("data/link"))?,
                Path::new("file")
            );
            let metadata = host.metadata(&output, Follow::No)?;
            assert_eq!(metadata.mode & 0o777, 0o751);
            assert_eq!(metadata.mtime, timestamp::seconds(1_700_000_000));
            host.remove_dir_all(&destination.join("data"))?;

            let option = format!("--cmake-tar-compression-method={method}");
            run(
                &host,
                &source,
                cmake.clone(),
                &[
                    "-E",
                    "tar",
                    "cf",
                    archive_arg,
                    "--format=7zip",
                    &option,
                    "data",
                ],
            )?;
            let entries = extract_archive(
                &host,
                &archive,
                &ExtractOptions {
                    destination: destination.clone(),
                    ..ExtractOptions::default()
                },
            )?;
            assert!(entries.iter().any(|entry| entry == "data/empty/"));
            assert_eq!(
                read_bytes(&host, &output, size_limit())?,
                payload,
                "{method}"
            );
            assert_eq!(
                host.read_link(&destination.join("data/link"))?,
                Path::new("file")
            );
            host.remove_dir_all(&destination.join("data"))?;
        }
        Ok(())
    }

    #[test]
    #[ignore = "Set RCMAKE_ARCHIVE_INTEROP_CMAKE to the CMake executable"]
    fn interops_with_cmake_archives() -> TestResult {
        let host = NativeHost::new()?;
        let cmake = host
            .env_get(b"RCMAKE_ARCHIVE_INTEROP_CMAKE")
            .ok_or("Set RCMAKE_ARCHIVE_INTEROP_CMAKE to the CMake executable")?;
        let directory = run(&host, &host.initial_cwd(), BString::from("mktemp"), &["-d"])?;
        let path = to_native(directory.strip_suffix(b"\n").unwrap_or(&directory));
        let temporary = TempDir { host: &host, path };
        let root = &temporary.path;
        let source = root.join("source");
        let output = root.join("output");
        host.create_dir_all(&source)?;
        host.create_dir_all(&output)?;

        // Incompressible data fills the LZW dictionary. Repeated data also tests
        // phrases that refer to the next dictionary entry.
        let mut state = 0x2a9d_1701_u32;
        let mut payload: Vec<_> = (0..196_608)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        payload.resize(2 * payload.len(), b'a');
        host.write(&source.join("input"), &payload, WriteMode::CreateNew)?;
        let rcmake_archive = root.join("rcmake-archive.Z");
        let oracle_archive = root.join("oracle-archive");
        let rcmake_path = rcmake_archive.to_str().ok_or("Invalid archive path")?;
        let oracle_path = oracle_archive.to_str().ok_or("Invalid archive path")?;
        for (format, flags, options) in [
            (ArchiveFormat::Tar, "cf", &[][..]),
            (ArchiveFormat::Tar, "cf", &["--format=pax"][..]),
            (ArchiveFormat::Tar, "cf", &["--format=paxr"][..]),
            (ArchiveFormat::GnuTar, "cf", &["--format=gnutar"][..]),
            (ArchiveFormat::TarGz, "czf", &[][..]),
            (ArchiveFormat::TarBz2, "cjf", &[][..]),
            (ArchiveFormat::TarXz, "cJf", &[][..]),
            (ArchiveFormat::TarLzma, "cf", &["--lzma"][..]),
            (ArchiveFormat::TarZstd, "cf", &["--zstd"][..]),
            (ArchiveFormat::TarZ, "cf", &[][..]),
            (ArchiveFormat::Zip, "cf", &["--format=zip"][..]),
            (
                ArchiveFormat::ZipStored,
                "cf",
                &["--format=zip", "--cmake-tar-compression-method=store"][..],
            ),
        ] {
            create_archive(
                &host,
                &rcmake_archive,
                &source,
                &[PathBuf::from("input")],
                &create_options(format),
            )?;
            let list = run(
                &host,
                &output,
                cmake.clone(),
                &["-E", "tar", "tf", rcmake_path],
            )?;
            assert_eq!(list, b"input\n", "{format:?}");
            run(
                &host,
                &output,
                cmake.clone(),
                &["-E", "tar", "xf", rcmake_path],
            )?;
            assert_eq!(
                read_bytes(&host, &output.join("input"), size_limit())?,
                payload
            );
            host.remove_file(&output.join("input"))?;

            let mut args = vec!["-E", "tar", flags, oracle_path];
            args.extend_from_slice(options);
            args.push("input");
            run(&host, &source, cmake.clone(), &args)?;
            if format == ArchiveFormat::TarZ {
                #[cfg(target_os = "macos")]
                {
                    let system_tar = run(
                        &host,
                        root,
                        BString::from("/usr/bin/uncompress"),
                        &["-c", rcmake_path],
                    )?;
                    assert_eq!(
                        system_tar,
                        read_bounded(
                            compress::Decoder::new(&read_bytes(
                                &host,
                                &rcmake_archive,
                                size_limit()
                            )?)?,
                            size_limit(),
                        )?
                    );
                    let encoded = run(
                        &host,
                        root,
                        BString::from("/usr/bin/compress"),
                        &["-c", oracle_path],
                    )?;
                    host.write(&oracle_archive, &encoded, WriteMode::Truncate)?;
                }
                #[cfg(not(target_os = "macos"))]
                continue;
            }
            assert_eq!(
                list_command_archive(&host, &oracle_archive, names(&["input"]), None)?,
                names(&["input"]),
                "{format:?}"
            );
            extract_archive(
                &host,
                &oracle_archive,
                &ExtractOptions {
                    destination: output.clone(),
                    patterns: names(&["input"]),
                    ..Default::default()
                },
            )?;
            assert_eq!(
                read_bytes(&host, &output.join("input"), size_limit())?,
                payload
            );
            host.remove_file(&output.join("input"))?;
        }
        Ok(())
    }
}
