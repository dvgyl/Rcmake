use super::*;

#[test]
fn rejects_tar_metadata_before_reading_its_payload() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for kind in [
        EntryType::XHeader,
        EntryType::XGlobalHeader,
        EntryType::GNULongName,
        EntryType::GNULongLink,
    ] {
        let mut header = Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(1024 * 1024 + 1);
        header.set_cksum();
        host.write(archive, header.as_bytes(), WriteMode::Truncate)?;
        let result = extract_archive(&host, archive, &extract_options());
        assert!(
            result.is_err_and(
                |error| error.message == "Tar metadata exceeds the record or size limit"
            )
        );
        assert!(absent(&host, "/extract"));
    }
    Ok(())
}

#[test]
fn tar_creation_rejects_metadata_over_its_reader_limit() -> TestResult {
    let host = test_host();
    let source = Path::new("/source");
    let input = source.join("value");
    host.seed_file(&input, b"")?;
    host.set_times(&input, None, Some(FileTime { secs: 1, nanos: 1 }))?;
    // Each fractional timestamp adds a 1 KiB PAX record.
    let inputs = vec![PathBuf::from("value"); 16 * 1024 + 1];
    let result = create_archive(
        &host,
        Path::new("/archive"),
        source,
        &inputs,
        &create_options(ArchiveFormat::Tar),
    );
    assert!(
        result.is_err_and(|error| error.message == "Tar metadata exceeds the record or size limit")
    );
    assert!(absent(&host, "/archive"));
    Ok(())
}

#[test]
fn uses_pax_size_to_find_the_next_tar_header() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut builder = Builder::new(Vec::new());
    builder.append_pax_extensions([("size", b"513".as_slice())])?;
    let mut first = Header::new_ustar();
    first.set_size(1);
    first.set_mode(0o644);
    first.set_path("first")?;
    first.set_cksum();
    builder.get_mut().extend_from_slice(first.as_bytes());
    builder.get_mut().extend_from_slice(&[0; 1024]);
    add_test_tar_file(&mut builder, "next")?;
    host.seed_file(archive, &builder.into_inner()?)?;
    assert_eq!(
        extract_archive(&host, archive, &extract_options())?,
        names(&["first", "next"])
    );
    assert_eq!(
        host.metadata(Path::new("/extract/first"), Follow::No)?.len,
        513
    );
    Ok(())
}

#[test]
fn charges_duplicate_zip_records_before_selection() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..17 {
        writer.start_file(format!("{index:05}"), SimpleFileOptions::default())?;
        writer.write_all(b"x")?;
    }
    let mut bytes = writer.finish()?.into_inner();
    let mut zip = ZipArchive::new(Cursor::new(&bytes))?;
    let mut offsets = Vec::new();
    for index in 0..17 {
        offsets.push(usize::try_from(
            zip.by_index_raw(index)?.central_header_start(),
        )?);
    }
    drop(zip);
    for offset in offsets {
        bytes
            .get_mut(offset + 24..offset + 28)
            .ok_or("Missing central directory size field")?
            .copy_from_slice(&(u32::MAX - 1).to_le_bytes());
        bytes
            .get_mut(offset + 46..offset + 51)
            .ok_or("Missing central directory name")?
            .copy_from_slice(b"first");
    }
    host.seed_file(archive, &bytes)?;
    assert!(
        list_command_archive(&host, archive, names(&["missing"]), None)
            .is_err_and(|error| error.message.starts_with("Archive exceeds the size limit"))
    );
    Ok(())
}

