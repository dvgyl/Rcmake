//! Write CMake-compatible traces and native configuration profiles.

#![forbid(unsafe_code)]

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

use rcmake_base::{BString, Limits};
use rcmake_eval::{Interp, SampleKind};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Stream, WriteMode};
use rcmake_json::{Value, write_compact};

mod flamegraph;
mod terminal;
mod trace;

/// Record a CMake-compatible Google Trace.
pub(crate) struct Recorder {
    pid: u64,
    output: Mutex<Output>,
}

struct Output {
    json: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl Recorder {
    /// Start a legacy trace for this process.
    pub(crate) fn new(host: &dyn Host) -> Self {
        Self {
            pid: u64::from(host.system().pid),
            output: Mutex::new(Output {
                json: b"[".to_vec(),
                limit: Limits::default().max_value_bytes,
                overflowed: false,
            }),
        }
    }

    /// Attach the legacy trace to the command evaluator.
    pub(crate) fn attach(self: &Arc<Self>, interp: &mut Interp) {
        attach_profilers(interp, Some(self), None);
    }

    fn event(&self, name: &[u8], file: &[u8], line: u32, start: Duration, elapsed: Duration) {
        let mut location = BString::from(file);
        location.push(b':');
        location.extend_from_slice(line.to_string().as_bytes());
        let micros = |duration: Duration| {
            Value::UInt(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX))
        };
        let event = crate::object([
            (
                "args",
                crate::object([("location", Value::String(location))]),
            ),
            ("cat", Value::string("script")),
            ("dur", micros(elapsed)),
            ("name", Value::string(name)),
            ("ph", Value::string("X")),
            ("pid", Value::UInt(self.pid)),
            ("tid", Value::UInt(0)),
            ("ts", micros(start)),
        ]);
        let bytes = write_compact(&event);
        if let Ok(mut output) = self.output.lock() {
            if output
                .json
                .len()
                .saturating_add(bytes.len())
                .saturating_add(3)
                > output.limit
            {
                output.overflowed = true;
            } else if !output.overflowed {
                if output.json.len() > 1 {
                    output.json.extend_from_slice(b",\n");
                }
                output.json.extend_from_slice(&bytes);
            }
        }
    }

    /// Write the legacy trace to the requested file.
    pub(crate) fn write(&self, host: &dyn Host, path: &Path) -> HostResult<()> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| HostError::unsupported("Profiling recorder stopped unexpectedly"))?;
        if output.overflowed {
            return Err(HostError::unsupported(
                "Profiling output exceeds the size limit",
            ));
        }
        output.json.push(b']');
        host.write(path, &output.json, WriteMode::Truncate)
    }
}

/// Attach the requested recorders to the evaluator. Use one callback for both outputs.
pub(crate) fn attach_profilers(
    interp: &mut Interp,
    legacy: Option<&Arc<Recorder>>,
    native: Option<&Arc<ProfileRecorder>>,
) {
    if legacy.is_none() && native.is_none() {
        return;
    }
    let legacy = legacy.cloned();
    let native = native.cloned();
    interp.set_command_profiler(Some(Arc::new(move |sample| {
        if let Some(legacy) = &legacy
            && sample.kind != SampleKind::ListFile
        {
            legacy.event(
                sample.name,
                sample.file,
                sample.line,
                sample.start,
                sample.elapsed,
            );
        }
        if let Some(native) = &native {
            native.record(ProfileSample {
                name: sample.name,
                file: sample.file,
                line: sample.line,
                depth: sample.depth,
                start: sample.start,
                elapsed: sample.elapsed,
                kind: match sample.kind {
                    SampleKind::Function => ProfileKind::Function,
                    SampleKind::Macro => ProfileKind::Macro,
                    SampleKind::ListFile => ProfileKind::ListFile,
                    SampleKind::Builtin => ProfileKind::Builtin,
                },
            });
        }
    })));
}

const MAX_SAMPLES: usize = 4_000_000;
const MAX_SAMPLE_BYTES: usize = 96 * 1024 * 1024;
const MAX_SITES: usize = 256 * 1024;
const MAX_LABEL_BYTES: usize = 32 * 1024 * 1024;
const MAX_LABEL_SIZE: usize = 16 * 1024;
const TRACE_BYTES: usize = 64 * 1024 * 1024;
const FOLDED_BYTES: usize = 32 * 1024 * 1024;
const HTML_BYTES: usize = 16 * 1024 * 1024;

