// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable cache manifests and an optional HTTP or S3 probe cache.

#![forbid(unsafe_code)]

use std::{fmt, path::Path, sync::atomic::Ordering, time::Duration};

use rcmake_base::BString;
use rcmake_host::{CommandSpec, Host, Input, Output, ProcessResult, ReadRange, RunRequest, path};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, IgnoredAny},
};

use crate::{
    Digest, Domain, Entry, NEXT_TEMP, Record, StagingFile, Store, StoreMode, VerifyOutcome,
    blob_limit, frame, verify_trace,
};

const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 65_536;
const MAX_BLOBS_PER_ENTRY: usize = 128;
const MAX_AUTH_BYTES: usize = 8 * 1024;

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
    pub blobs: Vec<Digest>,
}

impl ManifestEntry {
    fn identity(&self) -> (CacheEntryKind, Digest) {
        (self.kind, self.key)
    }

    /// Import a pinned entry and its blob frames. Publish the entry after its blobs.
    ///
    /// A stale input trace or missing blob is a cache miss.
    pub fn seed<T: DeserializeOwned + Serialize>(
        &self,
        store: &Store<'_>,
        encoded: &[u8],
        load_blob: impl FnMut(Digest) -> Result<Option<Vec<u8>>, RemoteError>,
    ) -> Result<Option<Entry<T>>, RemoteError> {
        let entry = decode_entry(self.kind, self.key, encoded, Some(self))?;
        publish_entry(store, self.kind, self.key, entry, load_blob)
    }
}

/// Versioned manifest for moving probe and fetch cache data between local stores.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheManifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

impl Default for CacheManifest {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl CacheManifest {
    /// Parse a bounded manifest. Reject other versions and duplicate entry keys.
    pub fn from_json(json: &[u8]) -> Result<Self, RemoteError> {
        if json.len() > MAX_MANIFEST_BYTES {
            return Err(RemoteError::TooLarge);
        }
        let mut manifest: Self =
            serde_json::from_slice(json).map_err(|_| RemoteError::InvalidManifest)?;
        if manifest.version != SCHEMA_VERSION {
            return Err(RemoteError::InvalidManifest);
        }
        manifest
            .entries
            .sort_unstable_by_key(ManifestEntry::identity);
        if manifest.entries.len() > MAX_MANIFEST_ENTRIES {
            return Err(RemoteError::TooLarge);
        }
        if !manifest.entries.iter().all(valid_record) || !unique_keys(&manifest.entries) {
            return Err(RemoteError::InvalidManifest);
        }
        Ok(manifest)
    }

    /// Encode deterministic JSON with a final line feed.
    pub fn to_json(&self) -> Result<Vec<u8>, RemoteError> {
        let mut json = serde_json::to_vec(self).map_err(|_| RemoteError::InvalidManifest)?;
        json.push(b'\n');
        if json.len() > MAX_MANIFEST_BYTES {
            return Err(RemoteError::TooLarge);
        }
        Ok(json)
    }

