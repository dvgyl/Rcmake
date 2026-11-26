use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_json::{FieldError, FromJson, Object, Value, req};
use rcmake_model::{CacheEntry, CacheType, PropertyMap};

use super::codec::{
    self, decode_variant, field, fields, unknown_variant, variant_data, variant_fields,
};
use super::frame::{EncodeError, EncodeJson, JsonSink, MAX_FRAME_BYTES, encoded_size};
use super::trace::{
    JournalDigest, JournalHasher, JournalInput, PropertyKey, PropertyOwner, StateKey, VariableScope,
};

pub(super) const MAX_INPUTS: usize = 50_000;
pub(super) const MAX_EFFECTS: usize = 200_000;

/// An evaluation unit that can have its own journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionKind {
    /// Whole configure.
    Configure,
    /// Evaluation of one `add_subdirectory` call.
    Subdirectory,
    /// A listfile included by a bootstrap script or by project code.
    Include {
        /// Absolute path to the included file.
        file: PathBuf,
    },
    /// Cold initialization of one enabled language.
    Bootstrap {
        /// Language name as bytes.
        language: Vec<u8>,
    },
}

/// Stable identity of a region across configure runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionKey {
    /// Absolute binary directory of the region.
    pub binary_dir: PathBuf,
    /// Absolute source directory of the region.
    pub source_dir: PathBuf,
    /// Kind of evaluation performed in the region.
    pub kind: RegionKind,
}

/// Context that must match before the region's inputs are checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryContext {
    /// Content identity of the Rcmake executable and its build options.
    pub rcmake: JournalDigest,
    /// Digest of the effective policy stack at entry.
    pub policies: JournalDigest,
    /// Digest of the effective diagnostic state at entry.
    pub diag: JournalDigest,
    /// Active log level at entry.
    pub log_level: u8,
    /// Digest of configure options and the selected generator and toolchain.
    pub flags: JournalDigest,
}

/// Reason a region must be evaluated again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpaqueReason {
    /// The evaluator has no replay classification for the operation.
    #[default]
    UnclassifiedCommand,
    /// The command can run a process whose effects cannot be verified safely.
    ExecuteProcess,
    /// The operation uses a clock or a random source that cannot be verified.
    RandomOrTimestamp,
    /// The operation changed state outside the recorded mutation APIs.
    ExternalMutation,
    /// The input or effect count exceeded the journal limit.
    LimitExceeded,
}

/// First command that prevented replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpaqueOperation {
    /// Reason the operation cannot replay.
    pub reason: OpaqueReason,
    /// Command name, when known.
    pub command: Option<Vec<u8>>,
    /// Source file, when known.
    pub file: PathBuf,
    /// One-based source line. Zero means unknown.
    pub line: u32,
}

/// Byte-preserving snapshot of a CMake cache entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheValue {
    value: Vec<u8>,
    ty: String,
    help: Vec<u8>,
    advanced: bool,
    props: Vec<(Vec<u8>, Vec<u8>)>,
}

impl From<&CacheEntry> for CacheValue {
    fn from(entry: &CacheEntry) -> Self {
        let props = entry.props.names().into_iter().filter_map(|name| {
            let value = entry.props.get(&name)?;
            Some((name.into(), value.to_vec()))
        });
        Self {
            value: entry.value.to_vec(),
            ty: entry.ty.as_str().into(),
            help: entry.help.to_vec(),
            advanced: entry.advanced,
            props: props.collect(),
        }
    }
}

impl CacheValue {
    /// Reconstruct the cache entry for the evaluator's cache mutation API.
    #[must_use]
    pub fn to_entry(&self) -> Option<CacheEntry> {
        let mut props = PropertyMap::default();
        for (name, value) in &self.props {
            if props.get(name).is_some() {
                return None;
            }
            props.set(name.as_slice().into(), value.as_slice().into());
        }
        Some(CacheEntry {
            value: self.value.as_slice().into(),
            ty: CacheType::parse(self.ty.as_bytes())?,
            help: self.help.as_slice().into(),
            advanced: self.advanced,
            props,
        })
    }

