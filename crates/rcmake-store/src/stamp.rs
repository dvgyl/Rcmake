#[cfg(test)]
mod sdk_tests;
mod tree;

pub use tree::{sdk_tree_listing_digest, tree_listing_digest};

use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcmake_base::Limits;
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Metadata};
use rcmake_json::{
    FieldError, FromJson, Object, ToJson, Value, deny_unknown, object, opt, opt_or_else, req,
    req_value,
};
use sha2::{Digest as _, Sha256};

use crate::{Digest, Domain, KeyHasher};

/// The metadata observed for a regular file.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FastStat {
    /// File size in bytes.
    pub len: u64,
    /// Modification time in whole seconds since the Unix epoch.
    pub mtime_secs: i64,
    /// Nanosecond part of the modification time.
    pub mtime_nanos: u32,
    /// File permission bits.
    pub mode: u32,
    /// Device identifier.
    pub dev: u64,
    /// File identifier on the device.
    pub ino: u64,
}

impl ToJson for FastStat {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("len", self.len.to_json()),
            ("mtime_secs", self.mtime_secs.to_json()),
            ("mtime_nanos", self.mtime_nanos.to_json()),
            ("mode", self.mode.to_json()),
            ("dev", self.dev.to_json()),
            ("ino", self.ino.to_json()),
        ])
    }
}

impl FromJson for FastStat {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        deny_unknown(
            obj,
            &["len", "mtime_secs", "mtime_nanos", "mode", "dev", "ino"],
        )?;
        Ok(Self {
            len: req(obj, "len")?,
            mtime_secs: req(obj, "mtime_secs")?,
            mtime_nanos: req(obj, "mtime_nanos")?,
            mode: req(obj, "mode")?,
            dev: opt_or_else(obj, "dev", u64::default)?,
            ino: opt_or_else(obj, "ino", u64::default)?,
        })
    }
}

impl FastStat {
    /// Read the metadata for a regular file. Follow symbolic links.
    pub fn read(host: &dyn Host, path: &Path) -> HostResult<Self> {
        let metadata = host.metadata(path, Follow::Yes)?;
        if metadata.kind != FileKind::File {
            return Err(HostError::unsupported("Expected a regular file."));
        }
        Ok(Self::from(&metadata))
    }
}

impl From<&Metadata> for FastStat {
    fn from(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len,
            mtime_secs: metadata.mtime.secs,
            mtime_nanos: metadata.mtime.nanos,
            mode: metadata.mode,
            dev: metadata.dev,
            ino: metadata.ino,
        }
    }
}

/// Select how a search listing handles directory links.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum TreePolicy {
    /// Reject directory links.
    #[default]
    Ordinary,
    /// Traverse SDK directory links in a bounded graph.
    Sdk,
}

impl TreePolicy {
    fn is_ordinary(&self) -> bool {
        *self == Self::Ordinary
    }
}

impl ToJson for TreePolicy {
    fn to_json(&self) -> Value {
        Value::string(match self {
            Self::Ordinary => "Ordinary",
            Self::Sdk => "Sdk",
        })
    }
}

impl FromJson for TreePolicy {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match rcmake_json::str(value)? {
            "Ordinary" => Ok(Self::Ordinary),
            "Sdk" => Ok(Self::Sdk),
            other => Err(FieldError::invalid(format!(
                "unknown tree policy '{other}'"
            ))),
        }
    }
}

/// One input observed during a computation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputStamp {
    /// A file identified by its permission bits and content digest.
    FileStat {
        /// Absolute file path.
        path: PathBuf,
        /// Observed metadata.
        stat: FastStat,
        /// Observed content digest. Old records without a digest cannot be reused.
        digest: Option<Digest>,
    },
    /// A file identified by SHA-256 of its contents.
    FileHash {
        /// Absolute file path.
        path: PathBuf,
        /// Observed content digest.
        digest: Digest,
    },
    /// A directory identified by its immediate entries.
    DirEntries {
        /// Absolute directory path.
        path: PathBuf,
        /// Observed digest from [directory_digest].
        digest: Digest,
    },
    /// A directory identified by a bounded search listing.
    TreeListing {
        /// Absolute search directory.
        path: PathBuf,
        /// Observed digest from the selected listing policy.
        digest: Digest,
        /// Select whether the listing follows SDK directory links.
        policy: TreePolicy,
    },
    /// A path that did not exist.
    Missing {
        /// Absolute path.
        path: PathBuf,
    },
    /// An environment variable identified by a digest of its raw value.
    EnvVar {
        /// Variable name as bytes.
        name: Vec<u8>,
        /// Observed value digest, or absence.
        digest: Option<Digest>,
    },
    /// A failed search for one candidate under a search root.
    NegativeLookup {
        /// Absolute search root.
        root: PathBuf,
        /// Candidate relative to the search root.
        relative: PathBuf,
        /// Absolute path of the last directory that existed.
        deepest: PathBuf,
        /// Observed digest of that directory's immediate entries.
        listing: Digest,
    },
}