/// Identifies a phase of configuration or generation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ProfilePhase {
    /// Read and parse list files.
    Parse,
    /// Evaluate the project.
    Eval,
    /// Run configuration probes.
    Probes,
    /// Resolve the project model.
    Resolve,
    /// Write the build system.
    Generate,
}

impl ProfilePhase {
    const ALL: [Self; 5] = [
        Self::Parse,
        Self::Eval,
        Self::Probes,
        Self::Resolve,
        Self::Generate,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Eval => "eval",
            Self::Probes => "probes",
            Self::Resolve => "resolve",
            Self::Generate => "generate",
        }
    }
}

/// Identifies the operation measured by one sample.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ProfileKind {
    /// A configuration or generation phase.
    Phase(ProfilePhase),
    /// Evaluation of one list file.
    ListFile,
    /// A built-in command.
    Builtin,
    /// A user-defined function.
    Function,
    /// A user-defined macro.
    Macro,
}

impl ProfileKind {
    const fn category(self) -> &'static str {
        match self {
            Self::Phase(_) => "phase",
            Self::ListFile => "listfile",
            Self::Builtin => "builtin",
            Self::Function => "function",
            Self::Macro => "macro",
        }
    }

    const fn is_command(self) -> bool {
        matches!(self, Self::Builtin | Self::Function | Self::Macro)
    }

    const fn is_eval_phase(self) -> bool {
        matches!(self, Self::Phase(ProfilePhase::Eval))
    }
}

/// Describes the timing and source location of a completed operation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProfileSample<'a> {
    /// Command, list file, or phase name.
    pub(crate) name: &'a [u8],
    /// Source path, or an empty slice if the operation has no source path.
    pub(crate) file: &'a [u8],
    /// One-based source line, or zero if the line is unknown.
    pub(crate) line: u32,
    /// Call depth in the evaluator.
    pub(crate) depth: u32,
    /// Start time on the host's monotonic clock.
    pub(crate) start: Duration,
    /// Time from the start of the operation to its completion.
    pub(crate) elapsed: Duration,
    /// Operation type.
    pub(crate) kind: ProfileKind,
}

/// Collect native profile samples for one invocation.
///
/// Create a recorder and attach its sample callback only when profiling is enabled.
pub(crate) struct ProfileRecorder {
    state: Mutex<State>,
    sample_limit: usize,
    trace_limit: usize,
}

#[derive(Default)]
struct State {
    labels: BTreeSet<Arc<[u8]>>,
    label_bytes: usize,
    spans: BinaryHeap<Span>,
    sites: BTreeMap<Site, usize>,
    clipped_labels: usize,
    dropped_spans: usize,
    dropped_totals: usize,
    command_count: usize,
}

impl State {
    fn label(&mut self, bytes: &[u8]) -> Arc<[u8]> {
        if let Some(label) = self.labels.get(bytes) {
            return Arc::clone(label);
        }
        let estimate = |bytes: &[u8]| bytes.len().saturating_add(64);
        let bytes = if bytes.len() > MAX_LABEL_SIZE
            || self.label_bytes.saturating_add(estimate(bytes)) > MAX_LABEL_BYTES
        {
            self.clipped_labels = self.clipped_labels.saturating_add(1);
            let fallback = b"[label limit reached]".as_slice();
            if let Some(label) = self.labels.get(fallback) {
                return Arc::clone(label);
            }
            fallback
        } else {
            bytes
        };
        let label: Arc<[u8]> = Arc::from(bytes);
        self.label_bytes += estimate(bytes);
        self.labels.insert(Arc::clone(&label));
        label
    }
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Site {
    name: Arc<[u8]>,
    file: Arc<[u8]>,
    line: u32,
    kind: ProfileKind,
}

#[derive(Clone)]
struct Span {
    site: Site,
    depth: u32,
    infer_depth: bool,
    start: Duration,
    elapsed: Duration,
    thread: ThreadId,
    parent: Option<usize>,
    tid: u32,
    self_time: Duration,
}

impl Span {
    fn end(&self) -> Duration {
        self.start.saturating_add(self.elapsed)
    }
}

impl PartialEq for Span {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Span {}

impl PartialOrd for Span {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Span {
    fn cmp(&self, other: &Self) -> Ordering {
        self.start
            .cmp(&other.start)
            .then_with(|| other.elapsed.cmp(&self.elapsed))
            .then_with(|| self.depth.cmp(&other.depth))
            .then_with(|| {
                other
                    .site
                    .kind
                    .is_eval_phase()
                    .cmp(&self.site.kind.is_eval_phase())
            })
            .then_with(|| self.site.cmp(&other.site))
    }
}

#[derive(Clone, Copy, Default)]
struct Coverage {
    until: Duration,
    elapsed: Duration,
}

impl Coverage {
    // Intervals arrive in start order.
    fn add(&mut self, start: Duration, end: Duration) {
        self.elapsed = self
            .elapsed
            .saturating_add(end.saturating_sub(start.max(self.until)));
        self.until = self.until.max(end);
    }
}

impl ProfileRecorder {
    /// Start an empty native profile.
    pub(crate) fn new() -> Self {
        Self::with_limits(
            MAX_SAMPLES.min(MAX_SAMPLE_BYTES / size_of::<Span>()),
            TRACE_BYTES,
        )
    }

