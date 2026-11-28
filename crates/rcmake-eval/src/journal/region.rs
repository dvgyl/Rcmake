use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    calls: CallIds,
}

/// Bit that marks a call identity read from a manifest. Live identities from
/// [`Backtrace::frame_nodes`] are smaller, so the two kinds never collide.
const DECODED_CALL: u64 = 1 << 63;

/// Restored call stack frames of one manifest, by call identity.
type RestoreTable = Mutex<HashMap<u64, Backtrace>>;

/// Call identity of each recorded frame (`cmListFileBacktrace` node identity).
///
/// Values that one command call records share the call stack nodes of that
/// call, and two calls with equal frames (for example two iterations of one
/// `foreach()` body) have different nodes. The File API `backtraceGraph` shows
/// this difference, so a replayed configure restores the same sharing.
///
/// The identities do not change what an origin records: equality and debug
/// output ignore them.
#[derive(Clone, Default)]
struct CallIds {
    /// One identity per frame, in frame order. Empty when unknown.
    ids: Vec<u64>,
    /// Frames already restored from the same manifest.
    table: Option<Arc<RestoreTable>>,
}

impl PartialEq for CallIds {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for CallIds {}

impl std::fmt::Debug for CallIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CallIds")
    }
}

thread_local! {
    /// Ordinals of the call identities in the manifest being encoded.
    static ENCODE_CALLS: RefCell<Option<HashMap<u64, u64>>> = const { RefCell::new(None) };
    /// Restore table of the manifest being decoded.
    static DECODE_TABLE: RefCell<Option<Arc<RestoreTable>>> = const { RefCell::new(None) };
    /// Live call stack at the entry of the region that is being replayed.
    static REPLAY_ENTRY: RefCell<Option<Backtrace>> = const { RefCell::new(None) };
}

/// Scope of one region replay hit.
///
/// The entry context of a region hashes its entry call stack, so on a hit the recorded
/// entry frames equal the live entry frames. While the scope exists, a restored
/// backtrace starts from the live entry nodes instead of new copies. Then a target
/// that the replayed region creates and a later live command on that target share
/// their caller nodes (for example one function call node) in the File API graph.
pub(super) struct EntryScope {
    previous: Option<Backtrace>,
}

impl EntryScope {
    /// Use `entry` as the live entry call stack until the scope ends.
    pub(super) fn enter(entry: &Backtrace) -> Self {
        let previous = REPLAY_ENTRY.with(|slot| slot.replace(Some(entry.clone())));
        Self { previous }
    }
}

impl Drop for EntryScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        REPLAY_ENTRY.with(|slot| *slot.borrow_mut() = previous);
    }
}

/// Scope of one manifest encode or decode. Call identities share one ordinal
/// space (encode) or one restore table (decode) in the scope.
pub(super) struct CallScope {
    encode: bool,
}

impl CallScope {
    /// Number the call identities in first-seen order while the scope exists.
    pub(super) fn encode() -> Self {
        ENCODE_CALLS.with(|calls| *calls.borrow_mut() = Some(HashMap::new()));
        Self { encode: true }
    }

    /// Share one restore table among the origins decoded while the scope exists.
    pub(super) fn decode() -> Self {
        DECODE_TABLE.with(|table| *table.borrow_mut() = Some(Arc::default()));
        Self { encode: false }
    }
}

impl Drop for CallScope {
    fn drop(&mut self) {
        if self.encode {
            ENCODE_CALLS.with(|calls| *calls.borrow_mut() = None);
        } else {
            DECODE_TABLE.with(|table| *table.borrow_mut() = None);
        }
    }
}

impl CallIds {
    /// Return the deterministic ordinals to write. Outside an encode scope, the
    /// ordinals are local to this origin.
    fn ordinals(&self) -> Vec<u64> {
        ENCODE_CALLS.with(|calls| {
            let mut scope = calls.borrow_mut();
            let mut local = HashMap::new();
            let calls = scope.as_mut().unwrap_or(&mut local);
            self.ids
                .iter()
                .map(|id| {
                    let next = calls.len() as u64 + 1;
                    *calls.entry(*id).or_insert(next)
                })
                .collect()
        })
    }

