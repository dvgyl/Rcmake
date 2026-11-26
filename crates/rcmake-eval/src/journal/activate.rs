//! Record and replay listfiles with fully classified commands.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, Host, path};
use rcmake_json::{FieldError, FromJson, Value, opt_or_else, req};
use rcmake_model::DirId;
use rcmake_syntax::{ArgKind, Argument};

use super::codec;
use super::frame::{EncodeError, EncodeJson, JsonSink, MAX_FRAME_BYTES, encoded_size};
use super::leaf::{LocalState, Usage, Writes};
use super::receipts::ReplayReceipts;
use super::region::RegionPayload;
use super::setup::{SetupRecord, SetupSession};
use super::trace::JournalHasher;
use super::{
    CacheValue, ConfigureSnapshot, EntryContext, JournalDigest, JournalEffect, JournalFileStat,
    JournalInput, JournalStamp, OpaqueReason, PropertyOrigin, RegionKey, RegionKind, RegionRecord,
    RegionState, StateLookup, VariableScope, VerifiedRegion, file_digest, verify_configure,
    verify_region,
};
use crate::code::Code;
use crate::interp::BodyMode;
use crate::{CmdResult, Flow, HermeticMode, Interp, LedgerLevel, RunMode, RunOutcome};

/// Base host used to verify journal inputs without a filesystem query cache.
pub struct ReplayHost(pub Arc<dyn Host>);

#[derive(Default)]
pub(crate) struct ReplayObservations {
    pub(crate) vars: BTreeSet<BString>,
    pub(crate) cache: BTreeSet<BString>,
    pub(crate) link_check: Option<PropertyOrigin>,
    variable_reads: Option<RefCell<Option<BTreeMap<BString, JournalDigest>>>>,
    cli_reads: RefCell<BTreeSet<BString>>,
    inputs: Vec<JournalInput>,
    opaque: bool,
}

impl ReplayObservations {
    fn record(&mut self, stamp: JournalStamp) {
        if self.inputs.len() < 4096 {
            self.inputs.push(JournalInput::Host(stamp));
        } else {
            self.opaque = true;
        }
    }
}

#[derive(Default)]
pub(super) struct Regions {
    root: Option<LocalState>,
    nested: Vec<(RegionRecord, LocalState)>,
    variable_inputs: Vec<Vec<String>>,
    receipts: Vec<JournalDigest>,
    /// Setup inputs and checkpoints. `None` in a manifest without a setup section.
    pub(super) setup: Option<SetupRecord>,
}

impl EncodeJson for Regions {
    fn encode(&self, sink: &mut JsonSink<'_>) -> Result<(), EncodeError> {
        sink.object(|obj| {
            obj.field("nested", |s| self.nested.encode(s))?;
            if !self.receipts.is_empty() {
                obj.field("receipts", |s| self.receipts.encode(s))?;
            }
            obj.field("root", |s| self.root.encode(s))?;
            if let Some(setup) = &self.setup {
                obj.field("setup", |s| setup.encode(s))?;
            }
            if !self.variable_inputs.is_empty() {
                obj.field("variable_inputs", |s| self.variable_inputs.encode(s))?;
            }
            Ok(())
        })
    }
}

impl FromJson for Regions {
    fn from_json(value: &Value) -> Result<Self, FieldError> {
        let obj = codec::fields(
            value,
            &["root", "nested", "variable_inputs", "receipts", "setup"],
        )?;
        Ok(Self {
            root: req(obj, "root")?,
            nested: codec::field(obj, "nested", |nested| {
                codec::from_list(nested, |item| {
                    let (record, state) = codec::from_pair(item)?;
                    Ok((
                        RegionRecord::from_json(record)?,
                        LocalState::from_json(state)?,
                    ))
                })
            })?,
            variable_inputs: opt_or_else(obj, "variable_inputs", Vec::new)?,
            receipts: opt_or_else(obj, "receipts", Vec::new)?,
            setup: rcmake_json::opt(obj, "setup")?,
        })
    }
}

type Manifest = ConfigureSnapshot<Regions>;

fn decode_and_digest(
    bytes: &[u8],
    digest: impl FnOnce() -> Option<JournalDigest>,
) -> (Option<JournalDigest>, Option<Manifest>) {
    let decode = || Manifest::decode(bytes).ok();
    #[cfg(not(target_arch = "wasm32"))]
    if bytes.len() >= 256 * 1024 && bytes.len() <= Manifest::frame_limit() {
        return std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("rcmake-replay-decode".into())
                .spawn_scoped(scope, decode);
            let executable = digest();
            let manifest = match worker {
                Ok(worker) => worker.join().ok().flatten(),
                Err(_) => executable.and_then(|_| decode()),
            };
            (executable, manifest)
        });
    }
    let executable = digest();
    (executable, executable.and_then(|_| decode()))
}

