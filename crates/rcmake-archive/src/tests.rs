use std::error::Error;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use rcmake_host::{FileKind, FileTime, Follow, PureHost, WriteMode};
use tar::{Builder, Header};
use zip::{ZipArchive, ZipWriter, extra_fields::ExtraField, write::SimpleFileOptions};

use super::*;

type TestResult = Result<(), Box<dyn Error>>;

const ALL_FORMATS: [ArchiveFormat; 3] =
    [ArchiveFormat::Tar, ArchiveFormat::TarGz, ArchiveFormat::Zip];

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
        for (path, mode) in [(source, 0o751), (directory, 0o750)] {
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
fn rejects_paths_through_links() -> TestResult {
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
    host.symlink(Path::new("/outside/file"), Path::new("/extract/tree/file"))?;
    assert!(extract_archive(&host, archive, &extract_options()).is_err());

    host.remove_file(Path::new("/extract/tree/file"))?;
    host.hard_link(Path::new("/outside/file"), Path::new("/extract/tree/file"))?;
    assert!(extract_archive(&host, archive, &extract_options()).is_err());
    assert_eq!(
        read_bytes(&host, Path::new("/outside/file"), 16)?,
        b"outside"
    );
    Ok(())
}

#[test]
fn rejects_an_escaping_symbolic_link_target() -> TestResult {
    let host = test_host();
    host.symlink(Path::new("../outside"), Path::new("/source/link"))?;
    let archive = Path::new("/archive");
    create_archive(
        &host,
        archive,
        Path::new("/source"),
        &[PathBuf::from("link")],
        &create_options(ArchiveFormat::Tar),
    )?;
    assert!(extract_archive(&host, archive, &extract_options()).is_err());
    assert!(absent(&host, "/extract"));
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
