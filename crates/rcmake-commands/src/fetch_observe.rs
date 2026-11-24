// SPDX-License-Identifier: MIT OR Apache-2.0

//! Observe FetchContent declarations and match transfers from bundled population code.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_eval::{FetchLockMode, Services};
use rcmake_expr::ExpandedArg;
use rcmake_host::Host;

#[cfg(all(feature = "native", any(unix, windows)))]
use rcmake_fetch::{
    ArchiveCache, ArchiveResult, ContentHash, FetchError, FetchPool, GitClone, LockEntry, LockFile,
    LockMode, LockSession, PrefetchSource, Ticket, UrlEntry, classify_git_checkout,
    classify_git_clone,
};

/// The source selected by a FetchContent declaration.
#[derive(Clone, Eq, PartialEq)]
pub enum DeclaredSource {
    /// One or more archive mirrors.
    Archive {
        /// Full ordered list of download URLs.
        urls: Vec<String>,
        /// Original URL_HASH or equivalent URL_MD5, when supplied.
        hash: Option<String>,
    },
    /// A Git repository and its checkout settings.
    Git {
        /// Repository as supplied to FetchContent_Declare.
        repository: String,
        /// Requested tag, branch, or full commit ID.
        tag: Option<String>,
        /// Whether the declaration requests a shallow clone.
        shallow: bool,
        /// An absent list means the standard template may update all submodules.
        submodules: Option<Vec<BString>>,
    },
    /// A local, custom, or other source with no standard transfer bridge.
    Other,
}

/// The first declaration of one dependency in a configure.
#[derive(Clone, Eq, PartialEq)]
pub struct DeclaredFetch {
    /// ASCII-lowercased dependency name.
    pub name: String,
    /// Declared source.
    pub source: DeclaredSource,
    /// Whether DOWNLOAD_COMMAND occurs, including an explicitly empty command.
    pub custom_download: bool,
    /// Nonempty SOURCE_DIR specified in the declaration.
    pub source_dir: Option<BString>,
    #[cfg(all(feature = "native", any(unix, windows)))]
    transfer_settings: bool,
    custom_update: bool,
}

