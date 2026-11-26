// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use rcmake_base::BString;
use rcmake_eval::{Cx, Interp, journal::ReplayReceipts};
use rcmake_host::{
    FileKind, Follow, Host, HostResult, Input, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, Stream, path,
};
use rcmake_json::{ReadOptions, Value, parse, write_compact};
use rcmake_store::{Digest, Domain, InputStamp, KeyHasher, TraceMemo, file_digest};

use super::try_compile::plan::{environment_fingerprint, process_environment};

mod prefetch;
mod receipts;

/// Shortest process bound that can reuse a completed sample. The try-compile
/// child configure bounds every process at 120 seconds. A system query that
/// completed without a bound gives the same result under a long bound.
const MIN_REPLAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PERSISTED_MAX_BYTES: u64 = 256 * 1024;
const PERSISTED_MAX_ENTRIES: usize = 64;
const PERSISTED_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PLIST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Query {
    program: PathBuf,
    args: Vec<BString>,
    env: Digest,
    stderr: u8,
    missing_programs: Vec<PathBuf>,
}

impl Query {
    /// Return `true` if no `PATH` directory holds the program.
    ///
    /// The program cannot start, so the query has a fixed result and runs no
    /// process. A `Missing` stamp for each candidate binds that result.
    fn is_absent(&self) -> bool {
        self.program.as_os_str().is_empty()
    }
}

/// Return what starting a program that no `PATH` directory holds gives.
///
/// This is the result of the native host for a spawn that fails with `ENOENT`.
fn absent_output(request: &RunRequest) -> RunOutput {
    let program = request
        .pipeline
        .first()
        .map(|command| path::to_native(&command.program));
    // `ENOENT` is 2 on every platform that admits a query.
    let error = rcmake_host::HostError::from_io(std::io::Error::from_raw_os_error(2), program);
    RunOutput {
        results: vec![ProcessResult::SpawnFailed(error)],
        ..RunOutput::default()
    }
}

fn absent_observation(request: &RunRequest) -> Observation {
    Observation {
        executable: Digest::hash(b"absent program"),
        result: absent_output(request),
        request: request.clone(),
        state_digest: None,
    }
}

#[derive(Clone)]
struct Observation {
    executable: Digest,
    result: RunOutput,
    /// The sampled request. A probe samples it again in a new environment.
    request: RunRequest,
    /// Live platform state captured around the query observation itself.
    state_digest: Option<Digest>,
}

struct ObservedLaunch {
    output: HostResult<RunOutput>,
    program_digest: Option<Digest>,
    state_digest: Option<Digest>,
}

#[derive(Clone)]
struct PersistedEntry {
    program: PathBuf,
    args: Vec<BString>,
    env: Digest,
    stderr: u8,
    missing_programs: Vec<PathBuf>,
    executable: Digest,
    state: Digest,
    stdout: Vec<u8>,
    stderr_bytes: Vec<u8>,
}

#[derive(Clone, Default)]
pub(super) struct RecordedQueries(BTreeMap<Query, Observation>);

#[derive(Default)]
pub(super) struct PlatformQueries(
    Mutex<RecordedQueries>,
    prefetch::Prefetched,
    Option<Vec<PersistedEntry>>,
    /// The session memo for program digests. Try-compile children share it.
    Option<std::sync::Arc<TraceMemo>>,
);

/// Hash a query program. Use the session memo if one is attached.
///
/// The memo reuses a digest only for a file that no process of this user can
/// change, and only while its fresh identity and metadata agree. In
/// verification mode, the memo calculates each digest again.
fn program_digest(memo: Option<&TraceMemo>, host: &dyn Host, program: &Path) -> HostResult<Digest> {
    match memo {
        Some(memo) => memo.file_digest(host, program),
        None => file_digest(host, program),
    }
}

impl PlatformQueries {
    /// Make an empty set of queries that hashes programs through `memo`.
    pub(in crate::configure) fn with_memo(memo: Option<std::sync::Arc<TraceMemo>>) -> Self {
        Self(
            Mutex::default(),
            prefetch::Prefetched::default(),
            None,
            memo,
        )
    }

    fn memo(&self) -> Option<&TraceMemo> {
        self.3.as_deref()
    }

    /// Sample each recorded query again in the environment of a try-compile
    /// child.
    ///
    /// Compiler identification can change the environment after the platform
    /// modules run. For example, it restores `LC_ALL` as an empty value, and
    /// policy `CMP0132` OLD exports `CC`. The child sees that environment.
    pub(super) fn refresh(&self, host: &dyn Host, env: &[(BString, BString)]) {
        let digests = self.memo();
        let Ok(mut memo) = self.0.lock() else {
            return;
        };
        let digest = environment_fingerprint(env);
        let pending: Vec<_> = memo
            .0
            .iter()
            .filter(|(query, _)| query.env != digest)
            .map(|(_, observation)| RunRequest {
                env: env.to_vec(),
                ..observation.request.clone()
            })
            .collect();
        let is_xcrun = |request: &RunRequest| {
            request
                .pipeline
                .first()
                .is_some_and(|command| command.program.ends_with(b"xcrun"))
        };
        // `xcrun` needs the developer directory sample for the same environment,
        // so it runs after the other queries are recorded.
        for xcrun in [false, true] {
            let mut ready: Vec<(Query, RunRequest)> = Vec::new();
            for request in pending.iter().filter(|request| is_xcrun(request) == xcrun) {
                let Some(key) = query(host, request) else {
                    continue;
                };
                if key.is_absent() {
                    if memo.0.len() < 64 {
                        memo.0
                            .entry(key)
                            .or_insert_with(|| absent_observation(request));
                    }
                    continue;
                }
                if memo.0.contains_key(&key)
                    || ready.iter().any(|(started, _)| *started == key)
                    || !admits_xcrun(host, &memo, &key, request)
                {
                    continue;
                }
                if program_digest(digests, host, &key.program).is_ok() {
                    ready.push((key, request.clone()));
                }
            }
            // The queries in one group are independent, so they run at the same time.
            let launches = crate::configure::concurrent::run_all(
                ready
                    .iter()
                    .map(
                        |(key, request)| -> crate::configure::concurrent::Job<'_, ObservedLaunch> {
                            Box::new(move || launch_observed(digests, host, key, request))
                        },
                    )
                    .collect(),
            );
            for ((key, request), launched) in ready.into_iter().zip(launches) {
                if let Some(launched) = launched {
                    let _ = record(digests, host, &mut memo, key, &request, launched, |_| {});
                }
            }
        }
    }

    /// Copy the samples that a try-compile child in this environment can use.
    pub(super) fn snapshot(
        &self,
        host: &dyn Host,
        env: &[(BString, BString)],
    ) -> Option<RecordedQueries> {
        let digest = environment_fingerprint(env);
        let mut snapshot = self.0.lock().ok()?.clone();
        snapshot.0.retain(|query, _| {
            query.env == digest
                && query
                    .missing_programs
                    .iter()
                    .all(|file| is_missing(host, file))
        });
        Some(snapshot)
    }

    /// Serialize verified platform query observations for read-only reuse by
    /// later no-process queries such as `rcmake explain`.
    pub(in crate::configure) fn render_persisted(&self) -> Option<BString> {
        if self.2.is_some() {
            return None;
        }
        let memo = self.0.lock().ok()?;
        if memo.0.is_empty() {
            return None;
        }
        let max_total = usize::try_from(PERSISTED_MAX_BYTES).ok()?;
        // Wrapper overhead for `{"entries":[...],"version":1}`.
        let mut total_bytes = 32usize;
        let mut entries = Vec::new();
        for (query, observation) in &memo.0 {
            if entries.len() >= PERSISTED_MAX_ENTRIES {
                break;
            }
            let [ProcessResult::Exited(0)] = observation.result.results.as_slice() else {
                continue;
            };
            if observation
                .result
                .stdout
                .len()
                .saturating_add(observation.result.stderr.len())
                > PERSISTED_MAX_OUTPUT_BYTES
            {
                continue;
            }
            let Some(program) = utf8_path(&query.program) else {
                continue;
            };
            let Some(args) = query
                .args
                .iter()
                .map(|arg| std::str::from_utf8(arg).ok().map(BString::from))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let Some(missing) = query
                .missing_programs
                .iter()
                .map(|missing| utf8_path(missing))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let Some(state) = observation.state_digest else {
                continue;
            };
            let entry = Value::Object(BTreeMap::from([
                ("program".into(), Value::String(program)),
                (
                    "args".into(),
                    Value::Array(args.into_iter().map(Value::String).collect()),
                ),
                (
                    "env".into(),
                    Value::String(
                        persisted_env_fingerprint(&observation.request.env)
                            .to_hex()
                            .into(),
                    ),
                ),
                ("stderr".into(), Value::UInt(u64::from(query.stderr))),
                (
                    "missing".into(),
                    Value::Array(missing.into_iter().map(Value::String).collect()),
                ),
                (
                    "executable".into(),
                    Value::String(observation.executable.to_hex().into()),
                ),
                ("state".into(), Value::String(state.to_hex().into())),
                ("exit_code".into(), Value::Int(0)),
                (
                    "stdout".into(),
                    Value::String(encode_hex(&observation.result.stdout).into()),
                ),
                (
                    "stderr_bytes".into(),
                    Value::String(encode_hex(&observation.result.stderr).into()),
                ),
            ]));
            let encoded_len = write_compact(&entry).len().saturating_add(1);
            if total_bytes.saturating_add(encoded_len) > max_total {
                break;
            }
            total_bytes = total_bytes.saturating_add(encoded_len);
            entries.push(entry);
        }
        if entries.is_empty() {
            return None;
        }
        let encoded = write_compact(&Value::Object(BTreeMap::from([
            ("version".into(), Value::UInt(1)),
            ("entries".into(), Value::Array(entries)),
        ])));
        (encoded.len() <= max_total).then_some(encoded)
    }

    /// Load previously persisted Darwin platform queries for strictly verified
    /// read-only reuse without spawning subprocesses.
    pub(in crate::configure) fn load_read_only(
        host: &dyn Host,
        build_root: &Path,
        file: &Path,
    ) -> Option<Self> {
        let verified = verified_cache_path(host, build_root, file, false)?;
        let meta = host.metadata(&verified, Follow::No).ok()?;
        if meta.kind != FileKind::File || meta.len > PERSISTED_MAX_BYTES {
            return None;
        }
        let bytes = host
            .read(
                &verified,
                ReadRange::All {
                    limit: PERSISTED_MAX_BYTES,
                },
            )
            .ok()?;
        let doc = parse(&bytes, &ReadOptions::default()).ok()?;
        if !matches!(doc.get("version"), Some(Value::UInt(1) | Value::Int(1))) {
            return None;
        }
        let raw_entries = doc.get("entries")?.as_array()?;
        let mut entries = Vec::new();
        for item in raw_entries.iter().take(PERSISTED_MAX_ENTRIES) {
            if let Some(entry) = parse_persisted_entry(item) {
                entries.push(entry);
            }
        }
        (!entries.is_empty()).then(|| {
            Self(
                Mutex::default(),
                prefetch::Prefetched::default(),
                Some(entries),
                None,
            )
        })
    }
}