    /// Add an export record. Return an error if the same key has other content.
    pub fn insert(&mut self, entry: ManifestEntry) -> Result<(), RemoteError> {
        if !valid_record(&entry) {
            return Err(RemoteError::InvalidManifest);
        }
        match self
            .entries
            .binary_search_by_key(&entry.identity(), ManifestEntry::identity)
        {
            Ok(index) if self.entries.get(index) == Some(&entry) => Ok(()),
            Ok(_) => Err(RemoteError::InvalidManifest),
            Err(_) if self.entries.len() >= MAX_MANIFEST_ENTRIES => Err(RemoteError::TooLarge),
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

fn valid_record(entry: &ManifestEntry) -> bool {
    entry.blobs.len() <= MAX_BLOBS_PER_ENTRY
        && entry
            .blobs
            .windows(2)
            .all(|pair| matches!(pair, [a, b] if a < b))
}

fn unique_keys(entries: &[ManifestEntry]) -> bool {
    entries
        .windows(2)
        .all(|pair| matches!(pair, [a, b] if a.identity() != b.identity()))
}

/// Export the verified frame for one existing entry and return its manifest record.
pub fn export_entry(
    store: &Store<'_>,
    kind: CacheEntryKind,
    key: Digest,
) -> Result<Option<(ManifestEntry, Vec<u8>)>, RemoteError> {
    if store.config.mode == StoreMode::Off {
        return Ok(None);
    }
    let path = store.entry_path(kind.domain(), key);
    let limit = frame::disk_limit(frame::ENTRY_LIMIT);
    let Some(stat) = store
        .file_metadata(&path)
        .map_err(|_| RemoteError::LocalStore)?
    else {
        return Ok(None);
    };
    if stat.len > limit {
        return Err(RemoteError::TooLarge);
    }
    let encoded = match store.host.read(&path, ReadRange::All { limit }) {
        Ok(bytes) => bytes,
        Err(error) if error.kind == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(RemoteError::LocalStore),
    };
    let entry = decode_entry::<IgnoredAny>(kind, key, &encoded, None)?;
    if verify_trace(store.host, &entry.trace) != VerifyOutcome::Hit {
        return Ok(None);
    }
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
    store
        .get_blob(digest)
        .map_err(|_| RemoteError::LocalStore)?
        .map(|bytes| {
            frame::encode(frame::BLOB_MAGIC, &bytes, blob_limit())
                .map_err(|_| RemoteError::LocalStore)
        })
        .transpose()
}

fn sorted_blobs(mut blobs: Vec<Digest>) -> Vec<Digest> {
    blobs.sort_unstable();
    blobs.dedup();
    blobs
}

fn decode_entry<T: DeserializeOwned>(
    kind: CacheEntryKind,
    key: Digest,
    encoded: &[u8],
    pin: Option<&ManifestEntry>,
) -> Result<Entry<T>, RemoteError> {
    if encoded.len() as u64 > frame::disk_limit(frame::ENTRY_LIMIT) {
        return Err(RemoteError::TooLarge);
    }
    if pin.is_some_and(|pin| {
        pin.identity() != (kind, key) || pin.frame_sha256 != Digest::hash(encoded)
    }) {
        return Err(RemoteError::InvalidContent);
    }
    let bytes = frame::decode(frame::ENTRY_MAGIC, encoded, frame::ENTRY_LIMIT)
        .ok_or(RemoteError::InvalidContent)?;
    let record: Record<Entry<T>> =
        serde_json::from_slice(&bytes).map_err(|_| RemoteError::InvalidContent)?;
    if record.domain != kind.domain()
        || record.key != key
        || record.entry.blobs.len() > MAX_BLOBS_PER_ENTRY
        || pin.is_some_and(|pin| pin.blobs != sorted_blobs(record.entry.blobs.clone()))
    {
        return Err(RemoteError::InvalidContent);
    }
    Ok(record.entry)
}

fn publish_entry<T: Serialize>(
    store: &Store<'_>,
    kind: CacheEntryKind,
    key: Digest,
    entry: Entry<T>,
    mut load_blob: impl FnMut(Digest) -> Result<Option<Vec<u8>>, RemoteError>,
) -> Result<Option<Entry<T>>, RemoteError> {
    if !store.writable() || verify_trace(store.host, &entry.trace) != VerifyOutcome::Hit {
        return Ok(None);
    }
    for digest in sorted_blobs(entry.blobs.clone()) {
        if store
            .get_blob(digest)
            .map_err(|_| RemoteError::LocalStore)?
            .is_some()
        {
            continue;
        }
        let Some(encoded) = load_blob(digest)? else {
            return Ok(None);
        };
        if encoded.len() as u64 > frame::disk_limit(blob_limit()) {
            return Err(RemoteError::TooLarge);
        }
        let bytes = frame::decode(frame::BLOB_MAGIC, &encoded, blob_limit())
            .filter(|bytes| Digest::hash(bytes) == digest)
            .ok_or(RemoteError::InvalidContent)?;
        store
            .put_blob(&bytes)
            .map_err(|_| RemoteError::LocalStore)?;
    }
    store
        .put_entry(kind.domain(), key, &entry)
        .map_err(|_| RemoteError::LocalStore)?;
    Ok(Some(entry))
}

/// Remote write policy. Both modes permit reads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RemoteAccess {
    /// Fetch entries from the remote cache.
    #[default]
    ReadOnly,
    /// Fetch and publish entries and blobs.
    ReadWrite,
}

/// Authentication settings. Values are read from the host for each request.
pub enum RemoteAuth {
    /// Send no credentials.
    Anonymous,
    /// Read a bearer token from RCMAKE_CACHE_REMOTE_TOKEN.
    Bearer,
    /// Read a value from RCMAKE_CACHE_REMOTE_TOKEN for the named HTTP header.
    Header(String),
    /// Sign requests to an S3 HTTPS endpoint. The value is the AWS region.
    S3(String),
}

/// Address and limits for an optional remote probe cache.
pub struct RemoteCacheConfig {
    endpoint: String,
    /// Whether publication is enabled.
    pub access: RemoteAccess,
    /// Source of the optional credentials.
    pub auth: RemoteAuth,
    /// Maximum time to connect.
    pub connect_timeout: Duration,
    /// Maximum time for a request, including its connection.
    pub total_timeout: Duration,
    /// Maximum encoded frame size for entries and blobs.
    pub max_entry_bytes: usize,
    /// Consecutive transport failures that disable the client for this configure.
    pub max_failures: u32,
}

impl RemoteCacheConfig {
    /// Use an HTTP or HTTPS base URL with no credentials, query, or fragment.
    pub fn new(endpoint: &str) -> Result<Self, RemoteError> {
        let (_, rest) = endpoint
            .split_once("://")
            .ok_or(RemoteError::Configuration)?;
        let host = rest.split('/').next().unwrap_or_default();
        if !(endpoint.starts_with("https://") || endpoint.starts_with("http://"))
            || endpoint.len() > 4096
            || endpoint
                .bytes()
                .any(|byte| !byte.is_ascii_graphic() || b"?#\\@".contains(&byte))
            || host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte))
        {
            return Err(RemoteError::Configuration);
        }
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').into(),
            access: RemoteAccess::ReadOnly,
            auth: RemoteAuth::Anonymous,
            connect_timeout: Duration::from_millis(300),
            total_timeout: Duration::from_secs(1),
            max_entry_bytes: 16 * 1024 * 1024,
            max_failures: 2,
        })
    }

    /// Read remote settings from the host. An unset or empty URL disables the client.
    ///
    /// Set `RCMAKE_CACHE_REMOTE_WRITE=1` for publication. Set
    /// `RCMAKE_CACHE_REMOTE_AUTH` to `bearer`, `header`, or `s3` to authenticate.
    /// Bearer and header modes read their values from `RCMAKE_CACHE_REMOTE_TOKEN`.
    /// Header mode also requires `RCMAKE_CACHE_REMOTE_HEADER`. S3 mode reads
    /// `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and the optional
    /// `AWS_SESSION_TOKEN`. Authentication requires HTTPS.
    pub fn from_host(host: &dyn Host) -> Result<Option<Self>, RemoteError> {
        let Some(endpoint) = host
            .env_get(b"RCMAKE_CACHE_REMOTE")
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let mut config =
            Self::new(std::str::from_utf8(&endpoint).map_err(|_| RemoteError::Configuration)?)?;
        if host
            .env_get(b"RCMAKE_CACHE_REMOTE_WRITE")
            .is_some_and(|value| value == "1")
        {
            config.access = RemoteAccess::ReadWrite;
        }
        let auth = host
            .env_get(b"RCMAKE_CACHE_REMOTE_AUTH")
            .unwrap_or_default();
        config.auth = match auth.as_slice() {
            b"" | b"anonymous" => RemoteAuth::Anonymous,
            b"bearer" => RemoteAuth::Bearer,
            b"header" => RemoteAuth::Header(config_text(host, b"RCMAKE_CACHE_REMOTE_HEADER")?),
            b"s3" => RemoteAuth::S3(config_text(host, b"AWS_REGION")?),
            _ => return Err(RemoteError::Configuration),
        };
        Ok(Some(config))
    }

    fn validate(&self) -> Result<(), RemoteError> {
        let valid_auth = match &self.auth {
            RemoteAuth::Anonymous | RemoteAuth::Bearer => true,
            RemoteAuth::Header(name) => valid_header(name),
            RemoteAuth::S3(region) => {
                !region.is_empty()
                    && region.len() <= 64
                    && region.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            }
        };
        if !valid_auth
            || (!matches!(self.auth, RemoteAuth::Anonymous)
                && !self.endpoint.starts_with("https://"))
            || self.connect_timeout.is_zero()
            || self.connect_timeout > self.total_timeout
            || self.max_entry_bytes == 0
            || self.max_entry_bytes > blob_limit()
            || self.max_failures == 0
        {
            return Err(RemoteError::Configuration);
        }
        Ok(())
    }
}

