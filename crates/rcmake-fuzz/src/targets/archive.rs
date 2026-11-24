use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use rcmake_archive::{
    ArchiveFormat, CreateOptions, ExtractOptions, create_archive, extract_archive,
};
use rcmake_base::Limits;
use rcmake_host::{FileKind, Follow, Host, Metadata, ReadRange, path::to_native};

use super::{Check, in_memory_host, mutate, pick, random_bytes, require, root, setup};
use crate::Rng;

pub(super) fn check(rng: &mut Rng, limits: &Limits) -> Check {
    let host = in_memory_host();
    let archive = root().join("archive");
    let destination = root().join("extract");
    let outside = root().join("outside");
    setup(host.seed_file(&outside.join("marker"), b"outside"))?;
    let mode = rng.below(12);
    let payload = random_bytes(rng);
    let mut expected_link = None;
    let mut expected_file = None;
    let mut input = Vec::new();
    match mode {
        0 => input.extend_from_slice(&payload),
        1 => input.extend_from_slice(b"PK\x03\x04"),
        2 => input.extend_from_slice(b"\x1f\x8b\x08\x00"),
        3 => {
            let name = *pick(
                rng,
                &[
                    b"../outside/marker".as_slice(),
                    b"/outside/marker",
                    br"dir\..\marker",
                ],
            );
            add_tar_entry(&mut input, name, b'0', b"", &payload)?;
        }
        4 | 5 => {
            let target = *pick(rng, &[b"/outside".as_slice(), b"../outside"]);
            add_tar_entry(&mut input, b"link", b'2', target, b"")?;
            expected_link = Some(to_native(target));
            if mode == 5 {
                add_tar_entry(&mut input, b"link/marker", b'0', b"", &payload)?;
            }
        }
        6..=8 => {
            add_tar_entry(&mut input, b"dir/data", b'0', b"", &payload)?;
            if mode == 6 {
                mutate(rng, &mut input, b"\xff\0ustar");
            } else {
                expected_file = Some(destination.join("dir/data"));
            }
        }
        _ => {
            let source = root().join("source");
            setup(host.seed_file(&source.join("data"), &payload))?;
            setup(create_archive(
                &host,
                &archive,
                &source,
                &[PathBuf::from("data")],
                &CreateOptions {
                    format: *pick(
                        rng,
                        &[
                            ArchiveFormat::ZipStored,
                            ArchiveFormat::Zip,
                            ArchiveFormat::TarGz,
                        ],
                    ),
                    mtime: Some(0),
                    compression_level: None,
                    verbose: false,
                },
            ))?;
            input = setup(host.read(
                &archive,
                ReadRange::All {
                    limit: limits.max_file_read,
                },
            ))?;
            if mode == 9 {
                mutate(rng, &mut input, b"\xffPK");
            } else {
                expected_file = Some(destination.join("data"));
            }
        }
    }
    if matches!(mode, 3..=8) {
        input.resize(input.len() + 1024, 0);
    }
    if input.len() as u64 > limits.max_file_read {
        return Ok(false);
    }
    setup(host.seed_file(&archive, &input))?;
    let before = snapshot(&host, &destination, limits)?;
    let options = ExtractOptions {
        destination: destination.clone(),
        list_only: true,
        ..Default::default()
    };
    let listing = extract_archive(&host, &archive, &options);
    require(snapshot(&host, &destination, limits)? == before, || {
        "Listing changed files outside the destination.".into()
    })?;
    require(host.metadata(&destination, Follow::No).is_err(), || {
        "Listing created the destination.".into()
    })?;

    let extracted = extract_archive(
        &host,
        &archive,
        &ExtractOptions {
            list_only: false,
            ..options
        },
    );
    require(snapshot(&host, &destination, limits)? == before, || {
        format!("Extraction changed files outside the destination in archive mode {mode}.")
    })?;
    check_destination(&host, &destination, limits)?;
    let link_path = destination.join("link");
    if let Some(link) = expected_link
        && (mode == 4 || host.metadata(&link_path, Follow::No).is_ok())
    {
        let actual = setup(host.read_link(&link_path))?;
        require(actual == link, || {
            format!(
                "Extraction changed a symbolic link target. Expected {link:?}. Read {actual:?}."
            )
        })?;
    }
    if let Some(path) = expected_file {
        let actual = setup(host.read(
            &path,
            ReadRange::All {
                limit: limits.max_file_read,
            },
        ))?;
        require(actual == payload, || {
            format!("Extraction changed the contents of {path:?}.")
        })?;
    }
    for error in [listing.as_ref().err(), extracted.as_ref().err()]
        .into_iter()
        .flatten()
    {
        require(!error.message.is_empty(), || {
            format!("Archive error has no diagnostic in mode {mode}.")
        })?;
    }
    if matches!(mode, 1..=3) {
        require(listing.is_err(), || {
            format!("Listing accepted invalid archive mode {mode}.")
        })?;
    }
    if matches!(mode, 1..=3 | 5) {
        require(extracted.is_err(), || {
            format!("Extraction accepted invalid archive mode {mode}.")
        })?;
    }
    if matches!(mode, 4 | 7 | 8 | 10 | 11) {
        require(
            matches!((&listing, &extracted), (Ok(a), Ok(b)) if a == b),
            || {
                format!(
                    "Cannot list and extract generated archive mode {mode}. Listed {listing:?}. Extracted {extracted:?}."
                )
            },
        )?;
    }
    Ok(listing.is_ok() && extracted.is_ok())
}

