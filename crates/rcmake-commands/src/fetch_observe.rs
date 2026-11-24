// SPDX-License-Identifier: MIT OR Apache-2.0

//! Observe FetchContent declarations and match transfers from bundled population code.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use rcmake_base::{
    BString,
    hash::HashAlgo,
    list::{self, Empty},
};
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
    #[cfg(all(feature = "native", any(unix, windows)))]
    package_lookup: bool,
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
        !self.custom_download && !self.custom_update
    }
}

/// Parse the expanded arguments of a bundled FetchContent_Declare call.
///
/// Return no declaration for a malformed name, source, hash, or package option combination.
pub fn parse_fetchcontent_declare(args: &[ExpandedArg]) -> Option<DeclaredFetch> {
    let (name, args) = args.split_first()?;
    let name = text(&name.value)?.to_ascii_lowercase();
    if name.len() > 256 {
        return None;
    }

    let package_args = args.iter().any(|arg| arg.value == "FIND_PACKAGE_ARGS");
    let override_package = args.iter().any(|arg| arg.value == "OVERRIDE_FIND_PACKAGE");
    if package_args && override_package {
        return None;
    }
    let options = declaration_options(args);
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
                .flat_map(|url| list::iter(url, Empty::Drop))
                .map(|url| text(&url).map(str::to_owned))
                .collect::<Option<_>>()?;
            if urls.is_empty() {
                return None;
            }
            let hash = match options.get(b"URL_HASH".as_slice()).map(Vec::as_slice) {
                Some([value]) if !value.is_empty() => Some(text(value)?.to_owned()),
                None | Some([_]) => match one(b"URL_MD5").filter(|value| !value.is_empty()) {
                    Some(value) => Some(format!("MD5={}", text(value)?)),
                    None => None,
                },
                Some(_) => return None,
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
                    .flat_map(|value| list::split(value, Empty::Drop))
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
        #[cfg(all(feature = "native", any(unix, windows)))]
        package_lookup: package_args || override_package,
    })
}

fn declaration_options(args: &[ExpandedArg]) -> BTreeMap<&[u8], Vec<&BString>> {
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

    options
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
        "GIT_PROGRESS GIT_CONFIG GIT_REMOTE_UPDATE_STRATEGY SVN_REPOSITORY SVN_REVISION SVN_USERNAME ",
        "SVN_PASSWORD SVN_TRUST_CERT HG_REPOSITORY HG_TAG CVS_REPOSITORY CVS_MODULE CVS_TAG ",
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
#[derive(Clone, Copy)]
pub struct PopulationSite<'a> {
    /// Dependency name from the enclosing bundled call.
    pub dependency: &'a str,
    /// Whether FETCHCONTENT_SOURCE_DIR_<NAME> currently overrides population.
    pub source_override: bool,
    /// Repository resolved by the bundled CMP0150 logic for a relative declaration.
    pub effective_git_repository: Option<&'a str>,
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
    /// Record an expanded bundled FetchContent_Declare call. The first valid declaration wins.
    pub fn observe(
        &mut self,
        args: &[ExpandedArg],
    ) -> Result<Option<&DeclaredFetch>, BlockedFetch> {
        if let Some((name, args)) = args.split_first()
            && declaration_options(args)
                .iter()
                .any(|(&key, values)| values.iter().any(|value| credentialed_option(key, value)))
        {
            return Err(credential_block(&String::from_utf8_lossy(&name.value)));
        }
        let Some(declaration) = parse_fetchcontent_declare(args) else {
            return Ok(None);
        };
        let std::collections::btree_map::Entry::Vacant(slot) =
            self.declarations.entry(declaration.name.clone())
        else {
            return Ok(None);
        };
        Ok(Some(slot.insert(declaration)))
    }
    /// Find the observed declaration by its case-insensitive name.
    pub fn get(&self, name: &str) -> Option<&DeclaredFetch> {
        self.declarations.get(&name.to_ascii_lowercase())
    }
    /// Match a bundled file(DOWNLOAD) to the same dependency, endpoint, and hash.
    /// Pass the expanded arguments after the URL, including destination and
    /// transfer options. Stop on error even if the caller would use a fallback.
    pub fn match_archive<'a>(
        &'a self,
        site: PopulationSite<'_>,
        url: &'a str,
        args: &'a [&'a [u8]],
    ) -> Result<Option<ArchiveTransfer<'a>>, BlockedFetch> {
        if !portable_source(url) || credential_options(args) {
            return Err(credential_block(site.dependency));
        }
        let Some(declaration) = self.for_transfer(site) else {
            return Ok(None);
        };
        let DeclaredSource::Archive { urls, hash } = &declaration.source else {
            return Ok(None);
        };
        if !urls.iter().any(|candidate| candidate == url)
            || !matches_transfer_hash(hash.as_deref(), args)
        {
            return Ok(None);
        }
        Ok(Some(ArchiveTransfer {
            dependency: &declaration.name,
            urls,
            declared_hash: hash.as_deref(),
            url,
            args,
        }))
    }
    /// Stop Git updates, submodule transfers, custom commands, CMP0168 sub-builds,
    /// and unmatched transfers in Offline or Frozen mode.
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
    const OPTIONS: &str = "STATUS LOG TIMEOUT INACTIVITY_TIMEOUT TLS_VERIFY TLS_CAINFO TLS_VERSION NETRC NETRC_FILE HTTPHEADER";
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
    /// Git update cannot run with an enforced fetch pin.
    GitUpdate,
    /// Submodule transfer requires separate pins.
    GitSubmodules,
    /// A sub-build bypasses the direct population checks.
    SubBuild,
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
            (BlockReason::CredentialedSource, _) => "FetchContent",
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
                " Use a public source identity without credentials. Use a credential helper, a private netrc file, or a prepared FETCHCONTENT_SOURCE_DIR_<NAME> directory.",
            BlockReason::GitUpdate =>
                " Git update requires network access outside the fetch pin. Use the locked checkout or a prepared FETCHCONTENT_SOURCE_DIR_<NAME> directory.",
            BlockReason::GitSubmodules =>
                " Git submodule transfer requires separate pins. Use a prepared FETCHCONTENT_SOURCE_DIR_<NAME> directory.",
            BlockReason::SubBuild =>
                " CMP0168 OLD runs a sub-build outside direct fetch checks. Set CMP0168 to NEW or use a prepared FETCHCONTENT_SOURCE_DIR_<NAME> directory.",
            BlockReason::LockMismatch =>
                " Restore rcmake-fetch.lock or verify the source change and run --fetch-lock=update.",
            BlockReason::CacheMiss =>
                " Seed the local fetch cache with the locked content or retry online if permitted.",
        })
    }
}