pub(crate) struct ReplaySession {
    host: Arc<dyn Host>,
    executable: JournalDigest,
    options: JournalDigest,
    root: RegionKey,
    source: Option<(BString, JournalDigest)>,
    previous: Option<Manifest>,
    variable_indices: Vec<Option<usize>>,
    pending: Regions,
    receipts: Option<Arc<ReplayReceipts>>,
    root_record: Option<RegionRecord>,
    bytes_left: usize,
}

struct ReplayPlan {
    writes: Writes,
    key: RegionKey,
    context: EntryContext,
    source: PathBuf,
    digest: JournalDigest,
    tools: Vec<PathBuf>,
}

pub(crate) type ForeachSaved = Option<crate::Binding>;

#[derive(Default)]
pub(crate) struct ReplayState {
    session: Option<ReplaySession>,
    hits: u64,
    replayed_listfiles: BTreeSet<PathBuf>,
    pub(crate) executed_listfiles: Option<BTreeSet<PathBuf>>,
    pub(crate) observations: Option<ReplayObservations>,
    /// Setup spans and checkpoints of the running incremental configure. This state
    /// stays here while a listfile body has the session.
    pub(crate) setup: Option<SetupSession>,
    setup_miss: Option<String>,
    setup_sources: Vec<PathBuf>,
}

impl ReplaySession {
    fn start(
        interp: &Interp,
        source: &Path,
        build: &Path,
        preloads: &[PathBuf],
        cached: Option<&[u8]>,
    ) -> Option<(Self, SetupSession)> {
        if !interp.can_replay() {
            return None;
        }
        let host = interp
            .session_data::<ReplayHost>()
            .map_or(&interp.config.host, |h| &h.0)
            .clone();
        let program = host.current_exe().ok()?;
        let digest = || file_digest(host.as_ref(), &program).ok();
        let (executable, previous) = if let Some(bytes) = cached {
            decode_and_digest(bytes, digest)
        } else {
            (digest(), None)
        };
        let executable = executable?;
        let absolute = |p: &Path| {
            path::to_native(&crate::util::collapse_full_path(
                &path::from_native_cmake(p),
                &path::from_native_cmake(&interp.config.cwd),
            ))
        };
        let root = RegionKey {
            source_dir: absolute(source),
            binary_dir: absolute(build),
            kind: RegionKind::Configure,
        };
        let mut h = JournalHasher::new(b"session-options");
        for p in [
            &interp.config.cmake_root,
            &interp.config.cmake_command,
            &interp.config.cwd,
            &root.source_dir,
            &root.binary_dir,
        ] {
            h.field(1, p.as_os_str().as_encoded_bytes());
        }
        for arg in &interp.config.script_args {
            h.field(2, arg);
        }
        for def in &interp.config.defines {
            h.field(3, &def.name);
            h.field(4, def.ty.map_or("", |ty| ty.as_str()).as_bytes());
            h.field(5, &def.value);
        }
        h.field(6, format!("{:?}", interp.config.limits).as_bytes());
        h.field(7, format!("{:?}", interp.config.diag).as_bytes());
        let previous = previous.filter(|s| {
            s.region.key == root
                && s.model.nested.len() <= 4096
                && s.model.variable_inputs.len() <= 4096
        });
        let receipts = interp.session_data::<Arc<ReplayReceipts>>().cloned();
        if let Some(receipts) = &receipts {
            let recorded = previous
                .as_ref()
                .filter(|frame| frame.region.context.rcmake == executable)
                .map_or(&[][..], |frame| frame.model.receipts.as_slice());
            receipts.restore(recorded);
        }
        let variable_indices = vec![
            None;
            previous
                .as_ref()
                .map_or(0, |s| s.model.variable_inputs.len())
        ];
        let options = h.finish();
        let setup = SetupSession::start(
            interp,
            Arc::clone(&host),
            options,
            preloads,
            previous.as_ref().map(|frame| frame.model.setup.clone()),
        );
        Some((
            Self {
                host,
                executable,
                options,
                root,
                previous,
                variable_indices,
                source: None,
                pending: Regions::default(),
                receipts,
                root_record: None,
                bytes_left: MAX_FRAME_BYTES - 4096,
            },
            setup,
        ))
    }

    fn keep(&mut self, record: RegionRecord, state: LocalState) {
        let Some(size) = encoded_size(&(&record, &state), self.bytes_left) else {
            return;
        };
        if record.key.kind == RegionKind::Configure {
            self.root_record = Some(record);
            self.pending.root = Some(state);
        } else if self.pending.nested.len() < 4096 {
            self.pending.nested.push((record, state));
        } else {
            return;
        }
        self.bytes_left = self.bytes_left.saturating_sub(size + 1);
    }