fn add_tar_entry(
    input: &mut Vec<u8>,
    name: &[u8],
    kind: u8,
    target: &[u8],
    bytes: &[u8],
) -> Result<(), String> {
    let mut header = [0; 512];
    header[..100]
        .get_mut(..name.len())
        .ok_or("Tar fixture name exceeds 100 bytes.")?
        .copy_from_slice(name);
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", bytes.len()).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].fill(b' ');
    header[156] = kind;
    header[157..257]
        .get_mut(..target.len())
        .ok_or("Tar fixture link target exceeds 100 bytes.")?
        .copy_from_slice(target);
    header[257..265].copy_from_slice(b"ustar\x0000");
    let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    input.extend_from_slice(&header);
    input.extend_from_slice(bytes);
    input.resize(input.len().next_multiple_of(512), 0);
    Ok(())
}

type Snapshot = BTreeMap<PathBuf, (Metadata, Vec<u8>)>;

fn snapshot(host: &dyn Host, excluded: &Path, limits: &Limits) -> Result<Snapshot, String> {
    let mut entries = Snapshot::new();
    let mut pending = vec![root().to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in setup(host.read_dir(&directory))? {
            let path = directory.join(to_native(&entry.name));
            if path == excluded {
                continue;
            }
            let mut metadata = setup(host.metadata(&path, Follow::No))?;
            // Reads by the archive engine and by this check update the access time.
            metadata.atime = Default::default();
            let data = match entry.kind {
                FileKind::File => setup(host.read(
                    &path,
                    ReadRange::All {
                        limit: limits.max_file_read,
                    },
                ))?,
                FileKind::Symlink => setup(host.read_link(&path))?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec(),
                FileKind::Dir => {
                    pending.push(path.clone());
                    Vec::new()
                }
                FileKind::Other => Vec::new(),
            };
            entries.insert(path, (metadata, data));
        }
    }
    Ok(entries)
}

fn check_destination(host: &dyn Host, destination: &Path, limits: &Limits) -> Result<(), String> {
    let Ok(metadata) = host.metadata(destination, Follow::No) else {
        return Ok(());
    };
    require(metadata.kind == FileKind::Dir, || {
        "Extraction destination is not a directory.".into()
    })?;
    let mut pending = vec![destination.to_path_buf()];
    let mut bytes = 0;
    while let Some(directory) = pending.pop() {
        for entry in setup(host.read_dir(&directory))? {
            let path = directory.join(to_native(&entry.name));
            require(path != destination && path.starts_with(destination), || {
                format!("Entry escaped the extraction directory. Path {path:?}.")
            })?;
            if entry.kind == FileKind::Symlink {
                continue;
            }
            let actual = setup(host.canonicalize(&path))?;
            require(
                actual != destination && actual.starts_with(destination),
                || {
                    format!(
                        "Entry resolves outside the extraction directory. Path {path:?}. Resolved path {actual:?}."
                    )
                },
            )?;
            match entry.kind {
                FileKind::Dir => pending.push(path),
                FileKind::File => bytes += setup(host.metadata(&path, Follow::No))?.len,
                _ => {}
            }
        }
    }
    require(bytes <= 16 * limits.max_file_read, || {
        format!("Extraction produced {bytes} bytes and exceeded the size limit.")
    })
}
