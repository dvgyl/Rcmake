//! Record and replay listfiles with fully classified commands.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, path};
use rcmake_model::DirId;
use serde::{Deserialize, Serialize};

use super::frame::{MAX_FRAME_BYTES, encoded_size};
use super::leaf::{LocalState, Usage, Writes};
use super::region::RegionPayload;
use super::trace::JournalHasher;
use super::{
    CacheValue, ConfigureSnapshot, EntryContext, JournalDigest, JournalEffect, JournalInput,
    JournalStamp, OpaqueReason, PropertyOrigin, RegionKey, RegionKind, RegionRecord, RegionState,
    StateLookup, VariableScope, file_digest, verify_configure, verify_region,
};
use crate::code::Code;
use crate::interp::BodyMode;
use crate::{CmdResult, Flow, HermeticMode, Interp, LedgerLevel, RunMode, RunOutcome};

/// Base host used to verify journal inputs without a filesystem query cache.
pub struct ReplayHost(pub Arc<dyn Host>);

#[derive(Serialize, Deserialize, Default)]
struct Regions {
    root: Option<LocalState>,
    nested: Vec<(RegionRecord, LocalState)>,
}

type Manifest = ConfigureSnapshot<Regions>;

pub(crate) struct ReplaySession {
    host: Arc<dyn Host>,
    executable: JournalDigest,
    options: JournalDigest,
    root: RegionKey,
    source: Option<(BString, JournalDigest)>,
    previous: Option<Manifest>,
    pending: Regions,
    root_record: Option<RegionRecord>,
    bytes_left: usize,
}

