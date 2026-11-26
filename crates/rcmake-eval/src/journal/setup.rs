//! Setup spans and setup checkpoints of configure replay.
//!
//! Setup is the part of a configure that loads the toolchain file, the system and
//! compiler modules, the project include hooks, and dependency providers. Setup
//! runs live in each configure. Replay uses no recorded listfile:
//!
//! - while a setup span is open;
//! - before the first outer `project()` call returns successfully;
//! - after a difference in the setup inputs or in a setup checkpoint.
//!
//! A setup checkpoint is a digest of the interpreter state that a later listfile
//! can observe. Replay takes one checkpoint each time an outermost setup span
//! ends. Each checkpoint must equal the checkpoint at the same position in the
//! previous successful configure. A difference disables hits for the remainder
//! of the configure. Hits before the difference stay valid, because the setup
//! that changed had not run yet when those listfiles replayed.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host};
use rcmake_json::{FieldError, FromJson, Value};
use rcmake_model::{Cache, CacheEntry, DirId};

use super::codec;
use super::frame::{EncodeError, EncodeJson, JsonSink};
use super::inputs::ReplaySetupInputs;
use super::trace::JournalHasher;
use super::{JournalDigest, file_digest};
use crate::{Command, Cx, Interp};

/// Version of the setup record. A record with another version is not decoded,
/// so a manifest from an older layout gives no replay hits.
const SETUP_VERSION: u64 = 1;

/// Most names in the cache ledger and the setup environment reads.
const MAX_LEDGER: usize = 16384;

/// One named part of a setup digest.
pub(super) type Domain = (String, JournalDigest);

/// Names with value digests. `None` marks an absent value.
pub(super) type Digests = Vec<(Vec<u8>, Option<JournalDigest>)>;

/// Setup data saved with the replay manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SetupRecord {
    /// Digests of the user-supplied inputs, one for each input label.
    pub(super) inputs: Vec<Domain>,
    /// Cache entries that the first setup checkpoint checks, with their digests.
    /// `None` marks a removed entry.
    pub(super) cache: Digests,
    /// One list of domain digests for each setup checkpoint, in order.
    pub(super) checkpoints: Vec<Vec<Domain>>,
    /// For each checkpoint, `true` when its setup ran a system or compiler
    /// determination module.
    pub(super) determined: Vec<bool>,
    /// Environment variables that setup read, with their value digests.
    pub(super) env: Digests,
    /// Setup files and their content digests. Only the replay report uses them.
    pub(super) sources: Vec<(PathBuf, Option<JournalDigest>)>,
    /// Effective command-line and preset cache edits, with value digests. `None`
    /// marks a removal.
    pub(super) edits: Digests,
}

/// Setup state of one configure attempt.
pub(crate) struct SetupSession {
    host: std::sync::Arc<dyn Host>,
    previous: Option<SetupRecord>,
    record: SetupRecord,
    loaded: BTreeMap<BString, JournalDigest>,
    depth: u32,
    complete: bool,
    gated: bool,
    miss: Option<String>,
    env_overflow: bool,
    determined: bool,
}