    fn intern_variables(&mut self, variables: Vec<String>) -> Option<usize> {
        if let Some(index) = self
            .pending
            .variable_inputs
            .iter()
            .position(|set| *set == variables)
        {
            return Some(index);
        }
        let size = encoded_size(&variables, self.bytes_left)?;
        let index = self.pending.variable_inputs.len();
        self.pending.variable_inputs.push(variables);
        self.bytes_left = self.bytes_left.saturating_sub(size + 1);
        Some(index)
    }

    fn retain_variable_inputs(&mut self, previous: usize) -> Option<usize> {
        if let Some(index) = self.variable_indices.get(previous).copied().flatten() {
            return Some(index);
        }
        let variables = self
            .previous
            .as_ref()?
            .model
            .variable_inputs
            .get(previous)?
            .clone();
        let index = self.intern_variables(variables)?;
        *self.variable_indices.get_mut(previous)? = Some(index);
        Some(index)
    }

    fn finish(mut self, setup: Option<SetupRecord>) -> Option<Vec<u8>> {
        let setup = setup?;
        let size = encoded_size(&setup, self.bytes_left)?;
        self.bytes_left = self.bytes_left.saturating_sub(size + 16);
        self.pending.setup = Some(setup);
        let confirmed = self
            .receipts
            .as_ref()
            .map_or_else(Vec::new, |session| session.saved());
        if encoded_size(&confirmed, self.bytes_left).is_some() {
            self.pending.receipts = confirmed;
        }
        if self.root_record.is_none()
            && self.pending.nested.is_empty()
            && self.pending.receipts.is_empty()
        {
            return None;
        }
        let region = self.root_record.unwrap_or_else(|| {
            let mut opaque = RegionState::default();
            opaque.mark_opaque(OpaqueReason::UnclassifiedCommand);
            opaque.finish(
                self.root,
                EntryContext {
                    rcmake: self.executable,
                    policies: self.options,
                    diag: self.options,
                    flags: self.options,
                    log_level: 0,
                },
            )
        });
        Manifest::from_region(region, self.pending).encode().ok()
    }
}

impl Interp {
    /// Configure a project. Reuse verified regions from a previous successful run.
    /// The caller can publish the returned frame after generation succeeds.
    pub fn configure_incremental(
        &mut self,
        source: &Path,
        build: &Path,
        preloads: &[PathBuf],
        cached: Option<&[u8]>,
    ) -> (RunOutcome, Option<Vec<u8>>) {
        self.replay.hits = 0;
        self.replay.replayed_listfiles.clear();
        self.replay.setup_miss = None;
        self.replay.setup_sources.clear();
        let started = ReplaySession::start(self, source, build, preloads, cached);
        let (session, setup) = started.map_or((None, None), |(session, setup)| {
            (Some(session), Some(setup))
        });
        // Each configure attempt records into a new session. A failed or restarted
        // attempt publishes nothing.
        self.replay.session = session;
        self.replay.setup = setup;
        let result = self.run_configure(source, build, preloads);
        let session = self.replay.session.take();
        let setup = self.replay.setup.take();
        if let Some(setup) = &setup {
            self.replay.setup_miss = setup.miss().map(str::to_owned);
        }
        let record = setup.and_then(SetupSession::finish);
        if let Some(record) = &record {
            self.replay.setup_sources = record
                .sources
                .iter()
                .map(|(path, _)| path.clone())
                .collect();
        }
        let bytes = if result.exit_code == 0 && self.can_replay() {
            session.and_then(|session| session.finish(record))
        } else {
            None
        };
        (result, bytes)
    }

    /// The reason that the last incremental configure used no recorded listfile
    /// because of setup, for example `input file:toolchain` or
    /// `checkpoint 0 variables`. `None` when setup allowed replay.
    #[must_use]
    pub fn replay_setup_miss(&self) -> Option<&str> {
        self.replay.setup_miss.as_deref()
    }

    /// The setup files of the last incremental configure: preload scripts and the
    /// files that the setup inputs name.
    #[must_use]
    pub fn replay_setup_sources(&self) -> &[PathBuf] {
        &self.replay.setup_sources
    }

    /// Number of listfile bodies restored by the last incremental configure.
    #[must_use]
    pub fn replay_hits(&self) -> u64 {
        self.replay.hits
    }

    /// Absolute paths of the listfiles restored by the last incremental configure.
    #[must_use]
    pub fn replayed_listfiles(&self) -> &std::collections::BTreeSet<PathBuf> {
        &self.replay.replayed_listfiles
    }

    /// Start a new record of listfile bodies that enter the evaluator.
    pub fn track_executed_listfiles(&mut self) {
        self.replay.executed_listfiles = Some(Default::default());
    }

    /// Return the recorded paths, or an empty iterator when tracking is off.
    pub fn executed_listfiles(&self) -> impl Iterator<Item = &Path> {
        self.replay
            .executed_listfiles
            .iter()
            .flatten()
            .map(PathBuf::as_path)
    }