    fn with_limits(sample_limit: usize, trace_limit: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            sample_limit,
            trace_limit,
        }
    }

    /// Store a sample with its evaluator depth and update the call counts.
    pub(crate) fn record(&self, sample: ProfileSample<'_>) {
        self.record_sample(sample, false);
    }

    fn record_sample(&self, sample: ProfileSample<'_>, infer_depth: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let site = Site {
            name: state.label(sample.name),
            file: state.label(sample.file),
            line: sample.line,
            kind: sample.kind,
        };
        if site.kind.is_command() {
            state.command_count = state.command_count.saturating_add(1);
        }
        let new_site_allowed = state.sites.len() < MAX_SITES;
        if let Some(count) = state.sites.get_mut(&site) {
            *count = count.saturating_add(1);
        } else if new_site_allowed || matches!(site.kind, ProfileKind::Phase(_)) {
            state.sites.insert(site.clone(), 1);
        } else {
            state.dropped_totals = state.dropped_totals.saturating_add(1);
        }

        let span = Span {
            site,
            depth: sample.depth,
            infer_depth,
            start: sample.start,
            elapsed: sample.elapsed,
            thread: thread::current().id(),
            parent: None,
            tid: 0,
            self_time: sample.elapsed,
        };
        if state.spans.len() < self.sample_limit {
            state.spans.push(span);
        } else {
            state.dropped_spans = state.dropped_spans.saturating_add(1);
            // Completion order puts children first. Keep the earliest spans, with
            // enclosing spans first when start times match.
            if let Some(mut latest) = state.spans.peek_mut()
                && span < *latest
            {
                *latest = span;
            }
        }
    }

    /// Store a phase sample with times from the host's monotonic clock.
    pub(crate) fn record_phase(&self, phase: ProfilePhase, start: Duration, finish: Duration) {
        self.record(ProfileSample {
            name: phase.name().as_bytes(),
            file: b"",
            line: 0,
            depth: 0,
            start,
            elapsed: finish.saturating_sub(start),
            kind: ProfileKind::Phase(phase),
        });
    }

    /// Write the terminal summary and publish the complete native profile files.
    pub(crate) fn write(
        &self,
        host: &dyn Host,
        directory: &Path,
        wall: Duration,
    ) -> HostResult<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| HostError::unsupported("Cannot read the profile samples."))?;
        let mut spans = state.spans.clone().into_sorted_vec();
        let coverage = resolve_spans(&mut spans);
        let dropped = state.dropped_spans;
        host.write_out(
            Stream::Stderr,
            terminal::render(&state, &spans, wall, coverage).as_bytes(),
        );
        drop(state);

        host.create_dir_all(directory)?;
        let directory = host.canonicalize(directory)?;
        let graph = flamegraph::Graph::from_spans(&spans, coverage);

