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
    for name in ["first", "other"] {
        writer.start_file(name, SimpleFileOptions::default())?;
        writer.write_all(b"x")?;
    }
    let mut bytes = writer.finish()?.into_inner();
    let zip = ZipArchive::new(Cursor::new(&bytes))?;
    let start = usize::try_from(zip.central_directory_start())?;
    let mut zip = zip;
    let second = usize::try_from(zip.by_index_raw(1)?.central_header_start())?;
    drop(zip);
    let size = u32::try_from(size_limit() / 2)?;
    for offset in [start, second] {
        bytes
            .get_mut(offset + 24..offset + 28)
            .ok_or("Missing central directory size field")?
            .copy_from_slice(&size.to_le_bytes());
    }
    bytes
        .get_mut(second + 46..second + 51)
        .ok_or("Missing central directory name")?
        .copy_from_slice(b"first");
    host.seed_file(archive, &bytes)?;
    assert!(
        list_command_archive(&host, archive, names(&["missing"]), None)
            .is_err_and(|error| error.message == "Archive exceeds the size limit")
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
                crate::zip_codec::open(commented.get(end + 22..).ok_or("Missing comment")?)
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
        assert!(seven_zip::open(&bytes)
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
        seven_zip::open(&seven_zip_bytes(b"x", &header))
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
        seven_zip::open(&seven_zip_bytes(&packed, &header))?
            .files
            .is_empty()
    );
    // This empty frame declares a 32 MiB window.
    packed.extend_from_slice(b"\x28\xb5\x2f\xfd\x00\x78\x01\0\0");
    let header = seven_zip_encoded_header(0, packed.len(), 1, &coders, &sizes)?;
    assert!(
        seven_zip::open(&seven_zip_bytes(&packed, &header))
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
