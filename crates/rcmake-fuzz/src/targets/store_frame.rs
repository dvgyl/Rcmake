//! Check cache frames through the store and parse portable cache manifests.

use std::fmt::Write as _;

use rcmake_host::{Host, ReadRange};
use rcmake_store::{Digest, Store, StoreConfig, StoreMode, remote::CacheManifest};

use super::{Check, in_memory_host, mutate, random_bytes, require, root, setup};
use crate::Rng;

const CHECKSUM_BYTES: usize = 32;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = (MAX_MANIFEST_BYTES - 64) / 1_280;
const MAX_BLOBS: usize = 16;

pub(super) fn check(rng: &mut Rng) -> Check {
    if rng.below(2) == 0 {
        check_frame(rng)
    } else {
        check_manifest(rng)
    }
}

fn check_frame(rng: &mut Rng) -> Check {
    let host = in_memory_host();
    let mut config = StoreConfig::new(root().join("fuzz-store"));
    let mut payload = random_bytes(rng);
    payload.push(rng.next_u64() as u8);
    let digest =
        setup(Store::new(&host, config.clone()).and_then(|store| store.put_blob(&payload)))?;
    let hex = digest.to_hex();
    let prefix = hex.get(..2).ok_or("Cannot get the blob shard prefix.")?;
    let path = config
        .root
        .join("blobs")
        .join(prefix)
        .join(format!("{hex}.blob"));
    let mut frame = setup(host.read(&path, ReadRange::All { limit: 4_096 }))?;
    config.mode = StoreMode::ReadOnly;
    let store = setup(Store::new(&host, config))?;
    require(
        setup(store.get_blob_with_limit(digest, payload.len()))?.as_deref()
            == Some(payload.as_slice()),
        || "The store could not read a valid frame at its size limit.".into(),
    )?;

    let mut limit = payload.len();
    let expect_hit = match rng.below(10) {
        0 => Some(true),
        1 => {
            limit -= 1;
            Some(false)
        }
        2 => {
            *frame.last_mut().ok_or("The frame has no checksum.")? ^= 1;
            Some(false)
        }
        3 => {
            let length = (limit as u64 + 1).to_le_bytes();
            frame
                .get_mut(9..17)
                .ok_or("The frame has no payload length.")?
                .copy_from_slice(&length);
            set_checksum(&mut frame)?;
            Some(false)
        }
        4 => {
            frame
                .get_mut(17..21)
                .ok_or("The frame has no compressed body.")?
                .fill(0);
            set_checksum(&mut frame)?;
            Some(false)
        }
        5 => {
            frame.truncate(rng.below(frame.len() as u64) as usize);
            Some(false)
        }
        6 => {
            frame = random_bytes(rng);
            None
        }
        _ => {
            mutate(rng, &mut frame, b"\xff\x00RCM_BLOB");
            None
        }
    };
    setup(host.seed_file(&path, &frame))?;
    let hit = setup(store.get_blob_with_limit(digest, limit))?;
    require(
        expect_hit.is_none_or(|expected| hit.is_some() == expected),
        || {
            format!(
                "Unexpected cache hit or miss for a {}-byte frame.",
                frame.len()
            )
        },
    )?;
    require(
        hit.as_ref()
            .is_none_or(|bytes| bytes == &payload && bytes.len() <= limit),
        || "The store returned different bytes or exceeded the payload limit.".into(),
    )?;
    Ok(hit.is_some())
}

fn set_checksum(frame: &mut [u8]) -> Result<(), String> {
    let footer = frame.len().saturating_sub(CHECKSUM_BYTES);
    let (body, checksum) = frame
        .split_at_mut_checked(footer)
        .ok_or("Invalid frame footer.")?;
    require(checksum.len() == CHECKSUM_BYTES, || {
        "Frame has a short checksum.".into()
    })?;
    checksum.copy_from_slice(Digest::hash(body).as_bytes());
    Ok(())
}

fn check_manifest(rng: &mut Rng) -> Check {
    let mode = rng.below(256);
    let mut json = String::from(r#"{"version":1,"entries":["#);
    let count = match mode {
        0 => MAX_MANIFEST_ENTRIES,
        1 => MAX_MANIFEST_ENTRIES + 1,
        2 | 3 => 0,
        _ => 2 + rng.below(4) as usize,
    };
    for index in (0..count).rev() {
        if index + 1 < count {
            json.push(',');
        }
        let kind = if index % 2 == 0 { "probe" } else { "fetch" };
        let (kind, key) = if mode == 4 {
            ("probe", 0)
        } else {
            (kind, index)
        };
        let blob_count = match mode {
            5 => MAX_BLOBS,
            6 => MAX_BLOBS + 1,
            7 | 8 => 2,
            _ => 0,
        };
        setup(write!(
            json,
            r#"{{"kind":"{kind}","key":"{key:064x}","frame_sha256":"{key:064x}","blobs":["#
        ))?;
        for blob in 0..blob_count {
            if blob > 0 {
                json.push(',');
            }
            let digest = match mode {
                7 => 0,
                8 => blob_count - blob,
                _ => blob,
            };
            setup(write!(json, r#""{digest:064x}""#))?;
        }
        json.push_str("]}");
    }
    json.push_str("]}");
    let mut bytes = json.into_bytes();
    let expected = match mode {
        2 | 3 => {
            bytes.resize(MAX_MANIFEST_BYTES + usize::from(mode == 3), b' ');
            Some(mode == 2)
        }
        1 | 4 | 6..=8 => Some(false),
        9..=31 => {
            bytes = random_bytes(rng);
            None
        }
        32..=159 => {
            mutate(rng, &mut bytes, b"\xff\x00{[\"entries\":");
            None
        }
        _ => Some(true),
    };
    let parsed = CacheManifest::from_json(&bytes);
    require(expected.is_none_or(|valid| parsed.is_ok() == valid), || {
        format!("Unexpected manifest parse result for case {mode}.")
    })?;
    let Ok(manifest) = parsed else {
        return Ok(false);
    };
    let entries = manifest.entries();
    require(
        entries.len() <= MAX_MANIFEST_ENTRIES
            && entries.is_sorted_by(|a, b| (a.kind, a.key) < (b.kind, b.key))
            && entries.iter().all(|entry| {
                entry.blobs.len() <= MAX_BLOBS && entry.blobs.is_sorted_by(|a, b| a < b)
            }),
        || "The cache manifest exceeds its limits or has unordered or duplicate keys.".into(),
    )?;
    let canonical = setup(manifest.to_json())?;
    require(
        setup(CacheManifest::from_json(&canonical))? == manifest,
        || "The cache manifest changed after a JSON round trip.".into(),
    )?;
    Ok(true)
}