#[test]
fn zip64_footer_with_a_gap_uses_its_central_directory() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("value", SimpleFileOptions::default())?;
    writer.write_all(b"x")?;
    let mut bytes = writer.finish()?.into_inner();
    let zip = ZipArchive::new(Cursor::new(&bytes))?;
    let start = usize::try_from(zip.central_directory_start())?;
    drop(zip);
    let end = bytes.len() - 22;
    bytes.truncate(end);
    bytes.extend_from_slice(b"PK\x06\x06");
    bytes.extend_from_slice(&44_u64.to_le_bytes());
    bytes.extend_from_slice(&[45, 0, 45, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    for field in [1, 1, (end - start) as u64, start as u64] {
        bytes.extend_from_slice(&field.to_le_bytes());
    }
    bytes.extend_from_slice(b"gapPK\x06\x07");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&(end as u64).to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(b"PK\x05\x06\0\0\0\0\xff\xff\xff\xff");
    bytes.extend_from_slice(&[0xff; 8]);
    bytes.extend_from_slice(&[0, 0]);
    host.seed_file(archive, &bytes)?;
    assert_eq!(
        extract_archive(&host, archive, &extract_options())?,
        names(&["value"])
    );
    assert_eq!(read_bytes(&host, Path::new("/extract/value"), 16)?, b"x");

    let count = (MAX_ENTRIES as u64 + 1).to_le_bytes();
    for offset in [end + 24, end + 32] {
        bytes
            .get_mut(offset..offset + count.len())
            .ok_or("Missing ZIP64 entry count")?
            .copy_from_slice(&count);
    }
    host.write(archive, &bytes, WriteMode::Truncate)?;
    let result = list_command_archive(&host, archive, Vec::new(), None);
    assert!(result.is_err_and(|error| error.message == "Archive exceeds the entry or size limit"));
    Ok(())
}

#[test]
fn ignores_zip_end_records_inside_the_comment() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("value", SimpleFileOptions::default())?;
    writer.write_all(b"x")?;
    let bytes = writer.finish()?.into_inner();
    let zip = ZipArchive::new(Cursor::new(&bytes))?;
    let start = u32::try_from(zip.central_directory_start())?;
    let end = bytes.len() - 22;

    for (count, size, offset) in [(1_u16, 46_u32, start), (0, 0, u32::MAX), (0, 0, 0)] {
        let mut commented = bytes.clone();
        commented
            .get_mut(end + 20..end + 22)
            .ok_or("Missing ZIP comment length")?
            .copy_from_slice(&22_u16.to_le_bytes());
        commented.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        commented.extend_from_slice(&count.to_le_bytes());
        commented.extend_from_slice(&count.to_le_bytes());
        commented.extend_from_slice(&size.to_le_bytes());
        commented.extend_from_slice(&offset.to_le_bytes());
        commented.extend_from_slice(&[0, 0]);
        if offset == u32::MAX {
            assert!(
                {
                    let comment = commented.get(end + 22..).ok_or("Missing comment")?;
                    crate::zip_codec::open(&mut Cursor::new(comment), comment.len() as u64, 128)
                }
                .is_err()
            );
        }
        host.write(archive, &commented, WriteMode::Truncate)?;
        assert_eq!(
            list_command_archive(&host, archive, Vec::new(), None)?,
            names(&["value"])
        );
    }
    Ok(())
}

#[test]
fn rejects_excessive_7zip_counts_in_raw_and_encoded_headers() -> TestResult {
    let too_many = [0xc1, 0xa1, 0x86]; // 100,001 in the 7-Zip integer format.
    let raw_files = [[1, 5].as_slice(), too_many.as_slice()].concat();
    let mut raw_packs = vec![1, 4, 6, 0];
    raw_packs.extend_from_slice(&too_many);
    let mut raw_folders = vec![1, 4, 7, 0x0b];
    raw_folders.extend_from_slice(&too_many);

    let size = u8::try_from(raw_files.len())?;
    let copy = [
        0x17, 6, 0, 1, 9, size, 0, 7, 0x0b, 1, 0, 1, 1, 0, 0x0c, size, 0, 0,
    ];
    for (packed, header) in [
        (b"".as_slice(), raw_files.as_slice()),
        (b"".as_slice(), raw_packs.as_slice()),
        (b"".as_slice(), raw_folders.as_slice()),
        (raw_files.as_slice(), copy.as_slice()),
    ] {
        let bytes = seven_zip_bytes(packed, header);
        assert!(open_seven_zip(&bytes)
            .is_err_and(|error| error.message == "7-Zip header exceeds the entry or size limit"));
    }
    Ok(())
}

