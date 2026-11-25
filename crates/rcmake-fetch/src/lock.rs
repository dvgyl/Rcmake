use std::{
    borrow::Cow,
    collections::BTreeMap,
    fmt,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use rcmake_host::{Host, ReadRange, path::to_native};
use rcmake_store::Digest;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{Error, MapAccess, Visitor},
};

use crate::{
    ArchiveCache, ArchiveResult, ContentHash, FetchError, GitMirror, LockMode,
    archive::confirmed,
    atomic_write,
    error::{has_url_suffix, sanitized_url},
    git::is_commit,
    invalid,
};

const FILE_NAME: &str = "rcmake-fetch.lock";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_URLS: usize = 32;

fn sha256_name() -> String {
    "SHA256".into()
}

fn is_sha256(name: &str) -> bool {
    name == "SHA256"
}

/// A locked archive and its source URLs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlEntry {
    /// Primary URL followed by fallback URLs in declaration order.
    pub urls: Vec<String>,
    /// SHA-256 content digest.
    pub sha256: Digest,
    /// Lockfile digest algorithm. Version 1 accepts only `SHA256`.
    #[serde(default = "sha256_name", skip_serializing_if = "is_sha256")]
    pub algo: String,
    /// Size of the archive in bytes.
    pub size: u64,
}

impl UrlEntry {
    /// Record the identity of downloaded archive bytes.
    pub fn from_bytes(urls: Vec<String>, bytes: &[u8]) -> Self {
        Self {
            urls,
            sha256: Digest::hash(bytes),
            algo: sha256_name(),
            size: bytes.len() as u64,
        }
    }
}

/// A locked Git source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitEntry {
    /// Git repository URL.
    pub repository: String,
    /// Tag, branch, or commit specified by the project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Full 40-digit Git commit ID.
    pub commit: String,
}

impl GitEntry {
    /// Compare repository spellings with the same Git transport identity.
    pub fn same_repository(left: &str, right: &str) -> bool {
        fn identity(repository: &str) -> String {
            let (scheme, host, path) = source_parts(repository);
            let host = host.to_ascii_lowercase();
            match scheme {
                "" => repository.into(),
                "ssh" if !repository.contains("://") && !path.starts_with('/') => {
                    format!("{host}:{path}")
                }
                _ => format!("{scheme}://{host}{path}"),
            }
        }
        identity(left) == identity(right)
    }
}

/// A dependency in a version 1 lockfile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LockEntry {
    /// Archive source.
    Url(UrlEntry),
    /// Git source.
    Git(GitEntry),
}

impl LockEntry {
    /// Check whether the source can be stored as a public lockfile identity.
    pub fn is_portable_source(source: &str) -> bool {
        if !valid_text(source) || source.starts_with('-') || Self::source_has_credentials(source) {
            return false;
        }
        let (scheme, host, path) = source_parts(source);
        let valid = match scheme {
            "" => !path.contains(':') || Path::new(path).is_absolute(),
            "file" => matches!(host, "" | "localhost"),
            "ssh" => valid_host(&percent_decode(ssh_host(host))),
            "http" | "https" | "ftp" | "ftps" | "git" => valid_host(&percent_decode(host)),
            _ => false,
        };
        valid && (scheme.is_empty() || !path.contains('\\'))
    }

    /// Check for user info, URL suffixes, and recognized credential forms.
    pub fn source_has_credentials(source: &str) -> bool {
        let (scheme, authority, path) = source_parts(source);
        let host = if scheme == "ssh" {
            ssh_host(authority)
        } else {
            authority
        };
        let decoded = percent_decode(host);
        decoded.contains(['@', '/', '\\'])
            || decoded.matches(':').count() > host.matches(':').count()
            || decoded.split(':').any(token_segment)
            || (!scheme.is_empty() && path.contains(['?', '#']))
            || percent_decode(path).split(['/', '\\', ';']).any(|segment| {
                segment
                    .split_once('=')
                    .is_some_and(|(name, _)| credential_key(name))
                    || segment.split('=').any(token_segment)
            })
    }

