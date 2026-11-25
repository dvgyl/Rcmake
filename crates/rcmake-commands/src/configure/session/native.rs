use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_eval::journal::{ConfigureSnapshot, ReplayHost, ReplayReceipts};
use rcmake_eval::{
    FetchLockMode, HermeticMode, Interp, LedgerLevel, ProbeCacheMode, RunOutcome, Services,
};
use rcmake_host::{FileKind, Follow, Host, MemoHost, ReadRange, WriteMode};
use rcmake_json::{Value, write_compact};

use crate::fetch_observe::FetchRuntime;

use super::super::try_compile::active::ProbeRuntime;

pub(super) struct NativeSession {
    base: Arc<dyn Host>,
    evaluated: Arc<dyn Host>,
    memo: Option<Arc<MemoHost<dyn Host>>>,
    services: Services,
    fetch: Option<FetchRuntime>,
    probe: Option<Arc<ProbeRuntime>>,
    snapshot: PathBuf,
    pending: Option<Vec<u8>>,
    report: Option<BString>,
    measure: bool,
}

impl NativeSession {
    pub(super) fn new(
        base: Arc<dyn Host>,
        evaluated: Arc<dyn Host>,
        source: &Path,
        build: &Path,
        services: &Services,
    ) -> Option<Self> {
        let speed = &services.speed;
        let measure = base
            .env_get(b"RCMAKE_MEASURE_REPLAY")
            .is_some_and(|value| value == "1");
        let enabled = speed.incremental
            || speed.probe_cache != ProbeCacheMode::Off
            || speed.probe_jobs.is_some_and(|jobs| jobs.get() > 1)
            || speed.fetch_prefetch
            || speed.fetch_lock != FetchLockMode::Off
            || measure
            || base
                .env_get(b"RCMAKE_FETCH_OFFLINE")
                .is_some_and(|value| value == "1");
        if !enabled {
            return None;
        }
        let can_accelerate = speed.hermetic == HermeticMode::Off;
        let memo = (can_accelerate && speed.probe_cache != ProbeCacheMode::Verify)
            .then(|| Arc::new(MemoHost::from_shared(Arc::clone(&base))));
        let fetch = FetchRuntime::new(Arc::clone(&base), source, services);
        let probe = can_accelerate
            .then(|| ProbeRuntime::new(Arc::clone(&base), build, services).map(Arc::new))
            .flatten();
        let snapshot = build
            .join("CMakeFiles/rcmake")
            .join(ConfigureSnapshot::<()>::FILE_NAME);
        Some(Self {
            base,
            evaluated,
            memo,
            services: services.clone(),
            fetch,
            probe,
            snapshot,
            pending: None,
            report: None,
            measure,
        })
    }

    pub(super) fn host(&self) -> Arc<dyn Host> {
        self.memo.as_ref().map_or_else(
            || Arc::clone(&self.evaluated),
            |memo| Arc::clone(memo) as Arc<dyn Host>,
        )
    }

    pub(super) fn attach(&self, interp: &mut Interp) {
        let replay = self.replay_enabled(interp);
        if (self.probe.is_some() || replay) && cfg!(target_os = "macos") {
            let queries = super::super::platform_queries::PlatformQueries::default();
            queries.prefetch(&self.host());
            interp.add_session_data(Arc::new(queries));
            if replay {
                interp.add_session_data(Arc::new(ReplayReceipts::default()));
            }
        }
        if let Some(fetch) = &self.fetch {
            fetch.install(interp);
        }
    }

    pub(super) fn run(
        &mut self,
        interp: &mut Interp,
        source: &Path,
        build: &Path,
        preloads: &[PathBuf],
    ) -> RunOutcome {
        if self.measure {
            interp.track_executed_listfiles();
        }
        if let Some(memo) = &self.memo {
            memo.clear();
        }
        if let Some(probe) = &self.probe {
            interp.add_session_data(Arc::clone(probe));
        }
        if !interp.has_command_observers() {
            interp.enable_parse_cache();
        }
        let outcome = if let Some(probe) = self.probe.clone() {
            let services = self.services.clone();
            probe.evaluate(interp, &services, |interp| {
                self.evaluate(interp, source, build, preloads)
            })
        } else {
            self.evaluate(interp, source, build, preloads)
        };
        if self.measure && outcome.exit_code == 0 {
            self.report = Some(replay_report(interp));
        }
        outcome
    }

    fn evaluate(
        &mut self,
        interp: &mut Interp,
        source: &Path,
        build: &Path,
        preloads: &[PathBuf],
    ) -> RunOutcome {
        if self.replay_enabled(interp) {
            interp.add_session_data(ReplayHost(Arc::clone(&self.base)));
            let cached = self.read_snapshot();
            let (outcome, pending) =
                interp.configure_incremental(source, build, preloads, cached.as_deref());
            self.pending = pending;
            outcome
        } else {
            self.pending = None;
            interp.run_configure(source, build, preloads)
        }
    }

    fn replay_enabled(&self, interp: &Interp) -> bool {
        self.services.speed.incremental
            && self.services.speed.hermetic == HermeticMode::Off
            && self.services.speed.probe_cache != ProbeCacheMode::Verify
            && !self.services.collect_annotations
            && matches!(self.services.provenance, LedgerLevel::Off)
            && self.services.step_budget.is_none()
            && !interp.has_command_observers()
    }

    fn read_snapshot(&self) -> Option<Vec<u8>> {
        let metadata = self.base.metadata(&self.snapshot, Follow::No).ok()?;
        if metadata.kind != FileKind::File {
            return None;
        }
        self.base
            .read(
                &self.snapshot,
                ReadRange::All {
                    limit: ConfigureSnapshot::<()>::frame_limit() as u64,
                },
            )
            .ok()
    }

    pub(super) fn finish(self, complete: bool) {
        let complete = complete
            && self
                .services
                .cancel
                .as_ref()
                .is_none_or(|token| !token.is_cancelled());
        if let Some(fetch) = self.fetch {
            fetch.finish(complete);
        }
        if complete && let Some(bytes) = self.pending {
            publish(self.base.as_ref(), &self.snapshot, &bytes);
        }
        if complete && let Some(report) = self.report {
            publish(
                self.base.as_ref(),
                &self.snapshot.with_file_name("replay-result.v1.json"),
                &report,
            );
        }
    }
}

fn replay_report(interp: &Interp) -> BString {
    let replayed = interp.replayed_listfiles();
    let replayed: BTreeSet<_> = replayed
        .iter()
        .map(|path| rcmake_host::path::from_native_cmake(path))
        .collect();
    let executed: BTreeSet<_> = interp
        .executed_listfiles()
        .map(rcmake_host::path::from_native_cmake)
        .collect();
    let paths = |set: BTreeSet<BString>| Value::Array(set.into_iter().map(Value::String).collect());
    write_compact(&Value::Object(BTreeMap::from([
        ("version".into(), Value::UInt(1)),
        ("executed".into(), paths(executed)),
        ("replayed".into(), paths(replayed)),
    ])))
}

fn publish(host: &dyn Host, file: &Path, bytes: &[u8]) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let Some(dir) = file.parent() else { return };
    if host.create_dir_all(dir).is_err() {
        return;
    }
    let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
    let staged = dir.join(format!(".configure-{}-{suffix:x}", host.system().pid));
    if host.write(&staged, bytes, WriteMode::CreateNew).is_ok()
        && (host.set_permissions(&staged, 0o600).is_err() || host.rename(&staged, file).is_err())
    {
        let _ = host.remove_file(&staged);
    }
}
