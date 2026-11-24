// SPDX-License-Identifier: MIT OR Apache-2.0

//! Encode, validate, and transfer portable cache manifest entries.

#![forbid(unsafe_code)]

use std::{fmt, io::ErrorKind, path::Path, time::Duration};

use rcmake_host::ReadRange;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, DeserializeOwned, IgnoredAny, SeqAccess, Visitor},
};

use super::{FRAME_BYTES, FrameResult, LOCAL_STORE, PROBE_BLOB_BYTES, RESPONSE_LIMIT, RemoteError};
use crate::{Digest, Domain, Entry, InputStamp, Record, Store, StoreMode, blob_limit, frame};

const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
// A compact record with 16 digests takes fewer than 1280 ASCII bytes.
const MAX_MANIFEST_ENTRIES: usize = (MAX_MANIFEST_BYTES - 64) / 1280;
const MAX_BLOBS_PER_ENTRY: usize = 16;
const MAX_STAMPS: usize = 256;
const INVALID_MANIFEST: RemoteError =
    RemoteError::Failure("Invalid cache manifest schema, version, entry limits, or keys.");

/// Kind of keyed entry in a portable cache export.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheEntryKind {
    /// Compiler or platform probe.
    Probe,
    /// FetchContent archive cache entry.
    Fetch,
}
impl CacheEntryKind {
    fn domain(self) -> Domain {
        match self {
            Self::Probe => Domain::ProbeV1,
            Self::Fetch => Domain::FetchV1,
        }
    }
}
/// Content identities for one exported entry and its referenced blobs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    /// Type of cache entry.
    pub kind: CacheEntryKind,
    /// Key used to find this entry.
    pub key: Digest,
    /// SHA-256 digest of the full exported frame.
    pub frame_sha256: Digest,
    /// Sorted SHA-256 identities of the referenced blobs.
    #[serde(deserialize_with = "read_sequence::<_, _, MAX_BLOBS_PER_ENTRY>")]
    pub blobs: Vec<Digest>,
}
impl ManifestEntry {
    pub(super) fn identity(&self) -> (CacheEntryKind, Digest) {
        (self.kind, self.key)
    }
    /// Import a pinned entry. Supply the trace built from verified local inputs.
    /// Check the payload and artifact identities against the local plan in `validate`.
    pub fn seed<T: DeserializeOwned>(
        &self,
        store: &Store<'_>,
        encoded: &[u8],
        local_trace: &[InputStamp],
        validate: impl FnOnce(&Entry<T>) -> bool,
        load_blob: impl FnMut(Digest, usize) -> Result<Option<Vec<u8>>, RemoteError>,
    ) -> Result<Option<Entry<T>>, RemoteError> {
        let entry = decode_entry(self.kind, self.key, encoded, Some(self))?;
        if entry.trace.as_slice() != local_trace || !validate(&entry) {
            return Ok(None);
        }
        let mut remaining = PROBE_BLOB_BYTES;
        let ready = attach_blobs(store, self, encoded, &mut remaining, load_blob)?;
        Ok(ready.then_some(entry))
    }
}
fn read_sequence<'de, D, T, const LIMIT: usize>(de: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for Bounded<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "a sequence of up to {LIMIT} items")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let items = std::iter::from_fn(|| seq.next_element().transpose())
                .take(LIMIT)
                .collect::<Result<Vec<_>, _>>()?;
            if items.len() == LIMIT && seq.next_element::<IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("Sequence limit exceeded."));
            }
            Ok(items)
        }
    }
    de.deserialize_seq(Bounded::<T, LIMIT>(std::marker::PhantomData))
}
/// Versioned manifest for moving probe and fetch cache data between local stores.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CacheManifest {
    entries: Vec<ManifestEntry>,
}
impl CacheManifest {
    /// Parse bounded JSON. Reject other versions and duplicate keys.
    pub fn from_json(json: &[u8]) -> Result<Self, RemoteError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Manifest {
            version: u32,
            #[serde(deserialize_with = "read_sequence::<_, _, MAX_MANIFEST_ENTRIES>")]
            entries: Vec<ManifestEntry>,
        }
        if json.len() > MAX_MANIFEST_BYTES {
            return Err(RemoteError::Failure("Cache manifest JSON is too large."));
        }
        let Manifest {
            version,
            mut entries,
        } = serde_json::from_slice(json).map_err(|_| INVALID_MANIFEST)?;
        entries.sort_unstable_by_key(ManifestEntry::identity);
        let count = entries.len();
        entries.dedup_by_key(|entry| entry.identity());
        if version != SCHEMA_VERSION || !entries.iter().all(valid_record) || entries.len() != count
        {
            return Err(INVALID_MANIFEST);
        }
        Ok(Self { entries })
    }
    /// Encode deterministic JSON with a final line feed.
    pub fn to_json(&self) -> Result<Vec<u8>, RemoteError> {
        #[derive(Serialize)]
        struct Manifest<'a> {
            version: u32,
            entries: &'a [ManifestEntry],
        }
        let data = Manifest {
            version: SCHEMA_VERSION,
            entries: self.entries(),
        };
        let mut json = serde_json::to_vec(&data).map_err(|_| INVALID_MANIFEST)?;
        json.push(b'\n');
        Ok(json)
    }
    /// Add an export record. Return an error if the same key has other content.
    pub fn insert(&mut self, entry: ManifestEntry) -> Result<(), RemoteError> {
        if !valid_record(&entry) {
            return Err(INVALID_MANIFEST);
        }
        let index = self
            .entries
            .binary_search_by_key(&entry.identity(), ManifestEntry::identity);
        match index {
            Ok(index) if self.entries.get(index) == Some(&entry) => Ok(()),
            Ok(_) => Err(INVALID_MANIFEST),
            Err(_) if self.entries.len() >= MAX_MANIFEST_ENTRIES => {
                Err(RemoteError::Failure("Cache manifest has too many entries."))
            }
            Err(index) => {
                self.entries.insert(index, entry);
                Ok(())
            }
        }
    }
    /// Return export records in stable order by kind and key.
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }
}
pub(super) fn valid_record(entry: &ManifestEntry) -> bool {
    entry.blobs.len() <= MAX_BLOBS_PER_ENTRY && entry.blobs.is_sorted_by(|a, b| a < b)
}
/// Export an existing entry frame and its manifest record. Check its trace at import.
pub fn export_entry(
    store: &Store<'_>,
    kind: CacheEntryKind,
    key: Digest,
) -> Result<Option<(ManifestEntry, Vec<u8>)>, RemoteError> {
    if store.config.mode == StoreMode::Off {
        return Ok(None);
    }
    let path = store.entry_path(kind.domain(), key);
    let Some(encoded) = read_frame(store, &path, FRAME_BYTES)? else {
        return Ok(None);
    };
    let entry = decode_entry::<IgnoredAny>(kind, key, &encoded, None)?;
    let manifest = ManifestEntry {
        kind,
        key,
        frame_sha256: Digest::hash(&encoded),
        blobs: sorted_blobs(entry.blobs),
    };
    Ok(Some((manifest, encoded)))
}
/// Export the verified frame for one existing content-addressed blob.
pub fn export_blob(store: &Store<'_>, digest: Digest) -> Result<Option<Vec<u8>>, RemoteError> {
    if store.config.mode == StoreMode::Off {
        return Ok(None);
    }
    let Some(bytes) = read_frame(store, &store.blob_path(digest), blob_limit())? else {
        return Ok(None);
    };
    decode_blob(&bytes, digest, &mut blob_limit())?;
    Ok(Some(bytes))
}
fn sorted_blobs(mut blobs: Vec<Digest>) -> Vec<Digest> {
    blobs.sort_unstable();
    blobs.dedup();
    blobs
}
pub(super) fn decode_entry<T: DeserializeOwned>(
    kind: CacheEntryKind,
    key: Digest,
    encoded: &[u8],
    pin: Option<&ManifestEntry>,
) -> Result<Entry<T>, RemoteError> {
    if encoded.len() as u64 > frame::disk_limit(FRAME_BYTES) {
        return Err(RESPONSE_LIMIT);
    }
    if pin.is_some_and(|pin| !valid_record(pin) || pin.frame_sha256 != Digest::hash(encoded)) {
        return Err(RemoteError::Failure(
            "Remote entry digest differs from its pin.",
        ));
    }
    let bytes = frame::decode(frame::ENTRY_MAGIC, encoded, frame::ENTRY_LIMIT)
        .ok_or(RemoteError::Failure("Invalid cache entry frame."))?;
    let record: Record<Entry<T>> = serde_json::from_slice(&bytes)
        .map_err(|_| RemoteError::Failure("Invalid cache entry schema."))?;
    let entry = record.entry;
    if record.domain != kind.domain() || record.key != key {
        return Err(RemoteError::Failure("Cache entry domain or key differs."));
    }
    if entry.trace.len() > MAX_STAMPS || entry.blobs.len() > MAX_BLOBS_PER_ENTRY {
        return Err(RemoteError::Failure(
            "Cache entry has too many stamps or blobs.",
        ));
    }
    if pin.is_some_and(|pin| pin.blobs != sorted_blobs(entry.blobs.clone())) {
        return Err(RemoteError::Failure(
            "Remote blob list differs from its pin.",
        ));
    }
    Ok(entry)
}
pub(super) fn attach_blobs(
    store: &Store<'_>,
    pin: &ManifestEntry,
    encoded: &[u8],
    remaining: &mut usize,
    mut load_blob: impl FnMut(Digest, usize) -> FrameResult,
) -> Result<bool, RemoteError> {
    if store.config.mode == StoreMode::Off {
        return Ok(false);
    }
    let mut local_remaining = PROBE_BLOB_BYTES;
    for &digest in &pin.blobs {
        let path = store.blob_path(digest);
        if let Some(bytes) = read_frame(store, &path, local_remaining)? {
            decode_blob(&bytes, digest, &mut local_remaining).map_err(|_| LOCAL_STORE)?;
        } else if store.writable()
            && let Some(bytes) = load_blob(digest, *remaining)?
        {
            let size = decode_blob(&bytes, digest, remaining)?;
            let locked = store.lock_cache(Duration::ZERO).map_err(|_| LOCAL_STORE)?;
            locked
                .put_blob(digest, &path, &bytes, size)
                .map_err(|_| LOCAL_STORE)?;
        } else {
            return Ok(false);
        }
    }
    if store.writable() {
        return publish_pinned(store, pin, encoded).map_err(|_| LOCAL_STORE);
    }
    Ok(true)
}
fn publish_pinned(
    store: &Store<'_>,
    pin: &ManifestEntry,
    encoded: &[u8],
) -> rcmake_host::HostResult<bool> {
    let locked = store.lock_cache(Duration::ZERO)?;
    for &digest in &pin.blobs {
        if store.file_metadata(&store.blob_path(digest))?.is_none() {
            return Ok(false);
        }
    }
    let path = store.entry_path(pin.kind.domain(), pin.key);
    if store.file_metadata(&path)?.is_none() {
        locked.publish(&path, encoded, false)?;
    }
    Ok(true)
}
pub(super) fn read_frame(store: &Store<'_>, path: &Path, limit: usize) -> FrameResult {
    if store
        .file_metadata(path)
        .map_err(|_| LOCAL_STORE)?
        .is_none()
    {
        return Ok(None);
    }
    let limit = frame::disk_limit(limit);
    match store.host.read(path, ReadRange::All { limit }) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(LOCAL_STORE),
    }
}
pub(super) fn decode_blob(
    encoded: &[u8],
    digest: Digest,
    budget: &mut usize,
) -> Result<usize, RemoteError> {
    if encoded.len() as u64 > frame::disk_limit(*budget) {
        return Err(RESPONSE_LIMIT);
    }
    let bytes = frame::decode(frame::BLOB_MAGIC, encoded, *budget)
        .ok_or(RemoteError::Failure("Invalid blob frame or decoded size."))?;
    *budget -= bytes.len();
    if Digest::hash(&bytes) != digest {
        return Err(RemoteError::Failure("Blob SHA-256 verification failed."));
    }
    Ok(bytes.len())
}

#[test]
fn round_trips_manifest_and_rejects_excess_blob_digests() -> Result<(), Box<dyn std::error::Error>>
{
    let mut manifest = CacheManifest::default();
    let key = Digest::hash(b"fixture");
    let mut pin = ManifestEntry {
        kind: CacheEntryKind::Probe,
        key,
        frame_sha256: key,
        blobs: vec![key],
    };
    manifest.insert(pin.clone())?;
    assert_eq!(CacheManifest::from_json(&manifest.to_json()?)?, manifest);
    pin.blobs.resize(MAX_BLOBS_PER_ENTRY + 1, key);
    assert!(serde_json::from_slice::<ManifestEntry>(&serde_json::to_vec(&pin)?).is_err());
    Ok(())
}