        let (trace, trace_omitted) =
            trace::render(&spans, host.system().pid, self.trace_limit, dropped);
        let (folded, folded_omitted) = graph.folded(FOLDED_BYTES, dropped);
        let (html, html_omitted) = graph.html(HTML_BYTES, dropped);
        let _lock = host.lock_file(
            &directory.join(".rcmake-profile.lock"),
            Some(Duration::from_secs(10)),
        )?;
        let (mut staged, run_name) = StagedRun::reserve(host, &directory)?;
        for (name, bytes, omitted) in [
            ("trace.json", trace.as_slice(), trace_omitted),
            ("stacks.folded", folded.as_bytes(), folded_omitted),
            ("flamegraph.html", html.as_bytes(), html_omitted),
        ] {
            if omitted > 0 {
                let message = format!("{name} omitted entries at the export limit ({omitted}).\n");
                host.write_out(Stream::Stderr, message.as_bytes());
            }
            host.write(&staged.path.join(name), bytes, WriteMode::CreateNew)?;
        }
        let run_directory = directory.join(&run_name);
        host.rename(&staged.path, &run_directory)?;
        staged.path = run_directory.clone();
        let current = Artifact::stage(
            host,
            &directory,
            "current",
            format!("{run_name}\n").as_bytes(),
        )?;
        current.check_destination()?;
        current.publish()?;
        staged.path.clear();
        let message = format!(
            "Profile files: {}\n",
            terminal::display_path(&run_directory)
        );
        host.write_out(Stream::Stderr, message.as_bytes());
        Ok(())
    }
}

struct StagedRun<'a> {
    host: &'a dyn Host,
    path: std::path::PathBuf,
}

impl<'a> StagedRun<'a> {
    fn reserve(host: &'a dyn Host, directory: &Path) -> HostResult<(Self, String)> {
        static NEXT_RUN: AtomicU64 = AtomicU64::new(0);
        let pid = host.system().pid;
        let now = host.now();
        for _ in 0..64 {
            let serial = NEXT_RUN.fetch_add(1, AtomicOrdering::Relaxed);
            let run = format!("run-{pid}-{}-{}-{serial}", now.secs, now.nanos);
            let path = directory.join(format!(".{run}.tmp"));
            if [path.clone(), directory.join(&run)]
                .iter()
                .try_fold(false, |exists, path| {
                    match host.metadata(path, Follow::No) {
                        Ok(_) => Ok(true),
                        Err(error) if error.kind == ErrorKind::NotFound => Ok(exists),
                        Err(error) => Err(error),
                    }
                })?
            {
                continue;
            }
            host.create_dir_all(&path)?;
            return Ok((Self { host, path }, run));
        }
        Err(HostError::unsupported(
            "Cannot allocate a profile run directory.",
        ))
    }
}

impl Drop for StagedRun<'_> {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = self.host.remove_dir_all(&self.path);
        }
    }
}

struct Artifact<'a> {
    host: &'a dyn Host,
    staged: std::path::PathBuf,
    final_path: std::path::PathBuf,
}

impl<'a> Artifact<'a> {
    fn stage(host: &'a dyn Host, directory: &Path, name: &str, bytes: &[u8]) -> HostResult<Self> {
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let pid = host.system().pid;
        let now = host.now();
        for _ in 0..64 {
            let serial = NEXT_TEMP.fetch_add(1, AtomicOrdering::Relaxed);
            let staged = directory.join(format!(
                ".{name}.{pid}-{}-{}-{serial}.tmp",
                now.secs, now.nanos
            ));
            match host.write(&staged, bytes, WriteMode::CreateNew) {
                Ok(()) => {
                    return Ok(Self {
                        host,
                        staged,
                        final_path: directory.join(name),
                    });
                }
                Err(error) if error.kind == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    let _ = host.remove_file(&staged);
                    return Err(error);
                }
            }
        }
        Err(HostError::unsupported(
            "Cannot allocate a temporary profile file.",
        ))
    }

    fn check_destination(&self) -> HostResult<()> {
        match self.host.metadata(&self.final_path, Follow::No) {
            Ok(metadata) if matches!(metadata.kind, FileKind::File | FileKind::Symlink) => Ok(()),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(HostError::from_io(
                std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "The profile destination is not a file.",
                ),
                Some(self.final_path.clone()),
            )),
        }
    }

    fn publish(mut self) -> HostResult<()> {
        let result = self.host.rename(&self.staged, &self.final_path);
        if result.is_ok() {
            self.staged.clear();
        }
        result
    }
}

impl Drop for Artifact<'_> {
    fn drop(&mut self) {
        if !self.staged.as_os_str().is_empty() {
            let _ = self.host.remove_file(&self.staged);
        }
    }
}

