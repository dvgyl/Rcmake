use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_host::{Host, HostError, path};
use rcmake_model::{AppendMode, DirId, ModelError, ProjectModel, PropOrigin, PropScope};
use serde::{Deserialize, Serialize};

use crate::Cx;

use super::region::{
    EntryContext, JournalEffect, MAX_EFFECTS, MAX_INPUTS, OpaqueReason, PropertyEdit, RegionKey,
    RegionKind, RegionPayload, RegionRecord,
};
use super::trace::{
    JournalInput, PropertyOwner, StateKey, StateLookup, VariableScope, file_digest,
};

/// Versioned manifest of a successful whole configure.
///
/// The caller supplies a portable model snapshot that uses stable identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureSnapshot<M = ()> {
    schema: String,
    region: RegionRecord,
    model: M,
}

impl<M> ConfigureSnapshot<M> {
    /// Version identifier in the serialized manifest.
    pub const SCHEMA: &'static str = "configure.v1";

    /// File name for the manifest under `CMakeFiles/rcmake`.
    pub const FILE_NAME: &'static str = "configure.v1.bin";

    /// Create a manifest from a completed whole configure and its portable model.
    pub fn new(region: RegionRecord, model: M) -> Result<Self, ReplayMiss> {
        if region.key.kind != RegionKind::Configure {
            return Err(ReplayMiss::Identity);
        }
        if let RegionPayload::Opaque(reason) = region.payload {
            return Err(ReplayMiss::Opaque(reason));
        }
        Ok(Self {
            schema: Self::SCHEMA.into(),
            region,
            model,
        })
    }
}

/// First reason a manifest or region could not be reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayMiss {
    /// The manifest version is unsupported.
    Schema,
    /// The source directory, binary directory, or region kind differs.
    Identity,
    /// The region entry context differs.
    Context,
    /// The region contains an operation that prevents replay.
    Opaque(OpaqueReason),
    /// The record exceeds its limits or has inconsistent fields.
    InvalidRecord,
    /// The input at this zero-based position changed or could not be read.
    Input(usize),
    /// The generated file at this zero-based effect position changed or went missing.
    GeneratedFile(usize),
}

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
    /// The output handler could not stage the effect.
    #[error(transparent)]
    Output(#[from] HostError),
}

/// A configure manifest whose region inputs and outputs matched.
#[derive(Debug)]
pub struct VerifiedConfigure<'a, M> {
    /// Checked portable model. Restore it through the configure entry point.
    pub model: &'a M,
    /// Checked trace and ordered effects.
    pub region: VerifiedRegion<'a>,
}