    /// Return the scheme and host for diagnostics, or a placeholder if the host is invalid.
    pub fn source_endpoint(source: &str) -> String {
        let (scheme, authority, _) = source_parts(source);
        let host = authority.rsplit('@').next().unwrap_or_default();
        if matches!(scheme, "https" | "http" | "ftp" | "ftps" | "ssh" | "git")
            && valid_host(host)
            && !host.split(':').any(token_segment)
        {
            format!("{scheme}://{host}")
        } else {
            "<source>".into()
        }
    }

    fn redact_legacy_identity(&mut self) {
        // Older version 1 lockfiles may contain transport credentials.
        match self {
            Self::Url(entry) => {
                for url in &mut entry.urls {
                    if valid_text(url) {
                        *url = transport_identity(url);
                    }
                }
            }
            Self::Git(entry) if valid_text(&entry.repository) => {
                entry.repository = transport_identity(&entry.repository);
            }
            Self::Git(_) => {}
        }
    }

    fn validate(&self) -> bool {
        match self {
            Self::Url(entry) => {
                entry.algo == "SHA256"
                    && !entry.urls.is_empty()
                    && entry.urls.len() <= MAX_URLS
                    && entry.urls.iter().all(|url| Self::is_portable_source(url))
            }
            Self::Git(entry) => {
                Self::is_portable_source(&entry.repository)
                    && entry.tag.as_deref().is_none_or(valid_text)
                    && is_commit(&entry.commit)
            }
        }
    }
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control)
}

fn source_parts(source: &str) -> (&str, &str, &str) {
    if let Some((scheme, rest)) = source.split_once("://") {
        let (host, path) = rest.split_at(rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len()));
        return (scheme, host, path);
    }
    if let Some((host, path)) = source.split_once(':')
        && !host.contains(['/', '\\'])
        && host.len() > 1
        && !path.starts_with(':')
        && !path.is_empty()
    {
        return ("ssh", host, path);
    }
    ("", "", source)
}

fn ssh_host(host: &str) -> &str {
    host.strip_prefix("git@").unwrap_or(host)
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
}

fn percent_decode(value: &str) -> Cow<'_, str> {
    let Some((head, tail)) = value.split_once('%') else {
        return Cow::Borrowed(value);
    };
    let mut decoded = String::from(head);
    for part in tail.split('%') {
        if let Some((hex, rest)) = part.split_at_checked(2)
            && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(char::from(byte));
            decoded.push_str(rest);
        } else {
            decoded.push('%');
            decoded.push_str(part);
        }
    }
    Cow::Owned(decoded)
}

fn credential_key(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "token"
            | "access_token"
            | "auth_token"
            | "private_token"
            | "password"
            | "passwd"
            | "secret"
            | "credential"
            | "authorization"
            | "api-key"
            | "api_key"
            | "apikey"
            | "signature"
            | "cookie"
    )
}

fn token_segment(segment: &str) -> bool {
    let bare = segment.split_once('.').map_or(segment, |(head, _)| head);
    for (prefix, len, extra) in [
        ("ghp_", 36, ""),
        ("gho_", 36, ""),
        ("ghu_", 36, ""),
        ("ghs_", 36, ""),
        ("ghr_", 36, ""),
        ("github_pat_", 82, "_"),
        ("glpat-", 20, "-_"),
    ] {
        if let Some(token) = bare.strip_prefix(prefix) {
            return token.len() == len
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || extra.as_bytes().contains(&b));
        }
    }
    let base64url = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    };
    let mut parts = segment.split('.');
    matches!((parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(header), Some(payload), Some(signature), None)
            if header.starts_with("eyJ") && base64url(header) && base64url(payload) && base64url(signature))
}

fn transport_identity(source: &str) -> String {
    // Older callers can supply user info for transport. Validate the public identity again.
    if LockEntry::is_portable_source(source) {
        source.to_owned()
    } else {
        sanitized_url(source)
    }
}

fn portable_urls(urls: &[&str]) -> Result<Vec<String>, &'static str> {
    if urls.is_empty() || urls.len() > MAX_URLS || urls.iter().any(|url| !valid_text(url)) {
        return Err("Supply a valid ordered URL declaration.");
    }
    let urls = urls
        .iter()
        .map(|url| transport_identity(url))
        .collect::<Vec<_>>();
    if urls.iter().all(|url| LockEntry::is_portable_source(url)) {
        Ok(urls)
    } else {
        Err("Supply a portable URL identity and pass credentials through the transport.")
    }
}