#[derive(Default)]
struct Ancestry {
    eval: EnclosingSpans,
    by_depth: BTreeMap<u32, EnclosingSpans>,
    legacy: EnclosingSpans,
}

impl Ancestry {
    fn parent(&self, span: &Span) -> Option<usize> {
        let command_parent = match span.site.kind {
            ProfileKind::Phase(_) => None,
            _ if span.infer_depth => self.legacy.find(span, true),
            _ => self
                .by_depth
                .range(..span.depth)
                .rev()
                .find_map(|(_, candidates)| candidates.find(span, false)),
        };
        command_parent.or_else(|| {
            if span.site.kind.is_eval_phase() {
                None
            } else {
                self.eval
                    .find(span, matches!(span.site.kind, ProfileKind::Phase(_)))
            }
        })
    }

    fn insert(&mut self, span: &Span, index: usize) {
        match span.site.kind {
            ProfileKind::Phase(ProfilePhase::Eval) => self.eval.insert(span, index),
            ProfileKind::Phase(_) => {}
            _ if span.infer_depth => self.legacy.insert(span, index),
            _ => self
                .by_depth
                .entry(span.depth)
                .or_default()
                .insert(span, index),
        }
    }
}

#[derive(Default)]
struct EnclosingSpans {
    // Starts increase and ends decrease. An entry with an earlier end than a new
    // span cannot enclose any future span better than that new span.
    spans: Vec<(Duration, Duration, usize)>,
}

impl EnclosingSpans {
    fn find(&self, child: &Span, strict: bool) -> Option<usize> {
        let end = child.end();
        let eligible = self.spans.partition_point(|&(_, candidate_end, _)| {
            candidate_end >= end && candidate_end > child.start
        });
        let mut candidates = self.spans.get(..eligible)?.iter().rev();
        let &(start, candidate_end, index) = candidates.next()?;
        if strict && (start, candidate_end) == (child.start, end) {
            candidates.next().map(|&(_, _, index)| index)
        } else {
            Some(index)
        }
    }

    fn insert(&mut self, span: &Span, index: usize) {
        let end = span.end();
        while self.spans.last().is_some_and(|&(start, previous_end, _)| {
            previous_end < end || (start, previous_end) == (span.start, end)
        }) {
            self.spans.pop();
        }
        self.spans.push((span.start, end, index));
    }
}

#[derive(Default)]
struct Tracks {
    tops: Vec<Option<usize>>,
    finishing: BinaryHeap<Reverse<(Duration, usize)>>,
    free: HashMap<ThreadId, BinaryHeap<Reverse<u32>>>,
}

impl Tracks {
    fn assign(&mut self, spans: &[Span], span: &Span, index: usize) -> u32 {
        while let Some(&Reverse((end, previous))) = self.finishing.peek() {
            if end > span.start {
                break;
            }
            self.finishing.pop();
            let Some(finished) = spans.get(previous) else {
                continue;
            };
            let Some(top) = self.tops.get_mut(finished.tid as usize) else {
                continue;
            };
            if *top != Some(previous) {
                continue;
            }
            // Equal endpoints are common for nested operations. Remove them
            // together so the next sibling finds the right top frame.
            *top = finished.parent;
            while let Some(parent) = *top {
                let Some(enclosing) = spans.get(parent).filter(|span| span.tid == finished.tid)
                else {
                    *top = None;
                    break;
                };
                if enclosing.end() > span.start {
                    break;
                }
                *top = enclosing.parent;
            }
            if top.is_none() {
                self.free
                    .entry(finished.thread)
                    .or_default()
                    .push(Reverse(finished.tid));
            }
        }

        let tid = span
            .parent
            .and_then(|parent| spans.get(parent))
            .map(|parent| parent.tid)
            .filter(|&tid| self.tops.get(tid as usize) == Some(&span.parent))
            .or_else(|| {
                self.free
                    .get_mut(&span.thread)?
                    .pop()
                    .map(|Reverse(tid)| tid)
            });
        let tid = if let Some(tid) = tid
            && let Some(top) = self.tops.get_mut(tid as usize)
        {
            *top = Some(index);
            tid
        } else {
            let tid = self.tops.len() as u32;
            self.tops.push(Some(index));
            tid
        };
        self.finishing.push(Reverse((span.end(), index)));
        tid
    }
}

