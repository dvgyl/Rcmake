use std::{path::Path, time::Duration};

use rcmake_base::{Limits, hash::HashAlgo};
use rcmake_host::{Host, HostError};
use rcmake_store::{Digest, Domain, KeyHasher, PublishOutcome, Store};

use crate::{
    ArchiveContext, FetchError, atomic_write, error::sanitized_url, invalid, offline, run,
};

const DESTINATION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// A validated hash from a CMake `URL_HASH` or `EXPECTED_HASH` argument.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContentHash {
    name: String,
    algorithm: HashAlgo,
    hex: String,
}

impl ContentHash {
    /// Parse an `ALGORITHM=hex` argument.
    pub fn parse(spec: &str) -> Result<Self, FetchError> {
        let (name, hex) = spec
            .split_once('=')
            .ok_or_else(|| invalid("Expected ALGORITHM=hex."))?;
        let algorithm = HashAlgo::parse(name.as_bytes())
            .ok_or_else(|| invalid("Unknown archive hash algorithm."))?;
        if hex.len() != algorithm.hex_digest(b"").len()
            || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid("Invalid archive digest."));
        }
        Ok(Self {
            name: name.into(),
            algorithm,
            hex: hex.to_ascii_lowercase(),
        })
    }

    /// Construct a SHA-256 hash from the store's content identity.
    pub fn sha256(digest: Digest) -> Self {
        Self {
            name: "SHA256".into(),
            algorithm: HashAlgo::Sha256,
            hex: digest.to_hex(),
        }
    }

    /// Compare the hash with archive bytes.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), FetchError> {
        self.verify_hex(self.algorithm.hex_digest(bytes))
    }

    /// Verify a regular file without reading it into memory.
    pub fn verify_file(&self, host: &dyn Host, file: &Path) -> Result<(), FetchError> {
        self.verify_hex(rcmake_host::path::hash_file(host, file, self.algorithm)?)
    }

    fn verify_hex(&self, actual: String) -> Result<(), FetchError> {
        if actual == self.hex {
            Ok(())
        } else {
            Err(FetchError::HashMismatch {
                algorithm: self.name.clone(),
                expected: self.hex.clone(),
                actual,
            })
        }
    }

    pub(crate) fn identity(&self) -> String {
        format!("{}={}", self.name, self.hex)
    }

    fn key(&self, url: Option<&str>) -> Digest {
        let mut key = KeyHasher::new(Domain::FetchV1);
        key.field(0, b"fetch/url/v1");
        key.field(1, self.name.as_bytes());
        key.field(2, self.hex.as_bytes());
        if let Some(url) = url {
            key.field(3, url.as_bytes());
        }
        key.finish()
    }
}

/// Verified archive bytes and their origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveResult {
    /// Archive bytes.
    pub bytes: Vec<u8>,
    /// Whether the bytes came from the store.
    pub cache_hit: bool,
    /// Whether the store supplied the pin or confirmed publication for offline use.
    pub offline_seeded: bool,
    /// Store error if the cache could not read or publish the archive.
    pub cache_warning: Option<FetchError>,
}

/// Archive cache that uses the store's SHA-256 blobs and versioned hash aliases.
pub struct ArchiveCache<'a> {
    host: &'a dyn Host,
    store: &'a Store<'a>,
    cache_only: bool,
    byte_limit: usize,
    timeout: Option<Duration>,
}

impl<'a> ArchiveCache<'a> {
    /// Connect to a store. Fetches respect the host offline setting.
    pub fn new(host: &'a dyn Host, store: &'a Store<'a>) -> Self {
        Self {
            host,
            store,
            cache_only: false,
            byte_limit: usize::try_from(Limits::default().max_file_read).unwrap_or(usize::MAX),
            timeout: None,
        }
    }

    /// Set a total timeout for curl downloads.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Require cache-only operation for this instance.
    pub fn cache_only(mut self) -> Self {
        self.cache_only = true;
        self
    }

    pub(crate) fn is_cache_only(&self) -> bool {
        self.cache_only || offline(self.host)
    }

    pub(crate) fn with_byte_limit(mut self, limit: usize) -> Self {
        self.byte_limit = self.byte_limit.min(limit);
        self
    }