#[test]
fn rejects_7zip_main_streams_outside_the_packed_region() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive");
    for (start, size) in [(0, 1), (127, 1), (0, 127)] {
        let raw = [
            1, 4, 6, start, 1, 9, size, 0, 7, 0x0b, 1, 0, 1, 1, 0, 0x0c, 1, 0, 8, 0, 0, 5, 1, 0x11,
            5, 0, b'x', 0, 0, 0, 0, 0,
        ];
        let encoded =
            seven_zip_encoded_header(1, raw.len(), 1, &[1, 0], &[u8::try_from(raw.len())?])?;
        let packed = [b"x".as_slice(), raw.as_slice()].concat();
        for bytes in [
            seven_zip_bytes(b"x", &raw),
            seven_zip_bytes(&packed, &encoded),
        ] {
            host.write(archive, &bytes, WriteMode::Truncate)?;
            let result = list_command_archive(&host, archive, Vec::new(), None);
            if (start, size) == (0, 1) {
                assert_eq!(result?, names(&["x"]));
            } else {
                assert!(result.is_err_and(|error| error.message == "Invalid 7-Zip header"));
            }
        }
    }
    Ok(())
}

#[test]
fn limits_total_7zip_header_decoder_memory() -> TestResult {
    // LZMA and LZMA2 each request an 8 MiB dictionary.
    let coders_and_bindings = [
        0x23, 3, 1, 1, 5, 0x5d, 0, 0, 0x80, 0, 0x21, 0x21, 1, 22, 1, 0,
    ];
    let header = seven_zip_encoded_header(0, 1, 2, &coders_and_bindings, &[2, 2])?;
    assert!(
        open_seven_zip(&seven_zip_bytes(b"x", &header))
            .is_err_and(|error| error.message == "7-Zip header exceeds the entry or size limit")
    );
    Ok(())
}

#[test]
fn checks_every_zstandard_frame_window_in_a_7zip_header() -> TestResult {
    let raw = [1, 5, 0, 0, 0];
    let mut packed = zstd::stream::encode_all(raw.as_slice(), 1)?;
    let coders = [4, 4, 0xf7, 0x11, 1];
    let sizes = [u8::try_from(raw.len())?];
    let header = seven_zip_encoded_header(0, packed.len(), 1, &coders, &sizes)?;
    assert!(
        open_seven_zip(&seven_zip_bytes(&packed, &header))?
            .files
            .is_empty()
    );
    // This empty frame declares a 32 MiB window.
    packed.extend_from_slice(b"\x28\xb5\x2f\xfd\x00\x78\x01\0\0");
    let header = seven_zip_encoded_header(0, packed.len(), 1, &coders, &sizes)?;
    assert!(
        open_seven_zip(&seven_zip_bytes(&packed, &header))
            .is_err_and(|error| error.message == "7-Zip header exceeds the entry or size limit")
    );
    Ok(())
}

fn seven_zip_encoded_header(
    start: u8,
    packed: usize,
    count: u8,
    coders_and_bindings: &[u8],
    sizes: &[u8],
) -> Result<Vec<u8>, std::num::TryFromIntError> {
    let mut header = vec![
        0x17,
        6,
        start,
        1,
        9,
        u8::try_from(packed)?,
        0,
        7,
        0x0b,
        1,
        0,
        count,
    ];
    header.extend_from_slice(coders_and_bindings);
    header.push(0x0c);
    header.extend_from_slice(sizes);
    header.extend_from_slice(&[0, 0]);
    Ok(header)
}