    /// Compute the trace digest for the complete cache entry.
    #[must_use]
    pub fn digest(&self) -> JournalDigest {
        let mut hasher = JournalHasher::new(b"cache-entry");
        hasher.field(1, &self.value);
        hasher.field(2, self.ty.as_bytes());
        hasher.field(3, &self.help);
        hasher.field(4, &[u8::from(self.advanced)]);
        for (name, value) in &self.props {
            hasher.field(5, name);
            hasher.field(6, value);
        }
        hasher.finish()
    }
}

/// Recorded call stack stored with a property mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyOrigin {
    /// Absolute binary directory of the caller.
    pub binary_dir: PathBuf,
    frames: Vec<RecordedFrame>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedFrame {
    file: Vec<u8>,
    line: u32,
    name: Option<Vec<u8>>,
    kind: RecordedFrameKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordedFrameKind {
    Command,
    Deferred,
    ModuleInclude,
}

impl RecordedFrameKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Command => "Command",
            Self::Deferred => "Deferred",
            Self::ModuleInclude => "ModuleInclude",
        }
    }
}

impl PropertyOrigin {
    /// Capture the caller directory and its current backtrace.
    #[must_use]
    pub fn new(binary_dir: PathBuf, backtrace: &Backtrace) -> Self {
        let frames = backtrace.frames().map(|frame| RecordedFrame {
            file: frame.file.to_vec(),
            line: frame.line,
            name: frame.name.as_deref().map(<[u8]>::to_vec),
            kind: match frame.kind {
                FrameKind::Command => RecordedFrameKind::Command,
                FrameKind::Deferred => RecordedFrameKind::Deferred,
                FrameKind::ModuleInclude => RecordedFrameKind::ModuleInclude,
            },
        });
        Self {
            binary_dir,
            frames: frames.collect(),
        }
    }

    pub(super) fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Hash the caller directory and every frame field.
    pub(super) fn digest(&self) -> JournalDigest {
        let mut hasher = JournalHasher::new(b"property-origin");
        hasher.field(1, self.binary_dir.as_os_str().as_encoded_bytes());
        for frame in &self.frames {
            hasher.field(2, &frame.file);
            hasher.field(3, &frame.line.to_le_bytes());
            if let Some(name) = &frame.name {
                hasher.field(4, name);
            }
            hasher.field(5, frame.kind.name().as_bytes());
        }
        hasher.finish()
    }

    /// Reconstruct the call stack for the project model's property mutation API.
    #[must_use]
    pub fn backtrace(&self) -> Backtrace {
        self.frames
            .iter()
            .rev()
            .fold(Backtrace::default(), |trace, frame| {
                trace.push(Frame {
                    file: frame.file.as_slice().into(),
                    line: frame.line,
                    name: frame.name.as_deref().map(Into::into),
                    kind: match frame.kind {
                        RecordedFrameKind::Command => FrameKind::Command,
                        RecordedFrameKind::Deferred => FrameKind::Deferred,
                        RecordedFrameKind::ModuleInclude => FrameKind::ModuleInclude,
                    },
                })
            })
    }
}

/// Mutation made to a property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyEdit {
    /// Set a value, or unset the property for `None`.
    Set(Option<Vec<u8>>),
    /// Append one CMake list item.
    AppendList(Vec<u8>),
    /// Append bytes without a list separator.
    AppendString(Vec<u8>),
}

/// One observable effect in execution order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalEffect {
    /// Set or unset a normal variable in the current or parent scope.
    SetVar {
        /// Scope that receives the write.
        scope: VariableScope,
        /// Variable name as bytes.
        name: Vec<u8>,
        /// New value, or absence for an unset operation.
        value: Option<Vec<u8>>,
    },
    /// Replace or remove a complete cache entry.
    SetCache {
        /// Cache name as bytes.
        name: Vec<u8>,
        /// New entry, or absence for a removal.
        entry: Option<CacheValue>,
    },
    /// Change a global, directory, or target property.
    Property {
        /// Property owner and name.
        key: PropertyKey,
        /// Change to apply through the model's property API.
        edit: PropertyEdit,
        /// Caller directory and backtrace for the write.
        origin: PropertyOrigin,
    },
}