/// A region whose entry context, inputs, and output files matched.
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
    /// The caller must stage mutations and output until every effect succeeds.
    /// Resolve persistent property owners in the current model. A generated file
    /// effect records a completed write for any enclosing region.
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
    /// Supply a staged evaluation context and a handler for diagnostics, configure
    /// log chunks, and checked generated files. Publish staged state and output only
    /// after success. Enclosing recorders receive generated files through the handler.
    pub fn replay_into(
        self,
        cx: &mut Cx<'_>,
        mut stage_output: impl FnMut(&JournalEffect) -> Result<(), ReplayApplyError>,
    ) -> Result<(), ReplayApplyError> {
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
                        cx.cache().insert(name.as_slice().into(), entry);
                    } else {
                        cx.cache().remove(name);
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
                JournalEffect::Diagnostic { .. }
                | JournalEffect::ConfigureLog(_)
                | JournalEffect::GeneratedFile { .. } => stage_output(effect)?,
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
    if &record.key != key || !key.binary_dir.is_absolute() || !key.source_dir.is_absolute() {
        return Err(ReplayMiss::Identity);
    }
    if &record.context != context {
        return Err(ReplayMiss::Context);
    }
    let (inputs, effects) = match &record.payload {
        RegionPayload::Complete { inputs, effects } => (inputs, effects),
        RegionPayload::Opaque(reason) => return Err(ReplayMiss::Opaque(*reason)),
    };
    if inputs.len() > MAX_INPUTS || effects.len() > MAX_EFFECTS {
        return Err(ReplayMiss::InvalidRecord);
    }
    let mut entry_keys = BTreeSet::new();
    for (index, input) in inputs.iter().enumerate() {
        if let JournalInput::State { key, .. } = input
            && !entry_keys.insert(key)
        {
            return Err(ReplayMiss::InvalidRecord);
        }
        if !input.matches(host, &mut lookup) {
            return Err(ReplayMiss::Input(index));
        }
    }

    let mut outputs = BTreeMap::<&Path, usize>::new();
    let mut written = BTreeSet::new();
    for (index, effect) in effects.iter().enumerate() {
        if !effect.is_well_formed() {
            return Err(ReplayMiss::InvalidRecord);
        }
        if let Some(key) = effect.written_state() {
            if effect.reads_written_state() && !entry_keys.contains(&key) && !written.contains(&key)
            {
                return Err(ReplayMiss::InvalidRecord);
            }
            written.insert(key);
        }
        if let JournalEffect::GeneratedFile { path, .. } = effect {
            outputs.insert(path, index);
        }
    }
    for (index, effect) in effects.iter().enumerate() {
        if let JournalEffect::GeneratedFile { path, digest } = effect
            && outputs.get(path.as_path()) == Some(&index)
            && file_digest(host, path).as_ref() != Ok(digest)
        {
            return Err(ReplayMiss::GeneratedFile(index));
        }
    }
    Ok(VerifiedRegion { inputs, effects })
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
        return Err(ReplayMiss::Schema);
    }
    if key.kind != RegionKind::Configure {
        return Err(ReplayMiss::Identity);
    }
    let region = verify_region(host, &snapshot.region, key, context, lookup)?;
    Ok(VerifiedConfigure {
        model: &snapshot.model,
        region,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{
        CacheValue, CommandClass, JournalDigest, JournalFileStat, JournalStamp, OutputStream,
        PropertyKey, PropertyOrigin, RegionState, VariableScope, directory_digest, policy_digest,
    };
    use rcmake_diag::{Backtrace, Frame, FrameKind, PolicyStatus};
    use rcmake_host::{FileTime, HostResult, PureHost};
    use rcmake_model::{CacheEntry, CacheType, PropertyMap};
    use std::path::PathBuf;

    fn fixture(kind: RegionKind) -> (PureHost, RegionKey, EntryContext) {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = PureHost::new(root.clone(), root.join("rcmake"));
        let key = RegionKey {
            binary_dir: root.join("build"),
            source_dir: root.join("src"),
            kind,
        };
        let context = EntryContext {
            policies: JournalDigest::hash(b"policies"),
            diag: JournalDigest::hash(b"diagnostics"),
            log_level: 0,
            flags: JournalDigest::hash(b"flags"),
        };
        (host, key, context)
    }

    fn variable(scope: VariableScope, name: &[u8]) -> StateKey {
        StateKey::Variable {
            scope,
            name: name.to_vec(),
        }
    }

    fn read(state: &mut RegionState, key: StateKey, bytes: Option<&[u8]>) {
        state.record_input(JournalInput::State {
            key,
            value: bytes.map(JournalDigest::hash),
        });
    }

    fn unavailable(_: &StateKey) -> StateLookup {
        StateLookup::Unavailable
    }

    #[test]
    fn only_first_read_before_write_constrains_replay() -> Result<(), ReplayMiss> {
        let (host, key, context) = fixture(RegionKind::Subdirectory);
        let mut state = RegionState::default();
        let parent = variable(VariableScope::Parent, b"X");
        let current = variable(VariableScope::Current, b"X");
        read(&mut state, parent.clone(), Some(b"old"));
        read(&mut state, parent.clone(), Some(b"old"));
        state.record_effect(JournalEffect::SetVar {
            scope: VariableScope::Parent,
            name: b"X".to_vec(),
            value: Some(b"write".to_vec()),
        });
        read(&mut state, parent.clone(), Some(b"write"));
        state.record_effect(JournalEffect::SetVar {
            scope: VariableScope::Current,
            name: b"X".to_vec(),
            value: None,
        });
        read(&mut state, current, None);
        let record = state.finish(key.clone(), context.clone());
        let verified = verify_region(&host, &record, &key, &context, |seen| {
            assert_eq!(seen, &parent);
            StateLookup::Observed(Some(JournalDigest::hash(b"old")))
        })?;
        assert_eq!(verified.inputs().len(), 1);
        let result = verify_region(&host, &record, &key, &context, |_| {
            StateLookup::Observed(None)
        });
        assert!(matches!(result, Err(ReplayMiss::Input(0))));
        Ok(())
    }

    #[test]
    fn absent_cache_input_and_empty_cache_value_are_distinct() {
        let (host, key, context) = fixture(RegionKind::Configure);
        let mut state = RegionState::default();
        read(&mut state, StateKey::Cache(b"X".to_vec()), None);
        let record = state.finish(key.clone(), context.clone());
        let empty = CacheValue::from(&CacheEntry {
            value: "".into(),
            ty: CacheType::String,
            help: "help".into(),
            advanced: false,
            props: PropertyMap::default(),
        });
        assert!(
            verify_region(&host, &record, &key, &context, |_| StateLookup::Observed(
                None
            ))
            .is_ok()
        );
        assert!(matches!(
            verify_region(&host, &record, &key, &context, unavailable),
            Err(ReplayMiss::Input(0))
        ));
        assert!(matches!(
            verify_region(&host, &record, &key, &context, |_| StateLookup::Observed(
                Some(empty.digest())
            )),
            Err(ReplayMiss::Input(0))
        ));
    }

    #[test]
    fn cache_digest_and_restore_include_metadata() {
        let mut props = PropertyMap::default();
        props.set(b"STRINGS".as_slice().into(), b"\xff;two".as_slice().into());
        let mut entry = CacheEntry {
            value: "one".into(),
            ty: CacheType::String,
            help: "Help text".into(),
            advanced: true,
            props,
        };
        let saved = CacheValue::from(&entry);
        assert_eq!(saved.to_entry().as_ref(), Some(&entry));
        entry.advanced = false;
        assert_ne!(saved.digest(), CacheValue::from(&entry).digest());
        entry.advanced = true;
        entry.props.unset(b"STRINGS");
        assert_ne!(saved.digest(), CacheValue::from(&entry).digest());
    }

    #[test]
    fn append_to_an_existing_property_requires_an_input() {
        let (host, key, context) = fixture(RegionKind::Subdirectory);
        let backtrace = Backtrace::default().push(Frame {
            file: b"\xff/CMakeLists.txt".as_slice().into(),
            line: 3,
            name: Some(b"set_property".as_slice().into()),
            kind: FrameKind::Command,
        });
        let origin = PropertyOrigin::new(key.binary_dir.clone(), &backtrace);
        assert_eq!(origin.backtrace().top(), backtrace.top());
        let property = PropertyKey {
            owner: PropertyOwner::Global,
            name: b"LIBS".to_vec(),
        };
        let append = JournalEffect::Property {
            key: property.clone(),
            edit: PropertyEdit::AppendList(b"next".to_vec()),
            origin,
        };

        let mut missing = RegionState::default();
        missing.record_effect(append.clone());
        assert!(matches!(
            missing,
            RegionState::Opaque(OpaqueReason::ExternalMutation)
        ));

        let input = StateKey::Property(property);
        let mut tracked = RegionState::default();
        read(&mut tracked, input.clone(), Some(b"first"));
        tracked.record_effect(append);
        read(&mut tracked, input.clone(), Some(b"first;next"));
        let record = tracked.finish(key.clone(), context.clone());
        let verify = |value: &[u8]| {
            verify_region(&host, &record, &key, &context, |seen| {
                assert_eq!(seen, &input);
                StateLookup::Observed(Some(JournalDigest::hash(value)))
            })
        };
        assert!(verify(b"first").is_ok());
        assert!(matches!(verify(b"other"), Err(ReplayMiss::Input(0))));
    }

    #[test]
    fn policy_input_and_region_context_are_checked() {
        let (host, key, context) = fixture(RegionKind::Subdirectory);
        let mut state = RegionState::default();
        state.record_input(JournalInput::State {
            key: StateKey::Policy(77),
            value: Some(policy_digest(PolicyStatus::New)),
        });
        let record = state.finish(key.clone(), context.clone());
        let read_new = |key: &StateKey| {
            assert_eq!(key, &StateKey::Policy(77));
            StateLookup::Observed(Some(policy_digest(PolicyStatus::New)))
        };
        assert!(verify_region(&host, &record, &key, &context, read_new).is_ok());
        assert!(matches!(
            verify_region(&host, &record, &key, &context, |_| {
                StateLookup::Observed(Some(policy_digest(PolicyStatus::Old)))
            }),
            Err(ReplayMiss::Input(0))
        ));
        let mut changed = context;
        changed.log_level += 1;
        assert!(matches!(
            verify_region(&host, &record, &key, &changed, unavailable),
            Err(ReplayMiss::Context)
        ));
    }

    #[test]
    fn unknown_command_makes_the_region_opaque() {
        let (host, key, context) = fixture(RegionKind::Configure);
        let mut state = RegionState::default();
        state.record_command(CommandClass::Tracked);
        state.record_command(CommandClass::default());
        state.record_command(CommandClass::Opaque(OpaqueReason::ExecuteProcess));
        let record = state.finish(key.clone(), context.clone());
        assert!(matches!(
            verify_region(&host, &record, &key, &context, unavailable),
            Err(ReplayMiss::Opaque(OpaqueReason::UnclassifiedCommand))
        ));
        assert!(matches!(
            ConfigureSnapshot::new(record, ()),
            Err(ReplayMiss::Opaque(OpaqueReason::UnclassifiedCommand))
        ));
    }

    #[test]
    fn changed_shared_value_without_a_tracked_write_makes_region_opaque() {
        let (_, key, context) = fixture(RegionKind::Subdirectory);
        let mut state = RegionState::default();
        let cache = StateKey::Cache(b"X".to_vec());
        read(&mut state, cache.clone(), Some(b"old"));
        read(&mut state, cache, Some(b"new"));
        let record = state.finish(key, context);
        assert!(matches!(
            record.payload,
            RegionPayload::Opaque(OpaqueReason::ExternalMutation)
        ));
    }

    #[test]
    fn trace_rechecks_file_directory_missing_path_and_environment() -> HostResult<()> {
        let (host, key, context) = fixture(RegionKind::Configure);
        let file = key.source_dir.join("CMakeLists.txt");
        let missing = key.binary_dir.join("candidate");
        host.seed_file(&file, b"original")?;
        host.env_set(b"OPTION", Some(b"initial"));
        let mut state = RegionState::default();
        for stamp in [
            JournalStamp::FileHash {
                path: file.clone(),
                digest: JournalDigest::hash(b"original"),
            },
            JournalStamp::DirEntries {
                path: key.source_dir.clone(),
                digest: directory_digest(&host, &key.source_dir)?,
            },
            JournalStamp::Missing {
                path: missing.clone(),
            },
            JournalStamp::EnvVar {
                name: b"OPTION".to_vec(),
                digest: Some(JournalDigest::hash(b"initial")),
            },
        ] {
            state.record_input(JournalInput::Host(stamp));
        }
        let record = state.finish(key.clone(), context.clone());
        let verify = || verify_region(&host, &record, &key, &context, unavailable);
        assert!(verify().is_ok());
        host.env_set(b"OPTION", None);
        assert!(matches!(verify(), Err(ReplayMiss::Input(3))));
        host.seed_file(&missing, b"new")?;
        assert!(matches!(verify(), Err(ReplayMiss::Input(2))));
        host.seed_file(&key.source_dir.join("sibling.cmake"), b"new")?;
        assert!(matches!(verify(), Err(ReplayMiss::Input(1))));
        host.seed_file(&file, b"changed!")?;
        assert!(matches!(verify(), Err(ReplayMiss::Input(0))));
        Ok(())
    }

    #[test]
    fn file_stat_trace_accepts_a_touch_and_detects_changed_contents() -> HostResult<()> {
        let (host, key, context) = fixture(RegionKind::Configure);
        let file = key.source_dir.join("input.cmake");
        host.seed_file(&file, b"initial")?;
        let mut state = RegionState::default();
        state.record_input(JournalInput::Host(JournalStamp::FileStat {
            path: file.clone(),
            stat: JournalFileStat::read(&host, &file)?,
            digest: Some(JournalDigest::hash(b"initial")),
        }));
        let record = state.finish(key.clone(), context.clone());
        let verify = || verify_region(&host, &record, &key, &context, unavailable);
        assert!(verify().is_ok());
        let timestamp = FileTime {
            secs: 123,
            nanos: 0,
        };
        host.set_times(&file, None, Some(timestamp))?;
        assert!(verify().is_ok());
        host.seed_file(&file, b"new content")?;
        assert!(matches!(verify(), Err(ReplayMiss::Input(0))));
        Ok(())
    }

    #[test]
    fn configure_checks_final_generated_contents() -> HostResult<()> {
        fn requires_serde<T: Serialize + serde::de::DeserializeOwned>() {}
        requires_serde::<ConfigureSnapshot<Vec<u8>>>();
        let (host, key, context) = fixture(RegionKind::Configure);
        let generated = key.binary_dir.join("generated.h");
        host.seed_file(&generated, b"finish")?;
        let mut state = RegionState::default();
        state.record_effect(JournalEffect::GeneratedFile {
            path: generated.clone(),
            digest: JournalDigest::hash(b"start"),
        });
        state.record_effect(JournalEffect::Diagnostic {
            stream: OutputStream::Stdout,
            bytes: vec![0xff],
        });
        state.record_effect(JournalEffect::GeneratedFile {
            path: generated.clone(),
            digest: JournalDigest::hash(b"finish"),
        });
        let snapshot =
            ConfigureSnapshot::new(state.finish(key.clone(), context.clone()), vec![0xff_u8]);
        assert!(snapshot.is_ok());
        let Ok(snapshot) = snapshot else {
            return Ok(());
        };
        let checked = verify_configure(&host, &snapshot, &key, &context, unavailable);
        assert!(checked.is_ok());
        if let Ok(checked) = checked {
            assert_eq!(checked.model, &[0xff]);
            let mut seen = Vec::new();
            checked.region.replay(|effect| {
                seen.push(
                    matches!(effect, JournalEffect::Diagnostic { bytes, .. } if bytes == &[0xff]),
                );
                Ok::<(), rcmake_host::HostError>(())
            })?;
            assert_eq!(seen, [false, true, false]);
        }
        let mut future_snapshot = snapshot.clone();
        future_snapshot.schema = "configure.v2".into();
        assert!(matches!(
            verify_configure(&host, &future_snapshot, &key, &context, unavailable),
            Err(ReplayMiss::Schema)
        ));
        host.remove_file(&generated)?;
        assert!(matches!(
            verify_configure(&host, &snapshot, &key, &context, unavailable),
            Err(ReplayMiss::GeneratedFile(2))
        ));
        Ok(())
    }
}