impl SetupSession {
    /// Start the setup state. `previous` is `None` when there is no previous manifest,
    /// and `Some(None)` when the previous manifest has no setup record. A configure
    /// with a manifest but no setup record uses no recorded listfile. It records
    /// again, so the next configure can replay.
    pub(super) fn start(
        interp: &Interp,
        host: std::sync::Arc<dyn Host>,
        options: JournalDigest,
        preloads: &[PathBuf],
        previous: Option<Option<SetupRecord>>,
    ) -> Self {
        let inputs = interp.session_data::<ReplaySetupInputs>();
        let mut record = SetupRecord::default();
        let mut domain = |label: &str, digest: JournalDigest| {
            record.inputs.push((label.to_owned(), digest));
        };
        domain("options", options);
        let mut files: Vec<(String, PathBuf)> = preloads
            .iter()
            .map(|path| ("preload".to_owned(), path.clone()))
            .collect();
        if let Some(inputs) = inputs {
            for (label, value) in &inputs.fields {
                let mut hash = JournalHasher::new(b"setup-input");
                hash.field(1, label.as_bytes());
                hash.field(2, value);
                domain(label, hash.finish());
            }
            files.extend(inputs.files.iter().cloned());
        }
        for (label, path) in &files {
            let mut hash = JournalHasher::new(b"setup-file");
            hash.field(1, path.as_os_str().as_encoded_bytes());
            match host.metadata(path, Follow::No) {
                Ok(metadata) => {
                    hash.field(2, format!("{:?}", metadata.kind).as_bytes());
                    if metadata.kind == FileKind::Symlink
                        && let Ok(target) = host.read_link(path)
                    {
                        hash.field(3, target.as_os_str().as_encoded_bytes());
                    }
                }
                Err(_) => hash.field(2, b"missing"),
            }
            if let Ok(resolved) = host.canonicalize_fresh(path) {
                hash.field(4, resolved.as_os_str().as_encoded_bytes());
            }
            domain(&format!("file:{label}"), hash.finish());
            record
                .sources
                .push((path.clone(), file_digest(host.as_ref(), path).ok()));
        }
        if let Some(inputs) = inputs {
            record.edits = inputs
                .edits
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_vec(),
                        value.as_ref().map(|v| JournalDigest::hash(v)),
                    )
                })
                .collect();
        }
        let loaded = match inputs.and_then(|inputs| inputs.loaded_cache.as_ref()) {
            Some(entries) => entries
                .iter()
                .map(|(name, entry)| (name.clone(), ledger_digest(entry)))
                .collect(),
            None => cache_digests(&interp.project.cache),
        };
        let mut session = Self {
            host,
            previous: None,
            record,
            loaded,
            depth: 0,
            complete: false,
            gated: interp.registry.get_lower(b"project").is_some(),
            miss: None,
            env_overflow: false,
            determined: false,
        };
        match previous {
            None => {}
            Some(None) => session.poison("no setup record"),
            Some(Some(previous)) => {
                let changed = session
                    .record
                    .inputs
                    .iter()
                    .zip(&previous.inputs)
                    .find(|(now, before)| now != before)
                    .map(|((label, _), _)| label.clone());
                if let Some(label) = changed {
                    session.poison(&format!("input {label}"));
                } else if previous.inputs.len() != session.record.inputs.len() {
                    session.poison("inputs");
                } else if let Some((name, _)) = previous.env.iter().find(|(name, digest)| {
                    session
                        .host
                        .env_get(name)
                        .map(|value| JournalDigest::hash(&value))
                        != *digest
                }) {
                    let reason = format!("environment {}", String::from_utf8_lossy(name));
                    session.poison(&reason);
                }
                session.previous = Some(previous);
            }
        }
        session
    }

    fn poison(&mut self, reason: &str) {
        if self.miss.is_none() {
            self.miss = Some(reason.to_owned());
        }
    }

    /// `true` when no listfile can replay or record now.
    pub(super) fn blocks(&self) -> bool {
        self.depth > 0 || (self.gated && !self.complete)
    }

    /// `true` when no listfile can replay now.
    pub(super) fn blocks_hits(&self) -> bool {
        self.miss.is_some() || self.blocks()
    }

    /// The reason that replay hits are disabled, if any.
    pub(super) fn miss(&self) -> Option<&str> {
        self.miss.as_deref()
    }

    pub(super) fn record_env(&mut self, name: &[u8], digest: Option<JournalDigest>) {
        if self.depth == 0 || self.record.env.iter().any(|(known, _)| known == name) {
            return;
        }
        if self.record.env.len() >= MAX_LEDGER || name.len() > 4096 {
            self.env_overflow = true;
        } else {
            self.record.env.push((name.to_vec(), digest));
        }
    }

    /// The setup record to save, or `None` when this configure cannot give one.
    pub(super) fn finish(self) -> Option<SetupRecord> {
        (!self.env_overflow && self.depth == 0).then_some(self.record)
    }

    /// Compare a checkpoint with the checkpoint at the same position in the
    /// previous configure.
    ///
    /// A cold configure runs the system and compiler determination modules. They
    /// leave reserved variables that a warm configure never sets, for example
    /// `CMAKE_C_COMPILER_ID_TEST_FLAGS`, and they compute some values again, for
    /// example `_CMAKE_C_IMPLICIT_INCLUDE_DIRECTORIES_INIT`. Thus the variable
    /// domains of a cold and a warm checkpoint are compared only between two
    /// checkpoints of the same kind. All other domains and the cache ledger are
    /// always compared. A recorded listfile also checks each variable that it reads.
    fn checkpoint(&mut self, domains: Vec<Domain>, cache: &Cache) {
        let index = self.record.checkpoints.len();
        if index == 0 {
            self.check_cache(cache);
        }
        let determined = std::mem::take(&mut self.determined);
        let previous = self.previous.as_ref().and_then(|record| {
            Some((
                record.checkpoints.get(index)?,
                *record.determined.get(index)?,
            ))
        });
        match previous {
            // Without a previous record, no listfile can replay anyway.
            None if self.previous.is_none() => {}
            None => self.poison("new setup checkpoint"),
            Some((previous, was_determined)) => {
                if let Some((name, _)) = domains
                    .iter()
                    .zip(previous)
                    .find(|(now, before)| {
                        now != before && (now.0 != "variables" || was_determined == determined)
                    })
                    .map(|(now, _)| now)
                {
                    let reason = format!("checkpoint {index} {name}");
                    self.poison(&reason);
                } else if previous.len() != domains.len() {
                    self.poison("checkpoint domains");
                }
            }
        }
        self.record.checkpoints.push(domains);
        self.record.determined.push(determined);
    }

    /// Check the cache ledger. The ledger holds each entry that differed from the
    /// loaded cache at the first checkpoint of an earlier configure. The current
    /// cache must give the same digest for each ledger entry, and each entry that
    /// now differs from the loaded cache must be in the ledger.
    ///
    /// Example: run 1 starts with no cache, so the ledger holds every entry. Run 2
    /// loads the saved cache, and setup writes the same values again. The ledger
    /// entries keep their digests, and no other entry changed, so run 2 can replay.
    fn check_cache(&mut self, cache: &Cache) {
        let current = cache_digests(cache);
        let mut changed: BTreeMap<Vec<u8>, Option<JournalDigest>> = BTreeMap::new();
        for (name, digest) in &current {
            if self.loaded.get(name) != Some(digest) {
                changed.insert(name.to_vec(), Some(*digest));
            }
        }
        for name in self.loaded.keys() {
            if !current.contains_key(name) {
                changed.insert(name.to_vec(), None);
            }
        }
        let mut ledger = changed.clone();
        if let Some(previous) = &self.previous {
            let known: BTreeMap<_, _> = previous.cache.iter().cloned().collect();
            // The same edit in both configures is the same user input, so its value
            // after setup can differ between a cold and a warm configure.
            let edits: BTreeMap<_, _> = self.record.edits.iter().cloned().collect();
            let same_edit = |name: &[u8]| {
                edits
                    .get(name)
                    .is_some_and(|edit| previous.edits.iter().any(|(n, e)| n == name && e == edit))
            };
            let changed: BTreeMap<_, _> = changed
                .iter()
                .filter(|(name, _)| !same_edit(name))
                .collect();
            if let Some((name, _)) = known.iter().find(|(name, digest)| {
                !same_edit(name) && current.get(name.as_slice()).copied() != **digest
            }) {
                let reason = format!("cache {}", String::from_utf8_lossy(name));
                self.poison(&reason);
            } else if let Some(name) = changed.keys().find(|name| !known.contains_key(**name)) {
                let reason = format!("cache {}", String::from_utf8_lossy(name));
                self.poison(&reason);
            }
            for name in known.into_keys() {
                let digest = current.get(name.as_slice()).copied();
                ledger.insert(name, digest);
            }
        }
        if ledger.len() > MAX_LEDGER {
            self.env_overflow = true;
        }
        self.record.cache = ledger.into_iter().collect();
    }
}