/// Resolve `file` inside `build_root` without following any symlink at an
/// intermediate directory component or the final file component.
///
/// When `create_dirs` is true, missing directory components inside `build_root`
/// are created step by step only after verifying every existing prefix is a real
/// directory inside the canonical build root.
pub(in crate::configure) fn verified_cache_path(
    host: &dyn Host,
    build_root: &Path,
    file: &Path,
    create_dirs: bool,
) -> Option<PathBuf> {
    let canon_build = host.canonicalize(build_root).ok()?;
    if !host
        .metadata(&canon_build, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return None;
    }
    let rel = file
        .strip_prefix(build_root)
        .or_else(|_| file.strip_prefix(&canon_build))
        .ok()?;
    let file_name = rel.file_name()?;
    let rel_parent = rel.parent()?;
    let mut current = canon_build.clone();
    for component in rel_parent.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        current.push(part);
        match host.metadata(&current, Follow::No) {
            Ok(meta) if meta.kind == FileKind::Dir => {}
            Ok(_) => return None,
            Err(err) if err.kind == ErrorKind::NotFound && create_dirs => {
                host.create_dir_all(&current).ok()?;
                if !host
                    .metadata(&current, Follow::No)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir)
                {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let canon_parent = host.canonicalize(&current).ok()?;
    if !canon_parent.starts_with(&canon_build) {
        return None;
    }
    let target = canon_parent.join(file_name);
    match host.metadata(&target, Follow::No) {
        Ok(meta) if meta.kind == FileKind::File => Some(target),
        Err(err) if err.kind == ErrorKind::NotFound && create_dirs => Some(target),
        _ => None,
    }
}

/// Destination of the inherited standard error of a query in a try-compile child.
///
/// A child process of a probe writes its inherited standard error into the probe output,
/// not into the diagnostics of the parent configure.
pub(in crate::configure) struct ChildStderr(Box<StderrSink>);

type StderrSink = dyn Fn(&[u8]) + Send + Sync;

impl ChildStderr {
    pub(in crate::configure) fn new(sink: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        Self(Box::new(sink))
    }
}

/// Write the inherited standard error of a launched query: into the child sink in a
/// try-compile child, else to the standard error of the host.
fn write_inherited_stderr(cx: &Cx<'_>, bytes: &[u8]) {
    match cx.session_data::<std::sync::Arc<ChildStderr>>() {
        Some(sink) => (sink.0)(bytes),
        None => cx.host().write_out(Stream::Stderr, bytes),
    }
}

/// Share the configure memo with a child that has no process journal.
pub(super) fn inherit(cx: &Cx<'_>, child: &mut Interp) {
    if let Some(queries) = cx.session_data::<std::sync::Arc<PlatformQueries>>()
        && !cx
            .host()
            .env_get(b"RCMAKE_NO_PLATFORM_QUERIES")
            .is_some_and(|value| value == "1")
    {
        child.add_session_data(queries.clone());
    }
}

/// Sample a system query when a bundled Darwin module reaches it.
pub(crate) fn run(cx: &Cx<'_>, request: &RunRequest) -> Option<HostResult<RunOutput>> {
    let memo_queries = cx.session_data::<std::sync::Arc<PlatformQueries>>()?;
    let host = cx.host();
    let key = query(host, request)?;
    let backtrace = cx.backtrace();
    let frame = backtrace.top()?;
    let file = path::to_native(&frame.file);
    let canonical = host.canonicalize(&file).ok()?;
    if ![
        "Modules/CMakeDetermineSystem.cmake",
        "Modules/Platform/Darwin-Initialize.cmake",
        "Modules/Platform/Darwin.cmake",
    ]
    .iter()
    .any(|relative| host.canonicalize(&cx.cmake_root().join(relative)).as_ref() == Ok(&canonical))
    {
        return None;
    }
    if key.is_absent() {
        let observation = absent_observation(request);
        let result = observation.result.clone();
        if let Ok(mut memo) = memo_queries.0.lock()
            && memo.0.len() < 64
        {
            memo.0.entry(key).or_insert(observation);
        }
        return Some(Ok(result));
    }
    let digests = memo_queries.memo();
    let executable = program_digest(digests, host, &key.program).ok()?;
    let mut memo = memo_queries.0.lock().ok()?;
    if !admits_xcrun(host, &memo, &key, request) {
        return None;
    }
    if let Some(observed) = memo.0.get(&key)
        && observed.executable == executable
    {
        return Some(Ok(observed.result.clone()));
    }
    if let Some(persisted) = &memo_queries.2 {
        let env_digest = persisted_env_fingerprint(&request.env);
        let matched = persisted.iter().find(|entry| {
            entry.program == key.program
                && entry.args == key.args
                && entry.env == env_digest
                && entry.stderr == key.stderr
                && entry.missing_programs == key.missing_programs
                && entry.executable == executable
        })?;
        if !key
            .missing_programs
            .iter()
            .all(|file| is_missing(host, file))
        {
            return None;
        }
        let live_state = compute_live_state_digest(
            host,
            &key.program,
            &key.args,
            &request.env,
            &matched.stdout,
        )?;
        if live_state != matched.state {
            return None;
        }
        if let Some((_, verified)) = receipts::verify(host, &key, executable, &canonical, request)
            && verified.stdout != matched.stdout
        {
            return None;
        }
        let result = RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: matched.stdout.clone(),
            stderr: if request.stderr == Output::Inherit {
                Vec::new()
            } else {
                matched.stderr_bytes.clone()
            },
        };
        if memo.0.len() < PERSISTED_MAX_ENTRIES {
            memo.0.insert(
                key,
                Observation {
                    executable,
                    result: result.clone(),
                    request: request.clone(),
                    state_digest: Some(live_state),
                },
            );
        }
        return Some(Ok(result));
    }
    let receipt_state_before =
        compute_input_state_digest(host, &key.program, &key.args, &request.env);
    let receipt = cx
        .session_data::<std::sync::Arc<ReplayReceipts>>()
        .and_then(|receipts| {
            let (digest, result) = receipts::verify(host, &key, executable, &canonical, request)?;
            Some((receipts, digest, result))
        });
    if let Some((receipts, digest, result)) = &receipt
        && receipts.contains(*digest)
    {
        receipts.record(*digest);
        let state_digest = finalize_state_digest(host, &key, request, receipt_state_before, result);
        if memo.0.len() < 64 {
            memo.0.insert(
                key,
                Observation {
                    executable,
                    result: result.clone(),
                    request: request.clone(),
                    state_digest,
                },
            );
        }
        return Some(Ok(result.clone()));
    }
    let launched = take_or_launch(digests, host, &memo_queries.1, &key, executable, request);
    let matches_receipt_executable = launched.program_digest == Some(executable);
    if let Ok(result) = &launched.output
        && request.stderr == Output::Inherit
        && !result.stderr.is_empty()
    {
        write_inherited_stderr(cx, &result.stderr);
    }
    Some(record(
        digests,
        host,
        &mut memo,
        key,
        request,
        launched,
        |result| {
            if matches_receipt_executable
                && let Some((receipts, digest, expected)) = &receipt
                && result == expected
            {
                receipts.record(*digest);
            }
        },
    ))
}

/// Admit `xcrun` only with a standard SDK and developer directory.
fn admits_xcrun(
    host: &dyn Host,
    memo: &RecordedQueries,
    key: &Query,
    request: &RunRequest,
) -> bool {
    key.program != Path::new("/usr/bin/xcrun")
        || (sdk_is_standard(host, request)
            && memo.0.iter().any(|(sample, result)| {
                sample.program == Path::new("/usr/bin/xcode-select")
                    && sample.env == key.env
                    && host
                        .canonicalize(&path::to_native(result.result.stdout.trim_ascii()))
                        .is_ok_and(|developer| {
                            developer == Path::new("/Applications/Xcode.app/Contents/Developer")
                                || developer == Path::new("/Library/Developer/CommandLineTools")
                        })
            }))
}

fn take_or_launch(
    digests: Option<&TraceMemo>,
    host: &dyn Host,
    prefetched: &prefetch::Prefetched,
    key: &Query,
    current_program_digest: Digest,
    request: &RunRequest,
) -> ObservedLaunch {
    prefetched
        .take(key, current_program_digest)
        .unwrap_or_else(|| launch_observed(digests, host, key, request))
}

/// Run an admitted query with captured output and no input, capturing its
/// executable digest and live platform-state digest strictly around the
/// process execution.
fn launch_observed(
    digests: Option<&TraceMemo>,
    host: &dyn Host,
    key: &Query,
    request: &RunRequest,
) -> ObservedLaunch {
    let program_before = program_digest(digests, host, &key.program).ok();
    let before = compute_input_state_digest(host, &key.program, &key.args, &request.env);
    let output = launch(host, key, request);
    let program_after = program_digest(digests, host, &key.program).ok();
    let (program_digest, state_digest) = match (program_before, program_after) {
        (Some(a), Some(b)) if a == b => {
            let state_digest = match &output {
                Ok(result) => finalize_state_digest(host, key, request, before, result)
                    .filter(|_| program_digest(digests, host, &key.program).ok() == Some(a)),
                Err(_) => None,
            };
            (Some(a), state_digest)
        }
        _ => (None, None),
    };
    ObservedLaunch {
        output,
        program_digest,
        state_digest,
    }
}

fn finalize_state_digest(
    host: &dyn Host,
    key: &Query,
    request: &RunRequest,
    before: Option<Digest>,
    result: &RunOutput,
) -> Option<Digest> {
    if result.results.as_slice() != [ProcessResult::Exited(0)] {
        return None;
    }
    let before = before?;
    let after = compute_input_state_digest(host, &key.program, &key.args, &request.env)?;
    if before != after {
        return None;
    }
    let full =
        compute_live_state_digest(host, &key.program, &key.args, &request.env, &result.stdout)?;
    let confirm = compute_input_state_digest(host, &key.program, &key.args, &request.env)?;
    (confirm == before).then_some(full)
}

/// Run an admitted query with captured output and no input.
fn launch(host: &dyn Host, key: &Query, request: &RunRequest) -> HostResult<RunOutput> {
    let mut captured = request.clone();
    if let Some(command) = captured.pipeline.first_mut() {
        command.program = path::from_native(&key.program);
    }
    captured.stdin = Input::Null;
    if captured.stderr == Output::Inherit {
        captured.stderr = Output::Capture;
    }
    host.run(&captured, &mut |_, _| {})
}

/// Record a successful query result with bounded output.
fn record(
    digests: Option<&TraceMemo>,
    host: &dyn Host,
    memo: &mut RecordedQueries,
    key: Query,
    request: &RunRequest,
    launched: ObservedLaunch,
    on_record: impl FnOnce(&RunOutput),
) -> HostResult<RunOutput> {
    let ObservedLaunch {
        output,
        program_digest: launched_digest,
        state_digest,
    } = launched;
    let inherited_error = request.stderr == Output::Inherit;
    if let (Ok(result), Some(executable)) = (&output, launched_digest)
        && result.results.as_slice() == [ProcessResult::Exited(0)]
        && result.stdout.len().saturating_add(result.stderr.len()) <= 64 * 1024
        && (!inherited_error || result.stderr.is_empty())
        && memo.0.len() < 64
        && program_digest(digests, host, &key.program) == Ok(executable)
    {
        on_record(result);
        memo.0.insert(
            key,
            Observation {
                executable,
                result: result.clone(),
                request: request.clone(),
                state_digest,
            },
        );
    }
    output.map(|mut result| {
        if inherited_error {
            result.stderr.clear();
        }
        result
    })
}

impl RecordedQueries {
    pub(super) fn identity(&self) -> Digest {
        let mut key = KeyHasher::new(Domain::ProbeV1);
        key.field(1, b"Darwin platform queries");
        for (query, observation) in &self.0 {
            key.field(2, query.program.as_os_str().as_encoded_bytes());
            key.field(3, observation.executable.as_bytes());
            key.field(4, query.env.as_bytes());
            key.field(5, &[query.stderr]);
            for arg in &query.args {
                key.field(6, arg);
            }
            for result in &observation.result.results {
                if let ProcessResult::Exited(code) = result {
                    key.field(7, &code.to_le_bytes());
                }
            }
            key.field(8, Digest::hash(&observation.result.stdout).as_bytes());
            key.field(9, Digest::hash(&observation.result.stderr).as_bytes());
            for missing in &query.missing_programs {
                key.field(10, missing.as_os_str().as_encoded_bytes());
            }
        }
        key.finish()
    }

    pub(super) fn stamps(&self) -> impl Iterator<Item = InputStamp> + '_ {
        self.0.iter().flat_map(|(query, observation)| {
            (!query.is_absent())
                .then(|| InputStamp::FileHash {
                    path: query.program.clone(),
                    digest: observation.executable,
                })
                .into_iter()
                .chain(
                    query
                        .missing_programs
                        .iter()
                        .map(|path| InputStamp::Missing { path: path.clone() }),
                )
        })
    }

    pub(super) fn get(&self, host: &dyn Host, request: &RunRequest) -> Option<RunOutput> {
        // The try-compile child captures an inherited error stream and
        // forwards it with `tee`. A sample of an inherited error stream has no
        // error output, so the forwarded stream is also empty.
        let forwarded;
        let request = if request.tee && request.stderr == Output::Capture {
            forwarded = RunRequest {
                tee: false,
                stderr: Output::Inherit,
                ..request.clone()
            };
            &forwarded
        } else {
            request
        };
        let key = query(host, request)?;
        self.0
            .get(&key)
            .filter(|observation| key.stderr != 1 || observation.result.stderr.is_empty())
            .map(|observation| observation.result.clone())
    }
}