fn valid_name(name: &str) -> bool {
    name.len() <= 256 && valid_text(name) && name == name.to_ascii_lowercase()
}

/// Version 1 project lockfile. Dependency names are ASCII case-insensitive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockFile {
    version: u32,
    #[serde(deserialize_with = "read_entries")]
    entries: BTreeMap<String, LockEntry>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: 1,
            entries: BTreeMap::new(),
        }
    }
}

impl LockFile {
    /// Parse bounded, versioned JSON and reject unknown or duplicate fields.
    pub fn parse(bytes: &[u8]) -> Result<Self, FetchError> {
        if bytes.len() > MAX_BYTES {
            return Err(invalid("Fetch lockfile exceeds the size limit."));
        }
        let file: Self = serde_json::from_slice(bytes).map_err(|error| {
            invalid(format!(
                "Invalid fetch lockfile at line {}, column {} ({:?}).",
                error.line(),
                error.column(),
                error.classify(),
            ))
        })?;
        if file.version != 1 {
            return Err(invalid("Unsupported fetch lockfile version."));
        }
        Ok(file)
    }

    /// Serialize with sorted keys and a final line feed.
    pub fn to_json(&self) -> Result<Vec<u8>, FetchError> {
        let result = serde_json::to_value(self).and_then(|value| serde_json::to_vec_pretty(&value));
        let mut bytes = result.map_err(|error| invalid(error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_BYTES {
            return Err(invalid("Fetch lockfile exceeds the size limit."));
        }
        Ok(bytes)
    }

    /// Look up a dependency by its declaration name.
    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.entries.get(&name.to_ascii_lowercase())
    }

    /// Insert or replace a dependency after validation.
    pub fn insert(&mut self, name: &str, entry: LockEntry) -> Result<(), FetchError> {
        let name = name.to_ascii_lowercase();
        if !valid_name(&name) || !entry.validate() {
            return Err(invalid("Invalid fetch lock entry."));
        }
        if self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(&name) {
            return Err(invalid("Fetch lockfile exceeds the entry limit."));
        }
        self.entries.insert(name, entry);
        Ok(())
    }
}

fn read_entries<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, LockEntry>, D::Error> {
    struct Entries;

    impl<'de> Visitor<'de> for Entries {
        type Value = BTreeMap<String, LockEntry>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of named fetch dependencies")
        }

        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut file = LockFile::default();
            while let Some(name) = map.next_key::<String>()? {
                let name = name.to_ascii_lowercase();
                if file.get(&name).is_some() {
                    return Err(M::Error::custom("Duplicate fetch dependency name."));
                }
                let mut entry: LockEntry = map.next_value()?;
                entry.redact_legacy_identity();
                file.insert(&name, entry).map_err(M::Error::custom)?;
            }
            Ok(file.entries)
        }
    }

    deserializer.deserialize_map(Entries)
}

/// Lockfile state for one configure. Call [Self::commit] after a successful configure.
pub struct LockSession<'a> {
    host: &'a dyn Host,
    mode: LockMode,
    path: PathBuf,
    original: LockFile,
    updates: LockFile,
}

impl<'a> LockSession<'a> {
    /// Read the lockfile at a project path or the host's `RCMAKE_FETCH_LOCK_FILE` override.
    pub fn open(host: &'a dyn Host, source_dir: &Path, mode: LockMode) -> Result<Self, FetchError> {
        let path = source_dir.join(
            host.env_get(b"RCMAKE_FETCH_LOCK_FILE")
                .filter(|value| !value.is_empty())
                .map_or_else(|| PathBuf::from(FILE_NAME), |value| to_native(&value)),
        );
        let original = match mode {
            LockMode::Off => LockFile::default(),
            LockMode::Frozen => read(host, &path)?.ok_or_else(|| FetchError::Lock {
                path: path.clone(),
                dependency: None,
                detail: "Frozen mode requires this file.".into(),
            })?,
            _ => read(host, &path)?.unwrap_or_default(),
        };
        Ok(Self {
            host,
            mode,
            path,
            original,
            updates: LockFile::default(),
        })
    }