fn cache_digests(cache: &Cache) -> BTreeMap<BString, JournalDigest> {
    cache
        .iter()
        .map(|(name, entry)| (name.clone(), ledger_digest(entry)))
        .collect()
}

/// Digest the value of a cache entry.
///
/// The type, help text, and properties are not part of the digest. Example: a
/// command-line `-DNAME=value` without a type sets the type of an existing entry
/// to `UNINITIALIZED` and replaces its help text. Only a cold configure runs the
/// module command that sets them again, so a warm configure has another type for
/// the same value. No command that replay admits reads these attributes through
/// setup: a listfile that writes the cache checks the complete entries itself.
fn ledger_digest(entry: &CacheEntry) -> JournalDigest {
    let mut hash = JournalHasher::new(b"setup-cache-value");
    hash.field(1, &entry.value);
    hash.finish()
}

/// Return `true` for a CMake-reserved variable name: `CMAKE_*` or `_CMAKE_*`.
///
/// The setup checkpoint covers only reserved names. Command implementations read
/// variables through the observed accessors of [`Cx`], so a recorded listfile
/// checks each variable that it reads. The checkpoint is a second check for the
/// reserved names that drive implicit command behavior, for example the target
/// property initializer `CMAKE_POSITION_INDEPENDENT_CODE`. Other names are local
/// to scripts. Example: the stock `CMakeDetermineSystem.cmake` leaves `val`,
/// `_uname_result`, and `_INCLUDED_TOOLCHAIN_FILE` only in a cold configure.
fn reserved_name(name: &[u8]) -> bool {
    name.starts_with(b"CMAKE_") || name.starts_with(b"_CMAKE_")
}