fn config_text(host: &dyn Host, key: &[u8]) -> Result<String, RemoteError> {
    host.env_get(key)
        .and_then(|value| String::try_from(value).ok())
        .ok_or(RemoteError::Configuration)
}

fn valid_header(name: &str) -> bool {
    const RESERVED: &str = concat!(
        "host content-length transfer-encoding connection expect proxy-authorization ",
        "proxy-connection trailer upgrade te",
    );
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !RESERVED
            .split_ascii_whitespace()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Remote cache operation failure. Values contain no endpoint, credentials, or tool output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteError {
    /// Invalid endpoint, limits, or authentication settings.
    Configuration,
    /// A required credential is missing or invalid.
    Authentication,
    /// The manifest is malformed or conflicts with an existing record.
    InvalidManifest,
    /// The frame, digest, key, or blob references failed verification.
    InvalidContent,
    /// A frame or manifest exceeds a size limit.
    TooLarge,
    /// The local cache could not read or publish the requested content.
    LocalStore,
    /// The transport failed before it returned a valid HTTP status.
    Transport,
    /// The process or curl timed out.
    Timeout,
    /// The endpoint rejected the request.
    HttpStatus(u16),
    /// Too many consecutive failures disabled requests for this configure.
    CircuitOpen,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match *self {
            Self::Configuration => "Invalid remote cache configuration.",
            Self::Authentication => "Missing or invalid remote cache credentials.",
            Self::InvalidManifest => "Invalid or conflicting cache manifest.",
            Self::InvalidContent => "Remote cache content failed verification.",
            Self::TooLarge => "Cache data exceeds its size limit.",
            Self::LocalStore => "Cannot read or publish data in the local cache.",
            Self::Transport => "Remote cache request failed.",
            Self::Timeout => "Remote cache request timed out.",
            Self::HttpStatus(status) => {
                return write!(formatter, "Remote cache returned HTTP {status}.");
            }
            Self::CircuitOpen => "Remote cache requests are disabled for this configure.",
        })
    }
}