impl ToJson for InputStamp {
    fn to_json(&self) -> Value {
        match self {
            Self::FileStat { path, stat, digest } => Value::object_from([(
                "FileStat",
                Value::object_from([
                    ("path", path_to_json(path)),
                    ("stat", stat.to_json()),
                    ("digest", digest.to_json()),
                ]),
            )]),
            Self::FileHash { path, digest } => Value::object_from([(
                "FileHash",
                Value::object_from([("path", path_to_json(path)), ("digest", digest.to_json())]),
            )]),
            Self::DirEntries { path, digest } => Value::object_from([(
                "DirEntries",
                Value::object_from([("path", path_to_json(path)), ("digest", digest.to_json())]),
            )]),
            Self::TreeListing {
                path,
                digest,
                policy,
            } => {
                let mut fields = vec![("path", path_to_json(path)), ("digest", digest.to_json())];
                if !policy.is_ordinary() {
                    fields.push(("policy", policy.to_json()));
                }
                Value::object_from([("TreeListing", Value::object_from(fields))])
            }
            Self::Missing { path } => Value::object_from([(
                "Missing",
                Value::object_from([("path", path_to_json(path))]),
            )]),
            Self::EnvVar { name, digest } => Value::object_from([(
                "EnvVar",
                Value::object_from([("name", name.to_json()), ("digest", digest.to_json())]),
            )]),
            Self::NegativeLookup {
                root,
                relative,
                deepest,
                listing,
            } => Value::object_from([(
                "NegativeLookup",
                Value::object_from([
                    ("root", path_to_json(root)),
                    ("relative", path_to_json(relative)),
                    ("deepest", path_to_json(deepest)),
                    ("listing", listing.to_json()),
                ]),
            )]),
        }
    }
}

impl FromJson for InputStamp {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = object(value)?;
        let mut iter = obj.iter();
        let Some((tag, payload)) = iter.next() else {
            return Err(FieldError::invalid("expected input stamp variant"));
        };
        if iter.next().is_some() {
            return Err(FieldError::invalid(
                "expected single-key input stamp object",
            ));
        }
        let tag = core::str::from_utf8(tag.as_slice())
            .map_err(|_| FieldError::invalid("input stamp variant is not valid UTF-8"))?;
        let fields = object(payload).map_err(|err| err.in_field(tag))?;
        let decode = || -> Result<Self, FieldError> {
            match tag {
                "FileStat" => {
                    deny_unknown(fields, &["path", "stat", "digest"])?;
                    Ok(Self::FileStat {
                        path: req_path(fields, "path")?,
                        stat: req(fields, "stat")?,
                        digest: opt(fields, "digest")?,
                    })
                }
                "FileHash" => {
                    deny_unknown(fields, &["path", "digest"])?;
                    Ok(Self::FileHash {
                        path: req_path(fields, "path")?,
                        digest: req(fields, "digest")?,
                    })
                }
                "DirEntries" => {
                    deny_unknown(fields, &["path", "digest"])?;
                    Ok(Self::DirEntries {
                        path: req_path(fields, "path")?,
                        digest: req(fields, "digest")?,
                    })
                }
                "TreeListing" => {
                    deny_unknown(fields, &["path", "digest", "policy"])?;
                    Ok(Self::TreeListing {
                        path: req_path(fields, "path")?,
                        digest: req(fields, "digest")?,
                        policy: opt_or_else(fields, "policy", TreePolicy::default)?,
                    })
                }
                "Missing" => {
                    deny_unknown(fields, &["path"])?;
                    Ok(Self::Missing {
                        path: req_path(fields, "path")?,
                    })
                }
                "EnvVar" => {
                    deny_unknown(fields, &["name", "digest"])?;
                    Ok(Self::EnvVar {
                        name: req(fields, "name")?,
                        digest: req(fields, "digest")?,
                    })
                }
                "NegativeLookup" => {
                    deny_unknown(fields, &["root", "relative", "deepest", "listing"])?;
                    Ok(Self::NegativeLookup {
                        root: req_path(fields, "root")?,
                        relative: req_path(fields, "relative")?,
                        deepest: req_path(fields, "deepest")?,
                        listing: req(fields, "listing")?,
                    })
                }
                other => Err(FieldError::invalid(format!(
                    "unknown input stamp '{other}'"
                ))),
            }
        };
        decode().map_err(|err| err.in_field(tag))
    }
}