impl DeclaredFetch {
    /// Return the explicit Git pin if the tag is a full commit ID.
    pub fn git_commit(&self) -> Option<&str> {
        let DeclaredSource::Git { tag: Some(tag), .. } = &self.source else {
            return None;
        };
        (tag.len() == 40 && tag.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(tag)
    }

    fn supported(&self) -> bool {
        self.source_dir.is_none() && !self.custom_download && !self.custom_update
    }
}

/// Parse the expanded arguments of a bundled FetchContent_Declare call.
///
/// Return no declaration for a malformed name, source, or hash. Strings that
/// reach transport are kept intact here. Use [BlockedFetch] for diagnostics.
pub fn parse_fetchcontent_declare(args: &[ExpandedArg]) -> Option<DeclaredFetch> {
    let (name, args) = args.split_first()?;
    let name = text(&name.value)?.to_ascii_lowercase();
    if name.len() > 256 {
        return None;
    }

    let mut options = BTreeMap::<&[u8], Vec<&BString>>::new();
    let mut key = None;
    for arg in args {
        let value = &arg.value;
        if value == "FIND_PACKAGE_ARGS" {
            break;
        }
        if is_option(value) {
            key = Some(value.as_slice());
            options.entry(value).or_default();
        } else if let Some(key) = key {
            options.entry(key).or_default().push(value);
        }
    }

    let one = |key: &[u8]| match options.get(key)?.as_slice() {
        [value] => Some(*value),
        _ => None,
    };
    let custom_download = options.contains_key(b"DOWNLOAD_COMMAND".as_slice());
    let custom_update = options.contains_key(b"UPDATE_COMMAND".as_slice());
    let archive = options.get(b"URL".as_slice());
    let git = options.get(b"GIT_REPOSITORY".as_slice());
    let other_vcs = "SVN_REPOSITORY HG_REPOSITORY CVS_REPOSITORY"
        .split_ascii_whitespace()
        .any(|key| options.contains_key(key.as_bytes()));
    let source = match (archive, git, other_vcs) {
        (Some(urls), None, false) => {
            let urls: Vec<_> = urls
                .iter()
                .map(|url| text(url).map(str::to_owned))
                .collect::<Option<_>>()?;
            if urls.is_empty() {
                return None;
            }
            let hash = if options.contains_key(b"URL_HASH".as_slice()) {
                Some(text(one(b"URL_HASH")?)?.to_owned())
            } else if options.contains_key(b"URL_MD5".as_slice()) {
                Some(format!("MD5={}", text(one(b"URL_MD5")?)?))
            } else {
                None
            };
            if hash
                .as_deref()
                .is_some_and(|spec| parse_hash(spec).is_none())
            {
                return None;
            }
            DeclaredSource::Archive { urls, hash }
        }
        (None, Some(_), false) => DeclaredSource::Git {
            repository: text(one(b"GIT_REPOSITORY")?)?.into(),
            tag: one(b"GIT_TAG").and_then(|tag| text(tag)).map(str::to_owned),
            shallow: one(b"GIT_SHALLOW").is_some_and(|value| !rcmake_base::truth::is_off(value)),
            submodules: options.get(b"GIT_SUBMODULES".as_slice()).map(|values| {
                values
                    .iter()
                    .filter(|value| !value.is_empty())
                    .map(|value| (*value).clone())
                    .collect()
            }),
        },
        _ => DeclaredSource::Other,
    };
    #[cfg(all(feature = "native", any(unix, windows)))]
    let transfer_settings = concat!(
        "TIMEOUT INACTIVITY_TIMEOUT HTTP_USERNAME HTTP_PASSWORD HTTP_HEADER TLS_VERSION ",
        "TLS_VERIFY TLS_CAINFO NETRC NETRC_FILE GIT_CONFIG GIT_SHALLOW GIT_REMOTE_NAME",
    )
    .split_ascii_whitespace()
    .any(|key| options.contains_key(key.as_bytes()));
    Some(DeclaredFetch {
        name,
        source,
        custom_download,
        source_dir: one(b"SOURCE_DIR").filter(|path| !path.is_empty()).cloned(),
        #[cfg(all(feature = "native", any(unix, windows)))]
        transfer_settings,
        custom_update,
    })
}

fn text(value: &[u8]) -> Option<&str> {
    let value = std::str::from_utf8(value).ok()?;
    (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
}

fn parse_hash(spec: &str) -> Option<(HashAlgo, &str)> {
    let (name, hex) = spec.split_once('=')?;
    let algorithm = HashAlgo::parse(name.as_bytes())?;
    (hex.len() == algorithm.hex_digest(b"").len() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some((algorithm, hex))
}

fn is_option(value: &[u8]) -> bool {
    // Keep this list in sync with the bundled ExternalProject option parser.
    const KEYWORDS: &str = concat!(
        "PREFIX TMP_DIR STAMP_DIR LOG_DIR DOWNLOAD_DIR SOURCE_DIR BINARY_DIR INSTALL_DIR SUBBUILD_DIR ",
        "DOWNLOAD_COMMAND URL URL_HASH URL_MD5 DOWNLOAD_NAME DOWNLOAD_EXTRACT_TIMESTAMP ",
        "DOWNLOAD_NO_EXTRACT DOWNLOAD_NO_PROGRESS TIMEOUT INACTIVITY_TIMEOUT HTTP_USERNAME ",
        "HTTP_PASSWORD HTTP_HEADER TLS_VERSION TLS_VERIFY TLS_CAINFO NETRC NETRC_FILE ",
        "GIT_REPOSITORY GIT_TAG GIT_REMOTE_NAME GIT_SUBMODULES GIT_SUBMODULES_RECURSE GIT_SHALLOW ",
        "GIT_PROGRESS GIT_CONFIG GIT_REMOTE_UPDATE_STRATEGY SVN_REPOSITORY SVN_REVISION ",
        "SVN_USERNAME SVN_PASSWORD SVN_TRUST_CERT HG_REPOSITORY HG_TAG CVS_REPOSITORY CVS_MODULE CVS_TAG ",
        "UPDATE_COMMAND UPDATE_DISCONNECTED PATCH_COMMAND CONFIGURE_COMMAND ",
        "CONFIGURE_ENVIRONMENT_MODIFICATION CMAKE_COMMAND CMAKE_GENERATOR CMAKE_GENERATOR_PLATFORM ",
        "CMAKE_GENERATOR_TOOLSET CMAKE_GENERATOR_INSTANCE CMAKE_ARGS CMAKE_CACHE_ARGS ",
        "CMAKE_CACHE_DEFAULT_ARGS SOURCE_SUBDIR CONFIGURE_HANDLED_BY_BUILD BUILD_COMMAND ",
        "BUILD_ENVIRONMENT_MODIFICATION BUILD_IN_SOURCE BUILD_ALWAYS BUILD_BYPRODUCTS BUILD_JOB_SERVER_AWARE ",
        "INSTALL_COMMAND INSTALL_ENVIRONMENT_MODIFICATION INSTALL_BYPRODUCTS INSTALL_JOB_SERVER_AWARE ",
        "TEST_COMMAND TEST_ENVIRONMENT_MODIFICATION TEST_BEFORE_INSTALL TEST_AFTER_INSTALL TEST_EXCLUDE_FROM_MAIN ",
        "LOG_DOWNLOAD LOG_UPDATE LOG_PATCH LOG_CONFIGURE LOG_BUILD LOG_INSTALL LOG_TEST LOG_MERGED_STDOUTERR ",
        "LOG_OUTPUT_ON_FAILURE USES_TERMINAL_DOWNLOAD USES_TERMINAL_UPDATE USES_TERMINAL_PATCH ",
        "USES_TERMINAL_CONFIGURE USES_TERMINAL_BUILD USES_TERMINAL_INSTALL USES_TERMINAL_TEST ",
        "DEPENDS EXCLUDE_FROM_ALL STEP_TARGETS INDEPENDENT_STEP_TARGETS LIST_SEPARATOR ",
        "EXTERNALPROJECT_INTERNAL_ARGUMENT_SEPARATOR SYSTEM OVERRIDE_FIND_PACKAGE",
    );
    KEYWORDS
        .split_ascii_whitespace()
        .any(|key| key.as_bytes() == value)
}

/// A configure-local index of FetchContent declarations and requested fetch rules.
pub struct FetchSession {
    declarations: BTreeMap<String, DeclaredFetch>,
    mode: FetchLockMode,
    offline: bool,
    #[cfg(all(feature = "native", any(unix, windows)))]
    prefetch: bool,
}

/// Identity provided by the population hook after it verifies the bundled call and template.
///
/// The hook must check both the enclosing FetchContent call and the command's
/// source. Matching a command name or arguments alone is insufficient.
pub struct PopulationSite<'a> {
    /// Dependency name from the enclosing bundled call.
    pub dependency: &'a str,
    /// Whether FETCHCONTENT_SOURCE_DIR_<NAME> currently overrides population.
    pub source_override: bool,
}

impl FetchSession {
    /// Start a session with the evaluator settings and the Host offline setting.
    pub fn new(services: &Services, host: &dyn Host) -> Self {
        Self {
            declarations: BTreeMap::new(),
            mode: services.speed.fetch_lock,
            offline: host
                .env_get(b"RCMAKE_FETCH_OFFLINE")
                .is_some_and(|value| value == "1"),
            #[cfg(all(feature = "native", any(unix, windows)))]
            prefetch: services.speed.fetch_prefetch,
        }
    }

    /// Record an expanded declaration. The first declaration of each name wins.
    ///
    /// The observer must check that this is the bundled FetchContent_Declare.
    /// Return a new declaration for optional prefetch.
    pub fn observe(&mut self, args: &[ExpandedArg]) -> Option<&DeclaredFetch> {
        let declaration = parse_fetchcontent_declare(args)?;
        let std::collections::btree_map::Entry::Vacant(slot) =
            self.declarations.entry(declaration.name.clone())
        else {
            return None;
        };
        Some(slot.insert(declaration))
    }

    /// Find the observed declaration by its case-insensitive name.
    pub fn get(&self, name: &str) -> Option<&DeclaredFetch> {
        self.declarations.get(&name.to_ascii_lowercase())
    }

    /// Match a bundled file(DOWNLOAD) to the same dependency, endpoint, and hash.
    ///
    /// Pass the expanded arguments after the URL, including destination and
    /// transfer options. The download bridge forwards this slice unchanged.
    pub fn match_archive<'a>(
        &'a self,
        site: PopulationSite<'_>,
        url: &'a str,
        args: &'a [&'a [u8]],
    ) -> Option<ArchiveTransfer<'a>> {
        let declaration = self.for_transfer(site)?;
        let DeclaredSource::Archive { urls, hash } = &declaration.source else {
            return None;
        };
        if !urls.iter().any(|candidate| candidate == url)
            || !matches_transfer_hash(hash.as_deref(), args)
        {
            return None;
        }
        Some(ArchiveTransfer {
            dependency: &declaration.name,
            urls,
            declared_hash: hash.as_deref(),
            url,
            args,
        })
    }

    /// Return a diagnostic for an unsupported transfer in Offline or Frozen mode.
    ///
    /// Call this from the download or update step when a transfer would run.
    /// A pre-populated source directory bypasses population.
    pub fn blocked(&self, site: PopulationSite<'_>, failure: BlockReason) -> Option<BlockedFetch> {
        if site.source_override || !(self.offline || self.mode == FetchLockMode::Frozen) {
            return None;
        }
        let endpoint = match self
            .get(site.dependency)
            .map(|declaration| &declaration.source)
        {
            Some(DeclaredSource::Archive { urls, .. }) => urls.first().map(String::as_str),
            Some(DeclaredSource::Git { repository, .. }) => Some(repository.as_str()),
            _ => None,
        };
        Some(BlockedFetch {
            dependency: safe_name(site.dependency),
            endpoint: endpoint.map(sanitize_url),
            reason: failure,
            offline: self.offline,
        })
    }

    fn for_transfer(&self, site: PopulationSite<'_>) -> Option<&DeclaredFetch> {
        let declaration = self.get(site.dependency)?;
        (!site.source_override && declaration.supported()).then_some(declaration)
    }
}