impl std::error::Error for RemoteError {}

/// Optional remote cache scoped to one configure.
pub struct RemoteStoreClient<'a> {
    host: &'a dyn Host,
    config: RemoteCacheConfig,
    failures: u32,
}

impl<'a> RemoteStoreClient<'a> {
    /// Create a client. Use the same host as the local store.
    pub fn new(host: &'a dyn Host, config: RemoteCacheConfig) -> Result<Self, RemoteError> {
        config.validate()?;
        Ok(Self {
            host,
            config,
            failures: 0,
        })
    }

    /// Return whether the client has stopped sending requests for this configure.
    pub fn circuit_is_open(&self) -> bool {
        self.failures >= self.config.max_failures
    }

    /// Fetch a probe entry and its missing blobs and publish them to the local store.
    ///
    /// A 404 or invalid input trace is a miss. An optional trusted manifest pins
    /// the full entry frame. The caller can continue with a local probe on error.
    pub fn get_probe<T: DeserializeOwned + Serialize>(
        &mut self,
        store: &Store<'_>,
        key: Digest,
        pin: Option<&ManifestEntry>,
    ) -> Result<Option<Entry<T>>, RemoteError> {
        if !store.writable() {
            return Ok(None);
        }
        let Some(encoded) = self.request(&probe_path(key), None)? else {
            return Ok(None);
        };
        let entry = decode_entry(CacheEntryKind::Probe, key, &encoded, pin)?;
        publish_entry(store, CacheEntryKind::Probe, key, entry, |digest| {
            self.request(&blob_path(digest), None)
        })
    }