impl JournalEffect {
    pub(super) fn implicit_read(&self) -> Option<StateKey> {
        match self {
            Self::SetVar {
                scope: VariableScope::Parent,
                name,
                ..
            } => Some(StateKey::EffectiveVariable(name.clone())),
            Self::Property {
                key,
                edit: PropertyEdit::AppendList(_) | PropertyEdit::AppendString(_),
                ..
            } => Some(StateKey::Property(key.clone())),
            _ => None,
        }
    }

    pub(super) fn written_states(&self) -> impl Iterator<Item = StateKey> {
        let (first, second) = match self {
            Self::SetVar { scope, name, .. } => (
                Some(StateKey::Variable {
                    scope: *scope,
                    name: name.clone(),
                }),
                Some(StateKey::EffectiveVariable(name.clone())),
            ),
            Self::SetCache { name, .. } => (Some(StateKey::Cache(name.clone())), None),
            Self::Property { key, .. } => (Some(StateKey::Property(key.clone())), None),
        };
        first.into_iter().chain(second).chain(match self {
            Self::SetVar {
                scope: VariableScope::Parent,
                name,
                ..
            } => Some(StateKey::Variable {
                scope: VariableScope::Current,
                name: name.clone(),
            }),
            _ => None,
        })
    }

    pub(super) fn is_well_formed(&self) -> bool {
        match self {
            Self::Property { key, origin, .. } => {
                origin.binary_dir.is_absolute()
                    && match &key.owner {
                        PropertyOwner::Global => true,
                        PropertyOwner::Directory(path) => path.is_absolute(),
                        PropertyOwner::Target(target) => target.binary_dir.is_absolute(),
                    }
            }
            _ => true,
        }
    }
}

/// Active recording state for one region.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    inputs: Vec<JournalInput>,
    effects: Vec<JournalEffect>,
    read: BTreeMap<StateKey, Option<JournalDigest>>,
    written: BTreeSet<StateKey>,
    bytes: usize,
}

impl Journal {
    fn record_input(&mut self, input: JournalInput) -> Result<(), OpaqueReason> {
        if let JournalInput::State { key, value } = &input {
            if self.written.contains(key) {
                return Ok(());
            }
            if let Some(previous) = self.read.get(key) {
                return if previous == value {
                    Ok(())
                } else {
                    Err(OpaqueReason::ExternalMutation)
                };
            }
        }
        if self.inputs.len() == MAX_INPUTS {
            return Err(OpaqueReason::LimitExceeded);
        }
        self.account(&input)?;
        if let JournalInput::State { key, value } = &input {
            self.read.insert(key.clone(), *value);
        }
        self.inputs.push(input);
        Ok(())
    }

    fn record_effect(&mut self, effect: JournalEffect) -> Result<(), OpaqueReason> {
        if self.effects.len() == MAX_EFFECTS {
            return Err(OpaqueReason::LimitExceeded);
        }
        if !effect.is_well_formed() {
            return Err(OpaqueReason::ExternalMutation);
        }
        if let Some(key) = effect.implicit_read()
            && !self.written.contains(&key)
            && !self.read.contains_key(&key)
        {
            return Err(OpaqueReason::ExternalMutation);
        }
        self.account(&effect)?;
        self.written.extend(effect.written_states());
        self.effects.push(effect);
        Ok(())
    }
    fn account(&mut self, value: &impl EncodeJson) -> Result<(), OpaqueReason> {
        // Reserve room for the manifest and for commas between records.
        let remaining = MAX_FRAME_BYTES
            .saturating_sub(self.bytes)
            .saturating_sub(4096);
        let size = encoded_size(value, remaining).ok_or(OpaqueReason::LimitExceeded)?;
        self.bytes += size + 1;
        Ok(())
    }
}

/// Recording state. An opaque state retains the first reason and discards its journal.
#[derive(Clone, Debug)]
pub enum RegionState {
    /// Inputs and effects are still being recorded.
    Recording(Journal),
    /// An operation prevents replay.
    Opaque(OpaqueOperation),
}