/// Encode a native filesystem path as a JSON [`Value`], preserving non-UTF-8
/// Unix byte sequences and unpaired Windows UTF-16 surrogates without loss.
#[must_use]
pub fn path_to_json(path: &Path) -> Value {
    if let Some(text) = path.to_str() {
        return Value::string(text);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Value::object_from([("unix_bytes", path.as_os_str().as_bytes().to_json())])
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        Value::object_from([("windows_wide", wide.to_json())])
    }
    #[cfg(not(any(unix, windows)))]
    {
        Value::string(path.to_string_lossy().as_ref())
    }
}

/// Decode a native filesystem path previously encoded by [`path_to_json`].
///
/// # Errors
///
/// Returns a [`FieldError`] if `value` is neither a valid UTF-8 JSON string nor
/// a platform-native non-UTF-8 path object for the current target.
pub fn path_from_json(value: &Value) -> Result<PathBuf, FieldError> {
    match value {
        Value::String(_) => rcmake_json::str(value).map(PathBuf::from),
        #[cfg(unix)]
        Value::Object(obj) => {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            deny_unknown(obj, &["unix_bytes"])?;
            let bytes: Vec<u8> = req(obj, "unix_bytes")?;
            Ok(PathBuf::from(OsString::from_vec(bytes)))
        }
        #[cfg(windows)]
        Value::Object(obj) => {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;
            deny_unknown(obj, &["windows_wide"])?;
            let wide: Vec<u16> = req(obj, "windows_wide")?;
            Ok(PathBuf::from(OsString::from_wide(&wide)))
        }
        _ => Err(FieldError::expected("string", value)),
    }
}

fn req_path(obj: &Object, key: &str) -> Result<PathBuf, FieldError> {
    let value = req_value(obj, key)?;
    path_from_json(value).map_err(|err| err.in_field(key))
}

impl InputStamp {
    /// Record a file input. Use only its digest when its timestamp is within
    /// the host filesystem's timestamp resolution of the current time.
    pub fn record_file(
        host: &dyn Host,
        path: PathBuf,
        timestamp_resolution: Duration,
    ) -> HostResult<Self> {
        let metadata = host.metadata_fresh(&path, Follow::Yes)?;
        let digest = hash_file(host, &path, &metadata)?;
        if crate::gc::older_than(host.now(), metadata.mtime, timestamp_resolution) {
            Ok(Self::FileStat {
                path,
                stat: FastStat::from(&metadata),
                digest: Some(digest),
            })
        } else {
            Ok(Self::FileHash { path, digest })
        }
    }
}

/// The input stamps required to reuse an entry.
pub type Trace = Vec<InputStamp>;

/// The result of checking a trace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyOutcome {
    /// Every stamp still matches.
    Hit,
    /// The first stamp that could not be verified.
    Miss(MissReason),
}

/// The reason a trace cannot be reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissReason {
    /// Zero-based position of the stamp in the trace.
    pub stamp: usize,
}

/// Hash a regular file using bounded reads through the host.
///
/// Return an error if the file exceeds the read limit or its metadata changes
/// during the read.
pub fn file_digest(host: &dyn Host, path: &Path) -> HostResult<Digest> {
    let before = host.metadata_fresh(path, Follow::Yes)?;
    hash_file(host, path, &before)
}

/// Reuse file digests within one session while file identity and metadata match.
///
/// Nothing in this memo persists after the session.
#[derive(Default)]
pub struct TraceMemo {
    files: Mutex<BTreeMap<(FastStat, rcmake_host::FileTime), Digest>>,
    generation_host: Option<Arc<rcmake_host::MemoHost<dyn Host>>>,
    /// An uncached host for directory walks that collect stamps.
    live: Option<Arc<dyn Host>>,
    /// Calculate every answer again, for independent verification.
    verify: bool,
    tools: Mutex<BTreeMap<PathBuf, (u64, FastStat, Digest)>>,
    sdk: Mutex<BTreeMap<PathBuf, Arc<SdkListing>>>,
    ordinary: Mutex<BTreeMap<PathBuf, OrdinaryListing>>,
    sdk_coverage_failures: AtomicU64,
    sdk_graph_hits: AtomicU64,
}

struct SdkListing {
    digest: Digest,
    /// The configure generation of the listing, or `None` for a listing that
    /// only fresh stamps can validate.
    generation: Option<u64>,
    links: Vec<tree::SdkLink>,
    target_generations: BTreeMap<PathBuf, u64>,
    /// Fresh stamps of every visited directory, if all of them are usable.
    dirs: Vec<tree::DirStamp>,
}

impl SdkListing {
    fn stat_revalidatable(&self) -> bool {
        !self.dirs.is_empty()
    }
}

struct OrdinaryListing {
    canonical: PathBuf,
    generation: u64,
    root_link: Option<PathBuf>,
    digest: Digest,
}