    /// Retrieve an archive by its expected hash. A URL by itself is no cache identity.
    pub fn lookup(&self, url: &str, hash: &ContentHash) -> Result<Option<Vec<u8>>, FetchError> {
        let mut first_error = None;
        if hash.algorithm == HashAlgo::Sha256
            && let Some(digest) = Digest::from_hex(&hash.hex)
        {
            match self.lookup_blob(digest) {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(error) => first_error = Some(error),
            }
        }
        for key in [hash.key(Some(url)), hash.key(None)] {
            match self.lookup_alias(key, hash) {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(None), Err)
    }

    fn lookup_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, FetchError> {
        self.store
            .get_blob_with_diagnostics(digest, self.byte_limit)
            .map_err(|cause| self.cache_read_error(cause))
    }

    fn lookup_alias(&self, key: Digest, hash: &ContentHash) -> Result<Option<Vec<u8>>, FetchError> {
        let entry = self
            .store
            .get_entry_with_diagnostics::<Digest>(Domain::FetchV1, key, self.byte_limit)
            .map_err(|cause| self.cache_read_error(cause))?;
        let Some(entry) = entry.filter(|entry| entry.blobs.as_slice() == [entry.payload]) else {
            return Ok(None);
        };
        let bytes = self.lookup_blob(entry.payload)?;
        if let Some(bytes) = &bytes {
            hash.verify(bytes)?;
        }
        Ok(bytes)
    }

    fn cache_read_error(&self, cause: HostError) -> FetchError {
        FetchError::CacheRead {
            path: cause
                .path
                .clone()
                .unwrap_or_else(|| self.store.root().to_owned()),
            cause,
        }
    }

    /// Verify bytes before adding them and their hash aliases to the store.
    pub fn insert(
        &self,
        url: &str,
        hash: &ContentHash,
        bytes: &[u8],
    ) -> Result<PublishOutcome, FetchError> {
        self.check_limit(bytes)?;
        hash.verify(bytes)?;
        self.insert_verified(&[url], &[hash], bytes)
    }

    pub(crate) fn insert_verified(
        &self,
        urls: &[&str],
        hashes: &[&ContentHash],
        bytes: &[u8],
    ) -> Result<PublishOutcome, FetchError> {
        let mut aliases = Vec::new();
        for hash in hashes {
            aliases.push(hash.key(None));
            aliases.extend(urls.iter().map(|url| hash.key(Some(url))));
        }
        let (_, outcome) = self
            .store
            .put_blob_with_aliases(bytes, Domain::FetchV1, &aliases)?;
        Ok(outcome)
    }

    pub(crate) fn context(&self, urls: &[&str], hashes: &[&ContentHash]) -> ArchiveContext {
        ArchiveContext {
            dependency: None,
            urls: urls.iter().map(|url| sanitized_url(url)).collect(),
            expected: hashes.iter().map(|hash| hash.identity()).collect(),
            lockfile: None,
            cache_root: self.store.root().to_owned(),
            recovery: if hashes.is_empty() {
                "Set URL_HASH or use a fetch lock pin for offline reuse."
            } else {
                "Check URL_HASH, the mirrors, and the cache path. Seed the exact pin online before an offline fetch."
            },
        }
    }

    /// Retrieve an archive or download it through the supplied transport.
    ///
    /// The transport runs only on a permitted miss. A missing hash bypasses the cache.
    pub fn fetch_with(
        &self,
        url: &str,
        hash: Option<&ContentHash>,
        download: impl FnOnce() -> Result<Vec<u8>, FetchError>,
    ) -> Result<ArchiveResult, FetchError> {
        let urls = [url];
        let hashes = hash.as_slice();
        let mut download = Some(download);
        let context = self.context(&urls, hashes);
        let mut result = self
            .fetch_candidates(&urls, hashes, None, |_| {
                download
                    .take()
                    .ok_or_else(|| invalid("The transport has already run."))?()
            })
            .map_err(|cause| context.clone().error(cause))?;
        result.cache_warning = result.cache_warning.map(|cause| context.error(cause));
        Ok(result)
    }

    pub(crate) fn fetch_candidates(
        &self,
        urls: &[&str],
        hashes: &[&ContentHash],
        size: Option<u64>,
        mut download: impl FnMut(&str) -> Result<Vec<u8>, FetchError>,
    ) -> Result<ArchiveResult, FetchError> {
        let mut cache_warning = None;
        if let (Some(url), Some(hash)) = (urls.first(), hashes.first()) {
            match self.lookup(url, hash) {
                Ok(Some(bytes)) => {
                    verify(&bytes, hashes, size)?;
                    return Ok(ArchiveResult {
                        bytes,
                        cache_hit: true,
                        offline_seeded: true,
                        cache_warning,
                    });
                }
                Ok(None) => {}
                Err(error) => cache_warning = Some(error),
            }
        }
        let offline_policy = if offline(self.host) {
            Some("RCMAKE_FETCH_OFFLINE=1 disables network access.")
        } else if self.cache_only {
            Some("This fetch uses cache-only mode.")
        } else {
            None
        };
        let result = match offline_policy {
            Some(policy) => Err(FetchError::ArchiveUnavailable {
                policy,
                reason: if hashes.is_empty() {
                    "There is no content pin for the archive."
                } else {
                    "The cache did not supply verified bytes for the pin."
                },
            }),
            None => {
                let mut last = Err(invalid("The archive has no download URL."));
                for url in urls {
                    last = download(url).and_then(|bytes| {
                        self.check_limit(&bytes)?;
                        verify(&bytes, hashes, size)?;
                        Ok(bytes)
                    });
                    if last.is_ok() {
                        break;
                    }
                }
                last
            }
        };
        let bytes = result.map_err(|download| match cache_warning.clone() {
            Some(cache) => FetchError::CacheAndDownload {
                cache: Box::new(cache),
                download: Box::new(download),
            },
            None => download,
        })?;
        let mut offline_seeded = false;
        if !hashes.is_empty() {
            match self
                .insert_verified(urls, hashes, &bytes)
                .and_then(confirmed)
            {
                Ok(()) => offline_seeded = true,
                Err(error) => cache_warning = cache_warning.or(Some(error)),
            }
        }
        Ok(ArchiveResult {
            bytes,
            cache_hit: false,
            offline_seeded,
            cache_warning,
        })
    }

    /// Retrieve an archive or download it with curl through the host.
    pub fn fetch(
        &self,
        url: &str,
        hash: Option<&ContentHash>,
    ) -> Result<ArchiveResult, FetchError> {
        self.fetch_with(url, hash, || self.download(url))
    }

    /// Retrieve and verify an archive before atomically replacing an absolute destination.
    pub fn materialize(
        &self,
        url: &str,
        hash: Option<&ContentHash>,
        destination: &Path,
    ) -> Result<ArchiveResult, FetchError> {
        if !destination.is_absolute() {
            return Err(rcmake_host::HostError::not_absolute(destination.to_owned()).into());
        }
        if destination.file_name().is_none() {
            return Err(invalid("Invalid archive destination."));
        }
        let result = self.fetch(url, hash)?;
        Self::write_verified(self.host, destination, &result.bytes)?;
        Ok(result)
    }

    /// Atomically write bytes that the caller verified against the requested pin.
    pub fn write_verified(
        host: &dyn Host,
        destination: &Path,
        bytes: &[u8],
    ) -> Result<(), FetchError> {
        if !destination.is_absolute() || destination.file_name().is_none() {
            return Err(invalid("Invalid archive destination."));
        }
        let guard = destination.with_added_extension("rcmake-fetch.guard");
        let _guard = host.lock_file(&guard, Some(DESTINATION_LOCK_TIMEOUT))?;
        atomic_write(host, destination, bytes)
    }

    pub(crate) fn download(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let result = run(
            self.host,
            "curl",
            vec![
                "-q".into(),
                "--fail".into(),
                "--silent".into(),
                "--show-error".into(),
                "--location".into(),
                "--globoff".into(),
                "--max-filesize".into(),
                self.byte_limit.to_string().into(),
                "--url".into(),
                url.into(),
            ],
            &self.host.initial_cwd(),
            self.host.env_all(),
            self.byte_limit,
            self.timeout,
        )?;
        Ok(result.stdout)
    }

    fn check_limit(&self, bytes: &[u8]) -> Result<(), FetchError> {
        if bytes.len() > self.byte_limit {
            return Err(invalid(format!(
                "Archive exceeds the {}-byte limit.",
                self.byte_limit
            )));
        }
        Ok(())
    }
}

pub(crate) fn confirmed(outcome: PublishOutcome) -> Result<(), FetchError> {
    match outcome {
        PublishOutcome::Published | PublishOutcome::AlreadyPresent => Ok(()),
        other => Err(FetchError::Publication { outcome: other }),
    }
}

fn verify(bytes: &[u8], hashes: &[&ContentHash], size: Option<u64>) -> Result<(), FetchError> {
    for hash in hashes {
        hash.verify(bytes)?;
    }
    if let Some(expected) = size
        && bytes.len() as u64 != expected
    {
        return Err(invalid(format!(
            "Archive size differs from the lockfile. Expected {expected} bytes. Got {}.",
            bytes.len()
        )));
    }
    Ok(())
}