impl std::error::Error for BlockedFetch {}

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

fn credential_block(dependency: &str) -> BlockedFetch {
    BlockedFetch {
        dependency: safe_name(dependency),
        endpoint: None,
        reason: BlockReason::CredentialedSource,
        offline: false,
    }
}
fn credentialed_option(key: &[u8], value: &[u8]) -> bool {
    match key {
        b"URL" => list::iter(value, Empty::Drop)
            .any(|url| std::str::from_utf8(&url).map_or(true, |url| !portable_source(url))),
        b"GIT_REPOSITORY" | b"SVN_REPOSITORY" | b"HG_REPOSITORY" | b"CVS_REPOSITORY" => {
            std::str::from_utf8(value).map_or(true, |url| !portable_source(url))
        }
        b"HTTP_HEADER" => list::iter(value, Empty::Drop).any(|header| credential_header(&header)),
        b"HTTP_USERNAME" | b"HTTP_PASSWORD" | b"SVN_USERNAME" | b"SVN_PASSWORD" => {
            !value.is_empty()
        }
        b"GIT_CONFIG" => {
            let value = value.to_ascii_lowercase();
            value.starts_with(b"url.")
                || value.starts_with(b"http.")
                    && value.windows(11).any(|part| part == b"extraheader")
        }
        _ => false,
    }
}
fn credential_header(value: &[u8]) -> bool {
    let name = value.split(|&b| b == b':').next().unwrap_or_default();
    value.starts_with(b"@")
        || secret_name(&String::from_utf8_lossy(name.trim_ascii()))
        || value.iter().any(u8::is_ascii_control)
}

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const MARKERS: &str = "token secret password passwd credential authorization api-key api_key apikey signature cookie";
    MARKERS
        .split_ascii_whitespace()
        .any(|marker| name.contains(marker))
}

fn credential_options(args: &[&[u8]]) -> bool {
    args.windows(2).any(|pair| match pair {
        [b"USERPWD", _] => true,
        [b"HTTPHEADER", value] => credential_header(value),
        _ => false,
    })
}

fn relative_repo(repository: &str) -> bool {
    repository.starts_with("../") && safe_path(repository)
}