    /// Publish a local probe and its referenced blobs when remote writes are enabled.
    ///
    /// Upload blobs first. Return false if writes are disabled or data is absent.
    pub fn put_probe(&mut self, store: &Store<'_>, key: Digest) -> Result<bool, RemoteError> {
        if self.config.access == RemoteAccess::ReadOnly {
            return Ok(false);
        }
        if self.circuit_is_open() {
            return Err(RemoteError::CircuitOpen);
        }
        let Some((manifest, encoded)) = export_entry(store, CacheEntryKind::Probe, key)? else {
            return Ok(false);
        };
        if encoded.len() > self.config.max_entry_bytes {
            return Err(RemoteError::TooLarge);
        }
        for &digest in &manifest.blobs {
            let Some(blob) = export_blob(store, digest)? else {
                return Ok(false);
            };
            self.upload(store, &blob_path(digest), &blob)?;
        }
        self.upload(store, &probe_path(key), &encoded)?;
        Ok(true)
    }

    fn upload(&mut self, store: &Store<'_>, suffix: &str, bytes: &[u8]) -> Result<(), RemoteError> {
        if bytes.len() > self.config.max_entry_bytes {
            return Err(RemoteError::TooLarge);
        }
        let directory = store.config.root.join("tmp");
        let pid = store.host.system().pid;
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let now = store.host.now();
        let path = directory.join(format!("remote-{pid}-{}-{}-{serial}", now.secs, now.nanos));
        if !store
            .publish(&path, bytes, false)
            .map_err(|_| RemoteError::LocalStore)?
        {
            return Err(RemoteError::LocalStore);
        }
        let _cleanup = StagingFile {
            host: store.host,
            path: &path,
            armed: true,
        };
        self.request(suffix, Some(&path)).map(|_| ())
    }

    fn request(
        &mut self,
        suffix: &str,
        upload: Option<&Path>,
    ) -> Result<Option<Vec<u8>>, RemoteError> {
        if self.circuit_is_open() {
            return Err(RemoteError::CircuitOpen);
        }
        let config = &self.config;
        let mut args: Vec<BString> = [
            "--disable",
            "--globoff",
            "--silent",
            "--proto",
            "=http,https",
            "--connect-timeout",
            &curl_timeout(config.connect_timeout),
            "--max-time",
            &curl_timeout(config.total_timeout),
            "--max-filesize",
            &config.max_entry_bytes.to_string(),
            "--write-out",
            "%{http_code}",
        ]
        .map(BString::from)
        .to_vec();
        if let Some(path) = upload {
            args.extend(["--upload-file".into(), path::from_native(path)]);
        }
        let stdin = credentials(self.host, &config.auth, &mut args)?;
        args.extend([
            "--url".into(),
            format!("{}/{suffix}", config.endpoint).into(),
        ]);
        let env = [b"PATH".as_slice(), b"SystemRoot"]
            .into_iter()
            .filter_map(|name| self.host.env_get(name).map(|value| (name.into(), value)))
            .collect();
        let result = self.host.run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "curl".into(),
                    args,
                }],
                cwd: self.host.initial_cwd(),
                env,
                stdin,
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(config.total_timeout),
                tee: false,
            },
            config.max_entry_bytes.saturating_add(3),
        );
        let response = result
            .map_err(|_| RemoteError::Transport)
            .and_then(|mut result| {
                match result.results.as_slice() {
                    [ProcessResult::Exited(0)] => {}
                    [ProcessResult::TimedOut | ProcessResult::Exited(28)] => {
                        return Err(RemoteError::Timeout);
                    }
                    [ProcessResult::Exited(63)] => return Err(RemoteError::TooLarge),
                    _ => return Err(RemoteError::Transport),
                }
                let tail = result
                    .stdout
                    .len()
                    .checked_sub(3)
                    .ok_or(RemoteError::Transport)?;
                let status = result
                    .stdout
                    .get(tail..)
                    .filter(|code| code.iter().all(u8::is_ascii_digit))
                    .and_then(|code| std::str::from_utf8(code).ok()?.parse::<u16>().ok())
                    .ok_or(RemoteError::Transport)?;
                if tail > config.max_entry_bytes {
                    return Err(RemoteError::TooLarge);
                }
                result.stdout.truncate(tail);
                match (upload.is_some(), status) {
                    (false, 200) | (true, 200..=299) => Ok(Some(result.stdout)),
                    (false, 404) => Ok(None),
                    (_, 100..=599) => Err(RemoteError::HttpStatus(status)),
                    _ => Err(RemoteError::Transport),
                }
            });
        if matches!(
            response,
            Err(RemoteError::Transport
                | RemoteError::Timeout
                | RemoteError::HttpStatus(429 | 500..=599))
        ) {
            self.failures = self.failures.saturating_add(1);
        } else {
            self.failures = 0;
        }
        response
    }
}