/// Arguments required for an archive cache lookup and the original transfer.
pub struct ArchiveTransfer<'a> {
    /// Dependency name used by the fetch lockfile.
    pub dependency: &'a str,
    /// All URLs in declaration order.
    pub urls: &'a [String],
    /// Original declared hash, when present.
    pub declared_hash: Option<&'a str>,
    /// Endpoint selected by this download attempt.
    pub url: &'a str,
    /// Original arguments after the URL, including the destination.
    pub args: &'a [&'a [u8]],
}

fn matches_transfer_hash(declared: Option<&str>, args: &[&[u8]]) -> bool {
    const OPTIONS: &str = concat!(
        "STATUS LOG TIMEOUT INACTIVITY_TIMEOUT TLS_VERIFY TLS_CAINFO TLS_VERSION ",
        "NETRC NETRC_FILE USERPWD HTTPHEADER",
    );
    let Some((destination, mut options)) = args.split_first() else {
        return false;
    };
    if destination.is_empty() {
        return false;
    }
    let mut actual = None;
    while let Some((option, rest)) = options.split_first() {
        if *option == b"SHOW_PROGRESS" {
            options = rest;
            continue;
        }
        let Some((value, tail)) = rest.split_first() else {
            return false;
        };
        if matches!(*option, b"EXPECTED_HASH" | b"EXPECTED_MD5") {
            if actual.is_some() {
                return false;
            }
            actual = if *option == b"EXPECTED_HASH" {
                text(value).and_then(parse_hash)
            } else {
                text(value).map(|hex| (HashAlgo::Md5, hex))
            };
            if actual.is_none() {
                return false;
            }
        } else if !OPTIONS
            .split_ascii_whitespace()
            .any(|key| key.as_bytes() == *option)
        {
            return false;
        }
        options = tail;
    }
    // The bundled script checks URL_HASH itself after file(DOWNLOAD) returns.
    actual.is_none_or(|(actual_algo, actual_hex)| {
        declared
            .and_then(parse_hash)
            .is_some_and(|(algo, hex)| algo == actual_algo && hex.eq_ignore_ascii_case(actual_hex))
    })
}

