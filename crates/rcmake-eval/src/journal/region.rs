use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rcmake_diag::{Backtrace, Frame, FrameKind};
use rcmake_model::{CacheEntry, CacheType, PropertyMap};
use serde::{Deserialize, Serialize};

use super::trace::{
    JournalDigest, JournalHasher, JournalInput, PropertyKey, PropertyOwner, StateKey, VariableScope,
};

pub(super) const MAX_INPUTS: usize = 50_000;
pub(super) const MAX_EFFECTS: usize = 200_000;

/// An evaluation unit that can have its own journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegionKind {
    /// Whole configure.
    Configure,
    /// Evaluation of one `add_subdirectory` call.
    Subdirectory,
    /// Cold initialization of one enabled language.
    Bootstrap {
        /// Language name as bytes.
        language: Vec<u8>,
    },
}

/// Stable identity of a region across configure runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionKey {
    /// Absolute binary directory of the region.
    pub binary_dir: PathBuf,
    /// Absolute source directory of the region.
    pub source_dir: PathBuf,
    /// Kind of evaluation performed in the region.
    pub kind: RegionKind,
}

/// Context that must match before the region's inputs are checked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryContext {
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A file glob has no verified input trace.
    FileGlobUncached,
    /// The input or effect count exceeded the journal limit.
    LimitExceeded,
}

/// Whether the evaluator tracks the inputs and effects of an operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommandClass {
    /// The evaluator could not classify the operation.
    #[default]
    Unknown,
    /// The evaluator tracks all inputs and observable effects of the operation.
    Tracked,
    /// The evaluator knows why the operation cannot be replayed.
    Opaque(OpaqueReason),
}

/// Byte-preserving snapshot of a CMake cache entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyOrigin {
    /// Absolute binary directory of the caller.
    pub binary_dir: PathBuf,
    frames: Vec<RecordedFrame>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RecordedFrame {
    file: Vec<u8>,
    line: u32,
    name: Option<Vec<u8>>,
    kind: RecordedFrameKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum RecordedFrameKind {
    Command,
    Deferred,
    ModuleInclude,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropertyEdit {
    /// Set a value, or unset the property for `None`.
    Set(Option<Vec<u8>>),
    /// Append one CMake list item.
    AppendList(Vec<u8>),
    /// Append bytes without a list separator.
    AppendString(Vec<u8>),
}

/// Output stream of a configure message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One observable effect in execution order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Rendered diagnostic or message that passed the recorder's diagnostic policy.
    Diagnostic {
        /// Stream that received the message.
        stream: OutputStream,
        /// Exact bytes written to the stream.
        bytes: Vec<u8>,
    },
    /// Bytes appended to the configure log.
    ConfigureLog(Vec<u8>),
    /// A written file whose final contents must still be present before replay.
    GeneratedFile {
        /// Absolute path of the written file.
        path: PathBuf,
        /// Digest of its contents after the write.
        digest: JournalDigest,
    },
}

impl JournalEffect {
    pub(super) fn written_state(&self) -> Option<StateKey> {
        match self {
            Self::SetVar { scope, name, .. } => Some(StateKey::Variable {
                scope: *scope,
                name: name.clone(),
            }),
            Self::SetCache { name, .. } => Some(StateKey::Cache(name.clone())),
            Self::Property { key, .. } => Some(StateKey::Property(key.clone())),
            Self::Diagnostic { .. } | Self::ConfigureLog(_) | Self::GeneratedFile { .. } => None,
        }
    }

    pub(super) fn reads_written_state(&self) -> bool {
        matches!(
            self,
            Self::Property {
                edit: PropertyEdit::AppendList(_) | PropertyEdit::AppendString(_),
                ..
            }
        )
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
            Self::GeneratedFile { path, .. } => path.is_absolute(),
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
        if let Some(key) = effect.written_state() {
            if effect.reads_written_state()
                && !self.written.contains(&key)
                && !self.read.contains_key(&key)
            {
                return Err(OpaqueReason::ExternalMutation);
            }
            self.written.insert(key);
        }
        self.effects.push(effect);
        Ok(())
    }
}

/// Recording state. An opaque state retains the first reason and discards its journal.
#[derive(Clone, Debug)]
pub enum RegionState {
    /// Inputs and effects are still being recorded.
    Recording(Journal),
    /// An operation prevents replay.
    Opaque(OpaqueReason),
}

impl Default for RegionState {
    fn default() -> Self {
        Self::Recording(Journal::default())
    }
}

impl RegionState {
    /// Record the evaluator's classification before performing an operation.
    pub fn record_command(&mut self, class: CommandClass) {
        match class {
            CommandClass::Tracked => {}
            CommandClass::Opaque(reason) => self.mark_opaque(reason),
            CommandClass::Unknown => self.mark_opaque(OpaqueReason::UnclassifiedCommand),
        }
    }

    /// Stop recording and retain the first reason replay is unsafe.
    pub fn mark_opaque(&mut self, reason: OpaqueReason) {
        if matches!(self, Self::Recording(_)) {
            *self = Self::Opaque(reason);
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum RegionPayload {
    Complete {
        inputs: Vec<JournalInput>,
        effects: Vec<JournalEffect>,
    },
    Opaque(OpaqueReason),
}

/// Recorded region keyed by its binary directory, source directory, and kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