impl TraceMemo {
    /// Use a configure host to invalidate SDK and tool digests after mutations.
    #[must_use]
    pub fn with_generation(host: Arc<rcmake_host::MemoHost<dyn Host>>) -> Self {
        Self {
            live: Some(Arc::clone(host.base())),
            generation_host: Some(host),
            ..Self::default()
        }
    }

    /// Calculate every digest and listing again with fresh reads through `live`.
    ///
    /// Use this memo for independent verification. It keeps no answer.
    #[must_use]
    pub fn verifying(live: Arc<dyn Host>) -> Self {
        Self {
            live: Some(live),
            verify: true,
            ..Self::default()
        }
    }

    fn generation(&self, path: &Path) -> Option<u64> {
        self.generation_host.as_ref()?.stable_generation_for(path)
    }

    /// Return the number of SDK graphs that could not supply input coverage.
    #[must_use]
    pub fn sdk_coverage_failures(&self) -> u64 {
        self.sdk_coverage_failures.load(Ordering::Relaxed)
    }

    /// Return the number of SDK searches that reused a graph.
    #[must_use]
    pub fn sdk_graph_hits(&self) -> u64 {
        self.sdk_graph_hits.load(Ordering::Relaxed)
    }

    /// Hash a compiler executable or compiler library.
    ///
    /// Hosts without change times reuse a digest only while the file identity
    /// and configure generation agree. A rewrite outside the configure host
    /// that preserves both identity and timestamps can require a new session.
    pub fn tool_digest(&self, host: &dyn Host, path: &Path) -> HostResult<Digest> {
        if self.verify {
            return file_digest(host, path);
        }
        let metadata = host.metadata(path, Follow::Yes)?;
        if metadata.ctime.is_some() {
            return self.file_digest(host, path);
        }
        self.tool_digest_with_metadata(host, path, &metadata)
    }

    fn tool_digest_with_metadata(
        &self,
        host: &dyn Host,
        path: &Path,
        metadata: &Metadata,
    ) -> HostResult<Digest> {
        let Some(canonical) = host
            .canonicalize(path)
            .ok()
            .filter(|_| metadata.ino != 0 && metadata.kind == FileKind::File)
        else {
            return hash_file(host, path, metadata);
        };
        let Some(generation) = self.generation(&canonical) else {
            return hash_file(host, path, metadata);
        };
        let stat = FastStat::from(metadata);
        if let Ok(tools) = self.tools.lock()
            && let Some(&(cached_generation, cached_stat, digest)) = tools.get(path)
            && cached_generation == generation
            && cached_stat == stat
        {
            return Ok(digest);
        }
        let digest = hash_file(host, path, metadata)?;
        if self.generation(&canonical) == Some(generation)
            && let Ok(mut tools) = self.tools.lock()
            && (tools.len() < 1024 || tools.contains_key(path))
        {
            tools.insert(path.to_owned(), (generation, stat, digest));
        }
        Ok(digest)
    }

