use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use rcmake_host::{Host, path};
use rcmake_json::{FieldError, FromJson, ReadOptions, ToJson, Value, parse, req};
use rcmake_model::{AppendMode, DirId, ModelError, ProjectModel, PropOrigin, PropScope};

use crate::Cx;

use super::codec::fields;
use super::frame::{MAX_FRAME_BYTES, encoded_size, write_json};
use super::region::{
    EntryContext, JournalEffect, MAX_EFFECTS, MAX_INPUTS, OpaqueOperation, PropertyEdit, RegionKey,
    RegionKind, RegionPayload, RegionRecord,
};
use super::trace::{
    JournalDigest, JournalInput, JournalStamp, PropertyOwner, StateKey, StateLookup, VariableScope,
};

/// Versioned manifest of a successful whole configure.
///
/// The caller supplies a portable model snapshot that uses stable identifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigureSnapshot<M = ()> {
    pub(super) schema: String,
    pub(super) region: RegionRecord,
    pub(super) model: M,
}

/// Version of the frame encoding. It also names the manifest file.
const FRAME_SCHEMA: &str = "configure.v1";

const READ_OPTIONS: ReadOptions = ReadOptions {
    allow_comments: false,
    dup_keys_last_wins: false,
};

impl<M> ConfigureSnapshot<M> {
    /// Version of the replay records. The frame encoding and its file name still use version 1.
    pub const SCHEMA: &'static str = "configure.v4";

    /// File name for the manifest under `CMakeFiles/rcmake`.
    pub const FILE_NAME: &'static str = "configure.v1.bin";

    /// Create a manifest from a completed whole configure and its portable model.
    pub fn new(region: RegionRecord, model: M) -> Result<Self, ReplayMiss> {
        if region.key.kind != RegionKind::Configure {
            return Err(ReplayMiss::new(&region.key, ReplayMissReason::Identity));
        }
        if let RegionPayload::Opaque(operation) = region.payload {
            return Err(ReplayMiss::new(
                &region.key,
                ReplayMissReason::Opaque(operation),
            ));
        }
        Ok(Self::from_region(region, model))
    }

    pub(super) fn from_region(region: RegionRecord, model: M) -> Self {
        Self {
            schema: Self::SCHEMA.into(),
            region,
            model,
        }
    }
}

impl<M: ToJson> ConfigureSnapshot<M> {
    /// Encode this snapshot in a length-checked native-path frame.
    pub fn encode(&self) -> Result<Vec<u8>, ReplayCodecError> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(FRAME_MAGIC);
        bytes.push(PATH_ENCODING);
        let length_field = bytes.len()..FRAME_HEADER;
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        write_json(self, &mut bytes, MAX_FRAME_BYTES).ok_or(ReplayCodecError::Payload)?;
        let len = (bytes.len() - FRAME_HEADER) as u32;
        bytes
            .get_mut(length_field)
            .ok_or(ReplayCodecError::Payload)?
            .copy_from_slice(&len.to_le_bytes());
        let checksum = JournalDigest::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }
}

impl<M: FromJson> ConfigureSnapshot<M> {
    /// Decode a snapshot after checking the frame version, platform, and size.
    ///
    /// Limit host reads to the size returned by `frame_limit` before calling this method.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayCodecError> {
        if bytes.len() > Self::frame_limit() {
            return Err(ReplayCodecError::Frame);
        }
        let (body, checksum) = bytes
            .split_at_checked(bytes.len().saturating_sub(FRAME_CHECKSUM))
            .ok_or(ReplayCodecError::Frame)?;
        if JournalDigest::hash(body).as_bytes() != checksum {
            return Err(ReplayCodecError::Frame);
        }
        let (_, len_and_payload) = body
            .strip_prefix(FRAME_MAGIC)
            .and_then(|bytes| bytes.split_first())
            .filter(|(encoding, _)| **encoding == PATH_ENCODING)
            .ok_or(ReplayCodecError::Frame)?;
        let (len, payload) = len_and_payload
            .split_first_chunk::<4>()
            .ok_or(ReplayCodecError::Frame)?;
        if payload.len() > MAX_FRAME_BYTES || u32::from_le_bytes(*len) as usize != payload.len() {
            return Err(ReplayCodecError::Frame);
        }
        let snapshot = parse(payload, &READ_OPTIONS)
            .ok()
            .and_then(|value| Self::from_json(&value).ok())
            .ok_or(ReplayCodecError::Payload)?;
        if snapshot.schema != Self::SCHEMA {
            return Err(ReplayCodecError::Frame);
        }
        Ok(snapshot)
    }
}

impl<M: ToJson> ToJson for ConfigureSnapshot<M> {
    fn to_json(&self) -> Value {
        Value::object_from([
            ("schema", Value::string(FRAME_SCHEMA)),
            ("replay_schema", self.schema.to_json()),
            ("region", self.region.to_json()),
            ("model", self.model.to_json()),
        ])
    }
}

