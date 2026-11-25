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
        (ArchiveFormat::Tar, "cf", [].as_slice()),
        (ArchiveFormat::Tar, "cf", ["--format=pax"].as_slice()),
        (ArchiveFormat::Tar, "cf", ["--format=paxr"].as_slice()),
        (ArchiveFormat::GnuTar, "cf", ["--format=gnutar"].as_slice()),
        (ArchiveFormat::TarGz, "czf", [].as_slice()),
        (ArchiveFormat::TarBz2, "cjf", [].as_slice()),
        (ArchiveFormat::TarXz, "cJf", [].as_slice()),
        (ArchiveFormat::TarLzma, "cf", ["--lzma"].as_slice()),
        (ArchiveFormat::TarZstd, "cf", ["--zstd"].as_slice()),
        (ArchiveFormat::TarZ, "cf", [].as_slice()),
        (ArchiveFormat::Zip, "cf", ["--format=zip"].as_slice()),
        (
            ArchiveFormat::ZipStored,
            "cf",
            ["--format=zip", "--cmake-tar-compression-method=store"].as_slice(),
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
                        compress::Decoder::new(
                            read_bytes(&host, &rcmake_archive, size_limit())?.as_slice()
                        )?,
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
