use std::time::Duration;

use std::path::Path;

use rcmake_base::{Limits, hash::HashAlgo};
use rcmake_host::{Host, WriteMode};
use rcmake_store::{Digest, Domain, Entry, KeyHasher, Store};

use crate::{FetchError, invalid, offline, run};

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
        let actual = self.algorithm.hex_digest(bytes);
        if actual == self.hex {
            Ok(())
        } else {
            Err(FetchError::HashMismatch {
                expected: self.hex.clone(),
                actual,
            })
        }
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
    /// Store error if the cache could not read or publish the archive.
    pub cache_warning: Option<FetchError>,
}

/// Archive cache that uses the store's SHA-256 blobs and versioned hash aliases.
pub struct ArchiveCache<'a> {
    host: &'a dyn Host,
    store: &'a Store<'a>,
    offline: bool,
    timeout: Option<Duration>,
}

impl<'a> ArchiveCache<'a> {
    /// Connect to a store and read the offline setting from the host.
    pub fn new(host: &'a dyn Host, store: &'a Store<'a>) -> Self {
        Self {
            host,
            store,
            offline: offline(host),
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
        self.offline = true;
        self
    }

    /// Retrieve an archive by its expected hash. A URL by itself is no cache identity.
    pub fn lookup(&self, url: &str, hash: &ContentHash) -> Result<Option<Vec<u8>>, FetchError> {
        if hash.algorithm == HashAlgo::Sha256
            && let Some(digest) = Digest::from_hex(&hash.hex)
            && let Some(bytes) = self.store.get_blob(digest)?
        {
            return Ok(Some(bytes));
        }
        for key in [hash.key(Some(url)), hash.key(None)] {
            if let Some(entry) = self.store.get_entry::<Digest>(Domain::FetchV1, key)?
                && entry.blobs.as_slice() == [entry.payload]
                && let Some(bytes) = self.store.get_blob(entry.payload)?
                && hash.verify(&bytes).is_ok()
            {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Verify bytes before adding them and their hash aliases to the store.
    pub fn insert(&self, url: &str, hash: &ContentHash, bytes: &[u8]) -> Result<(), FetchError> {
        hash.verify(bytes)?;
        self.insert_verified(url, hash, bytes)
    }

    fn insert_verified(
        &self,
        url: &str,
        hash: &ContentHash,
        bytes: &[u8],
    ) -> Result<(), FetchError> {
        let digest = self.store.put_blob(bytes)?;
        let entry = Entry {
            trace: Default::default(),
            blobs: vec![digest],
            payload: digest,
        };
        self.store
            .put_entry(Domain::FetchV1, hash.key(None), &entry)?;
        self.store
            .put_entry(Domain::FetchV1, hash.key(Some(url)), &entry)?;
        Ok(())
    }

    /// Retrieve an archive or download it through the supplied transport.
    ///
    /// The transport runs only on a permitted miss. A missing hash always bypasses the cache.
    pub fn fetch_with(
        &self,
        url: &str,
        hash: Option<&ContentHash>,
        download: impl FnOnce() -> Result<Vec<u8>, FetchError>,
    ) -> Result<ArchiveResult, FetchError> {
        let mut cache_warning = None;
        if let Some(hash) = hash {
            match self.lookup(url, hash) {
                Ok(Some(bytes)) => {
                    return Ok(ArchiveResult {
                        bytes,
                        cache_hit: true,
                        cache_warning,
                    });
                }
                Ok(None) => {}
                Err(error) => cache_warning = Some(error),
            }
        }
        if self.offline || offline(self.host) {
            return Err(FetchError::Offline);
        }
        let bytes = download()?;
        if let Some(hash) = hash {
            hash.verify(&bytes)?;
            let write_warning = self.insert_verified(url, hash, &bytes).err();
            cache_warning = cache_warning.or(write_warning);
        }
        Ok(ArchiveResult {
            bytes,
            cache_hit: false,
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

    /// Retrieve and verify an archive before writing it to an absolute destination.
    pub fn materialize(
        &self,
        url: &str,
        hash: Option<&ContentHash>,
        destination: &Path,
    ) -> Result<ArchiveResult, FetchError> {
        let result = self.fetch(url, hash)?;
        self.host
            .write(destination, &result.bytes, WriteMode::Truncate)?;
        Ok(result)
    }

    pub(crate) fn download(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let limit = Limits::default().max_file_read;
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
                limit.to_string().into(),
                "--url".into(),
                url.into(),
            ],
            &self.host.initial_cwd(),
            self.host.env_all(),
            usize::try_from(limit).unwrap_or(usize::MAX),
            self.timeout,
        )?;
        Ok(result.stdout)
    }
}