    fn can_replay(&self) -> bool {
        self.config.mode == RunMode::Configure
            && !self.fatal
            && self.exit_override.is_none()
            && !self.engine.error_occurred()
            && self.stop_reason.is_none()
            && self.var_ledger.is_none()
            && self.project.prop_ledger().is_none()
            && matches!(self.config.services.provenance, LedgerLevel::Off)
            && !self.config.services.collect_annotations
            && self.command_profiler.is_none()
            && !self.trace_enabled()
            && self.trace.is_none()
            && self.trace_stack.is_empty()
            && self.config.services.step_budget.is_none()
            && self.config.services.cancel.is_none()
            && self.config.limits.wall_budget.is_none()
            && self.config.services.speed.hermetic == HermeticMode::Off
            && !self.has_command_observers()
    }

    pub(crate) fn note_replay_source(&mut self, abs: &[u8], text: &crate::code::SourceText) {
        if let Some(session) = &mut self.replay.session {
            session.source = (text.bytes.len() <= 1024 * 1024).then(|| (abs.into(), text.digest()));
        }
    }

    pub(crate) fn run_listfile_body(
        &mut self,
        code: &Code,
        abs: &[u8],
        directory: bool,
    ) -> CmdResult {
        let Some(mut session) = self.replay.session.take() else {
            return self.run_code(code, 0, code.len(), BodyMode::File);
        };
        let Some(plan) = self.plan_replay(code, abs, directory, &mut session) else {
            self.replay.session = Some(session);
            return self.run_code(code, 0, code.len(), BodyMode::File);
        };

        let result = if let Some((record, state, checked)) = self.find_candidate(&session, &plan)
            && state.restore(self, &plan.writes)
        {
            checked
                .replay_into(&mut self.cx())
                .map_err(|error| crate::CmdError::status(error.to_string()))
                .map(|()| (record.clone(), state.clone()))
                .map(|(record, mut model)| {
                    self.replay.hits += 1;
                    self.replay.replayed_listfiles.insert(plan.source);
                    if let Some((index, digest)) = model.variable_inputs {
                        if let Some(index) = session.retain_variable_inputs(index) {
                            model.variable_inputs = Some((index, digest));
                            session.keep(record, model);
                        }
                    } else {
                        session.keep(record, model);
                    }
                    Flow::Normal
                })
        } else {
            self.record_region(code, &mut session, plan)
        };
        self.replay.session = Some(session);
        result
    }

    fn plan_replay(
        &self,
        code: &Code,
        abs: &[u8],
        directory: bool,
        session: &mut ReplaySession,
    ) -> Option<ReplayPlan> {
        let (_, digest) = session
            .source
            .take()
            .filter(|(path, _)| path.as_slice() == abs)?;
        if !self.can_replay()
            || self.depth > 128
            || self.unwinding_package
            || self.replay.setup.as_ref().is_none_or(SetupSession::blocks)
        {
            return None;
        }
        let mut writes = Writes::classify(self, code)?;
        let source = path::to_native(abs);
        let key = self.replay_key(&source, directory, &session.root);
        writes.record_variable_reads = match key.kind {
            RegionKind::Subdirectory => writes.record_variable_reads,
            RegionKind::Bootstrap { .. } => Writes::bootstrap_variable_reads(code),
            RegionKind::Include { .. } if self.generated_system_file(&source) => {
                Writes::bootstrap_variable_reads(code)
            }
            _ => false,
        };
        let tools = self.replay_tool_paths(&key.kind, &writes)?;
        let context = self.entry_context(session, &writes, digest)?;
        Some(ReplayPlan {
            writes,
            key,
            context,
            source,
            digest,
            tools,
        })
    }