fn curl_timeout(duration: Duration) -> String {
    let millis = duration.as_millis().max(1);
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

fn probe_path(key: Digest) -> String {
    format!("probe/v1/{key}.entry")
}

fn blob_path(digest: Digest) -> String {
    format!("blobs/sha256/{digest}.blob")
}

fn credentials(
    host: &dyn Host,
    auth: &RemoteAuth,
    args: &mut Vec<BString>,
) -> Result<Input, RemoteError> {
    let mut config = Vec::new();
    match auth {
        RemoteAuth::Anonymous => return Ok(Input::Null),
        RemoteAuth::Bearer => {
            let token = required_secret(host, b"RCMAKE_CACHE_REMOTE_TOKEN")?;
            append_config(&mut config, b"header", &[b"Authorization: Bearer ", &token]);
        }
        RemoteAuth::Header(name) => {
            let value = required_secret(host, b"RCMAKE_CACHE_REMOTE_TOKEN")?;
            append_config(&mut config, b"header", &[name.as_bytes(), b": ", &value]);
        }
        RemoteAuth::S3(region) => {
            let access = required_secret(host, b"AWS_ACCESS_KEY_ID")?;
            let secret = required_secret(host, b"AWS_SECRET_ACCESS_KEY")?;
            if access.contains(&b':') {
                return Err(RemoteError::Authentication);
            }
            append_config(&mut config, b"user", &[&access, b":", &secret]);
            if let Some(token) = host
                .env_get(b"AWS_SESSION_TOKEN")
                .filter(|token| !token.is_empty())
            {
                validate_secret(&token)?;
                append_config(&mut config, b"header", &[b"x-amz-security-token: ", &token]);
            }
            args.extend(["--aws-sigv4".into(), format!("aws:amz:{region}:s3").into()]);
        }
    }
    args.extend(["--config".into(), "-".into()]);
    Ok(Input::Bytes(config))
}

fn required_secret(host: &dyn Host, name: &[u8]) -> Result<BString, RemoteError> {
    let value = host.env_get(name).ok_or(RemoteError::Authentication)?;
    validate_secret(&value)?;
    Ok(value)
}

fn validate_secret(value: &[u8]) -> Result<(), RemoteError> {
    if value.is_empty()
        || value.len() > MAX_AUTH_BYTES
        || !value
            .iter()
            .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        return Err(RemoteError::Authentication);
    }
    Ok(())
}