fn seven_zip_bytes(packed: &[u8], header: &[u8]) -> Vec<u8> {
    let mut start = Vec::new();
    start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start.extend_from_slice(&(header.len() as u64).to_le_bytes());
    start.extend_from_slice(&crc32fast::hash(header).to_le_bytes());

    let mut bytes = seven_zip::MAGIC.to_vec();
    bytes.extend_from_slice(&[0, 4]);
    bytes.extend_from_slice(&crc32fast::hash(&start).to_le_bytes());
    bytes.extend_from_slice(&start);
    bytes.extend_from_slice(packed);
    bytes.extend_from_slice(header);
    bytes
}

fn open_seven_zip(bytes: &[u8]) -> Result<sevenz_rust2::Archive, ArchiveError> {
    seven_zip::open(&mut Cursor::new(bytes), bytes.len() as u64)
}

fn tar_gz_with_file(name: &str, payload: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut tar_bytes = Vec::new();
    {
        let mut builder = Builder::new(&mut tar_bytes);
        let mut header = Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, payload)?;
        builder.finish()?;
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&tar_bytes)?;
    Ok(encoder.finish()?)
}

#[test]
fn ratio_cap_stops_a_compression_bomb() -> TestResult {
    let host = test_host();
    let archive = Path::new("/archive.tar.gz");
    let bomb = tar_gz_with_file("zeros.bin", &vec![0_u8; 8 * 1024 * 1024])?;
    assert!(bomb.len() < 32 * 1024, "expected high compression ratio");
    host.seed_file(archive, &bomb)?;
    let options = extract_options();
    let err = crate::extract::run_with_limits(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        1024 * 1024,
        200,
    )
    .expect_err("compression bomb must be rejected");
    assert!(
        err.message.contains("Archive exceeds the size limit"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("RCMAKE_EXTRACT_MAX_BYTES"),
        "{}",
        err.message
    );
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn ratio_cap_allows_normal_compression() -> TestResult {
    let host = test_host();
    let archive = Path::new("/normal.tar.gz");
    let mut payload = vec![0_u8; 1024 * 1024];
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for byte in &mut payload {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *byte = (state >> 56) as u8;
    }
    let packed = tar_gz_with_file("random.bin", &payload)?;
    host.seed_file(archive, &packed)?;
    let options = extract_options();
    let names_out = crate::extract::run_with_limits(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        1024 * 1024,
        200,
    )?;
    assert_eq!(names_out, names(&["random.bin"]));
    assert_eq!(
        host.metadata(Path::new("/extract/random.bin"), Follow::No)?
            .len,
        1024 * 1024
    );
    Ok(())
}

#[test]
fn env_override_replaces_the_limit() -> TestResult {
    let host = test_host();
    let archive = Path::new("/override.tar.gz");
    let bomb = tar_gz_with_file("zeros.bin", &vec![0_u8; 8 * 1024 * 1024])?;
    host.seed_file(archive, &bomb)?;
    host.env_set(b"RCMAKE_EXTRACT_MAX_BYTES", Some(b"16777216"));
    let options = extract_options();
    let names_out = crate::extract::run_with_limits(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        1024 * 1024,
        200,
    )?;
    assert_eq!(names_out, names(&["zeros.bin"]));
    assert_eq!(
        host.metadata(Path::new("/extract/zeros.bin"), Follow::No)?
            .len,
        8 * 1024 * 1024
    );
    Ok(())
}

#[test]
fn trailer_drain_reads_at_most_16_mib() -> TestResult {
    use std::io::Read;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    struct TrailingZeros {
        prefix: Vec<u8>,
        pos: usize,
        total_read: Arc<AtomicU64>,
        fail_after: u64,
    }

    impl Read for TrailingZeros {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let so_far = self.total_read.load(Ordering::Relaxed);
            if so_far >= self.fail_after {
                return Err(std::io::Error::other("trailer drain exceeded 32 MiB"));
            }
            let n = if self.pos < self.prefix.len() {
                let avail = (self.prefix.len() - self.pos).min(buf.len());
                buf[..avail].copy_from_slice(&self.prefix[self.pos..self.pos + avail]);
                self.pos += avail;
                avail
            } else {
                buf.fill(0);
                buf.len()
            };
            self.total_read.fetch_add(n as u64, Ordering::Relaxed);
            Ok(n)
        }
    }

    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "hello.txt")?;
    let tar_bytes = builder.into_inner()?;
    let tar_len = tar_bytes.len() as u64;
    let total_read = Arc::new(AtomicU64::new(0));
    let reader = TrailingZeros {
        prefix: tar_bytes,
        pos: 0,
        total_read: Arc::clone(&total_read),
        fail_after: 32 * 1024 * 1024,
    };
    let host = test_host();
    let err = crate::extract::scan_tar_for_test(&host, Path::new("/extract"), reader)
        .expect_err("unbounded trailing zeros must be rejected");
    assert!(
        err.message.contains("Archive exceeds the size limit"),
        "{}",
        err.message
    );
    let read_bytes = total_read.load(Ordering::Relaxed);
    assert!(
        read_bytes <= tar_len + 16 * 1024 * 1024 + crate::extract::stage::BUFFER as u64,
        "trailer drain read {read_bytes} bytes for {tar_len}-byte tar"
    );
    Ok(())
}