    /// Hash a bounded SDK search graph and retain it for the current host generation.
    pub fn sdk_digest(&self, host: &dyn Host, root: &Path) -> HostResult<Digest> {
        self.sdk_digest_inner(host, root).inspect_err(|_| {
            self.sdk_coverage_failures.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn sdk_digest_inner(&self, host: &dyn Host, root: &Path) -> HostResult<Digest> {
        if self.verify {
            let host = self.live.as_deref().unwrap_or(host);
            let canonical = host.canonicalize_fresh(root)?;
            let graph = tree::sdk_graph_digest(host, &canonical)?;
            return tree::sdk_root_digest_fresh(host, root, graph);
        }
        let canonical = host.canonicalize(root)?;
        let generation = self.generation(&canonical);
        let cached = self
            .sdk
            .lock()
            .ok()
            .and_then(|sdk| sdk.get(&canonical).cloned());
        if let Some(listing) = &cached {
            // Fast path 1: no change since the listing.
            if let Some(generation) = generation
                && listing.generation == Some(generation)
                && self.sdk_links_match(host, listing)
            {
                let digest = tree::sdk_root_digest(host, root, listing.digest)?;
                if self.generation(&canonical) == Some(generation)
                    && self.sdk_targets_unchanged(listing)
                {
                    self.sdk_graph_hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(digest);
                }
                return Err(HostError::unsupported(
                    "The host changed during the SDK listing.",
                ));
            }
            // Fast path 2: fresh stamps of every directory and link still match.
            if listing.stat_revalidatable()
                && let Some(digest) = self.revalidate(host, root, &canonical, listing, generation)
            {
                self.sdk_graph_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(digest);
            }
        }

        let stamped = stamps_reliable(host);
        let mut stamps = tree::DirStamps::default();
        let mut links = Vec::new();
        let mut target_generations = BTreeMap::new();
        let mut links_complete = true;
        let mut targets_complete = generation.is_some();
        let graph = tree::sdk_graph(
            host,
            &canonical,
            stamped.then_some(&mut stamps),
            tree::FOREGROUND_WORKERS,
            None,
            |path, resolved| {
                if links.len() >= 512 {
                    links_complete = false;
                    return;
                }
                if let Some((target, _)) = &resolved {
                    if let Some(generation) = generation.and_then(|_| self.generation(target)) {
                        target_generations
                            .entry(target.clone())
                            .or_insert(generation);
                    } else {
                        targets_complete = false;
                    }
                }
                links.push(tree::SdkLink { path, resolved });
            },
        )?;
        let dirs = usable_stamps(host, stamps, links_complete);
        let digest = tree::sdk_root_digest(host, root, graph)?;
        let generation = match generation {
            Some(generation) => {
                if self.generation(&canonical) != Some(generation) {
                    return Err(HostError::unsupported(
                        "The host changed during the SDK listing.",
                    ));
                }
                let listing = SdkListing {
                    digest: graph,
                    generation: Some(generation),
                    links,
                    target_generations,
                    dirs,
                };
                if links_complete && !self.sdk_links_match(host, &listing) {
                    return Err(HostError::unsupported(
                        "The host changed during the SDK listing.",
                    ));
                }
                self.store_sdk(canonical, listing, links_complete && targets_complete);
                return Ok(digest);
            }
            None => None,
        };
        // Without a stable generation, only fresh stamps can validate the listing.
        self.store_sdk(
            canonical,
            SdkListing {
                digest: graph,
                generation,
                links,
                target_generations,
                dirs,
            },
            false,
        );
        Ok(digest)
    }

    /// Keep a listing that a generation or fresh stamps can validate.
    fn store_sdk(&self, canonical: PathBuf, mut listing: SdkListing, generation_valid: bool) {
        if !generation_valid {
            listing.generation = None;
        }
        if (listing.generation.is_some() || listing.stat_revalidatable())
            && let Ok(mut sdk) = self.sdk.lock()
            && (sdk.len() < 128 || sdk.contains_key(&canonical))
        {
            sdk.insert(canonical, Arc::new(listing));
        }
    }

    /// Check a stamped listing with fresh queries. Return the root digest on a match.
    ///
    /// Every stamped directory is immutable to this user, so a process of this
    /// configure cannot change it. A fresh stamp check detects other changes.
    fn revalidate(
        &self,
        host: &dyn Host,
        root: &Path,
        canonical: &Path,
        listing: &SdkListing,
        generation: Option<u64>,
    ) -> Option<Digest> {
        if !stamps_reliable(host) {
            return None;
        }
        // Sample every generation before the checks, so that a change during
        // the checks prevents a refresh.
        let targets = || {
            listing
                .links
                .iter()
                .filter_map(|link| link.resolved.as_ref())
                .map(|(target, _)| Some((target.clone(), self.generation(target)?)))
                .collect::<Option<BTreeMap<_, _>>>()
        };
        let before = generation.and_then(|generation| Some((generation, targets()?)));
        // Bind the requested spelling to the same canonical root again.
        if host.canonicalize_fresh(root).ok()? != canonical
            || !tree::stamps_match(host, &listing.dirs, &listing.links)
        {
            return None;
        }
        let digest = tree::sdk_root_digest_fresh(host, root, listing.digest).ok()?;
        if host.canonicalize_fresh(root).ok()? != canonical {
            return None;
        }
        // The checks ran within one stable generation. Fast path 1 can use the
        // listing until the next change.
        if let Some((generation, target_generations)) = before
            && self.generation(canonical) == Some(generation)
            && targets().as_ref() == Some(&target_generations)
        {
            self.store_sdk(
                canonical.to_owned(),
                SdkListing {
                    digest: listing.digest,
                    generation: Some(generation),
                    links: listing.links.clone(),
                    target_generations,
                    dirs: listing.dirs.clone(),
                },
                true,
            );
        }
        Some(digest)
    }

    /// Walk an SDK search directory before a probe needs it.
    ///
    /// Keep the listing only if fresh stamps can validate it later. Verification
    /// mode and hosts without reliable directory stamps do nothing. When
    /// `cancel` becomes `true`, stop the walk soon and keep nothing.
    pub fn prefetch_sdk_digest(&self, host: &dyn Host, root: &Path, cancel: &AtomicBool) {
        if self.verify || !stamps_reliable(host) {
            return;
        }
        let Ok(canonical) = host.canonicalize_fresh(root) else {
            return;
        };
        if self
            .sdk
            .lock()
            .map_or(true, |sdk| sdk.contains_key(&canonical))
        {
            return;
        }
        let mut stamps = tree::DirStamps::default();
        let mut links = Vec::new();
        let mut links_complete = true;
        let walk = tree::sdk_graph(
            host,
            &canonical,
            Some(&mut stamps),
            tree::BACKGROUND_WORKERS,
            Some(cancel),
            |path, resolved| {
                if links.len() >= 512 {
                    links_complete = false;
                } else {
                    links.push(tree::SdkLink { path, resolved });
                }
            },
        );
        let Ok(graph) = walk else {
            return;
        };
        let dirs = usable_stamps(host, stamps, links_complete);
        if dirs.is_empty() {
            return;
        }
        if let Ok(mut sdk) = self.sdk.lock()
            && sdk.len() < 128
        {
            sdk.entry(canonical).or_insert_with(|| {
                Arc::new(SdkListing {
                    digest: graph,
                    generation: None,
                    links,
                    target_generations: BTreeMap::new(),
                    dirs,
                })
            });
        }
    }

    fn sdk_links_match(&self, host: &dyn Host, listing: &SdkListing) -> bool {
        listing.links.iter().all(|link| link.matches(host)) && self.sdk_targets_unchanged(listing)
    }

    fn sdk_targets_unchanged(&self, listing: &SdkListing) -> bool {
        listing
            .target_generations
            .iter()
            .all(|(path, generation)| self.generation(path) == Some(*generation))
    }

    /// Hash a search tree. Reuse the listing while its generation stays stable.
    pub fn tree_digest(&self, host: &dyn Host, root: &Path) -> HostResult<Digest> {
        if self.verify {
            return tree_listing_digest(self.live.as_deref().unwrap_or(host), root);
        }
        let canonical = host.canonicalize(root)?;
        let Some(generation) = self.generation(&canonical) else {
            return tree_listing_digest(host, root);
        };
        let root_link = if host.metadata(root, Follow::No)?.kind == FileKind::Symlink {
            Some(host.read_link(root)?)
        } else {
            None
        };
        let mut ordinary = self
            .ordinary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = ordinary.get(root).filter(|listing| {
            listing.canonical == canonical
                && listing.generation == generation
                && listing.root_link == root_link
        });
        let (digest, confined) = match cached {
            Some(listing) => (listing.digest, true),
            None => tree::ordinary_tree(host, root)?,
        };
        if self.generation(&canonical) != Some(generation) || host.canonicalize(root)? != canonical
        {
            return Err(HostError::unsupported(
                "The host changed during the search listing.",
            ));
        }
        if confined && cached.is_none() && (ordinary.len() < 256 || ordinary.contains_key(root)) {
            ordinary.insert(
                root.to_owned(),
                OrdinaryListing {
                    canonical,
                    generation,
                    root_link,
                    digest,
                },
            );
        }
        Ok(digest)
    }

    /// Read the current metadata, then return or calculate the file digest.
    pub fn file_digest(&self, host: &dyn Host, path: &Path) -> HostResult<Digest> {
        let metadata = host.metadata_fresh(path, Follow::Yes)?;
        if self.verify || metadata.kind != FileKind::File || metadata.ino == 0 {
            return hash_file(host, path, &metadata);
        }
        let Some(ctime) = metadata.ctime else {
            if self
                .tools
                .lock()
                .is_ok_and(|tools| tools.contains_key(path))
            {
                return self.tool_digest_with_metadata(host, path, &metadata);
            }
            return hash_file(host, path, &metadata);
        };
        // Keep files with recent or second-resolution change times out of the
        // memo. Also keep out files that a process of this user can change,
        // because equal metadata does not prove equal bytes.
        let now = host.now();
        if ctime.nanos == 0
            || ctime.secs >= now.secs.saturating_sub(1)
            || !immutable(&metadata, host.effective_uid())
        {
            return hash_file(host, path, &metadata);
        }
        let stat = (FastStat::from(&metadata), ctime);
        if let Ok(files) = self.files.lock()
            && let Some(&digest) = files.get(&stat)
        {
            return Ok(digest);
        }
        let (digest, bound) = hash_bound_file(host, path, &metadata)?;
        // Keep only a digest of the object that the metadata describes.
        if bound
            && let Ok(mut files) = self.files.lock()
            && files.len() < 32_768
        {
            files.insert(stat, digest);
        }
        Ok(digest)
    }

    /// Hash a file before a probe needs it, but only if the memo can keep the digest.
    ///
    /// Skip a file that a process of this user can change, because the memo
    /// never reuses its digest. The memo checks a kept digest again with fresh
    /// identity and metadata before each use.
    pub fn prefetch_file_digest(&self, host: &dyn Host, path: &Path) {
        if self.verify {
            return;
        }
        if host
            .metadata_fresh(path, Follow::Yes)
            .is_ok_and(|metadata| immutable(&metadata, host.effective_uid()))
        {
            let _ = self.file_digest(host, path);
        }
    }

    /// Verify current inputs and share file digests across checks in this session.
    pub fn verify(&self, host: &dyn Host, trace: &[InputStamp]) -> VerifyOutcome {
        verify_with(host, trace, Some(self))
    }
}

fn hash_file(host: &dyn Host, path: &Path, before: &Metadata) -> HostResult<Digest> {
    hash_bound_file(host, path, before).map(|(digest, _)| digest)
}

/// Hash a file. Also return `true` if the host bound the metadata to the open
/// file, so that the digest is known to describe the object of `before`.
fn hash_bound_file(host: &dyn Host, path: &Path, before: &Metadata) -> HostResult<(Digest, bool)> {
    if before.kind != FileKind::File {
        return Err(HostError::unsupported("Expected a regular file."));
    }
    if before.len > Limits::default().max_file_read {
        return Err(crate::io_at(
            std::io::Error::new(
                ErrorKind::FileTooLarge,
                "Trace file exceeds the read limit.",
            ),
            path,
        ));
    }
    let mut hash = Sha256::new();
    // Read through one handle. Read at most one extra byte to detect growth.
    let (reader, opened) = host.open_read_with_metadata(path)?;
    let bound = opened.is_some();
    if let Some(opened) = opened
        && (opened.kind != FileKind::File
            || opened.ctime != before.ctime
            || FastStat::from(&opened) != FastStat::from(before))
    {
        return Err(crate::io_at(
            std::io::Error::new(
                ErrorKind::InvalidData,
                "Trace file changed before the read.",
            ),
            path,
        ));
    }
    let mut reader = reader.take(before.len + 1);
    let mut buffer = vec![0; reader.limit().min(1024 * 1024) as usize];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(len) => {
                let bytes = buffer.get(..len).ok_or_else(|| {
                    crate::io_at(std::io::Error::other("Invalid trace read length."), path)
                })?;
                hash.update(bytes);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(crate::io_at(error, path)),
        }
    }
    let offset = before.len + 1 - reader.limit();
    let after = host.metadata_fresh(path, Follow::Yes)?;
    if offset != before.len
        || after.kind != FileKind::File
        || before.ctime != after.ctime
        || FastStat::from(before) != FastStat::from(&after)
    {
        return Err(crate::io_at(
            std::io::Error::new(
                ErrorKind::InvalidData,
                "Trace file changed during the read.",
            ),
            path,
        ));
    }
    Ok((Digest::from_bytes(hash.finalize().into()), bound))
}

/// Hash the sorted byte names and kinds of a directory's immediate entries.
pub fn directory_digest(host: &dyn Host, path: &Path) -> HostResult<Digest> {
    let mut entries = host.read_dir(path)?;
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut hasher = KeyHasher::new(Domain::BlobV1);
    hasher.field(0, b"directory");
    for entry in entries {
        let tag = match entry.kind {
            FileKind::File => 1,
            FileKind::Dir => 2,
            FileKind::Symlink => 3,
            FileKind::Other => 4,
        };
        hasher.field(tag, &entry.name);
    }
    Ok(hasher.finish())
}

/// Check all inputs through the host. A changed, invalid, or unreadable input is a miss.
pub fn verify_trace(host: &dyn Host, trace: &[InputStamp]) -> VerifyOutcome {
    verify_with(host, trace, None)
}

fn verify_with(host: &dyn Host, trace: &[InputStamp], memo: Option<&TraceMemo>) -> VerifyOutcome {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let heavy = trace
            .iter()
            .filter(|input| {
                matches!(
                    input,
                    InputStamp::TreeListing { .. }
                        | InputStamp::FileHash { .. }
                        | InputStamp::FileStat { .. }
                )
            })
            .take(2)
            .count();
        if heavy >= 2 {
            return verify_parallel(host, trace, memo);
        }
    }
    for (stamp, input) in trace.iter().enumerate() {
        if !matches_input(host, input, memo) {
            return VerifyOutcome::Miss(MissReason { stamp });
        }
    }
    VerifyOutcome::Hit
}

#[cfg(not(target_arch = "wasm32"))]
fn verify_parallel(
    host: &dyn Host,
    trace: &[InputStamp],
    memo: Option<&TraceMemo>,
) -> VerifyOutcome {
    let next = AtomicUsize::new(0);
    let first_miss = AtomicUsize::new(usize::MAX);
    let worker_count = trace.len().min(4);
    let work = || loop {
        let idx = next.fetch_add(1, Ordering::Relaxed);
        if idx >= trace.len() || idx >= first_miss.load(Ordering::Relaxed) {
            break;
        }
        if let Some(input) = trace.get(idx)
            && !matches_input(host, input, memo)
        {
            first_miss.fetch_min(idx, Ordering::Relaxed);
            break;
        }
    };
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count.saturating_sub(1));
        for _ in 1..worker_count {
            if let Ok(handle) = std::thread::Builder::new().spawn_scoped(scope, work) {
                handles.push(handle);
            }
        }
        work();
        for handle in handles {
            let _ = handle.join();
        }
    });
    let miss = first_miss.load(Ordering::Relaxed);
    if miss == usize::MAX {
        VerifyOutcome::Hit
    } else {
        VerifyOutcome::Miss(MissReason { stamp: miss })
    }
}