    /// Return the pinned commit for a matching declaration in frozen mode.
    pub fn git_pin(
        &self,
        name: &str,
        repository: &str,
        tag: Option<&str>,
    ) -> Result<Option<&str>, FetchError> {
        if self.mode != LockMode::Frozen {
            return Ok(None);
        }
        let identity = transport_identity(repository);
        let endpoint = LockEntry::source_endpoint(repository);
        match self.original.get(name) {
            Some(LockEntry::Git(entry))
                if valid_text(repository)
                    && !has_url_suffix(repository)
                    && LockEntry::is_portable_source(&identity)
                    && (entry.repository == identity
                        || entry.repository == sanitized_url(&identity))
                    && entry.tag.as_deref() == tag =>
            {
                Ok(Some(&entry.commit))
            }
            Some(LockEntry::Git(entry)) => {
                let locked = LockEntry::source_endpoint(&entry.repository);
                Err(self.lock_error(name, format!(
                    "The Git repository or tag at {endpoint} differs from the lock entry at {locked}."
                )))
            }
            _ => Err(self.lock_error(
                name,
                format!("No Git entry matches the source at {endpoint}."),
            )),
        }
    }

    /// Record the checked-out Git commit in update mode.
    pub fn record_git(
        &mut self,
        mirror: &GitMirror<'_>,
        name: &str,
        repository: &str,
        tag: Option<&str>,
        source_dir: &Path,
    ) -> Result<(), FetchError> {
        if self.mode == LockMode::Update {
            let identity = transport_identity(repository);
            if !valid_text(repository)
                || has_url_suffix(repository)
                || !LockEntry::is_portable_source(&identity)
            {
                return Err(self.lock_error(
                    name,
                    "Supply a portable repository URL. Pass credentials through Git.",
                ));
            }
            self.updates.insert(
                name,
                LockEntry::Git(GitEntry {
                    repository: identity,
                    tag: tag.map(str::to_owned),
                    commit: mirror.head(source_dir)?,
                }),
            )?;
        }
        Ok(())
    }

    /// Fetch one archive URL with the current lock mode.
    ///
    /// Auto mode returns a warning with successful bytes when a lock hash differs.
    /// Update mode records the result. Frozen mode rejects differences.
    pub fn fetch_archive(
        &mut self,
        cache: &ArchiveCache<'_>,
        name: &str,
        url: &str,
        expected: Option<&ContentHash>,
    ) -> Result<(ArchiveResult, Option<FetchError>), FetchError> {
        self.fetch_archive_with(cache, name, url, expected, || cache.download(url))
    }

    /// Apply the lock mode with a caller-provided download operation for one URL.
    pub fn fetch_archive_with(
        &mut self,
        cache: &ArchiveCache<'_>,
        name: &str,
        url: &str,
        expected: Option<&ContentHash>,
        download: impl FnOnce() -> Result<Vec<u8>, FetchError>,
    ) -> Result<(ArchiveResult, Option<FetchError>), FetchError> {
        let mut download = Some(download);
        self.fetch_archive_urls_with(cache, name, &[url], expected, |_| {
            let run = download
                .take()
                .ok_or_else(|| invalid("The transport has already run."))?;
            run()
        })
    }

    /// Fetch the ordered URL declaration with the built-in curl transport.
    pub fn fetch_archive_urls(
        &mut self,
        cache: &ArchiveCache<'_>,
        name: &str,
        urls: &[&str],
        expected: Option<&ContentHash>,
    ) -> Result<(ArchiveResult, Option<FetchError>), FetchError> {
        self.fetch_archive_urls_with(cache, name, urls, expected, |url| cache.download(url))
    }

