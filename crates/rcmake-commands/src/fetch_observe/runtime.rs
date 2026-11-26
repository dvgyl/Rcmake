// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use rcmake_base::truth;
use rcmake_diag::{Effect, Frame, MessageKind};
use rcmake_eval::{CmdError, Cx, FetchLockMode, HermeticMode, Interp, Services};
use rcmake_expr::ExpandedArg;
use rcmake_fetch::{ContentHash, FetchError, FetchPool, LockSession, PrefetchSource, Ticket};
use rcmake_host::{FileKind, Follow, Host, ReadRange, path::to_native};
use rcmake_store::{Digest, StoreConfig, StoreMode};

use super::frozen::Frozen;
use super::{BlockReason, BlockedFetch, DeclaredSource, FetchSession, PopulationSite};
use super::{lock_mode, parse_fetchcontent_declare, relative_repo, text};

const FETCH_CONTENT: &str = "Modules/FetchContent.cmake";
const EXTERNAL_PROJECT: &str = "Modules/ExternalProject";
const MAX_GENERATED: u64 = 1024 * 1024;
const OBSERVED_COMMANDS: &[&[u8]] = &[
    b"fetchcontent_declare",
    b"fetchcontent_makeavailable",
    b"fetchcontent_populate",
    b"__fetchcontent_declaredetails",
    b"__fetchcontent_populate",
    b"__fetchcontent_dopopulation",
    b"__fetchcontent_populatedirect",
    b"__fetchcontent_populatesubbuild",
    b"_ep_add_download_command",
    b"_ep_write_downloadfile_script",
    b"_ep_write_gitclone_script",
    b"__fetchcontent_dostepdirect",
    b"fetchcontent_setpopulated",
    b"configure_file",
    b"include",
    b"file",
    b"execute_process",
];

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Script {
    Archive,
    GitClone,
    GitUpdate,
    Extract,
    Step,
}

#[derive(Clone, Eq, PartialEq)]
struct CallSite {
    depth: usize,
    file: Arc<[u8]>,
    line: u32,
}

impl CallSite {
    fn new(depth: usize, frame: &Frame) -> Self {
        Self {
            depth,
            file: Arc::clone(&frame.file),
            line: frame.line,
        }
    }
}

struct ActivePopulation {
    site: CallSite,
    dependency: String,
    fetch: FetchSession,
    frozen: Option<Frozen>,
}

struct GeneratedScript {
    site: CallSite,
    kind: Script,
    digest: Digest,
}

#[derive(Default)]
struct State {
    effective: BTreeMap<String, String>,
    direct: BTreeMap<String, bool>,
    effective_credentials: std::collections::BTreeSet<String>,
    active: Vec<ActivePopulation>,
    generated: BTreeMap<PathBuf, GeneratedScript>,
    pending: Option<(PathBuf, Script, CallSite)>,
    pool: Option<FetchPool>,
    tickets: std::collections::HashMap<PrefetchSource, Ticket>,
    populated: BTreeMap<String, Option<&'static str>>,
}

pub(crate) struct FetchRuntime(Arc<Runtime>);

pub(super) struct Runtime {
    pub(super) host: Arc<dyn Host>,
    pub(super) config: StoreConfig,
    pub(super) lock: Mutex<Option<Result<LockSession<'static>, FetchError>>>,
    services: Services,
    mode: FetchLockMode,
    offline: bool,
    frozen: bool,
    state: Mutex<State>,
    declarations: Mutex<FetchSession>,
}

impl FetchRuntime {
    pub(crate) fn new(host: Arc<dyn Host>, source: &Path, services: &Services) -> Option<Self> {
        if !host.supports_process_execution() {
            return None;
        }
        let declarations = FetchSession::new(services, host.as_ref());
        if declarations.mode == FetchLockMode::Off
            && !declarations.offline
            && !declarations.prefetch
        {
            return None;
        }
        let config = StoreConfig::from_host(host.as_ref());
        let lock =
            LockSession::open_shared(Arc::clone(&host), source, lock_mode(declarations.mode));
        Some(Self(Arc::new(Runtime {
            host,
            config,
            lock: Mutex::new(Some(lock)),
            services: services.clone(),
            mode: declarations.mode,
            offline: declarations.offline,
            frozen: declarations.mode == FetchLockMode::Frozen,
            state: Mutex::default(),
            declarations: Mutex::new(declarations),
        })))
    }