fn append_config(config: &mut Vec<u8>, option: &[u8], parts: &[&[u8]]) {
    config.extend_from_slice(option);
    config.extend_from_slice(b" = \"");
    for &part in parts {
        for &byte in part {
            if matches!(byte, b'\\' | b'"') {
                config.push(b'\\');
            }
            config.push(byte);
        }
    }
    config.extend_from_slice(b"\"\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::{PureHost, RunOutput};
    use std::sync::{Arc, atomic::AtomicUsize};

    fn host() -> PureHost {
        let root = if cfg!(windows) { "C:/work" } else { "/work" };
        PureHost::new(root.into(), Path::new(root).join("rcmake"))
    }

    fn store<'a>(host: &'a dyn Host, name: &str) -> Store<'a> {
        Store::new(host, crate::StoreConfig::new(host.initial_cwd().join(name)))
            .expect("local store")
    }

    fn output(bytes: &[u8], status: &[u8]) -> RunOutput {
        RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: [bytes, status].concat(),
            stderr: Vec::new(),
        }
    }

    fn fixture(store: &Store<'_>, key: Digest, blobs: Vec<Digest>) -> (ManifestEntry, Vec<u8>) {
        let entry = Entry {
            trace: Default::default(),
            blobs,
            payload: String::from("probe result"),
        };
        store
            .put_entry(Domain::ProbeV1, key, &entry)
            .expect("put test entry");
        export_entry(store, CacheEntryKind::Probe, key)
            .expect("export probe")
            .expect("probe present")
    }

    #[test]
    fn manifest_is_sorted_and_seeds_verified_entries() -> Result<(), Box<dyn std::error::Error>> {
        let host = host();
        let source = store(&host, "source");
        let destination = store(&host, "destination");
        let blob = source.put_blob(b"object")?;
        let first = Digest::from_bytes([1; 32]);
        let second = Digest::from_bytes([2; 32]);
        let (first_manifest, encoded) = fixture(&source, first, vec![blob]);
        let (second_manifest, _) = fixture(&source, second, vec![]);
        let mut manifest = CacheManifest::default();
        manifest.insert(second_manifest)?;
        manifest.insert(first_manifest)?;
        let json = manifest.to_json()?;
        assert_eq!(CacheManifest::from_json(&json)?, manifest);
        let pin = manifest.entries().first().ok_or("missing manifest entry")?;
        assert_eq!(pin.key, first);
        let result = pin.seed::<String>(&destination, &encoded, |digest| {
            export_blob(&source, digest)
        })?;
        assert_eq!(result.ok_or("missing seeded entry")?.blobs, vec![blob]);
        assert_eq!(destination.get_blob(blob)?, Some(b"object".to_vec()));
        assert!(matches!(
            pin.seed::<String>(&destination, b"bad", |_| Ok(None)),
            Err(RemoteError::InvalidContent)
        ));
        Ok(())
    }

    #[test]
    fn accepts_hits_and_misses_and_rejects_corruption() -> Result<(), Box<dyn std::error::Error>> {
        let host = host();
        let local = store(&host, "local");
        let source = store(&host, "source");
        let key = Digest::hash(b"hit");
        let (_, entry) = fixture(&source, key, vec![]);
        let mut bad = entry.clone();
        *bad.last_mut().ok_or("empty frame")? ^= 1;
        let config = RemoteCacheConfig::new("https://cache.test")?;
        let mut client = RemoteStoreClient::new(&host, config)?;
        host.set_command_handler(|_, _, _| Ok(output(b"missing", b"404")))?;
        assert!(client.get_probe::<String>(&local, key, None)?.is_none());
        host.set_command_handler(move |_, _, _| Ok(output(&bad, b"200")))?;
        assert_eq!(
            client.get_probe::<String>(&local, key, None),
            Err(RemoteError::InvalidContent)
        );
        assert!(local.get_entry::<String>(Domain::ProbeV1, key)?.is_none());
        host.set_command_handler(move |_, _, _| Ok(output(&entry, b"200")))?;
        assert!(client.get_probe::<String>(&local, key, None)?.is_some());
        assert!(local.get_entry::<String>(Domain::ProbeV1, key)?.is_some());
        Ok(())
    }

    #[test]
    fn stops_after_timeouts_and_keeps_credentials_out_of_arguments_and_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = host();
        let destination = store(&host, "destination");
        let key = Digest::hash(b"probe");
        let calls = Arc::new(AtomicUsize::new(0));
        let call_count = Arc::clone(&calls);
        host.env_set(b"RCMAKE_CACHE_REMOTE_TOKEN", Some(b"test-secret-token"));
        host.set_command_handler(move |_, request, _| {
            call_count.fetch_add(1, Ordering::Relaxed);
            assert_eq!(request.timeout, Some(Duration::from_secs(1)));
            assert!(!format!("{:?}", request.pipeline).contains("test-secret-token"));
            assert!(!format!("{:?}", request.env).contains("test-secret-token"));
            assert_eq!(
                request.stdin,
                Input::Bytes(b"header = \"X-Cache-Token: test-secret-token\"\n".to_vec())
            );
            Ok(RunOutput {
                results: vec![ProcessResult::TimedOut],
                stderr: b"test-secret-token".to_vec(),
                ..RunOutput::default()
            })
        })?;
        let mut config = RemoteCacheConfig::new("https://cache.test")?;
        config.auth = RemoteAuth::Header("X-Cache-Token".into());
        let mut client = RemoteStoreClient::new(&host, config)?;
        for _ in 0..2 {
            let result = client.get_probe::<String>(&destination, key, None);
            assert!(!format!("{result:?}").contains("test-secret-token"));
            assert_eq!(result, Err(RemoteError::Timeout));
        }
        assert_eq!(
            client.get_probe::<String>(&destination, key, None),
            Err(RemoteError::CircuitOpen)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        Ok(())
    }
}