impl Default for RegionState {
    fn default() -> Self {
        Self::Recording(Journal::default())
    }
}

impl RegionState {
    /// Stop recording and retain the first reason replay is unsafe.
    pub fn mark_opaque(&mut self, reason: OpaqueReason) {
        if matches!(self, Self::Recording(_)) {
            *self = Self::Opaque(OpaqueOperation {
                reason,
                command: None,
                file: PathBuf::new(),
                line: 0,
            });
        }
    }

    /// Record an observed input. For shared state, use the key that supplied the value.
    pub fn record_input(&mut self, input: JournalInput) {
        if let Self::Recording(journal) = self
            && let Err(reason) = journal.record_input(input)
        {
            self.mark_opaque(reason);
        }
    }

    /// Record a successful mutation at the point where the evaluator applies it.
    pub fn record_effect(&mut self, effect: JournalEffect) {
        if let Self::Recording(journal) = self
            && let Err(reason) = journal.record_effect(effect)
        {
            self.mark_opaque(reason);
        }
    }

    /// Finish a region after successful evaluation.
    #[must_use]
    pub fn finish(self, key: RegionKey, context: EntryContext) -> RegionRecord {
        let payload = match self {
            Self::Recording(journal) => RegionPayload::Complete {
                inputs: journal.inputs,
                effects: journal.effects,
            },
            Self::Opaque(reason) => RegionPayload::Opaque(reason),
        };
        RegionRecord {
            key,
            context,
            payload,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RegionPayload {
    Complete {
        inputs: Vec<JournalInput>,
        effects: Vec<JournalEffect>,
    },
    Opaque(OpaqueOperation),
}

/// Recorded region keyed by its binary directory, source directory, and kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionRecord {
    pub(super) key: RegionKey,
    pub(super) context: EntryContext,
    pub(super) payload: RegionPayload,
}

impl RegionRecord {
    /// Get the region identity.
    #[must_use]
    pub fn key(&self) -> &RegionKey {
        &self.key
    }
}

impl EncodeJson for RegionKind {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::Configure => sink.str("Configure"),
            Self::Subdirectory => sink.str("Subdirectory"),
            Self::Include { file } => sink.variant("Include", |s| {
                s.object(|obj| obj.field("file", |s| s.path(file)))
            }),
            Self::Bootstrap { language } => sink.variant("Bootstrap", |s| {
                s.object(|obj| obj.field("language", |s| s.bytes(language)))
            }),
        }
    }
}

impl FromJson for RegionKind {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| match (name, data) {
            ("Configure", None) => Ok(Self::Configure),
            ("Subdirectory", None) => Ok(Self::Subdirectory),
            ("Include", _) => Ok(Self::Include {
                file: field(
                    variant_fields(name, data, &["file"])?,
                    "file",
                    codec::from_path,
                )?,
            }),
            ("Bootstrap", _) => Ok(Self::Bootstrap {
                language: field(
                    variant_fields(name, data, &["language"])?,
                    "language",
                    codec::from_bytes,
                )?,
            }),
            _ => Err(unknown_variant(name)),
        })
    }
}

impl EncodeJson for RegionKey {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("binary_dir", |s| s.path(&self.binary_dir))?;
            obj.field("kind", |s| self.kind.encode(s))?;
            obj.field("source_dir", |s| s.path(&self.source_dir))
        })
    }
}

impl FromJson for RegionKey {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["binary_dir", "source_dir", "kind"])?;
        Ok(Self {
            binary_dir: field(obj, "binary_dir", codec::from_path)?,
            source_dir: field(obj, "source_dir", codec::from_path)?,
            kind: req(obj, "kind")?,
        })
    }
}

impl EncodeJson for EntryContext {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("diag", |s| self.diag.encode(s))?;
            obj.field("flags", |s| self.flags.encode(s))?;
            obj.field("log_level", |s| self.log_level.encode(s))?;
            obj.field("policies", |s| self.policies.encode(s))?;
            obj.field("rcmake", |s| self.rcmake.encode(s))
        })
    }
}