fn resolve_spans(spans: &mut [Span]) -> Duration {
    let mut ancestry = HashMap::<ThreadId, Ancestry>::new();
    let mut tracks = Tracks::default();
    let mut covered = vec![Coverage::default(); spans.len()];
    let mut root = Coverage::default();
    for index in 0..spans.len() {
        let (prior, remaining) = spans.split_at_mut(index);
        let [span, ..] = remaining else {
            break;
        };
        let thread = ancestry.entry(span.thread).or_default();
        span.parent = thread.parent(span);
        if span.infer_depth {
            span.depth = span
                .parent
                .and_then(|parent| prior.get(parent))
                .map_or(0, |parent| {
                    parent
                        .depth
                        .saturating_add(u32::from(parent.site.kind.is_command()))
                });
        }
        if let Some(coverage) = span.parent.and_then(|parent| covered.get_mut(parent)) {
            coverage.add(span.start, span.end());
        }
        root.add(span.start, span.end());
        thread.insert(span, index);
        span.tid = tracks.assign(prior, span, index);
    }
    for (span, children) in spans.iter_mut().zip(covered) {
        span.self_time = span.elapsed.saturating_sub(children.elapsed);
    }
    root.elapsed
}

#[cfg(test)]
mod tests {
    use rcmake_host::{PureHost, ReadRange};
    use rcmake_json::{ReadOptions, parse};

    use super::*;

    const RANGE: ReadRange = ReadRange::All { limit: 100_000 };

    fn pinned_run(host: &dyn Host, directory: &Path) -> HostResult<std::path::PathBuf> {
        let name = host.read(&directory.join("current"), RANGE)?;
        let name = std::str::from_utf8(&name)
            .ok()
            .and_then(|name| name.strip_suffix('\n'))
            .filter(|name| name.starts_with("run-") && !name.contains(['/', '\\']))
            .ok_or_else(|| HostError::unsupported("Invalid profile run name."))?;
        Ok(directory.join(name))
    }