    pub(crate) fn install(&self, interp: &mut Interp) {
        interp.add_session_data(Arc::clone(&self.0));
        interp.set_expanded_observer(OBSERVED_COMMANDS, observe);
    }

    pub(crate) fn finish(self, complete: bool) -> Result<(), FetchError> {
        let (pool, populated) = {
            let mut state = self.0.state();
            (state.pool.take(), std::mem::take(&mut state.populated))
        };
        drop(pool);
        let lock = lock_mutex(&self.0.lock).take();
        if complete && let Some(Ok(lock)) = lock {
            let report = lock.commit()?;
            if self.0.mode == FetchLockMode::Update {
                let count = report.pinned.len();
                let path = report.path.display();
                let summary = if report.written {
                    format!("-- rcmake: wrote {path} ({count} pinned)\n")
                } else {
                    format!("-- rcmake: {path} not changed ({count} pinned)\n")
                };
                self.0
                    .host
                    .write_out(rcmake_host::Stream::Stderr, summary.as_bytes());
                let pinned: std::collections::BTreeSet<&str> =
                    report.pinned.iter().map(String::as_str).collect();
                for (dep, reason) in populated {
                    if !pinned.contains(dep.as_str()) {
                        let reason = reason.unwrap_or("transfer cannot record a content hash");
                        let line = format!(
                            "-- rcmake: {dep} not pinned ({reason}); --fetch-lock=frozen will reject it.\n"
                        );
                        self.0
                            .host
                            .write_out(rcmake_host::Stream::Stderr, line.as_bytes());
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) fn runtime(cx: &Cx<'_>) -> Option<Arc<Runtime>> {
    cx.session_data::<Arc<Runtime>>().cloned()
}

pub(super) fn lock_mutex<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone)]
pub(super) struct Population {
    site: CallSite,
    pub(super) dependency: String,
    pub(super) subbuild: bool,
    pub(super) source: PathBuf,
}

impl Population {
    pub(super) fn site(&self) -> PopulationSite<'_> {
        PopulationSite {
            dependency: &self.dependency,
            source_override: false,
            effective_git_repository: None,
        }
    }
}

impl Runtime {
    fn state(&self) -> MutexGuard<'_, State> {
        lock_mutex(&self.state)
    }

    pub(super) fn population(&self, cx: &Cx<'_>) -> Option<Population> {
        let trace = cx.backtrace();
        let frames: Vec<_> = trace.frames().collect();
        let direct = frames.iter().position(|frame| {
            frame_is(cx, frame, FETCH_CONTENT, b"__fetchcontent_populatedirect")
                || frame_is(cx, frame, FETCH_CONTENT, b"__fetchcontent_populatesubbuild")
        })?;
        let frame = *frames.get(direct)?;
        let site = CallSite::new(frames.len() - direct - 1, frame);
        let dependency = self
            .state()
            .active
            .iter()
            .rev()
            .find(|active| active.site == site)
            .map(|active| active.dependency.clone())
            .unwrap_or_else(|| {
                let name = cx.var_or_empty(b"contentName");
                text(&name).unwrap_or("<dependency>").to_ascii_lowercase()
            });
        let source = to_native(&cx.var_or_empty(b"ARG_SOURCE_DIR"));
        Some(Population {
            site,
            dependency,
            subbuild: named(frame, b"__fetchcontent_populatesubbuild"),
            source,
        })
    }

    pub(super) fn with_fetch<T>(&self, pop: &Population, f: impl FnOnce(&FetchSession) -> T) -> T {
        let state = self.state();
        if let Some(active) = state
            .active
            .iter()
            .rev()
            .find(|active| active.site == pop.site)
        {
            return f(&active.fetch);
        }
        drop(state);
        // A matching module frame can require enforcement even when the
        // population arguments could not be authenticated.
        let mut fetch = FetchSession::new(&self.services, self.host.as_ref());
        fetch.offline = self.offline;
        f(&fetch)
    }

    pub(super) fn script(&self, cx: &Cx<'_>, pop: &Population) -> Option<Script> {
        let trace = cx.backtrace();
        let frame = trace.top()?;
        let path = canonical(self.host.as_ref(), &to_native(&frame.file));
        let wrapper = canonical(
            self.host.as_ref(),
            &to_native(&cx.var_or_empty(b"arg_SCRIPT_FILE")),
        );
        let (kind, expected, expected_wrapper) = {
            let state = self.state();
            let generated = state.generated.get(&path)?;
            let step = state.generated.get(&wrapper)?;
            if generated.site != pop.site || step.site != pop.site || step.kind != Script::Step {
                return None;
            }
            (generated.kind, generated.digest, step.digest)
        };
        let trusted_wrapper = bundled_command(cx, b"__fetchcontent_dostepdirect")
            && trace
                .frames()
                .any(|frame| frame_is(cx, frame, FETCH_CONTENT, b"include"))
            && trace
                .frames()
                .any(|frame| canonical(self.host.as_ref(), &to_native(&frame.file)) == wrapper);
        (trusted_wrapper
            && self.script_digest(&path) == Some(expected)
            && self.script_digest(&wrapper) == Some(expected_wrapper))
        .then_some(kind)
    }

    pub(super) fn generated_digest(
        &self,
        pop: &Population,
        path: &Path,
        kind: Script,
    ) -> Option<Digest> {
        let path = canonical(self.host.as_ref(), path);
        let expected = {
            let state = self.state();
            let generated = state.generated.get(&path)?;
            (generated.site == pop.site && generated.kind == kind).then_some(generated.digest)?
        };
        (self.script_digest(&path)? == expected).then_some(expected)
    }

    fn set_frozen(&self, pop: &Population, frozen: Option<Frozen>) {
        if let Some(active) = self
            .state()
            .active
            .iter_mut()
            .rev()
            .find(|active| active.site == pop.site)
        {
            active.frozen = frozen;
        }
    }

    fn take_frozen(&self, pop: &Population) -> Option<Frozen> {
        self.state()
            .active
            .iter_mut()
            .rev()
            .find(|active| active.site == pop.site)?
            .frozen
            .take()
    }

    pub(super) fn script_digest(&self, path: &Path) -> Option<Digest> {
        let bytes = self
            .host
            .read(
                path,
                ReadRange::All {
                    limit: MAX_GENERATED,
                },
            )
            .ok()?;
        Some(Digest::hash(&bytes))
    }

    fn generated_script(&self, cx: &Cx<'_>, frame: &Frame, args: &[ExpandedArg]) {
        if !named(frame, b"configure_file")
            || !module_is(
                cx,
                &frame.file,
                "Modules/ExternalProject/shared_internal_commands.cmake",
            )
            || !cx.is_builtin(b"configure_file")
        {
            return;
        }
        let [input, output, ..] = args else {
            return;
        };
        let input = to_native(&input.value);
        if !input.parent().is_some_and(|parent| {
            canonical(cx.host(), parent)
                == canonical(cx.host(), &cx.cmake_root().join(EXTERNAL_PROJECT))
        }) {
            return;
        }
        let kind = match input.file_name().and_then(|name| name.to_str()) {
            Some("download.cmake.in") => Script::Archive,
            Some("gitclone.cmake.in") => Script::GitClone,
            Some("gitupdate.cmake.in") => Script::GitUpdate,
            Some("extractfile.cmake.in") => Script::Extract,
            Some("stepscript.cmake.in") => Script::Step,
            _ => return,
        };
        let output = to_native(&output.value);
        if output.is_absolute()
            && let Some(pop) = self.population(cx)
        {
            let mut state = self.state();
            state
                .generated
                .remove(&canonical(self.host.as_ref(), &output));
            state.pending = Some((output, kind, pop.site));
        }
    }

    fn capture_generated(&self) {
        let mut state = self.state();
        if let Some((path, kind, site)) = state.pending.take()
            && let Some(digest) = self.script_digest(&path)
        {
            let path = canonical(self.host.as_ref(), &path);
            state
                .generated
                .insert(path, GeneratedScript { site, kind, digest });
        }
    }

    fn observe_declare(&self, args: &[ExpandedArg]) {
        // The stock module can later select a provider or a source override.
        // Defer any rejection to an actual population.
        let _ = lock_mutex(&self.declarations).observe(args);
    }

    fn prefetch(&self, cx: &mut Cx<'_>, args: &[ExpandedArg]) {
        let declarations = lock_mutex(&self.declarations);
        if !declarations.prefetch
            || declarations.offline
            || declarations.mode == FetchLockMode::Frozen
            || self.services.speed.hermetic != HermeticMode::Off
            || self.config.mode == StoreMode::Off
            || enabled(cx, b"FETCHCONTENT_FULLY_DISCONNECTED")
            || enabled(cx, b"FETCHCONTENT_UPDATES_DISCONNECTED")
            || cx
                .global()
                .props
                .get(b"__FETCHCONTENT_MAKEAVAILABLE_SERIAL_PROVIDER")
                .is_some_and(|command| !truth::is_off(command))
            || cx.var_or_empty(b"FETCHCONTENT_TRY_FIND_PACKAGE_MODE") == "ALWAYS"
            || !default_transport(cx, self.host.as_ref())
        {
            return;
        }
        let mut state = self.state();
        for arg in args {
            let Some(name) = text(&arg.value) else {
                continue;
            };
            let Some(declaration) = declarations.get(name) else {
                continue;
            };
            let upper = declaration.name.to_ascii_uppercase();
            let properties = &cx.global().props;
            if properties
                .get(format!("_FetchContent_{}_populated", declaration.name).as_bytes())
                .is_some()
                || properties
                    .get(format!("_FetchContent_{}_find_package_args", declaration.name).as_bytes())
                    .is_some()
            {
                continue;
            }
            if enabled(cx, format!("FETCHCONTENT_SOURCE_DIR_{upper}").as_bytes())
                || enabled(
                    cx,
                    format!("FETCHCONTENT_UPDATES_DISCONNECTED_{upper}").as_bytes(),
                )
                || !state
                    .direct
                    .get(&declaration.name)
                    .copied()
                    .unwrap_or(false)
            {
                continue;
            }
            let Some(source) = declarations.prefetch_source(declaration) else {
                continue;
            };
            if matches!(source, PrefetchSource::Git { .. }) && cx.var_defined(b"GIT_EXECUTABLE") {
                continue;
            }
            if state.tickets.contains_key(&source) {
                continue;
            }
            if state.pool.is_none() {
                state.pool = FetchPool::new_with_jobs(
                    Arc::clone(&self.host),
                    self.config.clone(),
                    self.services
                        .speed
                        .fetch_jobs
                        .map(std::num::NonZeroUsize::get),
                )
                .ok();
            }
            if let Some(ticket) = state
                .pool
                .as_ref()
                .and_then(|pool| pool.submit(source.clone()).ok())
            {
                state.tickets.insert(source, ticket);
            }
        }
    }

    pub(super) fn wait_for_archive(&self, url: &str, hash: &ContentHash) {
        self.wait_for(PrefetchSource::Url {
            url: url.into(),
            hash: hash.clone(),
        });
    }

    pub(super) fn wait_for_git(&self, repository: &str, commit: &str) {
        self.wait_for(PrefetchSource::Git {
            repository: repository.into(),
            commit: commit.into(),
        });
    }

    fn wait_for(&self, source: PrefetchSource) {
        let ticket = self.state().tickets.remove(&source);
        if let Some(ticket) = ticket {
            let _ = ticket.wait();
        }
    }

    fn remember_effective(&self, args: &[ExpandedArg]) {
        let Some(effective) = parse_fetchcontent_declare(args) else {
            return;
        };
        let declarations = lock_mutex(&self.declarations);
        let mut state = self.state();
        state
            .direct
            .entry(effective.name.clone())
            .or_insert_with(|| args.iter().any(|arg| arg.value == "__DIRECT_POPULATION"));
        if let DeclaredSource::Git { repository, .. } = effective.source
                        && declarations.get(&effective.name).is_some_and(|raw| {
                matches!(&raw.source, DeclaredSource::Git { repository, .. } if relative_repo(repository))
            })
        {
            if super::portable_source(&repository) {
                state.effective.entry(effective.name).or_insert(repository);
            } else {
                state.effective_credentials.insert(effective.name);
            }
        }
    }

    fn begin_population(
        &self,
        frame: &Frame,
        depth: usize,
        args: &[ExpandedArg],
        explicit: bool,
        source: PathBuf,
    ) -> Option<Population> {
        let name = text(&args.first()?.value)?.to_ascii_lowercase();
        let declarations = lock_mutex(&self.declarations);
        let raw = declarations.get(&name).cloned();
        let mut state = self.state();
        let mut fetch = FetchSession::new(&self.services, self.host.as_ref());
        fetch.offline = declarations.offline;
        let dependency = name.clone();
        if explicit {
            let _ = fetch.observe(args);
        } else if declarations.credentials.contains(&name)
            || state.effective_credentials.contains(&name)
        {
            fetch.credentials.insert(name);
        } else if let Some(mut declaration) = raw {
            if let DeclaredSource::Git { repository, .. } = &mut declaration.source
                && relative_repo(repository)
                && let Some(effective) = state.effective.get(&name)
            {
                *repository = effective.clone();
            }
            fetch.declarations.insert(name, declaration);
        }
        if self.mode == FetchLockMode::Update {
            let reason = match fetch.get(&dependency) {
                Some(decl) if !decl.supported() => Some("transfer cannot record a content hash"),
                Some(decl) => match &decl.source {
                    DeclaredSource::Archive { urls, hash: None }
                        if urls.iter().any(|url| url.starts_with("file://")) =>
                    {
                        Some("file:// URL has no URL_HASH")
                    }
                    DeclaredSource::Git { .. } if decl.git_commit().is_none() => {
                        Some("Git reference is not a full commit ID")
                    }
                    DeclaredSource::Other => Some("transfer cannot record a content hash"),
                    _ => Some("transfer cannot record a content hash"),
                },
                None => Some("transfer cannot record a content hash"),
            };
            state.populated.entry(dependency.clone()).or_insert(reason);
        }
        // Keep the same key that the transfer will obtain from the backtrace.
        state.active.retain(|active| active.site.depth < depth);
        let site = CallSite::new(depth, frame);
        state.active.push(ActivePopulation {
            site: site.clone(),
            dependency: dependency.clone(),
            fetch,
            frozen: None,
        });
        Some(Population {
            site,
            dependency,
            source,
            subbuild: named(frame, b"__fetchcontent_populatesubbuild"),
        })
    }
}

fn observe(cx: &mut Cx<'_>, args: &[ExpandedArg]) {
    let Some(runtime) = runtime(cx) else { return };
    runtime.capture_generated();
    let trace = cx.backtrace();
    let Some(frame) = trace.top() else { return };
    runtime.generated_script(cx, frame, args);
    if frame_is(cx, frame, FETCH_CONTENT, b"_ep_add_download_command")
        && cx
            .user_command_file(b"_ep_add_download_command")
            .is_some_and(|path| {
                module_is(
                    cx,
                    path,
                    "Modules/ExternalProject/shared_internal_commands.cmake",
                )
            })
        && let Some(pop) = runtime.population(cx)
        && !pop.subbuild
    {
        if super::archive::prepare_local_archive(cx, Arc::clone(&runtime), &pop).is_err() {
            return;
        }
        if let Some(mut frozen) = runtime.take_frozen(&pop) {
            frozen.local_archive(cx);
            runtime.set_frozen(&pop, Some(frozen));
        }
    }
    if named(frame, b"fetchcontent_declare") && bundled_command(cx, b"fetchcontent_declare") {
        runtime.observe_declare(args);
    } else if named(frame, b"fetchcontent_makeavailable")
        && bundled_command(cx, b"fetchcontent_makeavailable")
    {
        runtime.prefetch(cx, args);
    } else if named(frame, b"__fetchcontent_declaredetails")
        && bundled_command(cx, b"__fetchcontent_declaredetails")
        && trace
            .frames()
            .any(|frame| frame_is(cx, frame, FETCH_CONTENT, b"cmake_language"))
    {
        runtime.remember_effective(args);
    } else if (frame_is(cx, frame, FETCH_CONTENT, b"__fetchcontent_populatedirect")
        || frame_is(cx, frame, FETCH_CONTENT, b"__fetchcontent_populatesubbuild"))
        && frame
            .name
            .as_deref()
            .is_some_and(|name| bundled_command(cx, &name.to_ascii_lowercase()))
        && bundled_command(cx, b"__fetchcontent_dopopulation")
        && bundled_command(cx, b"__fetchcontent_populate")
        && trace
            .frames()
            .skip(1)
            .any(|frame| named(frame, b"__fetchcontent_dopopulation"))
    {
        let explicit = trace
            .frames()
            .skip(1)
            .find(|frame| named(frame, b"__fetchcontent_dopopulation"))
            .is_some_and(|calling| module_is(cx, &calling.file, FETCH_CONTENT));
        let name = cx.var_or_empty(b"contentName");
        let values = std::iter::once(name).chain(rcmake_base::list::split(
            cx.var_or_empty(b"ARG_UNPARSED_ARGUMENTS").as_slice(),
            rcmake_base::list::Empty::Keep,
        ));
        let values: Vec<_> = values
            .map(|value| ExpandedArg {
                value,
                kind: rcmake_syntax::ArgKind::Quoted,
                line: frame.line,
            })
            .collect();
        let source = to_native(&cx.var_or_empty(b"ARG_SOURCE_DIR"));
        if let Some(pop) =
            runtime.begin_population(frame, trace.frames().count() - 1, &values, explicit, source)
            && runtime.frozen
            && let Ok(frozen) = Frozen::select(cx, &runtime, &pop)
        {
            runtime.set_frozen(&pop, frozen);
        }
    } else if runtime.frozen
        && frame_is(cx, frame, FETCH_CONTENT, b"fetchcontent_setpopulated")
        && bundled_command(cx, b"fetchcontent_setpopulated")
        && bundled_command(cx, b"__fetchcontent_populate")
        && trace
            .frames()
            .nth(1)
            .is_some_and(|parent| named(parent, b"__fetchcontent_populate"))
        && enabled(cx, b"FETCHCONTENT_FULLY_DISCONNECTED")
        && !enabled(
            cx,
            &[
                b"FETCHCONTENT_SOURCE_DIR_".as_slice(),
                &cx.var_or_empty(b"contentNameUpper"),
            ]
            .concat(),
        )
        && let [_, source_key, source, ..] = args
        && source_key.value == "SOURCE_DIR"
        && let Some(pop) = runtime.begin_population(
            frame,
            trace.frames().count() - 1,
            args,
            false,
            to_native(&source.value),
        )
    {
        if let Ok(Some(frozen)) = Frozen::select(cx, &runtime, &pop) {
            let _ = frozen.disconnected(cx, &runtime, &pop);
        }
    } else if runtime.frozen
        && [
            b"_ep_write_downloadfile_script".as_slice(),
            b"_ep_write_gitclone_script",
            b"__fetchcontent_dostepdirect",
            b"include",
        ]
        .into_iter()
        .any(|name| named(frame, name))
        && let Some(pop) = runtime.population(cx)
        && let Some(mut frozen) = runtime.take_frozen(&pop)
    {
        observe_step(cx, &runtime, &pop, frame, args, &mut frozen);
        runtime.set_frozen(&pop, Some(frozen));
    }
}

fn observe_step(
    cx: &mut Cx<'_>,
    runtime: &Runtime,
    pop: &Population,
    frame: &Frame,
    args: &[ExpandedArg],
    frozen: &mut Frozen,
) {
    let shared = "Modules/ExternalProject/shared_internal_commands.cmake";
    let defined_in = |name, file| {
        cx.user_command_file(name)
            .is_some_and(|path| module_is(cx, path, file))
    };
    if frame_is(cx, frame, shared, b"_ep_write_downloadfile_script")
        && defined_in(b"_ep_write_downloadfile_script", shared)
        && let [script, _, file, ..] = args
    {
        let _ = frozen.remote_archive(cx, pop, to_native(&script.value), to_native(&file.value));
    } else if frame_is(cx, frame, shared, b"_ep_write_gitclone_script")
        && defined_in(b"_ep_write_gitclone_script", shared)
        && let [_, _, program, ..] = args
    {
        frozen.git_executable(&program.value);
    } else if frame_is(cx, frame, FETCH_CONTENT, b"__fetchcontent_dostepdirect")
        && bundled_command(cx, b"__fetchcontent_dostepdirect")
        && let [script_key, script, stamp_key, stamp, ..] = args
        && script_key.value == "SCRIPT_FILE"
        && stamp_key.value == "STAMP_FILE"
    {
        let path = to_native(&script.value);
        let stamp = to_native(&stamp.value);
        let stamp_dir = to_native(&cx.var_or_empty(b"_EP_STAMP_DIR"));
        let tmp_dir = to_native(&cx.var_or_empty(b"_EP_TMP_DIR"));
        let step = ["download", "update", "patch"].into_iter().any(|step| {
            path == tmp_dir.join(format!("{step}.cmake"))
                && stamp == stamp_dir.join(format!("{step}.stamp"))
        });
        if step && let Some(digest) = runtime.generated_digest(pop, &path, Script::Step) {
            let _ = frozen.step(cx, runtime, pop, &stamp, digest);
        } else {
            stop(cx, "Could not verify the generated FetchContent step.");
        }
    } else if named(frame, b"include")
        && cx.is_builtin(b"include")
        && bundled_command(cx, b"__fetchcontent_dostepdirect")
        && let [script] = args
    {
        let inner = to_native(&script.value);
        let outer = to_native(&frame.file);
        let tmp = to_native(&cx.var_or_empty(b"_EP_TMP_DIR"));
        for (wrapper, kind) in [
            ("update.cmake", Script::GitUpdate),
            ("download.cmake", Script::GitClone),
        ] {
            if outer == tmp.join(wrapper)
                && to_native(&cx.var_or_empty(b"arg_SCRIPT_FILE")) == outer
                && runtime
                    .generated_digest(pop, &outer, Script::Step)
                    .is_some()
                && runtime.generated_digest(pop, &inner, kind).is_some()
            {
                frozen.ran_update();
            }
        }
    }
}

pub(super) fn emit_block(cx: &mut Cx<'_>, error: BlockedFetch) {
    cx.issue(
        MessageKind::Error,
        Effect::Fatal,
        None,
        error.to_string().into(),
    );
}

pub(super) fn blocked(cx: &mut Cx<'_>, blocked: Option<BlockedFetch>) -> Result<(), CmdError> {
    if let Some(error) = blocked {
        emit_block(cx, error);
        Err(CmdError::Reported)
    } else {
        Ok(())
    }
}

pub(super) fn stop(cx: &mut Cx<'_>, reason: impl Into<rcmake_base::BString>) -> CmdError {
    cx.issue(MessageKind::Error, Effect::Fatal, None, reason.into());
    CmdError::Reported
}

pub(super) fn required(
    cx: &mut Cx<'_>,
    runtime: &Runtime,
    pop: &Population,
    reason: BlockReason,
) -> Result<(), CmdError> {
    let failure = runtime.with_fetch(pop, |fetch| {
        if fetch.credentials.contains(&pop.dependency) {
            return fetch.unportable::<()>(&pop.dependency).err();
        }
        fetch.blocked(pop.site(), reason)
    });
    blocked(cx, failure)
}

fn named(frame: &Frame, name: &[u8]) -> bool {
    frame
        .name
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(name))
}