fn safe_path(path: &str) -> bool {
    !path.chars().any(char::is_control)
        && !path.contains(['?', '#', '%', '@'])
        && path.split(['/', '\\', ';']).all(|segment| {
            !secret_name(segment.split('=').next().unwrap_or_default())
                && !["ghp_", "gho_", "github_pat_", "glpat-", "xox", "eyJ"]
                    .iter()
                    .any(|prefix| segment.starts_with(prefix))
                && !(segment.len() >= 32
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                    && segment.bytes().any(|byte| byte.is_ascii_digit()))
        })
}

/// Check a public archive or Git identity before transport or lockfile use.
pub fn portable_source(source: &str) -> bool {
    if source.is_empty() || source.starts_with('-') || source.len() > 8192 {
        return false;
    }
    if let Some((scheme, rest)) = source.split_once("://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let allowed = match scheme {
            "file" => matches!(host, "" | "localhost"),
            "ssh" => valid_host(ssh_host(host)),
            "http" | "https" | "ftp" | "ftps" | "git" => valid_host(host),
            _ => false,
        };
        return allowed && !path.contains('\\') && safe_path(path);
    }
    if let Some((host, path)) = scp_repo(source) {
        return valid_host(ssh_host(host)) && !path.contains('\\') && safe_path(path);
    }
    (!source.contains(':') || std::path::Path::new(source).is_absolute()) && safe_path(source)
}

fn ssh_host(host: &str) -> &str {
    host.strip_prefix("git@").unwrap_or(host)
}
fn scp_repo(source: &str) -> Option<(&str, &str)> {
    let (host, path) = source.split_once(':')?;
    (!host.contains(['/', '\\']) && host.len() > 1 && !path.starts_with(':') && !path.is_empty())
        .then_some((host, path))
}