fn query(host: &dyn Host, request: &RunRequest) -> Option<Query> {
    let [command] = request.pipeline.as_slice() else {
        return None;
    };
    if !cfg!(target_os = "macos")
        || !matches!(request.stdin, Input::Inherit | Input::Null)
        || request.stdout != Output::Capture
        || request.tee
        || request
            .timeout
            .is_some_and(|limit| limit < MIN_REPLAY_TIMEOUT)
        || request.env.iter().any(|(name, value)| {
            !value.is_empty()
                && (name.starts_with(b"DYLD_") || name.starts_with(b"LD_") || name == b"TOOLCHAINS")
        })
    {
        return None;
    }
    let stderr = match request.stderr {
        Output::Inherit => 1,
        Output::Capture => 2,
        Output::Null => 3,
        _ => return None,
    };
    let requested = path::to_native(&command.program);
    let name = requested.file_name()?.to_str()?;
    // Homebrew installs its launcher under a relocatable prefix, so `brew`
    // has no fixed path. The parent run binds its exit status and output.
    let fixed = match (name, command.args.as_slice()) {
        ("xcode-select", [flag]) if flag == "-print-path" => Some("/usr/bin/xcode-select"),
        ("sw_vers", [flag]) if flag == "-productVersion" => Some("/usr/bin/sw_vers"),
        ("sysctl", [flag, field])
            if matches!(flag.as_slice(), b"-q" | b"-n") && field == "hw.optional.arm64" =>
        {
            Some("/usr/sbin/sysctl")
        }
        ("xcrun", [flag])
            if matches!(flag.as_slice(), b"--show-sdk-path" | b"--show-sdk-version") =>
        {
            Some("/usr/bin/xcrun")
        }
        ("xcrun", [flag, tool]) if flag == "--find" && plain_name(tool) => Some("/usr/bin/xcrun"),
        ("xcrun", [sdk, name, flag])
            if matches!(sdk.as_slice(), b"-sdk" | b"--sdk")
                && plain_name(name)
                && matches!(flag.as_slice(), b"--show-sdk-path" | b"--show-sdk-version") =>
        {
            Some("/usr/bin/xcrun")
        }
        ("brew", [flag]) if flag == "--prefix" => None,
        // Apple `uname` reads `UNAME_<field>` and `UNAME_<FIELD>` overrides, also
        // with empty values, so any such variable prevents the query.
        ("uname", [flag])
            if matches!(flag.as_slice(), b"-s" | b"-r" | b"-m")
                && !has_uname_override(&request.env) =>
        {
            Some("/usr/bin/uname")
        }
        _ => return None,
    };
    let fixed = fixed.map(Path::new);
    let mut missing_programs = Vec::new();
    let selected = if requested.is_absolute() {
        requested
    } else {
        if requested != Path::new(name) {
            return None;
        }
        let value = &request.env.iter().find(|(key, _)| key == "PATH")?.1;
        let directories = path::to_native(value);
        let mut selected = None;
        for directory in std::env::split_paths(&directories).take(128) {
            if !directory.is_absolute() {
                return None;
            }
            let candidate = directory.join(name);
            if fixed.is_none_or(|fixed| candidate == fixed) && is_executable(host, &candidate) {
                selected = Some(candidate);
                break;
            }
            if !is_missing(host, &candidate) {
                return None;
            }
            missing_programs.push(candidate);
        }
        match selected {
            Some(selected) => selected,
            // Only `brew` has no fixed path. When every candidate is missing,
            // the spawn fails with a fixed error and runs no process.
            None if fixed.is_none()
                && !missing_programs.is_empty()
                && std::env::split_paths(&directories).count() <= 128 =>
            {
                return Some(Query {
                    program: PathBuf::new(),
                    args: command.args.clone(),
                    env: environment_fingerprint(&request.env),
                    stderr,
                    missing_programs,
                });
            }
            None => return None,
        }
    };
    // Require a system query to select the sealed system executable. A program
    // with the same name earlier in PATH can have different effects.
    if fixed.is_some_and(|fixed| selected != fixed) || !is_executable(host, &selected) {
        return None;
    }
    Some(Query {
        program: selected,
        args: command.args.clone(),
        env: environment_fingerprint(&request.env),
        stderr,
        missing_programs,
    })
}