fn matches_input(host: &dyn Host, input: &InputStamp, memo: Option<&TraceMemo>) -> bool {
    match input {
        InputStamp::FileStat { path, stat, digest } => {
            let Some(digest) = digest else {
                return false;
            };
            let Ok(current) = host.metadata_fresh(path, Follow::Yes) else {
                return false;
            };
            current.mode == stat.mode && hash_file(host, path, &current).as_ref() == Ok(digest)
        }
        InputStamp::FileHash { path, digest } => {
            memo.map_or_else(
                || file_digest(host, path),
                |memo| memo.file_digest(host, path),
            )
            .as_ref()
                == Ok(digest)
        }
        InputStamp::DirEntries { path, digest } => {
            directory_digest(host, path).as_ref() == Ok(digest)
        }
        InputStamp::TreeListing {
            path,
            digest,
            policy,
        } => {
            let current = match policy {
                TreePolicy::Ordinary => memo.map_or_else(
                    || tree_listing_digest(host, path),
                    |memo| memo.tree_digest(host, path),
                ),
                TreePolicy::Sdk => memo.map_or_else(
                    || sdk_tree_listing_digest(host, path),
                    |memo| memo.sdk_digest(host, path),
                ),
            };
            current.as_ref() == Ok(digest)
        }
        InputStamp::Missing { path } => missing(host, path),
        InputStamp::EnvVar { name, digest } => {
            !name.is_empty()
                && !name.iter().any(|byte| matches!(byte, 0 | b'='))
                && host.env_get(name).map(|value| Digest::hash(&value)) == *digest
        }
        InputStamp::NegativeLookup {
            root,
            relative,
            deepest,
            listing,
        } => {
            if !root.is_absolute() || !missing(host, &root.join(relative)) {
                return false;
            }
            let mut parent = root.to_owned();
            let mut components = relative.components().peekable();
            while let Some(Component::Normal(next)) = components.next() {
                let candidate = parent.join(next);
                if missing(host, &candidate) {
                    return components.all(|part| matches!(part, Component::Normal(_)))
                        && parent == *deepest
                        && host
                            .metadata(&parent, Follow::Yes)
                            .is_ok_and(|meta| meta.kind == FileKind::Dir)
                        && directory_digest(host, &parent).as_ref() == Ok(listing);
                }
                if components.peek().is_none()
                    || !host
                        .metadata(&candidate, Follow::Yes)
                        .is_ok_and(|meta| meta.kind == FileKind::Dir)
                {
                    return false;
                }
                parent = candidate;
            }
            false
        }
    }
}