    /// Apply the lock mode to the complete ordered URL declaration.
    ///
    /// Frozen mode checks the declaration before cache lookup or transport.
    /// On a cache miss, the transport receives each original URL in order
    /// until an endpoint returns verified bytes. Supply the project's transfer
    /// settings in the transport closure.
    pub fn fetch_archive_urls_with(
        &mut self,
        cache: &ArchiveCache<'_>,
        name: &str,
        urls: &[&str],
        expected: Option<&ContentHash>,
        download: impl FnMut(&str) -> Result<Vec<u8>, FetchError>,
    ) -> Result<(ArchiveResult, Option<FetchError>), FetchError> {
        let identities = portable_urls(urls).map_err(|reason| self.lock_error(name, reason))?;
        if matches!(self.mode, LockMode::Update | LockMode::Frozen)
            && urls.iter().any(|url| has_url_suffix(url))
        {
            return Err(self.lock_error(name,
                "Use URLs without queries or fragments in a fetch lock entry. Pass credentials through the transport."
            ));
        }
        let lock = match self.original.get(name) {
            Some(LockEntry::Url(entry)) if entry.urls == identities => Some(entry),
            Some(LockEntry::Url(entry)) if self.mode == LockMode::Frozen => {
                return Err(self.lock_error(
                    name,
                    format!(
                        "The declared URLs at {:?} differ from the locked URLs at {:?}.",
                        urls.iter()
                            .map(|url| LockEntry::source_endpoint(url))
                            .collect::<Vec<_>>(),
                        entry
                            .urls
                            .iter()
                            .map(|url| LockEntry::source_endpoint(url))
                            .collect::<Vec<_>>(),
                    ),
                ));
            }
            _ if self.mode == LockMode::Frozen => {
                return Err(self.lock_error(
                    name,
                    format!(
                        "No archive entry matches the declared URLs at {:?}.",
                        urls.iter()
                            .map(|url| LockEntry::source_endpoint(url))
                            .collect::<Vec<_>>(),
                    ),
                ));
            }
            _ => None,
        };

        let pin = lock.map(|entry| ContentHash::sha256(entry.sha256));
        let mut hashes = Vec::new();
        let mut size = None;
        if self.mode == LockMode::Frozen
            || (self.mode == LockMode::Auto && expected.is_none() && cache.is_cache_only())
        {
            hashes.extend(pin.as_ref());
            size = lock.map(|entry| entry.size);
        }
        hashes.extend(expected);
        let mut context = cache.context(urls, &hashes);
        context.dependency = Some(name.into());
        if let Some(pin) = &pin {
            context.lockfile = Some(self.path.clone());
            if !context.expected.contains(&pin.identity()) && expected.is_none() {
                context.expected.push(pin.identity());
            }
            if expected.is_none() {
                context.recovery = "Check the cache and mirrors. Restore the locked content or run --fetch-lock=update if the change is intentional.";
            }
        }
        let mut result = cache
            .fetch_candidates(urls, &hashes, size, download)
            .map_err(|cause| context.clone().error(cause))?;
        let warning = if self.mode == LockMode::Auto && expected.is_none() {
            pin.as_ref()
                .and_then(|pin| pin.verify(&result.bytes).err())
                .or_else(|| lock.and_then(|entry| verify_size(entry, &result.bytes).err()))
                .map(|cause| context.clone().error(cause))
        } else {
            None
        };
        if self.mode == LockMode::Update {
            let entry = UrlEntry::from_bytes(identities, &result.bytes);
            if expected.is_none() {
                match cache
                    .insert_verified(urls, &[&ContentHash::sha256(entry.sha256)], &result.bytes)
                    .and_then(confirmed)
                {
                    Ok(()) => result.offline_seeded = true,
                    Err(error) => result.cache_warning = Some(error),
                }
            }
            self.updates.insert(name, LockEntry::Url(entry))?;
        }
        result.cache_warning = result.cache_warning.map(|cause| context.error(cause));
        Ok((result, warning))
    }

    fn lock_error(&self, name: &str, detail: impl Into<String>) -> FetchError {
        FetchError::Lock {
            path: self.path.clone(),
            dependency: Some(name.into()),
            detail: detail.into(),
        }
    }

    /// Stage a validated update that was populated outside this crate.
    pub fn record(&mut self, name: &str, entry: LockEntry) -> Result<(), FetchError> {
        if self.mode == LockMode::Update {
            self.updates.insert(name, entry)?;
        }
        Ok(())
    }