impl FromJson for EntryContext {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["rcmake", "policies", "diag", "log_level", "flags"])?;
        Ok(Self {
            rcmake: req(obj, "rcmake")?,
            policies: req(obj, "policies")?,
            diag: req(obj, "diag")?,
            log_level: req(obj, "log_level")?,
            flags: req(obj, "flags")?,
        })
    }
}

impl EncodeJson for OpaqueReason {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.str(match self {
            Self::UnclassifiedCommand => "UnclassifiedCommand",
            Self::ExecuteProcess => "ExecuteProcess",
            Self::RandomOrTimestamp => "RandomOrTimestamp",
            Self::ExternalMutation => "ExternalMutation",
            Self::LimitExceeded => "LimitExceeded",
        })
    }
}

impl FromJson for OpaqueReason {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        match rcmake_json::str(value)? {
            "UnclassifiedCommand" => Ok(Self::UnclassifiedCommand),
            "ExecuteProcess" => Ok(Self::ExecuteProcess),
            "RandomOrTimestamp" => Ok(Self::RandomOrTimestamp),
            "ExternalMutation" => Ok(Self::ExternalMutation),
            "LimitExceeded" => Ok(Self::LimitExceeded),
            name => Err(unknown_variant(name)),
        }
    }
}

impl EncodeJson for OpaqueOperation {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("command", |s| s.opt_bytes(self.command.as_deref()))?;
            obj.field("file", |s| s.path(&self.file))?;
            obj.field("line", |s| self.line.encode(s))?;
            obj.field("reason", |s| self.reason.encode(s))
        })
    }
}

impl FromJson for OpaqueOperation {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["reason", "command", "file", "line"])?;
        Ok(Self {
            reason: req(obj, "reason")?,
            command: field(obj, "command", codec::from_opt_bytes)?,
            file: field(obj, "file", codec::from_path)?,
            line: req(obj, "line")?,
        })
    }
}

impl EncodeJson for CacheValue {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("advanced", |s| self.advanced.encode(s))?;
            obj.field("help", |s| s.bytes(&self.help))?;
            obj.field("props", |s| s.byte_pairs(&self.props))?;
            obj.field("ty", |s| self.ty.encode(s))?;
            obj.field("value", |s| s.bytes(&self.value))
        })
    }
}

impl FromJson for CacheValue {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["value", "ty", "help", "advanced", "props"])?;
        Ok(Self {
            value: field(obj, "value", codec::from_bytes)?,
            ty: req(obj, "ty")?,
            help: field(obj, "help", codec::from_bytes)?,
            advanced: req(obj, "advanced")?,
            props: field(obj, "props", codec::from_byte_pairs)?,
        })
    }
}

impl EncodeJson for PropertyOrigin {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("binary_dir", |s| s.path(&self.binary_dir))?;
            obj.field("frames", |s| self.frames.encode(s))
        })
    }
}

impl FromJson for PropertyOrigin {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["binary_dir", "frames"])?;
        Ok(Self {
            binary_dir: field(obj, "binary_dir", codec::from_path)?,
            frames: req(obj, "frames")?,
        })
    }
}

impl EncodeJson for RecordedFrame {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("file", |s| s.bytes(&self.file))?;
            obj.field("kind", |s| s.str(self.kind.name()))?;
            obj.field("line", |s| self.line.encode(s))?;
            obj.field("name", |s| s.opt_bytes(self.name.as_deref()))
        })
    }
}

impl FromJson for RecordedFrame {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["file", "line", "name", "kind"])?;
        let kind = field(obj, "kind", |kind| match rcmake_json::str(kind)? {
            "Command" => Ok(RecordedFrameKind::Command),
            "Deferred" => Ok(RecordedFrameKind::Deferred),
            "ModuleInclude" => Ok(RecordedFrameKind::ModuleInclude),
            name => Err(unknown_variant(name)),
        })?;
        Ok(Self {
            file: field(obj, "file", codec::from_bytes)?,
            line: req(obj, "line")?,
            name: field(obj, "name", codec::from_opt_bytes)?,
            kind,
        })
    }
}