pub(crate) fn missing(host: &dyn Host, path: &Path) -> bool {
    path.is_absolute()
        && host
            .metadata_fresh(path, Follow::No)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
}

/// Return `true` if no process of the current user can change the object.
///
/// The object must have an owner that is not the effective user, no group or
/// other write permission, and the effective user must not be the superuser.
/// Access control lists are outside this check.
pub(crate) fn immutable(metadata: &Metadata, effective_uid: Option<u32>) -> bool {
    effective_uid.is_some_and(|uid| uid != 0)
        && metadata
            .owner
            .is_some_and(|owner| Some(owner) != effective_uid)
        && metadata.mode & 0o022 == 0
}

/// Return `true` if fresh directory stamps on `host` can detect listing changes.
fn stamps_reliable(host: &dyn Host) -> bool {
    !cfg!(target_arch = "wasm32") && host.directory_times_track_names()
}

/// Return the stamps if a later fresh check of them can validate the listing.
///
/// An empty, incomplete, or truncated walk, or any unusable stamp, gives no stamps.
fn usable_stamps(
    host: &dyn Host,
    stamps: tree::DirStamps,
    links_complete: bool,
) -> Vec<tree::DirStamp> {
    let now = host.now();
    let uid = host.effective_uid();
    if links_complete
        && stamps.complete
        && !stamps.dirs.is_empty()
        && stamps.dirs.iter().all(|stamp| stamp.usable(now, uid))
    {
        stamps.dirs
    } else {
        Vec::new()
    }
}