fn frame_is(cx: &Cx<'_>, frame: &Frame, relative: &str, name: &[u8]) -> bool {
    named(frame, name) && module_is(cx, &frame.file, relative)
}

fn bundled_command(cx: &Cx<'_>, name: &[u8]) -> bool {
    cx.user_command_file(name)
        .is_some_and(|path| module_is(cx, path, FETCH_CONTENT))
}

pub(super) fn module_is(cx: &Cx<'_>, file: &[u8], relative: &str) -> bool {
    let file = to_native(file);
    let module = cx.cmake_root().join(relative);
    file == module || canonical(cx.host(), &file) == canonical(cx.host(), &module)
}

fn canonical(host: &dyn Host, path: &Path) -> PathBuf {
    host.canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

pub(super) fn check_unmatched(cx: &mut Cx<'_>) -> Result<(), CmdError> {
    let offline = runtime(cx).map_or_else(
        || {
            cx.host()
                .env_get(b"RCMAKE_FETCH_OFFLINE")
                .is_some_and(|value| value == "1")
        },
        |runtime| runtime.offline,
    );
    if offline {
        return Err(stop(
            cx,
            "Offline fetch blocks remote transfers outside a pinned population.",
        ));
    }
    if cx.services().speed.fetch_lock == FetchLockMode::Frozen {
        let host = cx.host();
        let external = canonical(host, &cx.cmake_root().join(EXTERNAL_PROJECT));
        let trace = cx.backtrace();
        if trace.frames().any(|frame| {
            module_is(cx, &frame.file, FETCH_CONTENT)
                || module_is(cx, &frame.file, "Modules/ExternalProject.cmake")
                || canonical(host, &to_native(&frame.file)).starts_with(&external)
        }) {
            return Err(stop(
                cx,
                "Frozen fetch blocks transfers outside a matching locked population.",
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_unmatched_url(cx: &mut Cx<'_>, url: &[u8]) -> Result<(), CmdError> {
    let local = std::str::from_utf8(url)
        .ok()
        .and_then(|url| url.split_once(':'))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
        .and_then(|(_, rest)| crate::hermetic::local_file(rest, cfg!(windows)))
        .is_some();
    if !local {
        check_unmatched(cx)?;
    }
    Ok(())
}

pub(crate) fn check_upload(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> Result<(), CmdError> {
    if let [op, _, url, ..] = args
        && op.value == "UPLOAD"
        && cx.host().supports_process_execution()
    {
        check_unmatched_url(cx, &url.value)?;
    }
    Ok(())
}

fn enabled(cx: &Cx<'_>, name: &[u8]) -> bool {
    !truth::is_off(&cx.var_or_empty(name))
}

fn default_transport(cx: &Cx<'_>, host: &dyn Host) -> bool {
    const VARS: &[&[u8]] = &[
        b"CMAKE_TLS_VERSION",
        b"CMAKE_TLS_VERIFY",
        b"CMAKE_TLS_CAINFO",
        b"CMAKE_NETRC",
        b"CMAKE_NETRC_FILE",
        b"GIT_CONFIG_COUNT",
    ];
    let global = b"GIT_CONFIG_GLOBAL";
    let windows = host.system().sysname == "Windows";
    !cx.var_defined(global)
        && host.env_get(global).is_none_or(|path| {
            if windows {
                path.eq_ignore_ascii_case(b"NUL")
            } else {
                path == b"/dev/null"
                    && host
                        .metadata(Path::new("/dev/null"), Follow::No)
                        .is_ok_and(|metadata| metadata.kind == FileKind::Other)
            }
        })
        && VARS
            .iter()
            .all(|name| !cx.var_defined(name) && host.env_get(name).is_none())
}
