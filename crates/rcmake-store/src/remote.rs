// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable cache manifests and an optional HTTP or S3 probe cache.

#![forbid(unsafe_code)]

use std::{
    fmt,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, WriteMode, path,
};
use rcmake_json::FromJson;

use crate::{
    Digest, Entry, InputStamp, NEXT_TEMP, PrivateDir, StagingFile, Store, frame, verify_private_dir,
};

mod manifest;

pub use manifest::{CacheEntryKind, CacheManifest, ManifestEntry, export_blob, export_entry};

use manifest::{attach_blobs, decode_blob, decode_entry, read_frame, valid_record};

const PROBE_BLOB_BYTES: usize = 16 * 1024 * 1024;
const PRIVATE_DIR_SETTING: &str = "RCMAKE_CACHE_REMOTE_PRIVATE_DIR";
const SESSION_BLOB_BYTES: usize = 64 * 1024 * 1024;
const MAX_AUTH_BYTES: usize = 8 * 1024;
const EXPORT_TIMEOUT_SETTING: &str = "RCMAKE_CACHE_REMOTE_EXPORT_TIMEOUT";
const FRAME_BYTES: usize = frame::ENTRY_LIMIT;
type FrameResult = Result<Option<Vec<u8>>, RemoteError>;
const RESPONSE_LIMIT: RemoteError =
    RemoteError::Failure("The response exceeds the frame or blob budget.");
const LOCAL_STORE: RemoteError =
    RemoteError::Failure("Cannot access local cache data. Check RCMAKE_CACHE_DIR.");
const TIMEOUT: RemoteError = RemoteError::Failure("The remote request timed out.");
const BUDGET: RemoteError = RemoteError::Failure("The remote cache time limit expired.");

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
#[derive(Clone)]
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
#[derive(Clone)]
pub struct RemoteCacheConfig {
    endpoint: String,
    /// Whether publication is enabled.
    pub access: RemoteAccess,
    /// Source of the optional credentials.
    pub auth: RemoteAuth,
    /// Trusted absolute curl executable. The default uses the system installation.
    pub curl_path: Option<PathBuf>,
    /// Existing private directory for curl input on Unix.
    pub private_dir: Option<PathBuf>,
    /// Permit publication of the complete probe capture and artifact bytes.
    pub export_captured_output: bool,
    /// Maximum cumulative time spent on remote cache work in one configure.
    pub session_timeout: Duration,
    /// Maximum cumulative time for one export job. The default is 300 seconds.
    pub export_timeout: Duration,
}
impl RemoteCacheConfig {
    /// Use an HTTP or HTTPS origin or base path without URL user info, query, or fragment.
    pub fn new(endpoint: &str) -> Result<Self, RemoteError> {
        let invalid = RemoteError::Setting("RCMAKE_CACHE_REMOTE");
        let (scheme, rest) = endpoint.split_once("://").ok_or(invalid)?;
        let host = rest.split('/').next().unwrap_or_default();
        if !matches!(scheme, "http" | "https")
            || endpoint.len() > 4096
            || host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
            || endpoint
                .bytes()
                .any(|b| !b.is_ascii_graphic() || b"?#\\@\"".contains(&b))
        {
            return Err(invalid);
        }
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').into(),
            access: RemoteAccess::ReadOnly,
            auth: RemoteAuth::Anonymous,
            curl_path: None,
            private_dir: None,
            export_captured_output: false,
            session_timeout: Duration::from_secs(3),
            export_timeout: Duration::from_secs(300),
        })
    }
    /// Read settings from the host. An unset or empty RCMAKE_CACHE_REMOTE disables access.
    /// Uploads require RCMAKE_CACHE_REMOTE_WRITE=1 and RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT=1.
    /// Set RCMAKE_CACHE_REMOTE_EXPORT_TIMEOUT to the export time limit in seconds.
    pub fn from_host(host: &dyn Host) -> Result<Option<Self>, RemoteError> {
        let env = |key: &[u8]| host.env_get(key);
        let enabled = |key: &[u8]| env(key).is_some_and(|s| s == "1");
        let text = |key: &'static str| {
            env(key.as_bytes())
                .and_then(|value| String::try_from(value).ok())
                .ok_or(RemoteError::Setting(key))
        };
        let Some(endpoint) = env(b"RCMAKE_CACHE_REMOTE").filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let url = std::str::from_utf8(&endpoint)
            .map_err(|_| RemoteError::Setting("RCMAKE_CACHE_REMOTE"))?;
        if !url.starts_with("https://") {
            return Err(RemoteError::Setting("RCMAKE_CACHE_REMOTE (HTTPS required)"));
        }
        let mut config = Self::new(url)?;
        if enabled(b"RCMAKE_CACHE_REMOTE_WRITE") {
            config.access = RemoteAccess::ReadWrite;
        }
        if env(EXPORT_TIMEOUT_SETTING.as_bytes()).is_some() {
            const INVALID_TIMEOUT: RemoteError = RemoteError::Setting(EXPORT_TIMEOUT_SETTING);
            let seconds = text(EXPORT_TIMEOUT_SETTING)?
                .parse()
                .map_err(|_| INVALID_TIMEOUT)?;
            config.export_timeout = Duration::from_secs(seconds);
            if seconds == 0 {
                return Err(INVALID_TIMEOUT);
            }
        }
        config.export_captured_output = enabled(b"RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT");
        config.curl_path = env(b"RCMAKE_CACHE_REMOTE_CURL").map(|s| path::to_native(&s));
        config.private_dir = env(b"RCMAKE_CACHE_REMOTE_PRIVATE_DIR").map(|s| path::to_native(&s));
        config.auth = match env(b"RCMAKE_CACHE_REMOTE_AUTH")
            .unwrap_or_default()
            .as_slice()
        {
            b"" | b"anonymous" => RemoteAuth::Anonymous,
            b"bearer" => RemoteAuth::Bearer,
            b"header" => RemoteAuth::Header(text("RCMAKE_CACHE_REMOTE_HEADER")?),
            b"s3" => RemoteAuth::S3(text("AWS_REGION")?),
            _ => return Err(RemoteError::Setting("RCMAKE_CACHE_REMOTE_AUTH")),
        };
        Ok(Some(config))
    }
}