impl EncodeJson for PropertyEdit {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::Set(value) => sink.variant("Set", |s| s.opt_bytes(value.as_deref())),
            Self::AppendList(value) => sink.variant("AppendList", |s| s.bytes(value)),
            Self::AppendString(value) => sink.variant("AppendString", |s| s.bytes(value)),
        }
    }
}

impl FromJson for PropertyEdit {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| {
            let data = variant_data(name, data)?;
            match name {
                "Set" => codec::from_opt_bytes(data).map(Self::Set),
                "AppendList" => codec::from_bytes(data).map(Self::AppendList),
                "AppendString" => codec::from_bytes(data).map(Self::AppendString),
                _ => Err(unknown_variant(name)),
            }
        })
    }
}

impl EncodeJson for JournalEffect {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::SetVar { scope, name, value } => sink.variant("SetVar", |s| {
                s.object(|obj| {
                    obj.field("name", |s| s.bytes(name))?;
                    obj.field("scope", |s| scope.encode(s))?;
                    obj.field("value", |s| s.opt_bytes(value.as_deref()))
                })
            }),
            Self::SetCache { name, entry } => sink.variant("SetCache", |s| {
                s.object(|obj| {
                    obj.field("entry", |s| entry.encode(s))?;
                    obj.field("name", |s| s.bytes(name))
                })
            }),
            Self::Property { key, edit, origin } => sink.variant("Property", |s| {
                s.object(|obj| {
                    obj.field("edit", |s| edit.encode(s))?;
                    obj.field("key", |s| key.encode(s))?;
                    obj.field("origin", |s| origin.encode(s))
                })
            }),
        }
    }
}

impl FromJson for JournalEffect {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| {
            let members = |allowed: &[&str]| variant_fields(name, data, allowed);
            let name_of = |obj: &Object| field(obj, "name", codec::from_bytes);
            match name {
                "SetVar" => {
                    let obj = members(&["scope", "name", "value"])?;
                    Ok(Self::SetVar {
                        scope: req(obj, "scope")?,
                        name: name_of(obj)?,
                        value: field(obj, "value", codec::from_opt_bytes)?,
                    })
                }
                "SetCache" => {
                    let obj = members(&["name", "entry"])?;
                    Ok(Self::SetCache {
                        name: name_of(obj)?,
                        entry: req(obj, "entry")?,
                    })
                }
                "Property" => {
                    let obj = members(&["key", "edit", "origin"])?;
                    Ok(Self::Property {
                        key: req(obj, "key")?,
                        edit: req(obj, "edit")?,
                        origin: req(obj, "origin")?,
                    })
                }
                _ => Err(unknown_variant(name)),
            }
        })
    }
}

impl EncodeJson for RegionPayload {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        match self {
            Self::Complete { inputs, effects } => sink.variant("Complete", |s| {
                s.object(|obj| {
                    obj.field("effects", |s| effects.encode(s))?;
                    obj.field("inputs", |s| inputs.encode(s))
                })
            }),
            Self::Opaque(operation) => sink.variant("Opaque", |s| operation.encode(s)),
        }
    }
}

impl FromJson for RegionPayload {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        decode_variant(value, |name, data| match name {
            "Complete" => {
                let obj = variant_fields(name, data, &["inputs", "effects"])?;
                Ok(Self::Complete {
                    inputs: req(obj, "inputs")?,
                    effects: req(obj, "effects")?,
                })
            }
            "Opaque" => OpaqueOperation::from_json(variant_data(name, data)?).map(Self::Opaque),
            _ => Err(unknown_variant(name)),
        })
    }
}

impl EncodeJson for RegionRecord {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("context", |s| self.context.encode(s))?;
            obj.field("key", |s| self.key.encode(s))?;
            obj.field("payload", |s| self.payload.encode(s))
        })
    }
}

impl FromJson for RegionRecord {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["key", "context", "payload"])?;
        Ok(Self {
            key: req(obj, "key")?,
            context: req(obj, "context")?,
            payload: req(obj, "payload")?,
        })
    }
}