#[test]
fn compressed_trailing_zeros_bomb_is_rejected() -> TestResult {
    let mut builder = Builder::new(Vec::new());
    add_test_tar_file(&mut builder, "hello.txt")?;
    let mut tar_bytes = builder.into_inner()?;
    tar_bytes.resize(tar_bytes.len() + 4 * 1024 * 1024, 0);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&tar_bytes)?;
    let bomb = encoder.finish()?;

    let host = test_host();
    let archive = Path::new("/trailer_bomb.tar.gz");
    host.seed_file(archive, &bomb)?;
    let options = extract_options();
    let err = crate::extract::run_with_limits(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        1024 * 1024,
        200,
    )
    .expect_err("compressed trailing-zero bomb must be rejected");
    assert!(
        err.message.contains("Archive exceeds the size limit"),
        "{}",
        err.message
    );
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn corrupt_gzip_trailer_after_tar_eof_is_rejected() -> TestResult {
    let mut packed = tar_gz_with_file("hello.txt", b"hello\n")?;
    let last = packed.last_mut().ok_or("empty gzip archive")?;
    *last ^= 0xff;

    let host = test_host();
    let archive = Path::new("/corrupt_trailer.tar.gz");
    host.seed_file(archive, &packed)?;
    let options = extract_options();
    let err = crate::extract::run_with_limits(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        1024 * 1024,
        200,
    )
    .expect_err("corrupt gzip trailer must be rejected");
    assert!(!err.message.is_empty());
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn header_only_tar_bomb_respects_stream_budget() -> TestResult {
    let mut builder = Builder::new(Vec::new());
    for i in 0..2500 {
        let mut header = Header::new_ustar();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, format!("e{i}.txt"), &[][..])?;
    }
    let tar_bytes = builder.into_inner()?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&tar_bytes)?;
    let packed = encoder.finish()?;

    let host = test_host();
    let archive = Path::new("/headers_bomb.tar.gz");
    host.seed_file(archive, &packed)?;
    let options = extract_options();
    let err = crate::extract::run_with_output_limit(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        256 * 1024,
    )
    .expect_err("header-only tar exceeding stream budget must be rejected");
    assert!(
        err.message.contains("Archive exceeds the size limit"),
        "{}",
        err.message
    );
    assert!(absent(&host, "/extract"));
    Ok(())
}