/// Read the local pin and validate its trace before requesting a missing remote blob.
pub fn local_probe_pin<T: FromJson>(
    store: &Store<'_>,
    key: Digest,
) -> Result<Option<(ManifestEntry, Entry<T>)>, RemoteError> {
    let Some((pin, frame)) = export_entry(store, CacheEntryKind::Probe, key)? else {
        return Ok(None);
    };
    let entry = decode_entry(CacheEntryKind::Probe, key, &frame, Some(&pin))?;
    if crate::verify_trace(store.host, &entry.trace) != crate::VerifyOutcome::Hit {
        return Ok(None);
    }
    Ok(Some((pin, entry)))
}
fn resolve(
    host: &dyn Host,
    path: Option<&Path>,
    setting: &'static str,
    check: impl FnOnce(&Path, &rcmake_host::Metadata) -> bool,
) -> Result<PathBuf, RemoteError> {
    let invalid = RemoteError::Setting(setting);
    let path = path.filter(|p| p.is_absolute()).ok_or(invalid)?;
    let path = host.canonicalize(path).map_err(|_| invalid)?;
    host.metadata_fresh(&path, Follow::No)
        .ok()
        .filter(|meta| check(&path, meta))
        .map(|_| path)
        .ok_or(invalid)
}
fn valid_header(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !("host content-length transfer-encoding connection expect proxy-authorization proxy-connection trailer upgrade te"
            .split_ascii_whitespace().any(|reserved| name.eq_ignore_ascii_case(reserved)))
}
/// Safe reason for a cache miss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteError {
    /// Invalid configuration setting.
    Setting(&'static str),
    /// Safe fixed description of a local or remote failure.
    Failure(&'static str),
    /// The endpoint returned an unexpected HTTP status.
    HttpStatus(u16),
}
impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Setting(name) => write!(f, "Invalid or missing {name}."),
            Self::Failure(reason) => f.write_str(reason),
            Self::HttpStatus(status) => write!(f, "The remote cache returned HTTP {status}."),
        }
    }
}
impl std::error::Error for RemoteError {}
/// Optional remote cache scoped to one configure or export job.
pub struct RemoteStoreClient<'a> {
    host: &'a dyn Host,
    config: RemoteCacheConfig,
    curl: PathBuf,
    read_time_left: Duration,
    export_time_left: Duration,
    blob_bytes: usize,
    failures: u32,
    diagnostic: Option<RemoteError>,
    pipe_config: bool,
    /// Identity of the private directory after it passed the privacy checks.
    private_identity: Option<PrivateDir>,
}
impl<'a> RemoteStoreClient<'a> {
    /// Create a client with bounded process capture and verify its HTTPS and directory settings.
    pub fn new(host: &'a dyn Host, mut config: RemoteCacheConfig) -> Result<Self, RemoteError> {
        let host_path = config
            .endpoint
            .split_once("://")
            .is_some_and(|(_, rest)| rest.contains('/'));
        let needs_private_dir = host_path || !matches!(config.auth, RemoteAuth::Anonymous);
        if (needs_private_dir || config.access == RemoteAccess::ReadWrite)
            && !config.endpoint.starts_with("https://")
        {
            return Err(RemoteError::Setting("RCMAKE_CACHE_REMOTE (HTTPS required)"));
        }
        if config.export_timeout.is_zero() {
            return Err(RemoteError::Setting(EXPORT_TIMEOUT_SETTING));
        }
        let windows = host.system().sysname == "Windows";
        let default = if windows {
            "C:/Windows/System32/curl.exe"
        } else {
            "/usr/bin/curl"
        };
        let curl = config.curl_path.as_deref().or(Some(Path::new(default)));
        let curl = resolve(host, curl, "RCMAKE_CACHE_REMOTE_CURL", |_, m| {
            m.kind == FileKind::File && (windows || m.mode & 0o111 != 0)
        })?;
        if config.session_timeout.is_zero() {
            return Err(RemoteError::Setting("session_timeout"));
        }
        let mut private_identity = None;
        config.private_dir = if needs_private_dir && !windows {
            let dir = resolve(
                host,
                config.private_dir.as_deref(),
                PRIVATE_DIR_SETTING,
                |_, m| m.mode & 0o077 == 0,
            )?;
            private_identity = Some(
                verify_private_dir(host, &dir)
                    .map_err(|_| RemoteError::Setting(PRIVATE_DIR_SETTING))?,
            );
            Some(dir)
        } else {
            None
        };
        Ok(Self {
            private_identity,
            pipe_config: needs_private_dir && windows,
            host,
            curl,
            read_time_left: config.session_timeout,
            export_time_left: config.export_timeout,
            config,
            blob_bytes: SESSION_BLOB_BYTES,
            failures: 0,
            diagnostic: None,
        })
    }
    /// Return whether the private directory is still the private directory that passed the
    /// checks at client creation, with mode `0o700` or narrower.
    fn private_dir_unchanged(&self) -> bool {
        let (Some(dir), Some(identity)) =
            (self.config.private_dir.as_deref(), self.private_identity)
        else {
            return false;
        };
        // One fresh sample gives both the identity and the mode.
        identity
            .recheck(self.host, dir)
            .is_ok_and(|meta| meta.mode & 0o077 == 0)
    }
    /// Return whether the client has stopped sending requests for this configure.
    pub fn circuit_is_open(&self) -> bool {
        self.failures >= 2
    }
    /// Return the fallback diagnostic once. Treat failed operations as local probe misses.
    pub fn take_diagnostic(&mut self) -> Option<String> {
        self.diagnostic.take().map(|reason| {
            format!("RCMAKE_CACHE_REMOTE disabled. {reason} Configure will use local probes.")
        })
    }
    /// Read a probe pinned by a trusted manifest. An absent pin is a miss without I/O.
    /// Supply the verified local trace and check the payload and artifacts in `validate`.
    /// A read-only or verify store can reuse entries with local blobs.
    pub fn get_probe<T: FromJson>(
        &mut self,
        store: &Store<'_>,
        key: Digest,
        pin: Option<&ManifestEntry>,
        local_trace: &[InputStamp],
        validate: impl FnOnce(&Entry<T>) -> bool,
    ) -> Result<Option<Entry<T>>, RemoteError> {
        let Some(pin) =
            pin.filter(|pin| pin.identity() == (CacheEntryKind::Probe, key) && valid_record(pin))
        else {
            return Ok(None);
        };
        if !store.readable().unwrap_or(false) {
            return Ok(None);
        }
        self.attempt(false, |client, deadline| {
            let Some(encoded) = client.request(
                &format!("probe/v1/{key}.entry"),
                None,
                frame::disk_limit(FRAME_BYTES) as usize,
                deadline,
            )?
            else {
                return Ok(None);
            };
            let entry = decode_entry(pin.kind, pin.key, &encoded, Some(pin))?;
            if entry.trace.as_slice() != local_trace || !validate(&entry) {
                return Ok(None);
            }
            let budget = client.blob_bytes.min(PROBE_BLOB_BYTES);
            let mut remaining = budget;
            let ready = attach_blobs(store, pin, &encoded, &mut remaining, |digest, cap| {
                let limit = frame::disk_limit(cap) as usize;
                client.request(
                    &format!("blobs/sha256/{digest}.blob"),
                    None,
                    limit,
                    deadline,
                )
            });
            client.blob_bytes -= budget - remaining;
            Ok(ready?.then_some(entry))
        })
    }
    /// Publish a local probe and its blobs. Call from an export job outside configure.
    /// The remote access mode, local write mode, and capture export setting must all permit writes.
    pub fn put_probe(&mut self, store: &Store<'_>, key: Digest) -> Result<bool, RemoteError> {
        if !self.config.export_captured_output
            || self.config.access == RemoteAccess::ReadOnly
            || !store.writable()
        {
            return Ok(false);
        }
        self.attempt(true, |client, deadline| {
            let Some((pin, encoded)) =
                export_entry(store, CacheEntryKind::Probe, key).map_err(|_| LOCAL_STORE)?
            else {
                return Ok(None);
            };
            let mut remaining = PROBE_BLOB_BYTES;
            for digest in pin.blobs {
                let Some(blob) = read_frame(store, &store.blob_path(digest), remaining)? else {
                    return Ok(None);
                };
                decode_blob(&blob, digest, &mut remaining).map_err(|_| LOCAL_STORE)?;
                client.upload(
                    store,
                    &format!("blobs/sha256/{digest}.blob"),
                    &blob,
                    deadline,
                )?;
            }
            client.upload(store, &format!("probe/v1/{key}.entry"), &encoded, deadline)?;
            Ok(Some(()))
        })
        .map(|result| result.is_some())
    }
    fn attempt<T>(
        &mut self,
        export: bool,
        operation: impl FnOnce(&mut Self, Duration) -> Result<Option<T>, RemoteError>,
    ) -> Result<Option<T>, RemoteError> {
        if self.circuit_is_open() {
            return Ok(None);
        }
        let start = self.host.monotonic();
        let budget = if export {
            self.export_time_left
        } else {
            self.read_time_left
        };
        let mut result = operation(self, start.saturating_add(budget));
        let remaining = budget.saturating_sub(self.host.monotonic().saturating_sub(start));
        if export {
            self.export_time_left = remaining;
        } else {
            self.read_time_left = remaining;
        }
        // A local error uses time and leaves the remote failure count unchanged.
        if matches!(result, Err(LOCAL_STORE)) {
            return result;
        }
        if remaining.is_zero() {
            result = Err(BUDGET);
        }
        match &result {
            Ok(Some(_)) => self.failures = 0,
            Ok(None) => {}
            Err(RemoteError::Setting(_) | RemoteError::HttpStatus(401 | 403) | BUDGET) => {
                self.failures = 2
            }
            Err(_) => self.failures += 1,
        }
        if self.failures >= 2 {
            self.diagnostic = result.as_ref().err().copied();
        }
        result
    }
    fn upload(
        &self,
        store: &Store<'_>,
        suffix: &str,
        bytes: &[u8],
        deadline: Duration,
    ) -> Result<(), RemoteError> {
        let scratch = store.config.root.join("tmp");
        store.ensure_dir(&scratch).map_err(|_| LOCAL_STORE)?;
        let path = temporary_path(self.host, &scratch);
        let _cleanup = stage(self.host, &path, bytes).map_err(|_| LOCAL_STORE)?;
        self.request(suffix, Some((&path, bytes.len())), FRAME_BYTES, deadline)
            .map(|_| ())
    }
    fn request(
        &self,
        suffix: &str,
        upload: Option<(&Path, usize)>,
        limit: usize,
        deadline: Duration,
    ) -> FrameResult {
        let config = &self.config;
        // Allow one second of overhead plus time to upload at 128 KiB/s.
        let timeout = upload.map_or(Duration::from_secs(1), |(_, size)| {
            Duration::from_secs(1 + size.div_ceil(128 * 1024) as u64)
        });
        let time = deadline.saturating_sub(self.host.monotonic()).min(timeout);
        if time.is_zero() {
            return Err(BUDGET);
        }
        let mut args = parse_curl_flags(time, limit, upload.is_some());
        if let Some((path, _)) = upload {
            args.extend(["--upload-file".into(), path::from_native(path)]);
        }
        let url = format!("{}/{suffix}", config.endpoint);
        let scratch = config.private_dir.as_deref();
        let path = scratch.map(|dir| temporary_path(self.host, dir));
        let mut staged = None;
        let stdin = if config.private_dir.is_some() || self.pipe_config {
            let mut input = credentials(self.host, &config.auth, &mut args)?;
            append_config(&mut input, b"url", &[url.as_bytes()]);
            args.extend(["--config".into(), "-".into()]);
            if let Some(path) = &path {
                let error = RemoteError::Setting(PRIVATE_DIR_SETTING);
                // Check the directory again just before credentials go into it.
                if !self.private_dir_unchanged() {
                    return Err(error);
                }
                staged = Some(stage(self.host, path, &input).map_err(|_| error)?);
                Input::File(path.to_owned())
            } else {
                Input::Bytes(input)
            }
        } else {
            args.extend(["--url".into(), url.into()]);
            Input::Null
        };
        let _cleanup = staged;
        let env = "SystemRoot HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy SSL_CERT_FILE CURL_CA_BUNDLE"
            .split_ascii_whitespace()
            .filter_map(|key| self.host.env_get(key.as_bytes()).map(|value| (key.into(), value)))
            .collect();
        let time = time.min(deadline.saturating_sub(self.host.monotonic()));
        if time.is_zero() {
            return Err(BUDGET);
        }
        let program = path::from_native(&self.curl);
        let cwd = self
            .curl
            .parent()
            .ok_or(RemoteError::Setting("RCMAKE_CACHE_REMOTE_CURL"))?;
        let request = RunRequest {
            pipeline: vec![CommandSpec { program, args }],
            cwd: cwd.to_owned(),
            env,
            stdin,
            stdout: Output::Capture,
            stderr: Output::Null,
            timeout: Some(time),
            tee: false,
        };
        let result = self
            .host
            .run_bounded(&request, limit + 3)
            .map_err(|error| match error.kind {
                ErrorKind::TimedOut => TIMEOUT,
                ErrorKind::FileTooLarge => RESPONSE_LIMIT,
                _ => RemoteError::Failure("The host could not run curl or capture its response."),
            })?;
        match result.results.as_slice() {
            [ProcessResult::Exited(0)] => response(result.stdout, limit, upload.is_some()),
            [ProcessResult::TimedOut | ProcessResult::Exited(28)] => Err(TIMEOUT),
            [ProcessResult::Exited(63)] => Err(RESPONSE_LIMIT),
            [ProcessResult::SpawnFailed(_)] => {
                Err(RemoteError::Setting("RCMAKE_CACHE_REMOTE_CURL"))
            }
            result => Err(RemoteError::Failure(match result {
                [ProcessResult::Exited(35 | 51 | 58..=60 | 77 | 83 | 90 | 91)] => {
                    "Curl TLS verification failed."
                }
                [ProcessResult::Exited(5)] => {
                    "Curl could not resolve the proxy. Check HTTPS_PROXY and HTTP_PROXY."
                }
                [ProcessResult::Exited(6)] => "Curl could not resolve the cache host.",
                [ProcessResult::Exited(7)] => {
                    "Curl could not connect. Check the cache endpoint and HTTPS_PROXY, HTTP_PROXY, and NO_PROXY."
                }
                _ => "Curl could not complete the transfer.",
            })),
        }
    }
}
fn response(mut bytes: Vec<u8>, limit: usize, upload: bool) -> FrameResult {
    let error = RemoteError::Failure("Curl returned no valid HTTP status.");
    let tail = bytes.len().checked_sub(3).ok_or(error)?;
    let status = bytes
        .get(tail..)
        .filter(|b| b.iter().all(u8::is_ascii_digit))
        .and_then(|code| std::str::from_utf8(code).ok()?.parse::<u16>().ok())
        .ok_or(error)?;
    if tail > limit {
        return Err(RESPONSE_LIMIT);
    }
    bytes.truncate(tail);
    match (upload, status) {
        (false, 200) | (true, 200..=299) => Ok(Some(bytes)),
        (false, 404) => Ok(None),
        (_, 100..=599) => Err(RemoteError::HttpStatus(status)),
        _ => Err(error),
    }
}
fn parse_curl_flags(time: Duration, limit: usize, upload: bool) -> Vec<BString> {
    let duration = |time: Duration| format!("{:.6}", time.as_secs_f64().max(0.001));
    let protocol = if upload { "=https" } else { "=http,https" };
    let flags = "--disable --globoff --silent --write-out %{http_code} --proto";
    let mut args: Vec<_> = flags.split_ascii_whitespace().map(BString::from).collect();
    args.push(protocol.into());
    let connect_timeout = if upload {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(300)
    };
    for (flag, value) in [
        ("--connect-timeout", duration(time.min(connect_timeout))),
        ("--max-time", duration(time)),
        ("--max-filesize", limit.to_string()),
    ] {
        args.extend([BString::from(flag), value.into()]);
    }
    args
}
fn stage<'a>(
    host: &'a dyn Host,
    path: &'a Path,
    bytes: &[u8],
) -> rcmake_host::HostResult<StagingFile<'a>> {
    if let Err(error) = host.write(path, bytes, WriteMode::CreateNew) {
        if error.kind != ErrorKind::AlreadyExists {
            let _ = host.remove_file(path);
        }
        return Err(error);
    }
    Ok(StagingFile {
        host,
        path,
        armed: true,
    })
}
fn temporary_path(host: &dyn Host, dir: &Path) -> PathBuf {
    let now = host.now();
    let pid = host.system().pid;
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("remote-{pid}-{}-{}-{serial}", now.secs, now.nanos))
}
fn credentials(
    host: &dyn Host,
    auth: &RemoteAuth,
    args: &mut Vec<BString>,
) -> Result<Vec<u8>, RemoteError> {
    let secret = |key: &'static str| match host.env_get(key.as_bytes()) {
        Some(value)
            if !value.is_empty()
                && value.len() <= MAX_AUTH_BYTES
                && value.iter().all(|b| b.is_ascii_graphic() || *b == b' ') =>
        {
            Ok(value)
        }
        _ => Err(RemoteError::Setting(key)),
    };
    let mut config = Vec::new();
    match auth {
        RemoteAuth::Anonymous => {}
        RemoteAuth::Bearer | RemoteAuth::Header(_) => {
            let token = secret("RCMAKE_CACHE_REMOTE_TOKEN")?;
            let name = match auth {
                RemoteAuth::Header(name) if valid_header(name) => name.as_bytes(),
                RemoteAuth::Header(_) => {
                    return Err(RemoteError::Setting("RCMAKE_CACHE_REMOTE_HEADER"));
                }
                _ => b"Authorization",
            };
            let scheme = if matches!(auth, RemoteAuth::Bearer) {
                b"Bearer ".as_slice()
            } else {
                b""
            };
            append_config(&mut config, b"header", &[name, b": ", scheme, &token]);
        }
        RemoteAuth::S3(region) => {
            if region.is_empty()
                || region.len() > 64
                || !region
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(RemoteError::Setting("AWS_REGION"));
            }
            let access = secret("AWS_ACCESS_KEY_ID")?;
            let password = secret("AWS_SECRET_ACCESS_KEY")?;
            if access.contains(&b':') {
                return Err(RemoteError::Setting("AWS_ACCESS_KEY_ID"));
            }
            append_config(&mut config, b"user", &[&access, b":", &password]);
            if host
                .env_get(b"AWS_SESSION_TOKEN")
                .is_some_and(|value| !value.is_empty())
            {
                let token = secret("AWS_SESSION_TOKEN")?;
                append_config(&mut config, b"header", &[b"x-amz-security-token: ", &token]);
            }
            args.extend(["--aws-sigv4".into(), format!("aws:amz:{region}:s3").into()]);
        }
    }
    Ok(config)
}
fn append_config(config: &mut Vec<u8>, option: &[u8], parts: &[&[u8]]) {
    config.extend_from_slice(option);
    config.extend_from_slice(b" = \"");
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        if matches!(byte, b'\\' | b'"') {
            config.push(b'\\');
        }
        config.push(byte);
    }
    config.extend_from_slice(b"\"\n");
}