impl<M: FromJson> FromJson for ConfigureSnapshot<M> {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["schema", "replay_schema", "region", "model"])?;
        if req::<String>(obj, "schema")? != FRAME_SCHEMA {
            return Err(FieldError::invalid("unknown frame schema").in_field("schema"));
        }
        Ok(Self {
            schema: req(obj, "replay_schema")?,
            region: req(obj, "region")?,
            model: req(obj, "model")?,
        })
    }
}

impl<M> ConfigureSnapshot<M> {
    /// Maximum bytes to read for one snapshot, including the header.
    #[must_use]
    pub const fn frame_limit() -> usize {
        FRAME_HEADER + MAX_FRAME_BYTES + FRAME_CHECKSUM
    }
}

const FRAME_MAGIC: &[u8] = b"RCMAKE-CONFIGURE\0\x01";
const FRAME_HEADER: usize = FRAME_MAGIC.len() + 5;
const FRAME_CHECKSUM: usize = 32;
const PATH_ENCODING: u8 = if cfg!(windows) {
    1
} else if cfg!(unix) {
    2
} else {
    3
};

/// Failure to encode or decode a journal snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReplayCodecError {
    /// Unsupported format or incorrect frame length.
    #[error("Invalid configure journal frame.")]
    Frame,
    /// The payload is invalid or exceeds its byte limit.
    #[error("Invalid or oversized configure journal payload.")]
    Payload,
}

/// Region and first reason that required a fresh evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayMiss {
    /// Region that will run again.
    pub region: Box<RegionKey>,
    /// First reason the record could not replay.
    pub reason: ReplayMissReason,
}

/// First reason a manifest or region could not be reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayMissReason {
    /// The manifest version is unsupported.
    Schema,
    /// The source directory, binary directory, or region kind differs.
    Identity,
    /// The entry context fields that changed.
    Context(Vec<&'static str>),
    /// The first operation that prevents replay.
    Opaque(OpaqueOperation),
    /// The record exceeds its limits or has inconsistent fields.
    InvalidRecord,
    /// A checked input changed, became unavailable, or could not be read.
    Input {
        /// Zero-based input index.
        index: usize,
        /// Path, environment key, or evaluator state that failed validation.
        description: String,
    },
}

impl ReplayMiss {
    fn new(region: &RegionKey, reason: ReplayMissReason) -> Self {
        Self {
            region: Box::new(region.clone()),
            reason,
        }
    }

    /// Action taken after a replay miss.
    #[must_use]
    pub const fn action(&self) -> &'static str {
        "Evaluate the region again."
    }
}

impl fmt::Display for ReplayMiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Replay miss for {:?} in {} (source {}). ",
            self.region.kind,
            self.region.binary_dir.display(),
            self.region.source_dir.display()
        )?;
        match &self.reason {
            ReplayMissReason::Schema => write!(f, "The journal version differs.")?,
            ReplayMissReason::Identity => write!(f, "The region identity differs.")?,
            ReplayMissReason::Context(fields) => {
                write!(f, "Entry context changed in {}.", fields.join(", "))?
            }
            ReplayMissReason::Opaque(operation) => {
                write!(
                    f,
                    "An opaque operation prevents replay ({:?})",
                    operation.reason
                )?;
                if let Some(command) = &operation.command {
                    write!(
                        f,
                        " for {} at {}:{}",
                        String::from_utf8_lossy(command),
                        operation.file.display(),
                        operation.line
                    )?;
                }
                write!(f, ".")?;
            }
            ReplayMissReason::InvalidRecord => write!(f, "The journal record is invalid.")?,
            ReplayMissReason::Input { description, .. } => {
                write!(f, "Input validation failed for {description}.")?
            }
        }
        write!(f, " {}", self.action())
    }
}

impl std::error::Error for ReplayMiss {}

/// Failure while applying a verified effect to a staged evaluation context.
#[derive(Debug, thiserror::Error)]
pub enum ReplayApplyError {
    /// A directory or target identity is absent from the current model.
    #[error("Cannot find the recorded property owner or caller.")]
    MissingObject,
    /// A cache entry could not be reconstructed.
    #[error("The recorded cache entry is invalid.")]
    InvalidCacheEntry,
    /// A variable value is larger than the current session permits.
    #[error("The recorded variable value exceeds the session limit.")]
    ValueLimit,
    /// The model rejected the effect.
    #[error(transparent)]
    Model(#[from] ModelError),
}

/// A configure manifest whose region inputs matched.
#[derive(Debug)]
pub struct VerifiedConfigure<'a, M> {
    /// Checked portable model, including final cache and property state.
    pub model: &'a M,
    pub(super) region: VerifiedRegion<'a>,
}