    fn decoded(ordinals: Vec<u64>) -> Self {
        Self {
            ids: ordinals
                .into_iter()
                .map(|ordinal| ordinal | DECODED_CALL)
                .collect(),
            table: DECODE_TABLE.with(|table| table.borrow().clone()),
        }
    }
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
        let (ids, frames) = backtrace
            .frame_nodes()
            .map(|(id, frame)| {
                let frame = RecordedFrame {
                    file: frame.file.to_vec(),
                    line: frame.line,
                    name: frame.name.as_deref().map(<[u8]>::to_vec),
                    kind: match frame.kind {
                        FrameKind::Command => RecordedFrameKind::Command,
                        FrameKind::Deferred => RecordedFrameKind::Deferred,
                        FrameKind::ModuleInclude => RecordedFrameKind::ModuleInclude,
                    },
                };
                (id, frame)
            })
            .unzip();
        Self {
            binary_dir,
            frames,
            calls: CallIds { ids, table: None },
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
    ///
    /// Origins decoded from one manifest share the restored frame of each
    /// recorded call, as the live configure did. Inside an `EntryScope`, the
    /// frames of the live entry call stack keep their live nodes.
    #[must_use]
    pub fn backtrace(&self) -> Backtrace {
        let ids = (self.calls.ids.len() == self.frames.len()).then_some(&self.calls.ids);
        let mut table = self
            .calls
            .table
            .as_ref()
            .filter(|_| ids.is_some())
            .and_then(|table| table.lock().ok());
        let (mut trace, inner) =
            REPLAY_ENTRY.with(|entry| self.live_entry(entry.borrow().as_ref()));
        for (index, frame) in self.frames.iter().enumerate().take(inner).rev() {
            let frame = frame.to_frame();
            let (Some(table), Some(&id)) = (table.as_mut(), ids.and_then(|ids| ids.get(index)))
            else {
                trace = trace.push(frame);
                continue;
            };
            let parent = trace.frame_nodes().next().map(|(id, _)| id);
            match table.get(&id) {
                // Reuse the frame only if it restores the same frame on the same caller.
                Some(restored)
                    if restored.top() == Some(&frame)
                        && restored.frame_nodes().nth(1).map(|(id, _)| id) == parent =>
                {
                    trace = restored.clone();
                }
                _ => {
                    trace = trace.push(frame);
                    table.insert(id, trace.clone());
                }
            }
        }
        trace
    }
}

impl PropertyOrigin {
    /// Return the live entry stack and the number of recorded frames above it, when the
    /// outermost recorded frames equal the frames of `entry`.
    fn live_entry(&self, entry: Option<&Backtrace>) -> (Backtrace, usize) {
        let all = (Backtrace::default(), self.frames.len());
        let Some(entry) = entry.filter(|entry| !entry.is_empty()) else {
            return all;
        };
        let depth = entry.frames().count();
        let Some(inner) = self.frames.len().checked_sub(depth) else {
            return all;
        };
        let outer = self.frames.get(inner..).unwrap_or_default();
        if outer
            .iter()
            .map(RecordedFrame::to_frame)
            .eq(entry.frames().cloned())
        {
            (entry.clone(), inner)
        } else {
            all
        }
    }
}

impl RecordedFrame {
    fn to_frame(&self) -> Frame {
        Frame {
            file: self.file.as_slice().into(),
            line: self.line,
            name: self.name.as_deref().map(Into::into),
            kind: match self.kind {
                RecordedFrameKind::Command => FrameKind::Command,
                RecordedFrameKind::Deferred => FrameKind::Deferred,
                RecordedFrameKind::ModuleInclude => FrameKind::ModuleInclude,
            },
        }
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
            obj.field("calls", |s| self.calls.ordinals().encode(s))?;
            obj.field("frames", |s| self.frames.encode(s))
        })
    }
}