#[test]
fn remote_private_dir_requires_the_same_owner() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
    let curl = root.join("bin/curl");
    let private = root.join("private");
    let host = rcmake_host::PureHost::new(root.to_owned(), root.join("bin/rcmake"));
    host.set_effective_uid(Some(1000))?;
    host.seed_file(&curl, b"")?;
    host.set_permissions(&curl, 0o700)?;
    host.create_private_dir(&private)?;
    let mut config = RemoteCacheConfig::new("https://cache.invalid")?;
    config.auth = RemoteAuth::Bearer;
    config.curl_path = Some(curl);
    config.private_dir = Some(private.clone());
    let client = RemoteStoreClient::new(&host, config.clone())?;
    assert!(client.private_dir_unchanged());
    // A directory that replaces the checked one must not receive credentials.
    host.rename(&private, &root.join("private-old"))?;
    host.create_private_dir(&private)?;
    assert!(!client.private_dir_unchanged());

    host.set_owner(&private, Some(1001))?;
    assert_eq!(
        RemoteStoreClient::new(&host, config).err(),
        Some(RemoteError::Setting("RCMAKE_CACHE_REMOTE_PRIVATE_DIR"))
    );
    Ok(())
}

#[test]
fn local_errors_charge_the_active_budget() -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
    let curl = root.join("bin/curl");
    let host = rcmake_host::PureHost::new(root.to_owned(), root.join("bin/rcmake"));
    host.seed_file(&curl, b"")?;
    host.set_permissions(&curl, 0o700)?;
    let mut config = RemoteCacheConfig::new("https://cache.invalid")?;
    config.curl_path = Some(curl);
    config.session_timeout = Duration::from_secs(2);
    config.export_timeout = Duration::from_secs(3);
    let mut client = RemoteStoreClient::new(&host, config)?;
    assert_eq!(
        client.attempt::<()>(false, |_, _| Err(TIMEOUT)),
        Err(TIMEOUT)
    );
    let fail_locally = |client: &mut RemoteStoreClient<'_>, _| {
        client.host.sleep(Duration::from_secs(2));
        Err(LOCAL_STORE)
    };

    assert_eq!(client.attempt::<()>(true, fail_locally), Err(LOCAL_STORE));
    assert_eq!(client.export_time_left, Duration::from_secs(1));
    assert_eq!(client.read_time_left, Duration::from_secs(2));
    assert_eq!(client.attempt::<()>(false, fail_locally), Err(LOCAL_STORE));
    assert_eq!(client.read_time_left, Duration::ZERO);
    assert_eq!(client.export_time_left, Duration::from_secs(1));
    assert_eq!(client.failures, 1);
    assert!(client.take_diagnostic().is_none());
    Ok(())
}