    /// Publish recorded entries atomically and preserve other concurrent updates.
    pub fn commit(self) -> Result<(), FetchError> {
        if self.mode != LockMode::Update || self.updates.entries.is_empty() {
            return Ok(());
        }
        let lock = self.path.with_added_extension("guard");
        let _guard = self.host.lock_file(&lock, Some(Duration::from_secs(10)))?;
        let mut current = read(self.host, &self.path)?.unwrap_or_default();
        let mut changed = false;
        for (name, entry) in self.updates.entries {
            let committed = current.get(&name);
            if committed == Some(&entry) {
                continue;
            }
            if committed != self.original.get(&name) {
                return Err(FetchError::LockConflict {
                    path: self.path,
                    dependency: name,
                });
            }
            current.insert(&name, entry)?;
            changed = true;
        }
        if changed {
            atomic_write(self.host, &self.path, &current.to_json()?)?;
        }
        Ok(())
    }
}

fn verify_size(entry: &UrlEntry, bytes: &[u8]) -> Result<(), FetchError> {
    if bytes.len() as u64 != entry.size {
        return Err(invalid("Archive size differs from the fetch lock entry."));
    }
    Ok(())
}

fn read(host: &dyn Host, path: &Path) -> Result<Option<LockFile>, FetchError> {
    match host.read(
        path,
        ReadRange::All {
            limit: MAX_BYTES as u64,
        },
    ) {
        Ok(bytes) => LockFile::parse(&bytes)
            .map(Some)
            .map_err(|error| FetchError::Lock {
                path: path.to_owned(),
                dependency: None,
                detail: error.to_string(),
            }),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(FetchError::LockRead {
            path: path.to_owned(),
            cause,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;
    use rcmake_store::{Store, StoreConfig};

    const COMMIT: &str = "40626af88bd7df9a5fb80be7b25ac85b122d6c21";

    #[test]
    fn lock_keeps_public_sources_and_rejects_credential_paths() -> Result<(), FetchError> {
        let mut lock = LockFile::default();
        let public = vec![
            "https://github.com/huggingface/tokenizers".into(),
            "https://gitlab.gnome.org/GNOME/libsecret.git".into(),
            format!("https://codeload.github.com/fmtlib/fmt/tar.gz/{COMMIT}"),
            "https://example.invalid/release/jdk-17.0.16%2B8".into(),
        ];
        let entry = LockEntry::Url(UrlEntry::from_bytes(public, b"archive"));
        lock.insert("public", entry)?;
        assert_eq!(LockFile::parse(&lock.to_json()?)?, lock);

        let token = format!(
            "https://example.invalid/download/ghp_{}.tar.gz",
            "A".repeat(36)
        );
        for source in [
            "https://example.invalid/download/access_token%3Dprivate/archive.tar.gz",
            "https://example.invalid/100%/download/%67lpat-0123456789abcdefghij",
            token.as_str(),
        ] {
            assert!(LockEntry::source_has_credentials(source));
            assert!(!LockEntry::is_portable_source(source));
            assert_eq!(
                LockEntry::source_endpoint(source),
                "https://example.invalid"
            );
            let archive = LockEntry::Url(UrlEntry::from_bytes(vec![source.into()], b"archive"));
            let git = LockEntry::Git(GitEntry {
                repository: source.into(),
                tag: None,
                commit: COMMIT.into(),
            });
            assert!(lock.insert("private", archive).is_err());
            assert!(lock.insert("private", git).is_err());
        }
        assert!(lock.get("private").is_none());
        Ok(())
    }

    #[test]
    fn lock_update_rejects_credential_paths_before_download() -> Result<(), FetchError> {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(root.clone(), root.join("tools").join("rcmake"));
        let store = Store::new(&host, StoreConfig::new(root.join("cache")))?;
        let cache = ArchiveCache::new(&host, &store);
        let mut lock = LockSession::open(&host, &root, LockMode::Update)?;
        let mut called = false;
        let source = "https://example.invalid/download/access_token%3Dprivate/archive.tar.gz";
        let result = lock.fetch_archive_with(&cache, "dep", source, None, || {
            called = true;
            Ok(b"archive".to_vec())
        });
        assert!(!called);
        assert!(
            matches!(result, Err(FetchError::Lock { detail, .. }) if !detail.contains("private"))
        );
        Ok(())
    }
}