#[cfg(all(feature = "native", any(unix, windows)))]
fn git_identity(repo: &str) -> String {
    if let Some((scheme, rest)) = repo.split_once("://")
        && let Some((host, path)) = rest.split_once('/')
    {
        return format!("{scheme}://{}/{path}", host.to_ascii_lowercase());
    }
    if let Some((host, path)) = scp_repo(repo) {
        let host = host.to_ascii_lowercase();
        if path.starts_with('/') {
            return format!("ssh://{host}{path}");
        }
        return format!("{host}:{path}");
    }
    repo.into()
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
        provider_active: bool,
    ) -> Result<Option<Ticket>, FetchError> {
        if !self.prefetch
            || self.offline
            || self.mode == FetchLockMode::Frozen
            || source_override
            || disconnected
            || !default_transport
            || provider_active
            || declaration.package_lookup
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
                let Some(url) = urls.first().filter(|url| portable_source(url)) else {
                    return Ok(None);
                };
                PrefetchSource::Url {
                    url: url.clone(),
                    hash: ContentHash::parse(hash)?,
                }
            }
            DeclaredSource::Git { repository, .. }
                if !relative_repo(repository) && portable_source(repository) =>
            {
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
    /// The hook verifies the checkout's working directory against the population directory.
    /// Git updates, custom commands, and submodule transfers use a separate decision.
    pub fn match_git<'a>(
        &self,
        site: PopulationSite<'_>,
        program: &[u8],
        git_executable: &[u8],
        args: &'a [BString],
    ) -> Result<Option<GitTransfer<'a>>, BlockedFetch> {
        if program.is_empty() || program != git_executable {
            return Ok(None);
        }
        if args
            .iter()
            .any(|value| credentialed_option(b"GIT_CONFIG", value))
            || args.iter().any(|value| value == "clone")
                && matches!(args, [.., repository, _] if credentialed_option(b"GIT_REPOSITORY", repository))
        {
            return Err(credential_block(site.dependency));
        }
        let Some(declaration) = self.for_transfer(site) else {
            return Ok(None);
        };
        let DeclaredSource::Git {
            repository, tag, ..
        } = &declaration.source
        else {
            return Ok(None);
        };
        let repository = if relative_repo(repository) {
            let Some(effective) = site.effective_git_repository else {
                return Ok(None);
            };
            effective
        } else {
            repository
        };
        if !portable_source(repository) {
            return Err(credential_block(site.dependency));
        }
        Ok(if let Some(clone) = classify_git_clone(args) {
            (git_identity(repository) == git_identity(clone.repository))
                .then_some(GitTransfer::Clone(clone))
        } else {
            (classify_git_checkout(args) == Some(tag.as_deref().unwrap_or("master")))
                .then_some(GitTransfer::Checkout)
        })
    }
    /// Apply archive cache and lock rules around the original download operation.
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
        transfer.check_credentials(self.offline)?;
        let expected = transfer.declared_hash.map(ContentHash::parse).transpose()?;
        let lock = match snapshot.and_then(|file| file.get(transfer.dependency)) {
            Some(LockEntry::Url(entry)) if entry.urls.as_slice() == transfer.urls => Some(entry),
            _ => None,
        };
        if self.mode == FetchLockMode::Frozen && lock.is_none() {
            return Err(transfer.failure(self.offline, BlockReason::LockMismatch));
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
        let verify = |bytes: &[u8]| -> Result<(), FetchError> {
            if let Some(expected) = &expected {
                expected.verify(bytes)?;
            }
            if let Some(entry) = enforced {
                verify_archive_size(entry, bytes)?;
            }
            Ok(())
        };
        let mut result = cache
            .fetch_with(transfer.url, pin, || {
                if self.offline {
                    return Err(FetchError::Offline);
                }
                let bytes = download(transfer.url, transfer.args)?;
                verify(&bytes)?;
                Ok(bytes)
            })
            .map_err(|error| transfer.transfer_error(self.offline, error))?;
        if result.cache_hit && enforced.is_some() {
            verify(&result.bytes).map_err(|error| transfer.transfer_error(self.offline, error))?;
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
        self.check_credentials(false)?;
        let entry = UrlEntry::from_bytes(self.urls.to_vec(), &result.bytes);
        lock.record(self.dependency, LockEntry::Url(entry))
    }

    fn check_credentials(&self, offline: bool) -> Result<(), FetchError> {
        if self.urls.iter().all(|url| portable_source(url))
            && portable_source(self.url)
            && !credential_options(self.args)
        {
            Ok(())
        } else {
            Err(self.failure(offline, BlockReason::CredentialedSource))
        }
    }
    fn failure(&self, offline: bool, reason: BlockReason) -> FetchError {
        let blocked = BlockedFetch {
            dependency: safe_name(self.dependency),
            endpoint: Some(sanitize_url(self.url)),
            reason,
            offline,
        };
        FetchError::Invalid(blocked.to_string())
    }

    fn transfer_error(&self, offline: bool, error: FetchError) -> FetchError {
        let cause = match error {
            FetchError::Offline => return self.failure(offline, BlockReason::CacheMiss),
            FetchError::HashMismatch { .. } => {
                "The archive hash differs. Check URL_HASH and the mirror."
            }
            _ => "Check the transfer settings and the fetch cache.",
        };
        FetchError::Invalid(format!(
            "Cannot retrieve the archive for {} at {}. {cause}",
            safe_name(self.dependency),
            sanitize_url(self.url),
        ))
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
    use rcmake_syntax::ArgKind;

    const SITE: PopulationSite<'static> = PopulationSite {
        dependency: "pkg",
        source_override: false,
        effective_git_repository: None,
    };

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

    #[test]
    fn parses_mirrors_and_md5_with_an_explicit_destination()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut session = FetchSession {
            declarations: BTreeMap::new(),
            mode: FetchLockMode::Frozen,
            offline: false,
            #[cfg(all(feature = "native", any(unix, windows)))]
            prefetch: false,
        };
        let url = "https://example.org/b";
        let md5 = "900150983cd24fb0d6963f7d28e17f72";
        let input = args(&[
            "Pkg",
            "URL",
            "https://example.org/a;https://example.org/b",
            "URL_HASH",
            "",
            "URL_MD5",
            md5,
            "SOURCE_DIR",
            "/destination",
        ]);
        assert!(session.observe(&input)?.is_some());
        assert!(session.observe(&input)?.is_none());
        let transfer = session
            .match_archive(SITE, url, &[b"archive"])?
            .ok_or("missing transfer")?;
        assert_eq!(transfer.urls, ["https://example.org/a", url]);
        assert_eq!(transfer.declared_hash, Some(format!("MD5={md5}").as_str()));
        let options = [b"file".as_slice(), b"HTTPHEADER", b"authorization: private"];
        assert!(session.match_archive(SITE, url, &options).is_err());
        assert!(!portable_source("https://example.org/token/private"));
        assert!(session.blocked(SITE, BlockReason::GitSubmodules).is_some());
        Ok(())
    }
}