/// Reason a FetchContent transfer cannot proceed under the requested mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockReason {
    /// A content pin is required.
    Unpinned,
    /// The transfer would run custom or unsupported population code.
    Unsupported,
    /// The declaration cannot be written to a portable lockfile without secrets.
    CredentialedSource,
    /// The exact ordered source or checkout differs from the lockfile.
    LockMismatch,
    /// The expected content is absent from the local cache.
    CacheMiss,
}

/// A diagnostic safe to display without the original transfer arguments or process output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockedFetch {
    dependency: String,
    endpoint: Option<String>,
    reason: BlockReason,
    offline: bool,
}

impl fmt::Display for BlockedFetch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match (self.reason, self.offline) {
            (BlockReason::CredentialedSource, _) => "Fetch lock update",
            (_, true) => "Offline fetch",
            _ => "Frozen fetch",
        };
        write!(formatter, "{mode} blocked for {}.", self.dependency)?;
        if let Some(endpoint) = &self.endpoint {
            write!(formatter, " Source {endpoint}.")?;
        }
        formatter.write_str(match self.reason {
            BlockReason::Unpinned =>
                " Set URL_HASH or record a verified pin in rcmake-fetch.lock with --fetch-lock=update.",
            BlockReason::Unsupported =>
                " This transfer cannot enforce a fetch pin. Set FETCHCONTENT_SOURCE_DIR_<NAME> to a pre-populated directory.",
            BlockReason::CredentialedSource =>
                " Supply a portable source URL and configure authentication outside that URL.",
            BlockReason::LockMismatch =>
                " Restore rcmake-fetch.lock or verify the source change and run --fetch-lock=update.",
            BlockReason::CacheMiss =>
                " Seed the local fetch cache with the locked content or retry online if permitted.",
        })
    }
}