impl ReplaySession {
    fn start(interp: &Interp, source: &Path, build: &Path, cached: Option<&[u8]>) -> Option<Self> {
        if !interp.can_replay() {
            return None;
        }
        let host = interp
            .session_data::<ReplayHost>()
            .map_or(&interp.config.host, |h| &h.0)
            .clone();
        let executable = file_digest(host.as_ref(), &host.current_exe().ok()?).ok()?;
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
        let previous = cached
            .and_then(|bytes| Manifest::decode(bytes).ok())
            .filter(|s| s.region.key == root && s.model.nested.len() <= 4096);
        Some(Self {
            host,
            executable,
            options: h.finish(),
            root,
            previous,
            source: None,
            pending: Regions::default(),
            root_record: None,
            bytes_left: MAX_FRAME_BYTES - 4096,
        })
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

    fn finish(self) -> Option<Vec<u8>> {
        if self.root_record.is_none() && self.pending.nested.is_empty() {
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
        Manifest {
            schema: Manifest::SCHEMA.into(),
            region,
            model: self.pending,
        }
        .encode()
        .ok()
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
        self.replay_hits = 0;
        self.replayed_listfiles.clear();
        self.replay_session = ReplaySession::start(self, source, build, cached);
        let result = self.run_configure(source, build, preloads);
        let session = self.replay_session.take();
        let bytes = if result.exit_code == 0 && self.can_replay() {
            session.and_then(ReplaySession::finish)
        } else {
            None
        };
        (result, bytes)
    }

    /// Number of listfile bodies restored by the last incremental configure.
    #[must_use]
    pub fn replay_hits(&self) -> u64 {
        self.replay_hits
    }

    /// Absolute paths of the listfiles restored by the last incremental configure.
    #[must_use]
    pub fn replayed_listfiles(&self) -> &std::collections::BTreeSet<PathBuf> {
        &self.replayed_listfiles
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
    }

    pub(crate) fn note_replay_source(&mut self, abs: &[u8], data: &[u8]) {
        if let Some(session) = &mut self.replay_session {
            session.source =
                (data.len() <= 1024 * 1024).then(|| (abs.into(), JournalDigest::hash(data)));
        }
    }

    pub(crate) fn run_replayable_listfile(&mut self, code: &Code, abs: &[u8]) -> CmdResult {
        self.run_listfile_body(code, abs, false)
    }

    pub(crate) fn run_listfile_body(
        &mut self,
        code: &Code,
        abs: &[u8],
        directory: bool,
    ) -> CmdResult {
        let Some(mut session) = self.replay_session.take() else {
            return self.run_code(code, 0, code.len(), BodyMode::File);
        };
        let plan = session
            .source
            .take()
            .filter(|(p, _)| p.as_slice() == abs)
            .and_then(|(_, digest)| {
                if !self.can_replay() || self.depth > 128 || self.unwinding_package {
                    return None;
                }
                let writes = Writes::classify(self, code)?;
                let source = path::to_native(abs);
                let key = self.replay_key(&source, directory, &session.root);
                let tools = self.replay_tool_paths(&key.kind, &writes)?;
                let context = self.entry_context(&session, &writes, digest)?;
                Some((writes, key, context, source, digest, tools))
            });
        let Some((writes, key, context, source, digest, tools)) = plan else {
            self.replay_session = Some(session);
            return self.run_code(code, 0, code.len(), BodyMode::File);
        };

        let valid_record = |record: &RegionRecord| {
            matches!(&record.payload, RegionPayload::Complete {inputs, effects}
                if matches!(inputs.split_first(), Some((JournalInput::Host(JournalStamp::FileHash { path, digest: sum }), rest))
                    if *path == source && *sum == digest && rest.len() == tools.len()
                        && rest.iter().zip(&tools).all(|(input, tool)| matches!(input,
                            JournalInput::Host(JournalStamp::FileHash { path, .. }) if path == tool)))
                    && writes.check_effects(effects, self.config.limits.max_value_bytes))
        };
        let candidate = session.previous.as_ref().and_then(|snapshot| {
            if key.kind == RegionKind::Configure {
                if !valid_record(&snapshot.region) {
                    return None;
                }
                let checked =
                    verify_configure(session.host.as_ref(), snapshot, &key, &context, |_| {
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
                    .filter(|(r, _)| r.key == key && r.context == context && valid_record(r))
                    .find_map(|(r, s)| {
                        let checked =
                            verify_region(session.host.as_ref(), r, &key, &context, |_| {
                                StateLookup::Unavailable
                            })
                            .ok()?;
                        Some((r, s, checked))
                    })
            }
        });
        if let Some((record, state, checked)) = candidate
            && state.restore(self, &writes)
        {
            // All effects passed check_effects. Each effect is a local variable
            // write within the value limit. No callback can observe the writes.
            checked
                .replay_into(&mut self.cx(), |_| Ok(()))
                .map_err(|error| crate::CmdError::status(error.to_string()))?;
            self.replay_hits += 1;
            self.replayed_listfiles.insert(source);
            let entry = (record.clone(), state.clone());
            session.keep(entry.0, entry.1);
            self.replay_session = Some(session);
            return Ok(Flow::Normal);
        }
        let diagnostics = self.engine.diagnostics().len();
        let log_bytes = self.configure_log.len();
        let unread: Vec<_> = self.unread_variables().map(BString::from).collect();
        let result = self.run_code(code, 0, code.len(), BodyMode::File);
        if matches!(result, Ok(Flow::Normal))
            && self.can_replay()
            && diagnostics == self.engine.diagnostics().len()
            && log_bytes == self.configure_log.len()
            && let Some(state) = LocalState::capture(self, &writes, &unread)
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
            for name in writes.vars.keys() {
                journal.record_effect(JournalEffect::SetVar {
                    scope: VariableScope::Current,
                    name: name.to_vec(),
                    value: self.vars.get(name).map(|value| value.to_vec()),
                });
            }
            if matches!(journal, RegionState::Recording(_)) {
                session.keep(journal.finish(key, context), state);
            }
        }
        self.replay_session = Some(session);
        result
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
            &serde_json::to_vec(&PropertyOrigin::new(self.current_binary_dir(), &self.bt)).ok()?,
        );
        h.field(
            5,
            format!("{:?}", self.vars.scope_identity(false)).as_bytes(),
        );
        for (name, state) in &self.variable_access {
            h.field(6, name);
            h.field(
                7,
                &[u8::from(state.load(std::sync::atomic::Ordering::Relaxed))],
            );
        }
        // Every command frame checks this environment fallback for the recursion limit.
        if let Some(v) = session.host.env_get(b"CMAKE_MAXIMUM_RECURSION_DEPTH") {
            h.field(8, &v);
        }
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
            let usage = Usage::capture(&current.usage, &binary)?;
            h.field(14, &serde_json::to_vec(&usage).ok()?);
            for name in current.props.names() {
                h.field(15, &name);
                h.field(16, current.props.get(&name)?);
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
