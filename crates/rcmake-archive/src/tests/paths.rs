use super::*;

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
    let size = 129;
    let gnu = header.as_gnu_mut().ok_or("Expected a GNU tar header")?;
    gnu.set_real_size(size);
    let block = gnu.sparse.first_mut().ok_or("Expected a sparse block")?;
    block.set_offset(size);
    block.set_length(0);
    builder.append_data(&mut header, "sparse", b"".as_slice())?;
    host.seed_file(archive, &builder.into_inner()?)?;
    let options = ExtractOptions {
        patterns: names(&["selected"]),
        ..extract_options()
    };
    assert!(
        crate::extract::run_with_output_limit(
            &host,
            archive,
            (&options).into(),
            crate::extract::Listing::Shared,
            128
        )
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
    let oversized = 129_u32;
    bytes
        .get_mut(offset..offset + 4)
        .ok_or("Missing central directory size field")?
        .copy_from_slice(&oversized.to_le_bytes());
    host.seed_file(archive, &bytes)?;
    let options = ExtractOptions {
        patterns: names(&["selected"]),
        ..extract_options()
    };
    assert!(
        crate::extract::run_with_output_limit(
            &host,
            archive,
            (&options).into(),
            crate::extract::Listing::Shared,
            128
        )
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
fn restores_symlink_mtime_unless_touched() -> TestResult {
    let formats = [
        ArchiveFormat::Tar,
        ArchiveFormat::Zip,
        ArchiveFormat::SevenZip {
            compression: SevenZipCompression::Store,
            threads: 1,
        },
    ];
    for format in formats {
        let host = test_host();
        let archive = Path::new("/archive");
        let target = Path::new("/target");
        let link = Path::new("/extract/link");
        host.seed_file(target, b"target")?;
        let target_mtime = host.metadata(target, Follow::No)?.mtime;
        host.symlink(target, Path::new("/source/link"))?;
        let mut create = create_options(format);
        create.mtime = Some(1_600_000_000);
        let archive_mtime = timestamp::seconds(1_600_000_000);
        create_archive(
            &host,
            archive,
            Path::new("/source"),
            &[PathBuf::from("link")],
            &create,
        )?;

        let mut extract = extract_options();
        extract_archive(&host, archive, &extract)?;
        assert_eq!(
            host.metadata(link, Follow::No)?.mtime,
            archive_mtime,
            "{format:?}"
        );
        assert_eq!(host.metadata(target, Follow::No)?.mtime, target_mtime);

        extract.touch = true;
        extract_archive(&host, archive, &extract)?;
        assert_ne!(
            host.metadata(link, Follow::No)?.mtime,
            archive_mtime,
            "{format:?}"
        );
        assert_eq!(host.metadata(target, Follow::No)?.mtime, target_mtime);
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
fn tar_extraction_can_replace_its_source_archive() -> TestResult {
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

#[test]
fn confined_mode_rejects_absolute_and_escaping_symbolic_links() -> TestResult {
    for format in ALL_FORMATS.into_iter().chain([ArchiveFormat::SevenZip {
        compression: SevenZipCompression::Store,
        threads: 0,
    }]) {
        for target in ["/outside", "../outside"] {
            let host = test_host();
            let archive = Path::new("/archive");
            host.symlink(Path::new(target), Path::new("/source/link"))?;
            create_archive(
                &host,
                archive,
                Path::new("/source"),
                &[PathBuf::from("link")],
                &create_options(format),
            )?;
            assert!(
                extract_archive(&host, archive, extract_options().with_confined_links()).is_err()
            );
            assert!(absent(&host, "/extract"));
            extract_archive(&host, archive, &extract_options())?;
            assert_eq!(
                host.read_link(Path::new("/extract/link"))?,
                Path::new(target)
            );
        }
    }
    Ok(())
}

#[test]
fn confined_mode_accepts_relative_links_that_stay_inside() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "value")?;
    add_test_tar_link(&mut builder, "sub/symlink", EntryType::Symlink, b"../value")?;
    add_test_tar_link(&mut builder, "hardlink", EntryType::Link, b"value")?;
    host.seed_file(archive, &builder.into_inner()?)?;

    let options = extract_options();
    extract_archive(&host, archive, options.with_confined_links())?;
    assert_eq!(
        host.read_link(Path::new("/extract/sub/symlink"))?,
        Path::new("../value")
    );
    let value = host.metadata(Path::new("/extract/value"), Follow::No)?;
    assert_eq!(
        host.metadata(Path::new("/extract/hardlink"), Follow::No)?,
        value
    );
    assert_eq!(value.nlink, 2);
    Ok(())
}

#[test]
fn confined_mode_rejects_preexisting_link_targets() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    host.seed_file(Path::new("/extract/value"), b"existing")?;
    host.symlink(Path::new("/outside"), Path::new("/extract/redirect"))?;

    for (kind, target) in [
        (EntryType::Link, "value"),
        (EntryType::Symlink, "redirect/value"),
    ] {
        let mut builder = Builder::new(Vec::new());
        add_test_tar_link(&mut builder, "link", kind, target.as_bytes())?;
        host.write(archive, &builder.into_inner()?, WriteMode::Truncate)?;
        let options = extract_options();
        assert!(extract_archive(&host, archive, options.with_confined_links()).is_err());
        assert!(absent(&host, "/extract/link"));
        extract_archive(&host, archive, &options)?;
        host.remove_file(Path::new("/extract/link"))?;
    }
    Ok(())
}