impl<'a, M> VerifiedConfigure<'a, M> {
    /// Get the checked entry inputs in observation order.
    #[must_use]
    pub fn inputs(&self) -> &'a [JournalInput] {
        self.region.inputs()
    }

    /// Apply effects outside the model after restoring its final state.
    ///
    /// Stage variable changes until this operation succeeds.
    /// The restored model supplies cache entries and properties.
    pub fn replay<E>(
        self,
        mut apply: impl FnMut(&JournalEffect) -> Result<(), E>,
    ) -> Result<(), E> {
        self.region.replay(|effect| match effect {
            JournalEffect::SetCache { .. } | JournalEffect::Property { .. } => Ok(()),
            _ => apply(effect),
        })
    }
}

/// A region whose entry context and inputs matched.
#[derive(Debug)]
pub struct VerifiedRegion<'a> {
    inputs: &'a [JournalInput],
    effects: &'a [JournalEffect],
}

impl<'a> VerifiedRegion<'a> {
    /// Get the inputs in observation order. Enclosing recorders can record these
    /// inputs again with keys relative to their own region.
    #[must_use]
    pub fn inputs(&self) -> &'a [JournalInput] {
        self.inputs
    }

    /// Replay effects in order through the evaluator's normal mutation APIs.
    ///
    /// The caller must stage mutations until every effect succeeds.
    pub fn replay<E>(
        self,
        mut apply: impl FnMut(&JournalEffect) -> Result<(), E>,
    ) -> Result<(), E> {
        for effect in self.effects {
            apply(effect)?;
        }
        Ok(())
    }

    /// Apply variables, cache entries, and properties through the evaluation APIs.
    ///
    /// Supply a staged evaluation context. Publish it after all effects succeed.
    pub fn replay_into(self, cx: &mut Cx<'_>) -> Result<(), ReplayApplyError> {
        self.replay(|effect| {
            match effect {
                JournalEffect::SetVar { scope, name, value } => {
                    if value
                        .as_ref()
                        .is_some_and(|bytes| bytes.len() > cx.limits().max_value_bytes)
                    {
                        return Err(ReplayApplyError::ValueLimit);
                    }
                    let value = value.as_deref().map(Into::into);
                    match (scope, value) {
                        (VariableScope::Current, Some(value)) => cx.set_var(name, value),
                        (VariableScope::Current, None) => cx.unset_var(name),
                        (VariableScope::Parent, value) => cx.set_parent_scope(name, value),
                    }
                }
                JournalEffect::SetCache { name, entry } => {
                    if let Some(entry) = entry {
                        let entry = entry
                            .to_entry()
                            .ok_or(ReplayApplyError::InvalidCacheEntry)?;
                        cx.insert_cache_entry(name.as_slice().into(), entry);
                    } else {
                        cx.remove_cache_entry(name);
                    }
                }
                JournalEffect::Property { key, edit, origin } => {
                    let model = cx.project();
                    let dir = find_dir(model, &origin.binary_dir)?;
                    let scope = match &key.owner {
                        PropertyOwner::Global => PropScope::Global,
                        PropertyOwner::Directory(path) => {
                            PropScope::Directory(find_dir(model, path)?)
                        }
                        PropertyOwner::Target(key) => {
                            let dir = find_dir(model, &key.binary_dir)?;
                            let (id, _) = model
                                .targets()
                                .find(|(_, target)| {
                                    target.dir == dir
                                        && target.name.as_slice() == key.name.as_slice()
                                })
                                .ok_or(ReplayApplyError::MissingObject)?;
                            PropScope::Target(id)
                        }
                    };
                    let backtrace = origin.backtrace();
                    let origin = PropOrigin {
                        dir,
                        backtrace: &backtrace,
                    };
                    match edit {
                        PropertyEdit::Set(value) => model.set_prop(
                            &scope,
                            &key.name,
                            value.as_deref().map(Into::into),
                            origin,
                        )?,
                        PropertyEdit::AppendList(value) => {
                            model.append_prop(&scope, &key.name, value, AppendMode::List, origin)?
                        }
                        PropertyEdit::AppendString(value) => model.append_prop(
                            &scope,
                            &key.name,
                            value,
                            AppendMode::String,
                            origin,
                        )?,
                    }
                }
            }
            Ok(())
        })
    }
}

fn find_dir(model: &ProjectModel, binary_dir: &Path) -> Result<DirId, ReplayApplyError> {
    let binary_dir = path::from_native_cmake(binary_dir);
    model
        .dirs()
        .find(|(_, dir)| dir.binary_dir == binary_dir)
        .map(|(id, _)| id)
        .ok_or(ReplayApplyError::MissingObject)
}