impl FromJson for PropertyOrigin {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = fields(value, &["binary_dir", "calls", "frames"])?;
        let frames: Vec<RecordedFrame> = req(obj, "frames")?;
        let calls: Vec<u64> = req(obj, "calls")?;
        if calls.len() != frames.len() || calls.iter().any(|&call| call & DECODED_CALL != 0) {
            return Err(FieldError::invalid("one call per frame").in_field("calls"));
        }
        Ok(Self {
            binary_dir: field(obj, "binary_dir", codec::from_path)?,
            frames,
            calls: CallIds::decoded(calls),
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

#[cfg(test)]
mod call_tests {
    use std::path::PathBuf;

    use rcmake_diag::{Backtrace, Frame, FrameKind};
    use rcmake_json::{FromJson, ReadOptions, parse};

    use super::super::frame::{EncodeJson, MAX_FRAME_BYTES, write_with};
    use super::{CallScope, PropertyOrigin};

    fn call(line: u32) -> Frame {
        Frame {
            file: b"/s/CMakeLists.txt".as_slice().into(),
            line,
            name: Some(b"install".as_slice().into()),
            kind: FrameKind::Command,
        }
    }

    fn top_id(trace: &Backtrace) -> Option<u64> {
        trace.frame_nodes().next().map(|(id, _)| id)
    }

    #[test]
    fn manifest_round_trip_keeps_call_identity() -> Result<(), Box<dyn std::error::Error>> {
        let file = Backtrace::default().push(Frame {
            file: b"/s/CMakeLists.txt".as_slice().into(),
            line: 0,
            name: None,
            kind: FrameKind::ModuleInclude,
        });
        let first = file.push(call(5));
        // An equal frame of another call, as in the next `foreach()` iteration.
        let second = file.push(call(5));
        let bin = PathBuf::from("/b");
        let origins = vec![
            PropertyOrigin::new(bin.clone(), &first),
            PropertyOrigin::new(bin.clone(), &first),
            PropertyOrigin::new(bin, &second),
        ];
        let encode = || -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            let _scope = CallScope::encode();
            let mut bytes = Vec::new();
            write_with(&mut bytes, MAX_FRAME_BYTES, |sink| origins.encode(sink)).ok_or("encode")?;
            Ok(bytes)
        };
        let bytes = encode()?;
        assert_eq!(bytes, encode()?, "the ordinals are deterministic");

        let decoded: Vec<PropertyOrigin> = {
            let _scope = CallScope::decode();
            Vec::from_json(&parse(&bytes, &ReadOptions::default())?)?
        };
        assert_eq!(decoded, origins);
        let traces: Vec<Backtrace> = decoded.iter().map(PropertyOrigin::backtrace).collect();
        let [a, b, c] = traces.as_slice() else {
            return Err("three traces".into());
        };
        assert_eq!(top_id(a), top_id(b), "one call shares one node");
        assert_ne!(top_id(a), top_id(c), "two calls have two nodes");
        assert_eq!(
            a.frame_nodes().nth(1).map(|(id, _)| id),
            c.frame_nodes().nth(1).map(|(id, _)| id),
            "both calls share the file frame"
        );
        assert!(a.frames().eq(c.frames()));
        Ok(())
    }

    fn module(line: u32, name: &[u8]) -> Frame {
        Frame {
            file: b"/s/cmake/helper.cmake".as_slice().into(),
            line,
            name: Some(name.into()),
            kind: FrameKind::Command,
        }
    }

    #[test]
    fn replayed_origins_share_live_entry_caller_nodes() -> Result<(), Box<dyn std::error::Error>> {
        let file = Backtrace::default().push(Frame {
            file: b"/s/CMakeLists.txt".as_slice().into(),
            line: 0,
            name: None,
            kind: FrameKind::ModuleInclude,
        });
        // `make_lib()` calls `include(lib.cmake)`; `lib.cmake` calls `add_library()`.
        let function_call = file.push(call(3));
        let entry = function_call.push(module(2, b"include"));
        let created = entry.push(Frame {
            file: b"/s/cmake/lib.cmake".as_slice().into(),
            line: 1,
            name: Some(b"add_library".as_slice().into()),
            kind: FrameKind::Command,
        });
        let bin = PathBuf::from("/b");
        let origins = [
            PropertyOrigin::new(bin.clone(), &created),
            PropertyOrigin::new(bin.clone(), &entry),
        ];
        let bytes = {
            let _scope = CallScope::encode();
            let mut bytes = Vec::new();
            write_with(&mut bytes, MAX_FRAME_BYTES, |sink| origins.encode(sink)).ok_or("encode")?;
            bytes
        };

        // A later configure reaches the same entry through new live nodes.
        let live_call = file.push(call(3));
        let live_entry = live_call.push(module(2, b"include"));
        // The live command after the replayed region, on the same function call node.
        let live_after = live_call.push(module(3, b"target_compile_definitions"));
        let node =
            |trace: &Backtrace, depth: usize| trace.frame_nodes().nth(depth).map(|(id, _)| id);

        for decoded in [false, true] {
            let restored: Vec<Backtrace> = {
                let _entry = super::EntryScope::enter(&live_entry);
                let origins: Vec<PropertyOrigin> = if decoded {
                    let _scope = CallScope::decode();
                    Vec::from_json(&parse(&bytes, &ReadOptions::default())?)?
                } else {
                    origins.to_vec()
                };
                origins.iter().map(PropertyOrigin::backtrace).collect()
            };
            let [target, at_entry] = restored.as_slice() else {
                return Err("two traces".into());
            };
            assert!(target.frames().eq(created.frames()), "decoded={decoded}");
            assert!(at_entry.same_call(&live_entry), "decoded={decoded}");
            assert_eq!(
                node(target, 2),
                node(&live_after, 1),
                "the replayed target and the live command share the function call node (decoded={decoded})"
            );
            assert_eq!(node(target, 1), node(&live_entry, 0), "decoded={decoded}");
        }

        // Without a live entry, or with other entry frames, restore new nodes.
        let other_entry = live_call.push(module(9, b"include"));
        for scope in [None, Some(&other_entry)] {
            let _entry = scope.map(super::EntryScope::enter);
            let target = origins[0].backtrace();
            assert!(target.frames().eq(created.frames()));
            assert_ne!(node(&target, 2), node(&live_after, 1));
        }
        Ok(())
    }
}