impl Interp {
    /// Note that the running setup span determines the system or a compiler.
    pub(crate) fn note_replay_determination(&mut self) {
        if let Some(setup) = &mut self.replay.setup {
            setup.determined = true;
        }
    }

    /// Start a setup span. Each call needs one later call of [`Self::end_replay_setup`].
    pub(crate) fn begin_replay_setup(&mut self) {
        if let Some(setup) = &mut self.replay.setup {
            setup.depth = setup.depth.saturating_add(1);
        }
    }

    /// End a setup span. At the end of the outermost span, take a setup checkpoint.
    /// The first `project()` call (`project` is `true`) that returns successfully
    /// ends the initial setup.
    pub(crate) fn end_replay_setup(&mut self, project: bool, ok: bool) {
        let Some(setup) = &mut self.replay.setup else {
            return;
        };
        setup.depth = setup.depth.saturating_sub(1);
        if setup.depth > 0 {
            return;
        }
        if !ok {
            setup.poison("setup failed");
            return;
        }
        if project {
            setup.complete = true;
        }
        if !setup.complete {
            return;
        }
        let domains = self.setup_domains();
        if let Some(setup) = &mut self.replay.setup {
            setup.checkpoint(domains, &self.project.cache);
        }
    }

    /// Digest each domain of the interpreter state that a listfile can observe after
    /// setup. Each domain digest must change when its state changes.
    pub(super) fn setup_domains(&self) -> Vec<Domain> {
        let mut domains = Vec::new();
        let mut add =
            |name: &str, hash: JournalHasher| domains.push((name.to_owned(), hash.finish()));

        let mut hash = JournalHasher::new(b"setup-variables");
        for name in self.vars.visible_names() {
            let Some(value) = self.vars.get(&name) else {
                continue;
            };
            // A normal binding with the value of its cache entry reads the same as
            // the cache entry. A cold configure of an absolute toolchain path
            // creates such a binding for `CMAKE_TOOLCHAIN_FILE`, and a warm one does
            // not. The cache ledger checks the cache value.
            let shadows_same = self
                .project
                .cache
                .get(&name)
                .is_some_and(|entry| entry.value == *value);
            if !reserved_name(&name) || shadows_same {
                continue;
            }
            hash.field(1, &name);
            hash.field(2, value);
        }
        add("variables", hash);

        let mut hash = JournalHasher::new(b"setup-policies");
        hash.field(1, format!("{:?}", self.policies).as_bytes());
        add("policies", hash);

        let mut hash = JournalHasher::new(b"setup-diagnostics");
        hash.field(1, format!("{:?}", self.diags).as_bytes());
        hash.field(2, &[self.log_level.map_or(255, |level| level as u8)]);
        add("diagnostics", hash);

        let mut hash = JournalHasher::new(b"setup-global");
        let global = &self.project.global;
        for name in global.props.names() {
            hash.field(1, &name);
            if let Some(value) = global.props.get(&name) {
                hash.field(2, value);
            }
        }
        hash.field(3, format!("{:?}", global.defined_props).as_bytes());
        hash.field(4, format!("{:?}", global.instrumentation).as_bytes());
        add("global", hash);

        let mut hash = JournalHasher::new(b"setup-directories");
        let mut dirs = vec![DirId::ROOT];
        if self.current_dir_id() != DirId::ROOT {
            dirs.push(self.current_dir_id());
        }
        for dir in dirs {
            let directory = self.project.dir(dir);
            hash.field(1, &directory.binary_dir);
            for name in directory.props.names() {
                // `MACROS` names each macro definition. The command domain holds
                // the definitions outside the stock modules, and a cold configure
                // defines more stock macros than a warm one.
                if name == "MACROS" {
                    continue;
                }
                hash.field(2, &name);
                if let Some(value) = directory.props.get(&name) {
                    hash.field(3, value);
                }
            }
            hash.field(4, format!("{:?}", directory.usage).as_bytes());
            hash.field(5, &directory.define_flags);
            hash.field(6, format!("{:?}", directory.source_groups).as_bytes());
            hash.field(7, format!("{:?}", directory.source_props).as_bytes());
            hash.field(8, format!("{:?}", directory.generated_sources).as_bytes());
            hash.field(9, format!("{:?}", directory.local_targets).as_bytes());
        }
        add("directories", hash);

        let mut hash = JournalHasher::new(b"setup-targets");
        for (_, target) in self.project.targets() {
            // Each field that a listfile can observe. An internal change counter of
            // the target is not part of the digest.
            let fields = (
                (&target.name, &target.kind, &target.origin, &target.dir),
                (&target.backtrace, &target.policies, &target.props),
                (&target.usage, &target.sources, &target.file_sets),
                (&target.utility_deps, &target.event_commands),
            );
            hash.field(1, format!("{fields:?}").as_bytes());
        }
        for (name, binding) in self.project.global_target_bindings() {
            hash.field(2, name);
            hash.field(3, format!("{binding:?}").as_bytes());
        }
        add("targets", hash);

        let mut hash = JournalHasher::new(b"setup-commands");
        let modules = self.config.cmake_root.join("Modules");
        let modules = modules.as_os_str().as_encoded_bytes();
        for (name, command) in &self.scripted {
            match command {
                Command::Builtin(_, flags, raw) => {
                    hash.field(1, name);
                    hash.field(2, format!("{flags:?} {raw}").as_bytes());
                }
                Command::User(user) => {
                    // Listfile replay rejects every call of a scripted command, so a
                    // definition can change a recorded listfile only through that
                    // check. A stock module definition depends only on the module
                    // tree, but the cold and warm configures define different sets.
                    let file = &user.body.file;
                    if file.starts_with(modules)
                        && file.get(modules.len()).is_some_and(|byte| *byte == b'/')
                    {
                        continue;
                    }
                    hash.field(3, name);
                    hash.field(4, format!("{:?}", user.kind).as_bytes());
                    hash.field(5, &user.name);
                    for param in &user.params {
                        hash.field(6, param);
                    }
                    hash.field(7, file);
                    hash.field(8, &user.line.to_le_bytes());
                    hash.field(9, format!("{:?}", user.policies).as_bytes());
                    hash.field(10, format!("{:?}", user.diags).as_bytes());
                    for cmd in user.body.cmds.iter() {
                        hash.field(11, &cmd.name);
                        hash.field(12, &cmd.line.to_le_bytes());
                        for arg in &cmd.args {
                            hash.field(13, format!("{:?}", arg.kind).as_bytes());
                            hash.field(14, &arg.text);
                            hash.field(15, &arg.line.to_le_bytes());
                        }
                    }
                }
            }
        }
        add("commands", hash);

        let mut hash = JournalHasher::new(b"setup-hooks");
        hash.field(1, format!("{:?}", self.hooks.watches).as_bytes());
        hash.field(2, &self.hooks.next_defer_id.to_le_bytes());
        for frame in std::iter::once(&self.root_frame).chain(&self.dir_stack) {
            hash.field(3, format!("{:?}", frame.defer).as_bytes());
        }
        add("hooks", hash);

        domains
    }
}