/// Verify a recorded region against the live host and entry state.
///
/// `lookup` must read without emitting output or changing evaluator state. Return
/// [`StateLookup::Unavailable`] whenever a key cannot be checked.
pub fn verify_region<'a>(
    host: &dyn Host,
    record: &'a RegionRecord,
    key: &RegionKey,
    context: &EntryContext,
    mut lookup: impl FnMut(&StateKey) -> StateLookup,
) -> Result<VerifiedRegion<'a>, ReplayMiss> {
    let miss = |reason| ReplayMiss::new(key, reason);
    if &record.key != key || !key.binary_dir.is_absolute() || !key.source_dir.is_absolute() {
        return Err(miss(ReplayMissReason::Identity));
    }
    let changed = [
        ("rcmake", record.context.rcmake != context.rcmake),
        ("policies", record.context.policies != context.policies),
        ("diag", record.context.diag != context.diag),
        ("log_level", record.context.log_level != context.log_level),
        ("flags", record.context.flags != context.flags),
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect::<Vec<_>>();
    if !changed.is_empty() {
        return Err(miss(ReplayMissReason::Context(changed)));
    }
    let (inputs, effects) = match &record.payload {
        RegionPayload::Complete { inputs, effects } => (inputs, effects),
        RegionPayload::Opaque(operation) => {
            return Err(miss(ReplayMissReason::Opaque(operation.clone())));
        }
    };
    if inputs.len() > MAX_INPUTS
        || effects.len() > MAX_EFFECTS
        || encoded_size(record, MAX_FRAME_BYTES).is_none()
    {
        return Err(miss(ReplayMissReason::InvalidRecord));
    }
    let mut entry_keys = BTreeSet::new();
    for (index, input) in inputs.iter().enumerate() {
        if let JournalInput::State { key, .. } = input
            && !entry_keys.insert(key)
        {
            return Err(miss(ReplayMissReason::InvalidRecord));
        }
        if !input.matches(host, &mut lookup) {
            return Err(miss(ReplayMissReason::Input {
                index,
                description: input_description(input),
            }));
        }
    }

    let mut written = BTreeSet::new();
    for effect in effects {
        if !effect.is_well_formed() {
            return Err(miss(ReplayMissReason::InvalidRecord));
        }
        if let Some(key) = effect.implicit_read()
            && !entry_keys.contains(&key)
            && !written.contains(&key)
        {
            return Err(miss(ReplayMissReason::InvalidRecord));
        }
        written.extend(effect.written_states());
    }
    Ok(VerifiedRegion { inputs, effects })
}

fn input_description(input: &JournalInput) -> String {
    let describe = |name: &[u8]| {
        let prefix = name.get(..256).unwrap_or(name);
        String::from_utf8_lossy(prefix).into_owned()
    };
    match input {
        JournalInput::Host(JournalStamp::EnvVar { name, .. }) => {
            format!("environment variable {}", describe(name))
        }
        JournalInput::Host(JournalStamp::NegativeLookup { root, relative, .. }) => {
            root.join(relative).display().to_string()
        }
        JournalInput::Host(stamp) => stamp
            .path()
            .map_or_else(|| "host input".into(), |path| path.display().to_string()),
        JournalInput::State { key, .. } => match key {
            StateKey::Variable { name, .. } | StateKey::EffectiveVariable(name) => {
                format!("variable {}", describe(name))
            }
            StateKey::Cache(name) => format!("cache entry {}", describe(name)),
            StateKey::Property(key) => {
                format!("property {} on {:?}", describe(&key.name), key.owner)
            }
            StateKey::Policy(id) => format!("policy CMP{id:04}"),
            StateKey::Target(target) => format!(
                "target {} in {}",
                describe(&target.name),
                target.binary_dir.display()
            ),
            StateKey::Command(name) => format!("command {}", describe(name)),
            StateKey::Probe(key) => format!("probe {:02x?}", key.as_bytes()),
        },
    }
}

/// Verify a whole-configure manifest against the live host and entry state.
pub fn verify_configure<'a, M>(
    host: &dyn Host,
    snapshot: &'a ConfigureSnapshot<M>,
    key: &RegionKey,
    context: &EntryContext,
    lookup: impl FnMut(&StateKey) -> StateLookup,
) -> Result<VerifiedConfigure<'a, M>, ReplayMiss> {
    if snapshot.schema != ConfigureSnapshot::<M>::SCHEMA {
        return Err(ReplayMiss::new(key, ReplayMissReason::Schema));
    }
    if key.kind != RegionKind::Configure {
        return Err(ReplayMiss::new(key, ReplayMissReason::Identity));
    }
    let region = verify_region(host, &snapshot.region, key, context, lookup)?;
    Ok(VerifiedConfigure {
        model: &snapshot.model,
        region,
    })
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