    fn sample(kind: ProfileKind, name: &[u8]) -> ProfileSample<'_> {
        ProfileSample {
            name,
            file: b"src/<test>.cmake",
            line: 8,
            depth: 2,
            start: Duration::from_secs(1),
            elapsed: Duration::from_secs(2),
            kind,
        }
    }

    #[test]
    fn reports_are_valid_and_independent_of_insertion_order() -> HostResult<()> {
        let host = PureHost::default();
        let first = ProfileRecorder::new();
        let second = ProfileRecorder::new();
        let samples = [
            sample(ProfileKind::ListFile, b"lib"),
            sample(ProfileKind::Macro, b"<macro>&\"'"),
        ];
        for entry in samples {
            first.record(entry);
        }
        for entry in samples.into_iter().rev() {
            second.record(entry);
        }
        let wall = Duration::from_secs(4);
        first.record_phase(ProfilePhase::Eval, Duration::ZERO, wall);
        second.record_phase(ProfilePhase::Eval, Duration::ZERO, wall);
        let a = host.initial_cwd().join("first");
        let b = host.initial_cwd().join("second");
        first.write(&host, &a, wall)?;
        second.write(&host, &b, wall)?;
        let a = pinned_run(&host, &a)?;
        let b = pinned_run(&host, &b)?;
        for name in ["trace.json", "stacks.folded", "flamegraph.html"] {
            assert_eq!(
                host.read(&a.join(name), RANGE)?,
                host.read(&b.join(name), RANGE)?
            );
        }
        let trace = host.read(&a.join("trace.json"), RANGE)?;
        assert!(
            matches!(parse(&trace, &ReadOptions::default()), Ok(Value::Array(events)) if events.len() == 3)
        );
        let html =
            String::from_utf8_lossy(&host.read(&a.join("flamegraph.html"), RANGE)?).into_owned();
        assert!(html.contains(r#"\u003cmacro\u003e\u0026"#));
        let stderr = host.stream_bytes(Stream::Stderr);
        let stderr = String::from_utf8_lossy(stderr.as_slice());
        assert!(stderr.contains("4.00 s"));
        assert!(stderr.contains("src/<test>.cmake"));
        assert!(stderr.contains("<macro>"));
        Ok(())
    }

    #[test]
    fn a_reader_can_keep_a_previous_profile_run() -> HostResult<()> {
        let host = PureHost::default();
        let recorder = ProfileRecorder::new();
        let directory = host.initial_cwd().join("profile");
        recorder.record(sample(ProfileKind::Builtin, b"first"));
        recorder.write(&host, &directory, Duration::from_secs(4))?;
        let first = pinned_run(&host, &directory)?;
        let trace = host.read(&first.join("trace.json"), RANGE)?;

        recorder.record(sample(ProfileKind::Builtin, b"second"));
        recorder.write(&host, &directory, Duration::from_secs(4))?;
        let second = pinned_run(&host, &directory)?;
        assert_ne!(first, second);
        assert_eq!(host.read(&first.join("trace.json"), RANGE)?, trace);
        assert_ne!(host.read(&second.join("trace.json"), RANGE)?, trace);
        for run in [first, second] {
            assert!(!host.read(&run.join("stacks.folded"), RANGE)?.is_empty());
            assert!(!host.read(&run.join("flamegraph.html"), RANGE)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn keeps_call_counts_and_marks_recording_omissions() -> HostResult<()> {
        let host = PureHost::default();
        let recorder = ProfileRecorder::with_limits(1, 4096);
        let entry = sample(ProfileKind::Builtin, b"set");
        recorder.record(entry);
        recorder.record(entry);
        let directory = host.initial_cwd().join("limited");
        recorder.write(&host, &directory, Duration::from_secs(5))?;
        let directory = pinned_run(&host, &directory)?;
        let trace = host.read(&directory.join("trace.json"), RANGE)?;
        assert!(
            matches!(parse(&trace, &ReadOptions::default()), Ok(Value::Array(events)) if events.len() == 2)
        );
        let stderr = host.stream_bytes(Stream::Stderr);
        let stderr = String::from_utf8_lossy(stderr.as_slice());
        assert!(stderr.contains("2 commands"));
        assert!(stderr.contains("Omitted samples at the recording limit (1)."));
        Ok(())
    }

    #[test]
    fn crossing_and_coextensive_phases_use_separate_tracks() -> HostResult<()> {
        let recorder = ProfileRecorder::new();
        for (phase, start, end) in [
            (ProfilePhase::Parse, 10, 65),
            (ProfilePhase::Probes, 50, 80),
            (ProfilePhase::Resolve, 81, 90),
            (ProfilePhase::Generate, 81, 90),
            (ProfilePhase::Eval, 0, 100),
        ] {
            recorder.record_phase(
                phase,
                Duration::from_micros(start),
                Duration::from_micros(end),
            );
        }
        let mut spans = recorder
            .state
            .lock()
            .map_err(|_| HostError::unsupported("Cannot read the profile samples."))?
            .spans
            .clone()
            .into_sorted_vec();
        assert_eq!(resolve_spans(&mut spans), Duration::from_micros(100));
        let [eval, parse, probes, resolve, generate] = spans.as_slice() else {
            return Err(HostError::unsupported("Expected five profile spans."));
        };
        assert_eq!(eval.self_time, Duration::from_micros(21));
        for span in [parse, probes, resolve, generate] {
            assert_eq!(span.parent, Some(0));
        }
        assert_ne!(parse.tid, probes.tid);
        assert_ne!(resolve.tid, generate.tid);
        Ok(())
    }

    #[test]
    fn retains_late_finishing_parent_and_writes_summary_on_export_error() -> HostResult<()> {
        let host = PureHost::default();
        let recorder = ProfileRecorder::with_limits(1, 4096);
        recorder.record(sample(ProfileKind::Builtin, b"include"));
        recorder.record_phase(ProfilePhase::Eval, Duration::ZERO, Duration::from_secs(4));
        let directory = host.initial_cwd().join("profile");
        host.create_dir_all(&directory.join("current"))?;
        assert!(
            recorder
                .write(&host, &directory, Duration::from_secs(4))
                .is_err()
        );
        assert!(host.read_dir(&directory)?.iter().all(|entry| {
            matches!(entry.name.as_slice(), b"current" | b".rcmake-profile.lock")
        }));
        let report = host.stream_bytes(Stream::Stderr);
        assert!(String::from_utf8_lossy(&report).contains("4.00 s of recorded wall time"));
        Ok(())
    }
}