impl Cx<'_> {
    /// Start a setup span: `project()`, language setup, or a dependency provider
    /// call. Configure replay uses and records no listfile until the span ends.
    pub fn begin_replay_setup(&mut self) {
        self.interp.begin_replay_setup();
    }

    /// End a language setup span or a dependency provider span that
    /// [`Self::begin_replay_setup`] started. `ok` is `false` when the span failed.
    pub fn end_replay_setup(&mut self, ok: bool) {
        self.interp.end_replay_setup(false, ok);
    }

    /// End the setup span of one `project()` call. `ok` is `false` when the call
    /// failed.
    pub fn end_replay_project(&mut self, ok: bool) {
        self.interp.end_replay_setup(true, ok);
    }

    /// Note that the running setup determines the system or a compiler: it runs
    /// `CMakeDetermineSystem.cmake` or `CMakeDetermine<LANG>Compiler.cmake`.
    pub fn note_replay_determination(&mut self) {
        self.interp.note_replay_determination();
    }
}

fn encode_digests(
    sink: &mut JsonSink<'_>,
    items: &[(Vec<u8>, Option<JournalDigest>)],
) -> Result<(), EncodeError> {
    sink.array(items, |s, (name, digest)| {
        s.pair(|s| s.bytes(name), |s| digest.encode(s))
    })
}