fn safe_name(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
    {
        value.into()
    } else {
        "<dependency>".into()
    }
}

/// Return a diagnostic URL identity without user info, path, query, or fragment.
pub fn sanitize_url(url: &str) -> String {
    let Some((scheme, authority)) = authority(url) else {
        return "<source>".into();
    };
    let host = authority.rsplit('@').next().unwrap_or_default();
    if valid_host(host) {
        format!("{scheme}://{host}")
    } else {
        "<source>".into()
    }
}

fn authority(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "https" | "http" | "ftp" | "ftps" | "ssh" | "git") {
        return None;
    }
    Some((scheme, rest.split(['/', '?', '#', '\\']).next()?))
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn portable_url(url: &str) -> bool {
    authority(url).is_some_and(|(_, host)| valid_host(host)) && !url.contains(['?', '#', '\\'])
}

#[cfg(all(feature = "native", any(unix, windows)))]
/// Convert the evaluator mode explicitly so the two defaults do not affect each other.
pub const fn lock_mode(mode: FetchLockMode) -> LockMode {
    match mode {
        FetchLockMode::Off => LockMode::Off,
        FetchLockMode::Auto => LockMode::Auto,
        FetchLockMode::Update => LockMode::Update,
        FetchLockMode::Frozen => LockMode::Frozen,
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
/// Recognized Git commands from the bundled clone template.
pub enum GitTransfer<'a> {
    /// A clone of the observed repository with the original template options.
    Clone(GitClone<'a>),
    /// Checkout of the declared tag or commit.
    Checkout,
}

#[cfg(all(feature = "native", any(unix, windows)))]
impl FetchSession {
    /// Queue a pinned declaration without borrowing evaluator state.
    ///
    /// Pass current override and disconnect settings. The archive worker can
    /// fetch the first mirror with default options. Foreground population keeps
    /// the full mirror sequence and original transfer options.
    pub fn prefetch(
        &self,
        pool: &FetchPool,
        declaration: &DeclaredFetch,
        source_override: bool,
        disconnected: bool,
        default_transport: bool,
    ) -> Result<Option<Ticket>, FetchError> {
        if !self.prefetch
            || self.offline
            || self.mode == FetchLockMode::Frozen
            || source_override
            || disconnected
            || !default_transport
            || !declaration.supported()
            || declaration.transfer_settings
        {
            return Ok(None);
        }
        let source = match &declaration.source {
            DeclaredSource::Archive {
                urls,
                hash: Some(hash),
            } => {
                let Some(url) = urls.first().filter(|url| portable_url(url)) else {
                    return Ok(None);
                };
                PrefetchSource::Url {
                    url: url.clone(),
                    hash: ContentHash::parse(hash)?,
                }
            }
            DeclaredSource::Git { repository, .. } if portable_url(repository) => {
                let Some(commit) = declaration.git_commit() else {
                    return Ok(None);
                };
                PrefetchSource::Git {
                    repository: repository.clone(),
                    commit: commit.into(),
                }
            }
            _ => return Ok(None),
        };
        pool.submit(source)
            .map(Some)
            .map_err(|_| FetchError::Invalid("Cannot queue fetch prefetch.".into()))
    }

    /// Match the Git executable and a recognized clone or checkout to the observed source.
    ///
    /// The hook verifies the checkout's working directory against the population directory.
    /// Git updates, custom commands, and submodule transfers use a separate decision.
    pub fn match_git<'a>(
        &self,
        site: PopulationSite<'_>,
        program: &[u8],
        git_executable: &[u8],
        args: &'a [BString],
    ) -> Option<GitTransfer<'a>> {
        if program.is_empty() || program != git_executable {
            return None;
        }
        let declaration = self.for_transfer(site)?;
        let DeclaredSource::Git {
            repository, tag, ..
        } = &declaration.source
        else {
            return None;
        };
        if !portable_url(repository) {
            return None;
        }
        if let Some(clone) = classify_git_clone(args) {
            return (repository.as_str() == clone.repository).then_some(GitTransfer::Clone(clone));
        }
        (classify_git_checkout(args)? == tag.as_deref().unwrap_or("master"))
            .then_some(GitTransfer::Checkout)
    }

    /// Apply archive cache and lock rules around the original download operation.
    ///
    /// Supply the lock snapshot opened for this configure. The caller supplies the transport
    /// and its CMake status handling. In Update, call [ArchiveTransfer::record]
    /// on the successful result before committing the session.
    pub fn fetch_archive_with(
        &self,
        cache: &ArchiveCache<'_>,
        transfer: &ArchiveTransfer<'_>,
        snapshot: Option<&LockFile>,
        download: impl FnOnce(&str, &[&[u8]]) -> Result<Vec<u8>, FetchError>,
    ) -> Result<(ArchiveResult, Option<FetchError>), FetchError> {
        let expected = transfer.declared_hash.map(ContentHash::parse).transpose()?;
        let lock = match snapshot.and_then(|file| file.get(transfer.dependency)) {
            Some(LockEntry::Url(entry)) if entry.urls.as_slice() == transfer.urls => Some(entry),
            _ => None,
        };
        if self.mode == FetchLockMode::Frozen && lock.is_none() {
            return Err(transfer.failure(self.offline, BlockReason::LockMismatch));
        }
        if self.mode == FetchLockMode::Update && !transfer.urls.iter().all(|url| portable_url(url))
        {
            return Err(transfer.failure(self.offline, BlockReason::CredentialedSource));
        }
        let enforced = lock.filter(|_| {
            self.mode == FetchLockMode::Frozen
                || (self.offline && self.mode != FetchLockMode::Off && expected.is_none())
        });
        let lock_pin = enforced.map(|entry| ContentHash::sha256(entry.sha256));
        let pin = lock_pin.as_ref().or(expected.as_ref());
        if self.offline && pin.is_none() {
            return Err(transfer.failure(true, BlockReason::Unpinned));
        }
        let mut result = cache
            .fetch_with(transfer.url, pin, || {
                if self.offline {
                    return Err(FetchError::Offline);
                }
                let bytes = download(transfer.url, transfer.args)?;
                if let Some(expected) = &expected {
                    expected.verify(&bytes)?;
                }
                if let Some(entry) = enforced {
                    verify_archive_size(entry, &bytes)?;
                }
                Ok(bytes)
            })
            .map_err(|error| transfer.transfer_error(self.offline, error))?;
        if let Some(entry) = enforced {
            if let Some(expected) = &expected {
                expected
                    .verify(&result.bytes)
                    .map_err(|error| transfer.transfer_error(self.offline, error))?;
            }
            verify_archive_size(entry, &result.bytes)?;
        }
        if self.mode == FetchLockMode::Update && expected.is_none() {
            let entry = UrlEntry::from_bytes(Vec::new(), &result.bytes);
            result.cache_warning = cache
                .insert(
                    transfer.url,
                    &ContentHash::sha256(entry.sha256),
                    &result.bytes,
                )
                .err();
        }
        result.cache_warning = result
            .cache_warning
            .map(|_| FetchError::Invalid("Cannot read or write the fetch cache.".into()));
        let warning = lock
            .filter(|_| self.mode == FetchLockMode::Auto && expected.is_none())
            .and_then(|entry| {
                ContentHash::sha256(entry.sha256)
                    .verify(&result.bytes)
                    .and_then(|()| verify_archive_size(entry, &result.bytes))
                    .err()
            });
        Ok((result, warning))
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
impl ArchiveTransfer<'_> {
    /// Record the full mirror list and content identity in an Update session.
    pub fn record(
        &self,
        lock: &mut LockSession<'_>,
        result: &ArchiveResult,
    ) -> Result<(), FetchError> {
        if !self.urls.iter().all(|url| portable_url(url)) {
            return Err(self.failure(false, BlockReason::CredentialedSource));
        }
        let entry = UrlEntry::from_bytes(self.urls.to_vec(), &result.bytes);
        lock.record(self.dependency, LockEntry::Url(entry))
    }

    fn failure(&self, offline: bool, reason: BlockReason) -> FetchError {
        FetchError::Invalid(
            BlockedFetch {
                dependency: safe_name(self.dependency),
                endpoint: Some(sanitize_url(self.url)),
                reason,
                offline,
            }
            .to_string(),
        )
    }

    fn transfer_error(&self, offline: bool, error: FetchError) -> FetchError {
        match error {
            FetchError::Offline => self.failure(offline, BlockReason::CacheMiss),
            FetchError::HashMismatch { expected, actual }
                if [expected.as_str(), actual.as_str()]
                    .into_iter()
                    .all(|hex| hex.len() <= 128 && hex.bytes().all(|b| b.is_ascii_hexdigit())) =>
            {
                FetchError::Invalid(format!(
                    "Archive hash mismatch for {} at {}. Expected {expected}. Got {actual}. Check URL_HASH and the mirror. If the locked source changed, verify it and run --fetch-lock=update.",
                    safe_name(self.dependency),
                    sanitize_url(self.url),
                ))
            }
            _ => FetchError::Invalid(format!(
                "Cannot retrieve the archive for {} at {}. Check the transfer settings and the fetch cache.",
                safe_name(self.dependency),
                sanitize_url(self.url),
            )),
        }
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn verify_archive_size(entry: &UrlEntry, bytes: &[u8]) -> Result<(), FetchError> {
    if entry.size == bytes.len() as u64 {
        Ok(())
    } else {
        Err(FetchError::Invalid(
            "Archive size differs from the fetch lock entry.".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;
    use rcmake_syntax::ArgKind;

    fn args(values: &[&str]) -> Vec<ExpandedArg> {
        values
            .iter()
            .map(|value| ExpandedArg {
                value: (*value).into(),
                kind: ArgKind::Quoted,
                line: 1,
            })
            .collect()
    }

    fn session(mode: FetchLockMode) -> FetchSession {
        let host = PureHost::new("/project".into(), "/rcmake".into());
        let mut services = Services::default();
        services.speed.fetch_lock = mode;
        FetchSession::new(&services, &host)
    }

    fn site(dependency: &str) -> PopulationSite<'_> {
        PopulationSite {
            dependency,
            source_override: false,
        }
    }

    #[test]
    fn keeps_first_declaration_full_mirrors_and_original_options() -> Result<(), &'static str> {
        let first = "https://example.org/primary";
        let second = "https://example.org/mirror";
        let hash = "SHA256=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let mut session = session(FetchLockMode::Update);
        assert!(
            session
                .observe(&args(&[
                    "Widgets",
                    "URL",
                    first,
                    second,
                    "URL_HASH",
                    hash,
                    "HTTP_HEADER",
                    "Authorization: test",
                    "SYSTEM"
                ]))
                .is_some()
        );
        assert!(
            session
                .observe(&args(&["WIDGETS", "URL", "https://elsewhere"]))
                .is_none()
        );
        let options = [
            b"archive".as_slice(),
            b"HTTPHEADER",
            b"Authorization: test",
            b"EXPECTED_HASH",
            hash.as_bytes(),
            b"TLS_VERIFY",
            b"ON",
        ];
        let transfer = session
            .match_archive(site("WIDGETS"), second, &options)
            .ok_or("missing transfer")?;
        assert_eq!(transfer.dependency, "widgets");
        assert_eq!(transfer.urls, [first, second]);
        assert_eq!(transfer.declared_hash, Some(hash));
        assert_eq!(transfer.args, options);
        assert!(
            session
                .match_archive(site("other"), second, &options)
                .is_none()
        );
        let bundled = [b"archive".as_slice(), b"STATUS", b"status"];
        assert!(
            session
                .match_archive(site("widgets"), first, &bundled)
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn detects_git_pins_and_population_overrides() {
        let id = "1234567890123456789012345678901234567890";
        let repo = "https://example.org/widgets";
        let commit = args(&["pkg", "GIT_REPOSITORY", repo, "GIT_TAG", id]);
        let tag = args(&["pkg", "GIT_REPOSITORY", repo, "GIT_TAG", "v1"]);
        assert_eq!(
            parse_fetchcontent_declare(&commit)
                .as_ref()
                .and_then(DeclaredFetch::git_commit),
            Some(id)
        );
        assert!(parse_fetchcontent_declare(&tag).is_some_and(|decl| decl.git_commit().is_none()));

        for (keyword, value) in [("SOURCE_DIR", "/existing"), ("DOWNLOAD_COMMAND", "")] {
            let mut session = session(FetchLockMode::Frozen);
            assert!(
                session
                    .observe(&args(&["pkg", "URL", repo, keyword, value]))
                    .is_some()
            );
            assert!(
                session
                    .match_archive(site("pkg"), repo, &[b"archive"])
                    .is_none()
            );
        }
    }

    #[test]
    fn redacts_credentials_and_names_the_recovery() -> Result<(), &'static str> {
        let url = "https://user:password@example.org/private?token=secret#key";
        let mut session = session(FetchLockMode::Frozen);
        session
            .observe(&args(&["pkg", "URL", url]))
            .ok_or("missing declaration")?;
        let error = session
            .blocked(site("pkg"), BlockReason::Unsupported)
            .ok_or("missing block")?
            .to_string();
        assert!(error.contains("pkg") && error.contains("https://example.org"));
        assert!(error.contains("FETCHCONTENT_SOURCE_DIR_<NAME>"));
        assert!(
            !["password", "private", "token", "secret", "#key"]
                .iter()
                .any(|&part| error.contains(part))
        );
        Ok(())
    }

    #[cfg(all(feature = "native", any(unix, windows)))]
    #[test]
    fn maps_modes_and_matches_only_the_expected_git_command() {
        for (input, expected) in [
            (FetchLockMode::Off, LockMode::Off),
            (FetchLockMode::Auto, LockMode::Auto),
            (FetchLockMode::Update, LockMode::Update),
            (FetchLockMode::Frozen, LockMode::Frozen),
        ] {
            assert_eq!(lock_mode(input), expected);
        }

        let mut session = session(FetchLockMode::Frozen);
        let url = "https://example.org/widgets";
        assert!(
            session
                .observe(&args(&["widgets", "GIT_REPOSITORY", url, "GIT_TAG", "v1"]))
                .is_some()
        );
        let clone = ["clone", "--no-checkout", url, "widgets-src"].map(BString::from);
        assert!(matches!(
            session.match_git(site("widgets"), b"git", b"git", &clone),
            Some(GitTransfer::Clone(_))
        ));
        assert!(
            session
                .match_git(site("widgets"), b"other", b"git", &clone)
                .is_none()
        );
        let checkout = ["checkout", "v1", "--"].map(BString::from);
        assert!(matches!(
            session.match_git(site("widgets"), b"git", b"git", &checkout),
            Some(GitTransfer::Checkout)
        ));
    }
}