    fn find_candidate<'a>(
        &self,
        session: &'a ReplaySession,
        plan: &ReplayPlan,
    ) -> Option<(&'a RegionRecord, &'a LocalState, VerifiedRegion<'a>)> {
        let ReplayPlan {
            writes,
            key,
            context,
            source,
            digest,
            tools,
        } = plan;
        let valid_record = |record: &RegionRecord| {
            let RegionPayload::Complete { inputs, effects } = &record.payload else {
                return false;
            };
            let Some((JournalInput::Host(JournalStamp::FileHash { path, digest: sum }), rest)) =
                inputs.split_first()
            else {
                return false;
            };
            let Some((tool_inputs, other_inputs)) = rest.split_at_checked(tools.len()) else {
                return false;
            };
            path == source
                && sum == digest
                && tool_inputs.iter().zip(tools).all(|(input, tool)| {
                    matches!(input,
                    JournalInput::Host(JournalStamp::FileHash { path, .. }) if path == tool)
                })
                && other_inputs.iter().all(|input| {
                    matches!(
                        input,
                        JournalInput::Host(
                            JournalStamp::Metadata { .. }
                                | JournalStamp::Missing { .. }
                                | JournalStamp::EnvVar { .. }
                        )
                    )
                })
                && writes.check_effects(effects, self.config.limits.max_value_bytes)
        };
        if self
            .replay
            .setup
            .as_ref()
            .is_none_or(SetupSession::blocks_hits)
        {
            return None;
        }
        let snapshot = session.previous.as_ref()?;
        if key.kind == RegionKind::Configure {
            if !valid_record(&snapshot.region) {
                return None;
            }
            let checked = verify_configure(session.host.as_ref(), snapshot, key, context, |_| {
                StateLookup::Unavailable
            })
            .ok()?;
            Some((
                &snapshot.region,
                checked.model.root.as_ref()?,
                checked.region,
            ))
        } else {
            snapshot
                .model
                .nested
                .iter()
                .filter(|(r, _)| r.key == *key && r.context == *context && valid_record(r))
                .find_map(|(r, s)| {
                    match s.variable_inputs {
                        Some((index, digest)) if writes.record_variable_reads => {
                            let variables = snapshot.model.variable_inputs.get(index)?;
                            if self.variable_input_digest(variables) != digest {
                                return None;
                            }
                        }
                        None if !writes.record_variable_reads => {}
                        _ => return None,
                    }
                    let checked = verify_region(session.host.as_ref(), r, key, context, |_| {
                        StateLookup::Unavailable
                    })
                    .ok()?;
                    Some((r, s, checked))
                })
        }
    }

    fn record_region(
        &mut self,
        code: &Code,
        session: &mut ReplaySession,
        plan: ReplayPlan,
    ) -> CmdResult {
        let ReplayPlan {
            writes,
            key,
            context,
            source,
            digest,
            tools,
        } = plan;
        let diagnostics = self.engine.diagnostics().len();
        let log_bytes = self.configure_log.len();
        let unread: Vec<_> = if writes.record_variable_reads {
            Vec::new()
        } else {
            self.unread_variables().map(BString::from).collect()
        };
        let install_start = writes
            .installs
            .then(|| (self.project.installs.len(), self.project.targets().count()));
        let sources = self.project.directory_source_mark(self.current_dir_id());
        self.replay.observations = Some(ReplayObservations {
            variable_reads: writes
                .record_variable_reads
                .then(|| RefCell::new(Some(BTreeMap::new()))),
            ..ReplayObservations::default()
        });
        let result = self.run_code(code, 0, code.len(), BodyMode::File);
        let observations = self.replay.observations.take().unwrap_or_default();
        let used_cli = if writes.record_variable_reads {
            let mut used = observations.cli_reads.into_inner();
            used.extend(observations.vars.iter().cloned());
            used
        } else {
            let still_unread: BTreeSet<_> = self.unread_variables().collect();
            unread
                .into_iter()
                .filter(|name| !still_unread.contains(name.as_slice()))
                .collect()
        };
        if matches!(result, Ok(Flow::Normal))
            && self.can_replay()
            && diagnostics == self.engine.diagnostics().len()
            && log_bytes == self.configure_log.len()
            && !observations.opaque
            && observations
                .vars
                .iter()
                .all(|name| writes.vars.contains_key(name))
            && observations.cache.is_subset(&writes.cache)
            && let Some(mut state) = LocalState::capture(
                self,
                &writes,
                used_cli,
                install_start,
                observations.link_check,
                &sources,
            )
        {
            let mut journal = RegionState::default();
            journal.record_input(JournalInput::Host(JournalStamp::FileHash {
                path: source,
                digest,
            }));
            for tool in tools {
                match file_digest(session.host.as_ref(), &tool) {
                    Ok(digest) => {
                        journal.record_input(JournalInput::Host(JournalStamp::FileHash {
                            path: tool,
                            digest,
                        }))
                    }
                    Err(_) => {
                        journal.mark_opaque(OpaqueReason::ExternalMutation);
                        break;
                    }
                }
            }
            for input in observations.inputs {
                journal.record_input(input);
            }
            for name in &observations.vars {
                journal.record_effect(JournalEffect::SetVar {
                    scope: VariableScope::Current,
                    name: name.to_vec(),
                    value: self.vars.get(name).map(|value| value.to_vec()),
                });
            }
            for name in &observations.cache {
                journal.record_effect(JournalEffect::SetCache {
                    name: name.to_vec(),
                    entry: self.project.cache.get(name).map(CacheValue::from),
                });
            }
            let complete = match observations.variable_reads {
                Some(reads) => reads
                    .into_inner()
                    .and_then(|reads| {
                        let mut digest = JournalHasher::new(b"leaf-variable-inputs");
                        let mut variables = Vec::with_capacity(reads.len());
                        for (name, value) in reads {
                            digest.field(1, &name);
                            digest.field(2, value.as_bytes());
                            variables.push(String::from_utf8(name.to_vec()).ok()?);
                        }
                        let digest = digest.finish();
                        Some((session.intern_variables(variables)?, digest))
                    })
                    .is_some_and(|inputs| {
                        state.variable_inputs = Some(inputs);
                        true
                    }),
                None => !writes.record_variable_reads,
            };
            if complete && matches!(journal, RegionState::Recording(_)) {
                session.keep(journal.finish(key, context), state);
            }
        }
        result
    }

    pub(crate) fn observe_replay_variable(&self, name: &[u8]) {
        if let Some(observations) = &self.replay.observations
            && !observations.vars.contains(name)
            && let Some(reads) = &observations.variable_reads
        {
            let mut reads = reads.borrow_mut();
            if let Some(names) = reads.as_mut()
                && !names.contains_key(name)
            {
                if name.len() > 8192 || names.len() >= 4096 {
                    *reads = None;
                } else {
                    names.insert(name.into(), self.variable_binding_digest(name));
                }
            }
        }
    }

    pub(crate) fn observe_replay_variable_access(&self, name: &[u8]) {
        self.observe_replay_variable(name);
        if let Some(observations) = &self.replay.observations
            && observations
                .variable_reads
                .as_ref()
                .is_some_and(|reads| reads.borrow().is_some())
        {
            observations.cli_reads.borrow_mut().insert(name.into());
        }
    }

    fn variable_input_digest(&self, names: &[String]) -> JournalDigest {
        let mut hash = JournalHasher::new(b"leaf-variable-inputs");
        for name in names {
            let name = name.as_bytes();
            hash.field(1, name);
            hash.field(2, self.variable_binding_digest(name).as_bytes());
        }
        hash.finish()
    }

    fn variable_binding_digest(&self, name: &[u8]) -> JournalDigest {
        use crate::Binding;

        let mut hash = JournalHasher::new(b"variable-binding");
        match self.vars.lookup(name) {
            Some(Binding::Set(value)) => hash.field(1, value),
            Some(Binding::Unset) => hash.field(2, b""),
            None => hash.field(3, b""),
        }
        if let Some(entry) = self.project.cache.get(name) {
            hash.field(4, &entry.value);
        }
        hash.finish()
    }

    pub(crate) fn observe_replay_env(&mut self, name: &[u8]) {
        let in_setup = self.replay.setup.as_ref().is_some_and(SetupSession::blocks);
        if self.replay.observations.is_none() && !in_setup {
            return;
        }
        let digest = self
            .config
            .host
            .env_get(name)
            .map(|value| JournalDigest::hash(&value));
        if let Some(setup) = &mut self.replay.setup {
            setup.record_env(name, digest);
        }
        if let Some(observed) = &mut self.replay.observations {
            observed.record(JournalStamp::EnvVar {
                name: name.to_vec(),
                digest,
            });
        }
    }

    pub(crate) fn observe_replay_condition(&mut self, args: &[ExpandedArg]) {
        if self.replay.observations.is_none() {
            return;
        }
        let base = <Self as rcmake_expr::CondEnv>::base_dir(self).to_path_buf();
        let host = self.config.host.as_ref();
        let Some(observed) = &mut self.replay.observations else {
            return;
        };
        if args.iter().any(|arg| {
            matches!(
                arg.value.as_slice(),
                b"COMMAND"
                    | b"TARGET"
                    | b"TEST"
                    | b"RULE"
                    | b"MATCHES"
                    | b"IS_NEWER_THAN"
                    | b"IS_SYMLINK"
                    | b"IS_READABLE"
                    | b"IS_WRITABLE"
                    | b"IS_EXECUTABLE"
            )
        }) {
            observed.opaque = true;
            return;
        }
        for pair in args.windows(2) {
            let [op, operand] = pair else { continue };
            if matches!(op.value.as_slice(), b"EXISTS" | b"IS_DIRECTORY")
                && !operand.value.is_empty()
            {
                let path = base.join(path::to_native(&operand.value));
                match host.metadata(&path, Follow::Yes) {
                    Ok(metadata) => observed.record(JournalStamp::Metadata {
                        path,
                        follow: true,
                        stat: JournalFileStat::from(&metadata),
                    }),
                    Err(error) if error.kind == ErrorKind::NotFound => {
                        observed.record(JournalStamp::Missing { path })
                    }
                    Err(_) => observed.opaque = true,
                }
            }
        }
    }

    pub(crate) fn mark_replay_opaque(&mut self) {
        if let Some(observed) = &mut self.replay.observations {
            observed.opaque = true;
        }
    }

    pub(crate) fn note_replay_var_write(&mut self, name: &[u8]) {
        if let Some(observed) = &mut self.replay.observations {
            observed.vars.insert(name.into());
        }
    }

    pub(crate) fn note_replay_cache_write(&mut self, name: &[u8]) {
        if let Some(observed) = &mut self.replay.observations {
            observed.cache.insert(name.into());
        }
    }

    pub(crate) fn note_replay_defer(&mut self, dir: DirId, command: &[u8], args: &[Argument]) {
        if self.replay.observations.is_none() {
            return;
        }
        if dir == DirId::ROOT
            && command == b"target_link_libraries"
            && matches!(args, [arg] if arg.kind == ArgKind::Bracket
                && arg.text == super::leaf::REPLAY_LINK_CHECK)
        {
            let origin = PropertyOrigin::new(self.current_binary_dir(), &self.bt);
            if let Some(observed) = &mut self.replay.observations {
                observed.link_check = Some(origin);
            }
        } else {
            self.mark_replay_opaque();
        }
    }

    pub(crate) fn note_replay_expansion(
        &mut self,
        lower: &[u8],
        raw_args: &[Argument],
        args: &[ExpandedArg],
    ) {
        if self.replay.observations.is_some()
            && !super::leaf::shape::valid_expansion(lower, raw_args, args)
        {
            self.mark_replay_opaque();
        }
    }

    pub(crate) fn note_replay_executed_listfile(&mut self, file: &[u8]) {
        if let Some(paths) = &mut self.replay.executed_listfiles {
            paths.insert(path::to_native(file));
        }
    }

    pub(crate) fn capture_replay_foreach_var(&self, name: Option<&[u8]>) -> ForeachSaved {
        name.filter(|_| self.replay.observations.is_some())
            .and_then(|name| self.vars.lookup(name))
            .cloned()
    }

    pub(crate) fn finish_replay_foreach_var(&mut self, name: Option<&[u8]>, before: ForeachSaved) {
        if let Some(name) = name
            && before.as_ref() != self.vars.lookup(name)
            && let Some(observed) = &mut self.replay.observations
        {
            observed.vars.insert(name.into());
        }
    }

    fn replay_key(&self, source: &Path, directory: bool, root: &RegionKey) -> RegionKey {
        let binary_dir = self.current_binary_dir();
        let source_dir = self.current_source_dir();
        let kind = if directory {
            if self.current_dir_id() == DirId::ROOT
                && source_dir == root.source_dir
                && binary_dir == root.binary_dir
            {
                RegionKind::Configure
            } else {
                RegionKind::Subdirectory
            }
        } else if let Some(language) = self.bootstrap_language(source) {
            RegionKind::Bootstrap { language }
        } else {
            RegionKind::Include {
                file: source.into(),
            }
        };
        RegionKey {
            source_dir,
            binary_dir,
            kind,
        }
    }

    fn bootstrap_language(&self, source: &Path) -> Option<Vec<u8>> {
        let directory = self.vars.get(b"CMAKE_PLATFORM_INFO_DIR")?;
        if source.parent()? != path::to_native(directory) {
            return None;
        }
        let name = source.file_name()?.as_encoded_bytes();
        let language = name
            .strip_prefix(b"CMake")?
            .strip_suffix(b"Compiler.cmake")?;
        (!language.is_empty()
            && language
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_'))
        .then(|| language.to_vec())
    }

    fn generated_system_file(&self, source: &Path) -> bool {
        self.vars
            .get(b"CMAKE_PLATFORM_INFO_DIR")
            .is_some_and(|directory| source == path::to_native(directory).join("CMakeSystem.cmake"))
    }

    fn replay_tool_paths(&self, kind: &RegionKind, writes: &Writes) -> Option<Vec<PathBuf>> {
        // A target-only leaf uses the configured language settings. The bootstrap
        // and the whole listfile additionally bind their selected tools by content.
        if !matches!(kind, RegionKind::Configure | RegionKind::Bootstrap { .. }) {
            return Some(Vec::new());
        }
        let mut names = self.vars.visible_names();
        names.extend(self.project.cache.iter().map(|(name, _)| name.clone()));
        names.extend(writes.vars.keys().cloned());
        names.sort_unstable();
        names.dedup();
        let mut paths = std::collections::BTreeSet::new();
        for name in names {
            if name != b"CMAKE_TOOLCHAIN_FILE"
                && !(name.starts_with(b"CMAKE_") && name.ends_with(b"_COMPILER"))
            {
                continue;
            }
            let initial = self
                .vars
                .get(&name)
                .or_else(|| self.project.cache.get(&name).map(|e| &e.value));
            let assigned = writes.vars.get(&name).and_then(Option::as_ref);
            for value in initial
                .into_iter()
                .chain(assigned)
                .filter(|v| !v.is_empty())
            {
                let tool = path::to_native(value);
                if !tool.is_absolute() || value.contains(&b';') {
                    return None;
                }
                paths.insert(tool);
            }
            if paths.len() > 32 {
                return None;
            }
        }
        Some(paths.into_iter().collect())
    }

    fn entry_context(
        &self,
        session: &ReplaySession,
        writes: &Writes,
        source: JournalDigest,
    ) -> Option<EntryContext> {
        let mut h = JournalHasher::new(b"listfile-entry");
        h.field(1, session.options.as_bytes());
        h.field(2, source.as_bytes());
        h.field(3, &self.depth.to_le_bytes());
        h.field(
            4,
            PropertyOrigin::new(self.current_binary_dir(), &self.bt)
                .digest()
                .as_bytes(),
        );
        let scope = self.vars.scope_identity(false);
        if writes.record_variable_reads {
            let directory_scope = scope.map(|(depth, _, directory)| (depth, directory));
            h.field(5, format!("{directory_scope:?}").as_bytes());
        } else {
            h.field(5, format!("{scope:?}").as_bytes());
        }
        if !writes.record_variable_reads {
            for (name, state) in &self.variable_access {
                h.field(6, name);
                h.field(
                    7,
                    &[u8::from(state.load(std::sync::atomic::Ordering::Relaxed))],
                );
            }
        }
        // Every command frame checks this environment fallback for the recursion limit.
        if let Some(v) = session.host.env_get(b"CMAKE_MAXIMUM_RECURSION_DEPTH") {
            h.field(8, &v);
        }
        if writes.record_variable_reads {
            h.field(30, b"variable-reads");
            let name = b"CMAKE_MAXIMUM_RECURSION_DEPTH";
            if let Some(value) = self
                .vars
                .get(name)
                .or_else(|| self.project.cache.get(name).map(|entry| &entry.value))
            {
                h.field(31, value);
            }
        } else {
            for name in self.vars.visible_names() {
                h.field(10, &name);
                h.field(11, self.vars.get(&name)?);
            }
            let mut cache: Vec<_> = self.project.cache.iter().collect();
            cache.sort_unstable_by(|a, b| a.0.cmp(b.0));
            for (name, entry) in cache {
                h.field(12, name);
                h.field(13, CacheValue::from(entry).digest().as_bytes());
            }
        }
        if writes.directory_usage || !writes.targets.is_empty() {
            let dir = self.current_dir_id();
            let binary = self.current_binary_dir();
            let current = self.project.dir(dir);
            // Target constructors can read every declared property initializer.
            // Skip custom initializers and linked directory usage until recorded.
            if self
                .project
                .global
                .defined_props
                .iter()
                .any(|p| p.scope == b"TARGET" && p.initialize_from_variable.is_some())
                || current.props.get(b"LINK_LIBRARIES").is_some()
            {
                return None;
            }
            let usage = Usage::capture(&current.usage, &binary, dir)?;
            h.field(14, usage.digest().as_bytes());
            h.field(30, &current.define_flags);
            for name in current.props.names() {
                h.field(15, &name);
                h.field(16, current.props.get(&name)?);
            }
            for name in self.project.global.props.names() {
                if writes.installs
                    && writes.record_variable_reads
                    && super::leaf::earlier_install_property(&name, self.project.installs.len())
                {
                    continue;
                }
                h.field(20, &name);
                h.field(21, self.project.global.props.get(&name)?);
            }
            for id in self.project.ancestors(dir) {
                let ancestor = self.project.dir(id);
                h.field(17, &ancestor.binary_dir);
                if let Some(v) = ancestor.props.get(b"_RCMAKE_PROJECT_NAME") {
                    h.field(18, v);
                }
            }
            h.field(
                19,
                self.project.generator.map_or("", |g| g.name()).as_bytes(),
            );
        }
        if writes.links || (!writes.record_variable_reads && !writes.targets.is_empty()) {
            h.field(22, &self.hooks.next_defer_id.to_le_bytes());
            h.field(23, &[u8::from(self.root_frame.defer.is_some())]);
        }
        if writes.links {
            for (name, binding) in self.project.global_target_bindings() {
                let target = self.project.target(binding.id())?;
                h.field(24, name);
                h.field(25, target.kind.as_str().as_bytes());
                h.field(26, &[u8::from(target.is_imported())]);
                h.field(
                    27,
                    target
                        .props
                        .get(b"ENABLE_EXPORTS")
                        .map(|value| value.as_slice())
                        .unwrap_or_default(),
                );
            }
            for ancestor in self.project.ancestors(self.current_dir_id()) {
                for (name, binding) in &self.project.dir(ancestor).local_targets {
                    let target = self.project.target(binding.id())?;
                    h.field(28, name);
                    h.field(29, target.kind.as_str().as_bytes());
                }
            }
        }
        Some(EntryContext {
            rcmake: session.executable,
            flags: h.finish(),
            policies: JournalDigest::hash(format!("{:?}", self.policies).as_bytes()),
            diag: JournalDigest::hash(format!("{:?}", self.diags).as_bytes()),
            log_level: self.log_level.map_or(255, |level| level as u8),
        })
    }
}

#[cfg(test)]
#[path = "activation_tests.rs"]
mod tests;