fn decode_digests(value: &Value) -> Result<Digests, FieldError> {
    codec::from_list(value, |item| {
        let (name, digest) = codec::from_pair(item)?;
        Ok((codec::from_bytes(name)?, Option::from_json(digest)?))
    })
}

fn encode_domains(sink: &mut JsonSink<'_>, items: &[Domain]) -> Result<(), EncodeError> {
    sink.array(items, |s, (name, digest)| {
        s.pair(|s| s.str(name), |s| digest.encode(s))
    })
}

fn decode_domains(value: &Value) -> Result<Vec<Domain>, FieldError> {
    codec::from_list(value, |item| {
        let (name, digest) = codec::from_pair(item)?;
        Ok((String::from_json(name)?, JournalDigest::from_json(digest)?))
    })
}

impl EncodeJson for SetupRecord {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("cache", |s| encode_digests(s, &self.cache))?;
            obj.field("checkpoints", |s| {
                s.array(&self.checkpoints, |s, domains| encode_domains(s, domains))
            })?;
            obj.field("determined", |s| self.determined.encode(s))?;
            obj.field("edits", |s| encode_digests(s, &self.edits))?;
            obj.field("env", |s| encode_digests(s, &self.env))?;
            obj.field("inputs", |s| encode_domains(s, &self.inputs))?;
            obj.field("sources", |s| {
                s.array(&self.sources, |s, (path, digest)| {
                    s.pair(|s| s.path(path), |s| digest.encode(s))
                })
            })?;
            obj.field("version", |s| SETUP_VERSION.encode(s))
        })
    }
}

impl FromJson for SetupRecord {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = codec::fields(
            value,
            &[
                "version",
                "inputs",
                "cache",
                "checkpoints",
                "determined",
                "edits",
                "env",
                "sources",
            ],
        )?;
        if codec::field(obj, "version", u64::from_json)? != SETUP_VERSION {
            return Err(FieldError::invalid("Unsupported setup record version."));
        }
        Ok(Self {
            inputs: codec::field(obj, "inputs", decode_domains)?,
            cache: codec::field(obj, "cache", decode_digests)?,
            checkpoints: codec::field(obj, "checkpoints", |value| {
                codec::from_list(value, decode_domains)
            })?,
            determined: codec::field(obj, "determined", Vec::<bool>::from_json)?,
            edits: codec::field(obj, "edits", decode_digests)?,
            env: codec::field(obj, "env", decode_digests)?,
            sources: codec::field(obj, "sources", |value| {
                codec::from_list(value, |item| {
                    let (path, digest) = codec::from_pair(item)?;
                    Ok((codec::from_path(path)?, Option::from_json(digest)?))
                })
            })?,
        })
    }
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