/// Return `true` if the environment has a variable that can change `uname` output.
pub(super) fn has_uname_override(env: &[(BString, BString)]) -> bool {
    env.iter().any(|(name, _)| {
        name.get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"UNAME_"))
    })
}

fn is_executable(host: &dyn Host, program: &Path) -> bool {
    host.metadata(program, Follow::Yes)
        .is_ok_and(|info| info.kind == FileKind::File && info.mode & 0o111 != 0)
}

fn is_missing(host: &dyn Host, program: &Path) -> bool {
    host.metadata(program, Follow::No)
        .is_err_and(|error| error.kind == ErrorKind::NotFound)
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
}

fn sdk_is_standard(host: &dyn Host, request: &RunRequest) -> bool {
    let Some((_, sdk)) = request.env.iter().find(|(name, _)| name == "SDKROOT") else {
        return true;
    };
    sdk.is_empty()
        || plain_name(sdk)
        || host.canonicalize(&path::to_native(sdk)).is_ok_and(|sdk| {
            sdk.starts_with("/Library/Developer/CommandLineTools/SDKs")
                || sdk.starts_with("/Applications/Xcode.app/Contents/Developer/Platforms")
        })
}

fn persisted_env_fingerprint(environment: &[(BString, BString)]) -> Digest {
    let mut normalized = environment.to_vec();
    process_environment(&mut normalized);
    normalized.retain(|(name, value)| {
        !(name == b"LC_ALL" && value.is_empty())
            && !matches!(
                name.as_slice(),
                b"RCMAKE_DIAGNOSTICS" | b"RCMAKE_CACHE_DIR" | b"NO_COLOR" | b"CLICOLOR_FORCE"
            )
    });
    normalized.sort();
    environment_fingerprint(&normalized)
}

fn utf8_path(path: &Path) -> Option<BString> {
    path.to_str().map(BString::from)
}

fn stamp_file_or_missing(key: &mut KeyHasher, host: &dyn Host, path: &Path) -> Option<()> {
    key.field(20, path.as_os_str().as_encoded_bytes());
    match host.metadata(path, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File && meta.len <= MAX_PLIST_BYTES => {
            let digest = file_digest(host, path).ok()?;
            key.field(21, b"file");
            key.field(22, digest.as_bytes());
            Some(())
        }
        Err(err) if err.kind == ErrorKind::NotFound => {
            key.field(21, b"missing");
            Some(())
        }
        _ => None,
    }
}

fn stamp_link_or_missing(key: &mut KeyHasher, host: &dyn Host, path: &Path) -> Option<()> {
    key.field(30, path.as_os_str().as_encoded_bytes());
    match host.metadata(path, Follow::No) {
        Ok(meta) if meta.kind == FileKind::Symlink => {
            let target = host.read_link(path).ok()?;
            key.field(31, b"symlink");
            key.field(32, target.as_os_str().as_encoded_bytes());
            if let Ok(canonical) = host.canonicalize(path) {
                key.field(33, canonical.as_os_str().as_encoded_bytes());
            }
            Some(())
        }
        Ok(meta) if meta.kind == FileKind::Dir => {
            let canonical = host.canonicalize(path).ok()?;
            key.field(31, b"dir");
            key.field(33, canonical.as_os_str().as_encoded_bytes());
            Some(())
        }
        Err(err) if err.kind == ErrorKind::NotFound => {
            key.field(31, b"missing");
            Some(())
        }
        _ => None,
    }
}

fn stamp_dir_and_plists(key: &mut KeyHasher, host: &dyn Host, dir: &Path) -> Option<()> {
    stamp_link_or_missing(key, host, dir)?;
    if host
        .metadata(dir, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        stamp_file_or_missing(key, host, &dir.join("version.plist"))?;
        if let Some(parent) = dir.parent() {
            stamp_file_or_missing(key, host, &parent.join("version.plist"))?;
            stamp_file_or_missing(key, host, &parent.join("Info.plist"))?;
        }
    }
    Some(())
}

fn stamp_xcode_selection(key: &mut KeyHasher, host: &dyn Host) -> Option<()> {
    for link in [
        Path::new("/var/select/developer_dir"),
        Path::new("/var/db/xcode_select_link"),
        Path::new("/usr/share/xcode-select/xcode_dir_link"),
    ] {
        stamp_link_or_missing(key, host, link)?;
        if let Ok(target) = host.canonicalize(link)
            && host
                .metadata(&target, Follow::No)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            stamp_dir_and_plists(key, host, &target)?;
        }
    }
    stamp_dir_and_plists(
        key,
        host,
        Path::new("/Applications/Xcode.app/Contents/Developer"),
    )?;
    stamp_dir_and_plists(key, host, Path::new("/Library/Developer/CommandLineTools"))?;
    Some(())
}

fn stamp_sdk(key: &mut KeyHasher, host: &dyn Host, sdk: &Path) -> Option<()> {
    stamp_link_or_missing(key, host, sdk)?;
    if host
        .metadata(sdk, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        stamp_file_or_missing(key, host, &sdk.join("SDKSettings.plist"))?;
    }
    Some(())
}

fn stamp_required_sdk(key: &mut KeyHasher, host: &dyn Host, sdk: &Path) -> Option<()> {
    if !sdk.is_absolute()
        || !host
            .metadata(sdk, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return None;
    }
    let canon = host.canonicalize(sdk).ok()?;
    stamp_link_or_missing(key, host, sdk)?;
    key.field(39, canon.as_os_str().as_encoded_bytes());
    let plist = canon.join("SDKSettings.plist");
    let meta = host.metadata(&plist, Follow::Yes).ok()?;
    if meta.kind != FileKind::File || meta.len > MAX_PLIST_BYTES {
        return None;
    }
    let digest = file_digest(host, &plist).ok()?;
    key.field(21, b"file");
    key.field(22, digest.as_bytes());
    Some(())
}

fn validate_explicit_sdk(key: &mut KeyHasher, host: &dyn Host, sdk: &[u8]) -> Option<()> {
    if sdk == b"macosx" {
        key.field(38, b"macosx");
        return Some(());
    }
    let dir = path::to_native(sdk);
    if !dir.is_absolute() {
        return None;
    }
    stamp_required_sdk(key, host, &dir)
}

fn stamp_input_state(
    key: &mut KeyHasher,
    host: &dyn Host,
    program: &Path,
    args: &[BString],
    env: &[(BString, BString)],
) -> Option<()> {
    let system = host.system();
    key.field(0, b"Darwin persisted query live state v2");
    key.field(1, &system.sysname);
    key.field(2, &system.machine);
    key.field(3, &system.release);
    key.field(4, &system.version);

    let system_version_plist = Path::new("/System/Library/CoreServices/SystemVersion.plist");
    if program == Path::new("/usr/sbin/sysctl") {
        stamp_file_or_missing(key, host, system_version_plist)?;
        return Some(());
    }
    if program == Path::new("/usr/bin/uname") {
        // The kernel fields above are the complete input of an admitted query.
        return Some(());
    }
    if program == Path::new("/usr/bin/sw_vers") {
        let meta = host.metadata(system_version_plist, Follow::Yes).ok()?;
        if meta.kind != FileKind::File || meta.len > MAX_PLIST_BYTES {
            return None;
        }
        let digest = file_digest(host, system_version_plist).ok()?;
        key.field(10, digest.as_bytes());
        return Some(());
    }
    if program == Path::new("/usr/bin/xcode-select") {
        stamp_file_or_missing(key, host, system_version_plist)?;
        stamp_xcode_selection(key, host)?;
        if let Some((_, dev_dir)) = env.iter().find(|(name, _)| name == "DEVELOPER_DIR")
            && !dev_dir.is_empty()
        {
            let dir = path::to_native(dev_dir);
            if !dir.is_absolute() {
                return None;
            }
            stamp_dir_and_plists(key, host, &dir)?;
        }
        return Some(());
    }
    if program == Path::new("/usr/bin/xcrun") {
        stamp_file_or_missing(key, host, system_version_plist)?;
        stamp_xcode_selection(key, host)?;
        if let Some((_, dev_dir)) = env.iter().find(|(name, _)| name == "DEVELOPER_DIR")
            && !dev_dir.is_empty()
        {
            let dir = path::to_native(dev_dir);
            if !dir.is_absolute() {
                return None;
            }
            stamp_dir_and_plists(key, host, &dir)?;
        }
        if let Some((_, sdk_root)) = env.iter().find(|(name, _)| name == "SDKROOT")
            && !sdk_root.is_empty()
        {
            validate_explicit_sdk(key, host, sdk_root)?;
        }
        let mut idx = 0usize;
        while let Some(arg) = args.get(idx) {
            let sdk_arg = if matches!(arg.as_slice(), b"-sdk" | b"--sdk") {
                let next = args.get(idx.checked_add(1)?)?;
                idx = idx.saturating_add(2);
                Some(next.as_slice())
            } else if let Some(rest) = arg
                .strip_prefix(b"--sdk=")
                .or_else(|| arg.strip_prefix(b"-sdk="))
            {
                idx = idx.saturating_add(1);
                Some(rest)
            } else {
                idx = idx.saturating_add(1);
                None
            };
            if let Some(sdk) = sdk_arg {
                validate_explicit_sdk(key, host, sdk)?;
            }
        }
        stamp_sdk(
            key,
            host,
            Path::new("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk"),
        )?;
        stamp_sdk(
            key,
            host,
            Path::new(
                "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
            ),
        )?;
        return Some(());
    }
    if program.file_name() == Some(OsStr::new("brew"))
        && matches!(args, [flag] if flag == "--prefix")
    {
        let canon_prog = host.canonicalize(program).ok()?;
        key.field(40, canon_prog.as_os_str().as_encoded_bytes());
        return Some(());
    }
    None
}