#[test]
fn grzip_external_filter_respects_output_limit() -> TestResult {
    let host = test_host();
    let bin_dir = if cfg!(windows) {
        Path::new("C:\\bin")
    } else {
        Path::new("/bin")
    };
    let grzip = bin_dir.join(format!("grzip{}", std::env::consts::EXE_SUFFIX));
    host.seed_file(&grzip, b"")?;
    host.set_permissions(&grzip, 0o755)?;
    host.env_set(b"PATH", Some(&rcmake_host::path::from_native(bin_dir)));
    host.set_command_handler(|_, _, _| {
        Ok(rcmake_host::RunOutput {
            results: vec![rcmake_host::ProcessResult::Exited(0)],
            stdout: vec![0_u8; 512 * 1024],
            ..Default::default()
        })
    })?;
    let archive = Path::new("/bomb.tar.grz");
    host.seed_file(archive, b"GRZipII\0payload")?;
    let options = extract_options();
    let err = crate::extract::run_with_output_limit(
        &host,
        archive,
        (&options).into(),
        crate::extract::Listing::Shared,
        256 * 1024,
    )
    .expect_err("grzip output exceeding limit must be rejected");
    assert!(
        err.message.contains("Archive exceeds the size limit"),
        "{}",
        err.message
    );
    Ok(())
}

fn ustar_magic_gnu_longname_tar(
    long_path: &[u8],
    corrupt_checksum: bool,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut payload = long_path.to_vec();
    payload.push(0);
    let mut long_hdr = Header::new_old();
    long_hdr.set_path("././@LongLink")?;
    long_hdr.set_mode(0o644);
    long_hdr.set_size(payload.len() as u64);
    long_hdr.set_entry_type(EntryType::GNULongName);
    if let Some(slot) = long_hdr.as_mut_bytes().get_mut(257..265) {
        // Apache Ant / Commons Compress ustar-magic GNU long-name header (`ustar\0\0\0`).
        slot.copy_from_slice(b"ustar\0\0\0");
    }
    long_hdr.set_cksum();
    if corrupt_checksum {
        long_hdr.as_mut_bytes()[0] ^= 0x01;
    }

    let mut out = Vec::new();
    out.extend_from_slice(long_hdr.as_bytes());
    out.extend_from_slice(&payload);
    let pad = (512 - (payload.len() % 512)) % 512;
    out.resize(out.len() + pad, 0);

    let mut file_hdr = Header::new_ustar();
    file_hdr.set_path("truncated-name")?;
    file_hdr.set_mode(0o644);
    file_hdr.set_size(4);
    file_hdr.set_mtime(0);
    file_hdr.set_entry_type(EntryType::Regular);
    file_hdr.set_cksum();
    out.extend_from_slice(file_hdr.as_bytes());
    out.extend_from_slice(b"body");
    out.resize(out.len() + 508, 0);
    out.resize(out.len() + 1024, 0);
    Ok(out)
}

#[test]
fn accepts_gnu_longname_with_ustar_magic_while_enforcing_checksum_and_traversal() -> TestResult {
    let host = test_host();
    let archive = Path::new("/long.tar");
    let long_name = format!(
        "activemq-cpp-library-3.9.5/{}/header.h",
        "nested/".repeat(20)
    );
    let tar_bytes = ustar_magic_gnu_longname_tar(long_name.as_bytes(), false)?;
    host.seed_file(archive, &tar_bytes)?;
    let extracted = extract_archive(&host, archive, &extract_options())?;
    assert_eq!(extracted, vec![BString::from(long_name.as_str())]);
    assert_eq!(
        read_bytes(&host, &Path::new("/extract").join(&long_name), 16)?,
        b"body"
    );

    let bad_cksum = ustar_magic_gnu_longname_tar(long_name.as_bytes(), true)?;
    let corrupt_archive = Path::new("/corrupt.tar");
    host.seed_file(corrupt_archive, &bad_cksum)?;
    assert!(extract_archive(&host, corrupt_archive, &extract_options()).is_err());

    let traversal = ustar_magic_gnu_longname_tar(
        format!("{}/../escape.txt", "a".repeat(120)).as_bytes(),
        false,
    )?;
    let traversal_archive = Path::new("/traversal.tar");
    host.seed_file(traversal_archive, &traversal)?;
    assert!(extract_archive(&host, traversal_archive, &extract_options()).is_err());
    Ok(())
}