fn compute_input_state_digest(
    host: &dyn Host,
    program: &Path,
    args: &[BString],
    env: &[(BString, BString)],
) -> Option<Digest> {
    let mut key = KeyHasher::new(Domain::ConfigureV1);
    stamp_input_state(&mut key, host, program, args, env)?;
    Some(key.finish())
}

fn compute_live_state_digest(
    host: &dyn Host,
    program: &Path,
    args: &[BString],
    env: &[(BString, BString)],
    stdout: &[u8],
) -> Option<Digest> {
    let mut key = KeyHasher::new(Domain::ConfigureV1);
    stamp_input_state(&mut key, host, program, args, env)?;
    if program == Path::new("/usr/bin/xcode-select") {
        let selected = path::to_native(stdout.trim_ascii());
        if !selected.is_absolute()
            || !host
                .metadata(&selected, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            return None;
        }
        stamp_dir_and_plists(&mut key, host, &selected)?;
    } else if program == Path::new("/usr/bin/xcrun") {
        if args.iter().any(|arg| arg == "--show-sdk-path") {
            let sdk = path::to_native(stdout.trim_ascii());
            if !sdk.is_absolute()
                || !host
                    .metadata(&sdk, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir)
            {
                return None;
            }
            stamp_sdk(&mut key, host, &sdk)?;
        }
        if args.iter().any(|arg| arg == "--find") {
            let tool = path::to_native(stdout.trim_ascii());
            if !tool.is_absolute() || !is_executable(host, &tool) {
                return None;
            }
            let canon_tool = host.canonicalize(&tool).ok()?;
            let meta = host.metadata(&canon_tool, Follow::No).ok()?;
            if meta.kind != FileKind::File {
                return None;
            }
            key.field(34, canon_tool.as_os_str().as_encoded_bytes());
            key.field(35, &meta.len.to_le_bytes());
            key.field(36, &meta.mtime.secs.to_le_bytes());
            key.field(37, &meta.mtime.nanos.to_le_bytes());
        }
    } else if program.file_name() == Some(OsStr::new("brew"))
        && matches!(args, [flag] if flag == "--prefix")
    {
        let prefix = path::to_native(stdout.trim_ascii());
        if !prefix.is_absolute()
            || !host
                .metadata(&prefix, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
        {
            return None;
        }
        let canon_prefix = host.canonicalize(&prefix).ok()?;
        key.field(41, canon_prefix.as_os_str().as_encoded_bytes());
    }
    Some(key.finish())
}

fn parse_persisted_entry(value: &Value) -> Option<PersistedEntry> {
    let program = PathBuf::from(value.get("program")?.as_str()?);
    if !program.is_absolute() {
        return None;
    }
    let args = value
        .get("args")?
        .as_array()?
        .iter()
        .map(|arg| arg.as_str().map(BString::from))
        .collect::<Option<Vec<_>>>()?;
    let env = Digest::from_hex(value.get("env")?.as_str()?)?;
    let stderr = match value.get("stderr")? {
        Value::UInt(v) => u8::try_from(*v).ok().filter(|v| (1..=3).contains(v))?,
        Value::Int(v) => u8::try_from(*v).ok().filter(|v| (1..=3).contains(v))?,
        _ => return None,
    };
    let missing_programs = value
        .get("missing")?
        .as_array()?
        .iter()
        .map(|item| {
            let path = PathBuf::from(item.as_str()?);
            path.is_absolute().then_some(path)
        })
        .collect::<Option<Vec<_>>>()?;
    let executable = Digest::from_hex(value.get("executable")?.as_str()?)?;
    let state = Digest::from_hex(value.get("state")?.as_str()?)?;
    match value.get("exit_code")? {
        Value::Int(0) | Value::UInt(0) => {}
        _ => return None,
    }
    let stdout = decode_hex(value.get("stdout")?.as_str()?)?;
    let stderr_bytes = decode_hex(value.get("stderr_bytes")?.as_str()?)?;
    if stdout.len().saturating_add(stderr_bytes.len()) > PERSISTED_MAX_OUTPUT_BYTES {
        return None;
    }
    Some(PersistedEntry {
        program,
        args,
        env,
        stderr,
        missing_programs,
        executable,
        state,
        stdout,
        stderr_bytes,
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(pairs.len());
    for &[high, low] in pairs {
        out.push((hex_nibble(high)? << 4) | hex_nibble(low)?);
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use rcmake_host::{CommandSpec, PureHost};

    use super::*;

    #[test]
    fn rejects_mutating_arguments_and_path_shadows() -> HostResult<()> {
        let host = PureHost::default();
        let shadow = Path::new("/work/bin/sysctl");
        host.seed_file(shadow, b"other program")?;
        host.set_permissions(shadow, 0o755)?;
        let fixed = CommandSpec {
            program: "/usr/sbin/sysctl".into(),
            args: vec!["-q".into(), "hw.optional.arm64".into()],
        };
        let system = Path::new("/usr/sbin/sysctl");
        host.seed_file(system, b"system program")?;
        host.set_permissions(system, 0o755)?;
        let request = |command| RunRequest {
            pipeline: vec![command],
            cwd: PathBuf::from("/work"),
            env: vec![("PATH".into(), "/work/bin:/usr/sbin".into())],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };
        assert!(query(&host, &request(fixed.clone())).is_some());
        let mut bounded = request(fixed.clone());
        bounded.timeout = Some(std::time::Duration::from_secs(120));
        assert!(query(&host, &bounded).is_some());
        bounded.timeout = Some(std::time::Duration::from_secs(1));
        assert!(query(&host, &bounded).is_none());
        assert!(
            query(
                &host,
                &request(CommandSpec {
                    program: "sysctl".into(),
                    ..fixed.clone()
                })
            )
            .is_none()
        );
        let mut bare = request(CommandSpec {
            program: "sysctl".into(),
            ..fixed.clone()
        });
        bare.env = vec![("PATH".into(), "/work/missing:/usr/sbin".into())];
        let observed = query(&host, &bare)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the system program."))?;
        let missing = PathBuf::from("/work/missing/sysctl");
        assert_eq!(
            observed.missing_programs.as_slice(),
            std::slice::from_ref(&missing)
        );
        let unused = vec!["/work/unused"; 150].join(":");
        bare.env = vec![(
            "PATH".into(),
            format!("/work/missing:/usr/sbin:{unused}").into(),
        )];
        let observed = query(&host, &bare)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the system program."))?;
        assert_eq!(
            observed.missing_programs.as_slice(),
            std::slice::from_ref(&missing)
        );
        host.seed_file(&missing, b"shadow")?;
        assert!(query(&host, &bare).is_none());
        bare.env = vec![("PATH".into(), format!("{unused}:/usr/sbin").into())];
        assert!(query(&host, &bare).is_none());
        assert!(
            query(
                &host,
                &request(CommandSpec {
                    args: vec!["-w".into(), "hw.optional.arm64=0".into()],
                    ..fixed
                })
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn homebrew_prefix_binds_the_selected_launcher_and_earlier_candidates() -> HostResult<()> {
        let host = PureHost::default();
        let launcher = Path::new("/opt/homebrew/bin/brew");
        host.seed_file(launcher, b"launcher")?;
        host.set_permissions(launcher, 0o755)?;
        let request = |program: &str, args: Vec<BString>, search: &str| RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: PathBuf::from("/work"),
            env: vec![("PATH".into(), search.into())],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Inherit,
            timeout: None,
            tee: false,
        };
        let prefix = || vec![BString::from("--prefix")];
        let search = "/work/missing:/opt/homebrew/bin:/usr/bin";
        let observed = query(&host, &request("brew", prefix(), search))
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the launcher."))?;
        assert_eq!(observed.program, launcher);
        assert_eq!(
            observed.missing_programs,
            vec![PathBuf::from("/work/missing/brew")]
        );
        for args in [
            vec![BString::from("--prefix"), BString::from("openssl")],
            vec![BString::from("--repository")],
            vec![BString::from("update")],
        ] {
            assert!(query(&host, &request("brew", args, search)).is_none());
        }
        assert!(query(&host, &request("bin/brew", prefix(), search)).is_none());
        // No candidate exists, so the query runs no process and binds each candidate.
        let request_absent = request("brew", prefix(), "/work/missing:/usr/bin");
        let absent = query(&host, &request_absent)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected an absent query."))?;
        assert!(absent.is_absent());
        assert_eq!(
            absent.missing_programs,
            vec![
                PathBuf::from("/work/missing/brew"),
                PathBuf::from("/usr/bin/brew")
            ]
        );
        let mut recorded = RecordedQueries::default();
        recorded
            .0
            .insert(absent.clone(), absent_observation(&request_absent));
        let stamps: Vec<_> = recorded.stamps().collect();
        assert_eq!(
            stamps,
            absent
                .missing_programs
                .iter()
                .map(|path| InputStamp::Missing { path: path.clone() })
                .collect::<Vec<_>>()
        );
        let replayed = recorded
            .get(&host, &request_absent)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected a replay."))?;
        let [ProcessResult::SpawnFailed(error)] = replayed.results.as_slice() else {
            return Err(rcmake_host::HostError::unsupported(
                "Expected a spawn failure.",
            ));
        };
        assert_eq!(error.kind, ErrorKind::NotFound);
        assert_eq!(error.errno, Some(2));
        assert!(replayed.stdout.is_empty() && replayed.stderr.is_empty());
        // A search path that the query cannot check fully gives no query.
        let long = vec!["/work/unused"; 150].join(":");
        assert!(query(&host, &request("brew", prefix(), &long)).is_none());
        assert!(query(&host, &request("brew", prefix(), "relative:/usr/bin")).is_none());
        assert!(query(&host, &request("brew", prefix(), "")).is_none());
        let shadow = Path::new("/work/missing/brew");
        host.seed_file(shadow, b"shadow")?;
        assert!(query(&host, &request("brew", prefix(), search)).is_none());
        // A file that is not executable is not missing, so the absent query stops.
        assert!(query(&host, &request_absent).is_none());
        host.set_permissions(shadow, 0o755)?;
        let shadowed = query(&host, &request("brew", prefix(), search))
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the shadow."))?;
        assert_eq!(shadowed.program, shadow);
        assert_ne!(shadowed.program, observed.program);
        Ok(())
    }

    #[test]
    fn persisted_queries_bind_live_state_and_enforce_containment_and_limits() -> HostResult<()> {
        let host = PureHost::default();
        let build = Path::new("/work/build");
        host.create_dir_all(&build.join("CMakeFiles/rcmake"))?;
        let plist = Path::new("/System/Library/CoreServices/SystemVersion.plist");
        host.seed_file(plist, b"<key>ProductVersion</key><string>27.0</string>")?;
        let sw_vers = Path::new("/usr/bin/sw_vers");
        host.seed_file(sw_vers, b"sw_vers binary")?;
        host.set_permissions(sw_vers, 0o755)?;
        let xcode_select = Path::new("/usr/bin/xcode-select");
        host.seed_file(xcode_select, b"xcode-select binary")?;
        host.set_permissions(xcode_select, 0o755)?;
        let xcode_dev = Path::new("/Applications/Xcode.app/Contents/Developer");
        host.create_dir_all(xcode_dev)?;
        let xcode_plist = Path::new("/Applications/Xcode.app/Contents/version.plist");
        host.seed_file(xcode_plist, b"xcode-v1")?;

        let sw_req = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/usr/bin/sw_vers".into(),
                args: vec!["-productVersion".into()],
            }],
            cwd: PathBuf::from("/work"),
            env: vec![
                ("PATH".into(), "/usr/bin".into()),
                ("LC_ALL".into(), "".into()),
                ("RCMAKE_DIAGNOSTICS".into(), "rich".into()),
            ],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Inherit,
            timeout: None,
            tee: false,
        };
        let xs_req = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/usr/bin/xcode-select".into(),
                args: vec!["-print-path".into()],
            }],
            cwd: PathBuf::from("/work"),
            env: vec![("PATH".into(), "/usr/bin".into())],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };
        let sw_key = query(&host, &sw_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected sw_vers query."))?;
        let xs_key = query(&host, &xs_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected xcode-select query."))?;
        let sw_exec = file_digest(&host, &sw_key.program)
            .map_err(|err| rcmake_host::HostError::unsupported(err.to_string()))?;
        let xs_exec = file_digest(&host, &xs_key.program)
            .map_err(|err| rcmake_host::HostError::unsupported(err.to_string()))?;

        let sw_state =
            compute_live_state_digest(&host, &sw_key.program, &sw_key.args, &sw_req.env, b"27.0\n");
        let xs_state = compute_live_state_digest(
            &host,
            &xs_key.program,
            &xs_key.args,
            &xs_req.env,
            b"/Applications/Xcode.app/Contents/Developer\n",
        );
        let queries = PlatformQueries::default();
        if let Ok(mut memo) = queries.0.lock() {
            memo.0.insert(
                sw_key.clone(),
                Observation {
                    executable: sw_exec,
                    result: RunOutput {
                        results: vec![ProcessResult::Exited(0)],
                        stdout: b"27.0\n".to_vec(),
                        stderr: Vec::new(),
                    },
                    request: sw_req,
                    state_digest: sw_state,
                },
            );
            memo.0.insert(
                xs_key.clone(),
                Observation {
                    executable: xs_exec,
                    result: RunOutput {
                        results: vec![ProcessResult::Exited(0)],
                        stdout: b"/Applications/Xcode.app/Contents/Developer\n".to_vec(),
                        stderr: Vec::new(),
                    },
                    request: xs_req,
                    state_digest: xs_state,
                },
            );
        }
        let persisted = queries
            .render_persisted()
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected persisted JSON."))?;
        assert!(persisted.len() <= usize::try_from(PERSISTED_MAX_BYTES).unwrap_or(usize::MAX));
        let cache_file = build.join("CMakeFiles/rcmake/platform-queries.v1.json");
        host.seed_file(&cache_file, &persisted)?;
        let loaded = PlatformQueries::load_read_only(&host, build, &cache_file)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected loaded cache."))?;
        let entries = loaded
            .2
            .as_ref()
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected read-only entries."))?;
        assert_eq!(entries.len(), 2);

        // 1. Env normalization ignores empty LC_ALL and RCMAKE_DIAGNOSTICS, keeps non-empty LC_ALL.
        let sw_entry = entries
            .iter()
            .find(|entry| entry.program == sw_vers)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Missing sw_vers entry."))?;
        assert_eq!(
            sw_entry.env,
            persisted_env_fingerprint(&[("PATH".into(), "/usr/bin".into())])
        );
        assert_ne!(
            sw_entry.env,
            persisted_env_fingerprint(&[
                ("PATH".into(), "/usr/bin".into()),
                ("LC_ALL".into(), "C".into()),
            ])
        );

        // 2. Live state matches initially, and invalidates when SystemVersion.plist or Xcode changes.
        let xs_entry = entries
            .iter()
            .find(|entry| entry.program == xcode_select)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Missing xcode-select entry."))?;
        let base_env = vec![("PATH".into(), "/usr/bin".into())];
        assert_eq!(
            compute_live_state_digest(
                &host,
                &sw_entry.program,
                &sw_entry.args,
                &base_env,
                &sw_entry.stdout
            ),
            Some(sw_entry.state)
        );
        assert_eq!(
            compute_live_state_digest(
                &host,
                &xs_entry.program,
                &xs_entry.args,
                &base_env,
                &xs_entry.stdout
            ),
            Some(xs_entry.state)
        );
        host.seed_file(plist, b"<key>ProductVersion</key><string>27.1</string>")?;
        assert_ne!(
            compute_live_state_digest(
                &host,
                &sw_entry.program,
                &sw_entry.args,
                &base_env,
                &sw_entry.stdout
            ),
            Some(sw_entry.state)
        );
        host.seed_file(plist, b"<key>ProductVersion</key><string>27.0</string>")?;
        host.seed_file(xcode_plist, b"xcode-v2")?;
        assert_ne!(
            compute_live_state_digest(
                &host,
                &xs_entry.program,
                &xs_entry.args,
                &base_env,
                &xs_entry.stdout
            ),
            Some(xs_entry.state)
        );

        // 3. Directory symlink escaping build root is refused for both reader and publisher.
        let escaped_build = Path::new("/work/escaped-build");
        let outside = Path::new("/outside/cache");
        host.create_dir_all(escaped_build)?;
        host.create_dir_all(&outside.join("rcmake"))?;
        host.seed_file(&outside.join("rcmake/platform-queries.v1.json"), &persisted)?;
        host.symlink(outside, &escaped_build.join("CMakeFiles"))?;
        let escaped_file = escaped_build.join("CMakeFiles/rcmake/platform-queries.v1.json");
        assert!(PlatformQueries::load_read_only(&host, escaped_build, &escaped_file).is_none());
        assert!(verified_cache_path(&host, escaped_build, &escaped_file, true).is_none());

        // 4. Nonzero exit_code and >256 KiB payloads are rejected by the reader.
        let nonzero =
            String::from_utf8_lossy(&persisted).replace("\"exit_code\":0", "\"exit_code\":1");
        let nonzero_file = build.join("CMakeFiles/rcmake/nonzero.json");
        host.seed_file(&nonzero_file, nonzero.as_bytes())?;
        assert!(PlatformQueries::load_read_only(&host, build, &nonzero_file).is_none());

        let oversized_file = build.join("CMakeFiles/rcmake/oversized.json");
        host.seed_file(&oversized_file, &vec![b' '; 256 * 1024 + 16])?;
        assert!(PlatformQueries::load_read_only(&host, build, &oversized_file).is_none());

        // 5. Writer enforces the same 256 KiB total cap before emitting JSON.
        let big_queries = PlatformQueries::default();
        if let Ok(mut memo) = big_queries.0.lock() {
            for idx in 0..8usize {
                let req = RunRequest {
                    pipeline: vec![CommandSpec {
                        program: "/usr/bin/sw_vers".into(),
                        args: vec!["-productVersion".into()],
                    }],
                    cwd: PathBuf::from("/work"),
                    env: vec![
                        ("PATH".into(), "/usr/bin".into()),
                        ("SLOT".into(), format!("{idx}").into()),
                    ],
                    stdin: Input::Inherit,
                    stdout: Output::Capture,
                    stderr: Output::Capture,
                    timeout: None,
                    tee: false,
                };
                let key = query(&host, &req)
                    .ok_or_else(|| rcmake_host::HostError::unsupported("Expected query."))?;
                let big_stdout = vec![b'a'; 60 * 1024];
                let big_state = compute_live_state_digest(
                    &host,
                    &key.program,
                    &key.args,
                    &req.env,
                    &big_stdout,
                );
                memo.0.insert(
                    key,
                    Observation {
                        executable: sw_exec,
                        result: RunOutput {
                            results: vec![ProcessResult::Exited(0)],
                            stdout: big_stdout,
                            stderr: Vec::new(),
                        },
                        request: req,
                        state_digest: big_state,
                    },
                );
            }
        }
        let capped = big_queries.render_persisted().ok_or_else(|| {
            rcmake_host::HostError::unsupported("Expected capped persisted JSON.")
        })?;
        assert!(capped.len() <= usize::try_from(PERSISTED_MAX_BYTES).unwrap_or(usize::MAX));
        Ok(())
    }

    #[test]
    fn observation_captures_state_digest_around_launch_and_skips_unstable_state() -> HostResult<()>
    {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let host = Arc::new(PureHost::default());
        let build = Path::new("/work/build");
        host.create_dir_all(&build.join("CMakeFiles/rcmake"))?;
        let plist = Path::new("/System/Library/CoreServices/SystemVersion.plist");
        host.seed_file(plist, b"<key>ProductVersion</key><string>27.0</string>")?;
        let xcode_select = Path::new("/usr/bin/xcode-select");
        host.seed_file(xcode_select, b"xcode-select binary")?;
        host.set_permissions(xcode_select, 0o755)?;
        let xcrun = Path::new("/usr/bin/xcrun");
        host.seed_file(xcrun, b"xcrun binary")?;
        host.set_permissions(xcrun, 0o755)?;
        let sw_vers = Path::new("/usr/bin/sw_vers");
        host.seed_file(sw_vers, b"sw_vers binary")?;
        host.set_permissions(sw_vers, 0o755)?;

        let xcode_dev = Path::new("/Applications/Xcode.app/Contents/Developer");
        let sdk_dir = Path::new(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
        );
        host.create_dir_all(xcode_dev)?;
        host.create_dir_all(sdk_dir)?;
        let xcode_plist = Path::new("/Applications/Xcode.app/Contents/version.plist");
        host.seed_file(xcode_plist, b"xcode-v1")?;
        let sdk_plist = sdk_dir.join("SDKSettings.plist");
        host.seed_file(&sdk_plist, b"sdk-v1")?;

        let mutate_during_run = Arc::new(AtomicBool::new(true));
        let handler_mutate = Arc::clone(&mutate_during_run);
        host.set_command_handler(move |pure, request, _| {
            let [command] = request.pipeline.as_slice() else {
                return Err(rcmake_host::HostError::unsupported("expected one command"));
            };
            if command.program == b"/usr/bin/xcode-select" {
                if handler_mutate.load(Ordering::SeqCst) {
                    let _ = pure.seed_file(
                        Path::new("/Applications/Xcode.app/Contents/version.plist"),
                        b"xcode-mutated-mid-flight",
                    );
                }
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: b"/Applications/Xcode.app/Contents/Developer\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            if command.program == b"/usr/bin/xcrun"
                && command.args.iter().any(|arg| arg == "--show-sdk-path")
            {
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: format!("{}\n", sdk_dir.display()).into_bytes(),
                    stderr: Vec::new(),
                });
            }
            if command.program == b"/usr/bin/sw_vers" {
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: b"27.0\n".to_vec(),
                    stderr: Vec::new(),
                });
            }
            Err(rcmake_host::HostError::unsupported("unexpected command"))
        })?;

        let make_req = |program: &str, args: Vec<BString>| RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: PathBuf::from("/work"),
            env: vec![("PATH".into(), "/usr/bin".into())],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };

        // 1. State mutation between before and after launch produces state_digest: None and is not persisted.
        let xs_req = make_req("/usr/bin/xcode-select", vec!["-print-path".into()]);
        let xs_key = query(host.as_ref(), &xs_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected xcode-select query."))?;
        let unstable = launch_observed(None, host.as_ref(), &xs_key, &xs_req);
        assert!(unstable.state_digest.is_none());

        let unstable_queries = PlatformQueries::default();
        if let Ok(mut memo) = unstable_queries.0.lock() {
            let _ = record(
                None,
                host.as_ref(),
                &mut memo,
                xs_key.clone(),
                &xs_req,
                unstable,
                |_| {},
            )?;
        }
        assert!(unstable_queries.render_persisted().is_none());

        // 2. Stable observations capture their digest at observation time and do not re-stamp new state at session finish.
        mutate_during_run.store(false, Ordering::SeqCst);
        host.seed_file(xcode_plist, b"xcode-v1")?;

        let xcrun_req = make_req("/usr/bin/xcrun", vec!["--show-sdk-path".into()]);
        let xcrun_key = query(host.as_ref(), &xcrun_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected xcrun query."))?;

        let sw_req = make_req("/usr/bin/sw_vers", vec!["-productVersion".into()]);
        let sw_key = query(host.as_ref(), &sw_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected sw_vers query."))?;

        let xs_obs = launch_observed(None, host.as_ref(), &xs_key, &xs_req);
        let xcrun_obs = launch_observed(None, host.as_ref(), &xcrun_key, &xcrun_req);
        let sw_obs = launch_observed(None, host.as_ref(), &sw_key, &sw_req);
        let expected_xs_digest = xs_obs
            .state_digest
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected xcode-select digest."))?;
        assert!(xcrun_obs.state_digest.is_some());
        assert!(sw_obs.state_digest.is_some());

        let stable_queries = PlatformQueries::default();
        if let Ok(mut memo) = stable_queries.0.lock() {
            let _ = record(
                None,
                host.as_ref(),
                &mut memo,
                xs_key,
                &xs_req,
                xs_obs,
                |_| {},
            )?;
            let _ = record(
                None,
                host.as_ref(),
                &mut memo,
                xcrun_key,
                &xcrun_req,
                xcrun_obs,
                |_| {},
            )?;
            let _ = record(
                None,
                host.as_ref(),
                &mut memo,
                sw_key,
                &sw_req,
                sw_obs,
                |_| {},
            )?;
        }

        // Mutate Xcode selection state AFTER observation, BEFORE session finish.
        host.seed_file(xcode_plist, b"xcode-v2-after-observation")?;
        let persisted = stable_queries
            .render_persisted()
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected persisted JSON."))?;
        let cache_file = build.join("CMakeFiles/rcmake/platform-queries.v1.json");
        host.seed_file(&cache_file, &persisted)?;
        let loaded = PlatformQueries::load_read_only(host.as_ref(), build, &cache_file)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected loaded cache."))?;
        let entries = loaded
            .2
            .as_ref()
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected read-only entries."))?;
        assert_eq!(entries.len(), 3);

        let xs_entry = entries
            .iter()
            .find(|entry| entry.program == xcode_select)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Missing xcode-select entry."))?;
        assert_eq!(xs_entry.state, expected_xs_digest);
        // Under mutated live state, persisted entry does not match live state (stale reuse rejected).
        assert_ne!(
            compute_live_state_digest(
                host.as_ref(),
                &xs_entry.program,
                &xs_entry.args,
                &xs_req.env,
                &xs_entry.stdout,
            ),
            Some(xs_entry.state)
        );
        // Restoring observation-time state makes live validation match again.
        host.seed_file(xcode_plist, b"xcode-v1")?;
        for (entry, env) in [
            (xs_entry, &xs_req.env),
            (
                entries
                    .iter()
                    .find(|entry| entry.program == xcrun)
                    .ok_or_else(|| rcmake_host::HostError::unsupported("Missing xcrun entry."))?,
                &xcrun_req.env,
            ),
            (
                entries
                    .iter()
                    .find(|entry| entry.program == sw_vers)
                    .ok_or_else(|| rcmake_host::HostError::unsupported("Missing sw_vers entry."))?,
                &sw_req.env,
            ),
        ] {
            assert_eq!(
                compute_live_state_digest(
                    host.as_ref(),
                    &entry.program,
                    &entry.args,
                    env,
                    &entry.stdout,
                ),
                Some(entry.state)
            );
        }
        Ok(())
    }

    #[test]
    fn xcrun_named_sdk_queries_are_not_persisted_or_reused() -> HostResult<()> {
        let host = PureHost::default();
        let plist = Path::new("/System/Library/CoreServices/SystemVersion.plist");
        host.seed_file(plist, b"<key>ProductVersion</key><string>27.0</string>")?;
        let xcrun = Path::new("/usr/bin/xcrun");
        host.seed_file(xcrun, b"xcrun binary")?;
        host.set_permissions(xcrun, 0o755)?;
        let default_sdk = Path::new(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk",
        );
        host.create_dir_all(default_sdk)?;
        host.seed_file(&default_sdk.join("SDKSettings.plist"), b"macosx-v1")?;

        host.set_command_handler(|_, _, _| {
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: b"18.0\n".to_vec(),
                stderr: Vec::new(),
            })
        })?;

        let base_env = vec![("PATH".into(), "/usr/bin".into())];
        // Named non-macosx SDKs cannot be resolved without spawning xcrun and must not persist/reuse.
        for args in [
            vec![
                BString::from("--sdk"),
                BString::from("iphoneos"),
                BString::from("--show-sdk-version"),
            ],
            vec![
                BString::from("-sdk"),
                BString::from("iphonesimulator"),
                BString::from("--show-sdk-version"),
            ],
            vec![
                BString::from("--sdk=appletvos"),
                BString::from("--show-sdk-version"),
            ],
            vec![
                BString::from("--sdk"),
                BString::from("watchos"),
                BString::from("--show-sdk-path"),
            ],
            vec![
                BString::from("--sdk"),
                BString::from("macosx15.0"),
                BString::from("--show-sdk-version"),
            ],
        ] {
            assert!(
                compute_live_state_digest(&host, xcrun, &args, &base_env, b"18.0\n").is_none(),
                "expected None for args={args:?}"
            );
        }
        // Named non-macosx SDK in SDKROOT env var must also be rejected.
        let ios_env = vec![
            ("PATH".into(), "/usr/bin".into()),
            ("SDKROOT".into(), "iphoneos".into()),
        ];
        assert!(
            compute_live_state_digest(
                &host,
                xcrun,
                &[BString::from("--show-sdk-version")],
                &ios_env,
                b"18.0\n",
            )
            .is_none()
        );

        // Launching an iphoneos query yields state_digest: None and is omitted by render_persisted.
        let ios_req = RunRequest {
            pipeline: vec![CommandSpec {
                program: "/usr/bin/xcrun".into(),
                args: vec![
                    "--sdk".into(),
                    "iphoneos".into(),
                    "--show-sdk-version".into(),
                ],
            }],
            cwd: PathBuf::from("/work"),
            env: base_env.clone(),
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };
        let ios_key = query(&host, &ios_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected iphoneos xcrun query."))?;
        let ios_obs = launch_observed(None, &host, &ios_key, &ios_req);
        assert!(ios_obs.state_digest.is_none());
        let queries = PlatformQueries::default();
        if let Ok(mut memo) = queries.0.lock() {
            let _ = record(None, &host, &mut memo, ios_key, &ios_req, ios_obs, |_| {})?;
        }
        assert!(queries.render_persisted().is_none());

        // Explicit --sdk macosx validates the default macOS SDK paths and invalidates on plist edits.
        let mac_args = vec![
            BString::from("--sdk"),
            BString::from("macosx"),
            BString::from("--show-sdk-version"),
        ];
        let mac_v1 = compute_live_state_digest(&host, xcrun, &mac_args, &base_env, b"27.0\n")
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected macosx SDK digest."))?;
        host.seed_file(&default_sdk.join("SDKSettings.plist"), b"macosx-v2")?;
        let mac_v2 = compute_live_state_digest(&host, xcrun, &mac_args, &base_env, b"27.0\n")
            .ok_or_else(|| {
                rcmake_host::HostError::unsupported("Expected updated macosx digest.")
            })?;
        assert_ne!(mac_v1, mac_v2);

        // Explicit absolute SDK path requires a readable directory and SDKSettings.plist.
        let custom_sdk = Path::new("/custom/My.sdk");
        host.create_dir_all(custom_sdk)?;
        let custom_args = vec![
            BString::from("--sdk"),
            BString::from("/custom/My.sdk"),
            BString::from("--show-sdk-version"),
        ];
        assert!(
            compute_live_state_digest(&host, xcrun, &custom_args, &base_env, b"27.0\n").is_none()
        );
        host.seed_file(&custom_sdk.join("SDKSettings.plist"), b"custom-v1")?;
        let custom_v1 = compute_live_state_digest(&host, xcrun, &custom_args, &base_env, b"27.0\n")
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected custom SDK digest."))?;
        host.seed_file(&custom_sdk.join("SDKSettings.plist"), b"custom-v2")?;
        let custom_v2 = compute_live_state_digest(&host, xcrun, &custom_args, &base_env, b"27.0\n")
            .ok_or_else(|| {
                rcmake_host::HostError::unsupported("Expected updated custom SDK digest.")
            })?;
        assert_ne!(custom_v1, custom_v2);
        Ok(())
    }

    #[test]
    fn memo_program_digest_follows_a_changed_program() -> HostResult<()> {
        let host = PureHost::default();
        let program = Path::new("/opt/tools/bin/query");
        host.seed_file(program, b"query-v1")?;
        host.set_permissions(program, 0o755)?;
        let memo = TraceMemo::default();
        let first = program_digest(Some(&memo), &host, program)?;
        assert_eq!(first, file_digest(&host, program)?);
        host.seed_file(program, b"query-v2")?;
        host.set_permissions(program, 0o755)?;
        let second = program_digest(Some(&memo), &host, program)?;
        assert_ne!(first, second);
        assert_eq!(second, file_digest(&host, program)?);
        Ok(())
    }

    #[test]
    fn prefetched_result_is_discarded_when_executable_changes_on_disk() -> HostResult<()> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let host = Arc::new(PureHost::default());
        let brew = Path::new("/opt/homebrew/bin/brew");
        let prefix_v1 = Path::new("/opt/homebrew-v1");
        let prefix_v2 = Path::new("/opt/homebrew-v2");
        host.create_dir_all(prefix_v1)?;
        host.create_dir_all(prefix_v2)?;
        host.seed_file(brew, b"brew-v1")?;
        host.set_permissions(brew, 0o755)?;
        host.env_set(b"PATH", Some(b"/opt/homebrew/bin"));

        let runs = Arc::new(AtomicUsize::new(0));
        let mutate_mid_flight = Arc::new(AtomicBool::new(false));
        let handler_runs = Arc::clone(&runs);
        let handler_mutate = Arc::clone(&mutate_mid_flight);
        host.set_command_handler(move |pure, request, _| {
            let [command] = request.pipeline.as_slice() else {
                return Err(rcmake_host::HostError::unsupported("expected one command"));
            };
            if command.program == b"/opt/homebrew/bin/brew" {
                handler_runs.fetch_add(1, Ordering::SeqCst);
                if handler_mutate.load(Ordering::SeqCst) {
                    let _ = pure.seed_file(Path::new("/opt/homebrew/bin/brew"), b"brew-mutated");
                    let _ = pure.set_permissions(Path::new("/opt/homebrew/bin/brew"), 0o755);
                }
                let binary = pure.read(
                    Path::new("/opt/homebrew/bin/brew"),
                    ReadRange::All { limit: 1024 },
                )?;
                let out = if binary.as_slice() == b"brew-v1" {
                    b"/opt/homebrew-v1\n".to_vec()
                } else {
                    b"/opt/homebrew-v2\n".to_vec()
                };
                return Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    stdout: out,
                    stderr: Vec::new(),
                });
            }
            Err(rcmake_host::HostError::unsupported("unexpected command"))
        })?;

        let brew_req = RunRequest {
            pipeline: vec![CommandSpec {
                program: "brew".into(),
                args: vec!["--prefix".into()],
            }],
            cwd: host.initial_cwd(),
            env: host.env_all(),
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Inherit,
            timeout: None,
            tee: false,
        };
        let brew_key = query(host.as_ref(), &brew_req)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected brew query."))?;
        let digest_v1 = file_digest(host.as_ref(), &brew_key.program)
            .map_err(|err| rcmake_host::HostError::unsupported(err.to_string()))?;

        // 1. Live prefetch handoff succeeds when executable is unchanged.
        let matching_queries = PlatformQueries::default();
        let dyn_host: Arc<dyn Host> = host.clone();
        matching_queries.prefetch(&dyn_host);
        let reused = take_or_launch(
            None,
            host.as_ref(),
            &matching_queries.1,
            &brew_key,
            digest_v1,
            &brew_req,
        );
        assert_eq!(reused.program_digest, Some(digest_v1));
        assert!(reused.state_digest.is_some());
        assert_eq!(
            reused.output?.stdout.as_slice(),
            b"/opt/homebrew-v1\n".as_slice()
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // 2. When executable changes between prefetch completion and consumer pickup,
        // the stale prefetched result is discarded and replaced by a fresh launch.
        let prefetched_v1 = launch_observed(None, host.as_ref(), &brew_key, &brew_req);
        assert_eq!(prefetched_v1.program_digest, Some(digest_v1));
        assert!(prefetched_v1.state_digest.is_some());
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        let stale_queries = PlatformQueries::default();
        stale_queries
            .1
            .push_completed(brew_key.clone(), prefetched_v1);

        host.seed_file(brew, b"brew-v2")?;
        host.set_permissions(brew, 0o755)?;
        let digest_v2 = file_digest(host.as_ref(), &brew_key.program)
            .map_err(|err| rcmake_host::HostError::unsupported(err.to_string()))?;
        assert_ne!(digest_v1, digest_v2);

        let fresh = take_or_launch(
            None,
            host.as_ref(),
            &stale_queries.1,
            &brew_key,
            digest_v2,
            &brew_req,
        );
        assert_eq!(fresh.program_digest, Some(digest_v2));
        assert!(fresh.state_digest.is_some());
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let recorded = if let Ok(mut memo) = stale_queries.0.lock() {
            record(
                None,
                host.as_ref(),
                &mut memo,
                brew_key.clone(),
                &brew_req,
                fresh,
                |_| {},
            )?
        } else {
            return Err(rcmake_host::HostError::unsupported("Lock poisoned."));
        };
        assert_eq!(recorded.stdout.as_slice(), b"/opt/homebrew-v2\n".as_slice());
        assert!(stale_queries.render_persisted().is_some());

        // 3. Executable mutation during launch clears program_digest and state_digest
        // and prevents recording or persisting the observation.
        mutate_mid_flight.store(true, Ordering::SeqCst);
        let mid_flight = launch_observed(None, host.as_ref(), &brew_key, &brew_req);
        assert!(mid_flight.program_digest.is_none());
        assert!(mid_flight.state_digest.is_none());
        let unstable_queries = PlatformQueries::default();
        if let Ok(mut memo) = unstable_queries.0.lock() {
            let _ = record(
                None,
                host.as_ref(),
                &mut memo,
                brew_key,
                &brew_req,
                mid_flight,
                |_| {},
            )?;
            assert!(memo.0.is_empty());
        }
        assert!(unstable_queries.render_persisted().is_none());
        Ok(())
    }
}
