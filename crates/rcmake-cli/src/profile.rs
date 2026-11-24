//! Write CMake-compatible traces and native configuration profiles.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcmake_base::{BString, Limits};
use rcmake_eval::{CommandSample, Interp, SampleKind};
use rcmake_host::{Host, HostError, HostResult, Stream, WriteMode};
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
            native.record_command(sample);
        }
    })));
}

const MAX_SAMPLES: usize = 4_000_000;
const MAX_SITES: usize = 256 * 1024;
const MAX_LABEL_BYTES: usize = 32 * 1024 * 1024;

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
    spans: Vec<Span>,
    sites: BTreeMap<Site, Totals>,
    trace_bytes: usize,
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
        let bytes = if self.label_bytes.saturating_add(estimate(bytes)) > MAX_LABEL_BYTES {
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

struct Span {
    site: Site,
    depth: u32,
    start: Duration,
    elapsed: Duration,
}

impl Span {
    fn end(&self) -> Duration {
        self.start.saturating_add(self.elapsed)
    }
}

#[derive(Clone, Copy, Default)]
struct Totals {
    count: usize,
    elapsed: Duration,
}

impl Totals {
    fn add(&mut self, other: Self) {
        self.count = self.count.saturating_add(other.count);
        self.elapsed = self.elapsed.saturating_add(other.elapsed);
    }
}

impl ProfileRecorder {
    /// Start an empty native profile.
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_SAMPLES, Limits::default().max_value_bytes)
    }

    fn with_limits(sample_limit: usize, trace_limit: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            sample_limit,
            trace_limit,
        }
    }

    fn record_command(&self, sample: &CommandSample<'_>) {
        self.record(ProfileSample {
            name: sample.name,
            file: sample.file,
            line: sample.line,
            depth: sample.depth,
            start: sample.start,
            elapsed: sample.elapsed,
            kind: match sample.kind {
                SampleKind::Builtin => ProfileKind::Builtin,
                SampleKind::Function => ProfileKind::Function,
                SampleKind::Macro => ProfileKind::Macro,
                SampleKind::ListFile => ProfileKind::ListFile,
            },
        });
    }

    /// Store a sample and update the terminal totals.
    pub(crate) fn record(&self, sample: ProfileSample<'_>) {
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
        if let Some(totals) = state.sites.get_mut(&site) {
            totals.add(Totals {
                count: 1,
                elapsed: sample.elapsed,
            });
        } else if new_site_allowed || matches!(site.kind, ProfileKind::Phase(_)) {
            state.sites.insert(
                site.clone(),
                Totals {
                    count: 1,
                    elapsed: sample.elapsed,
                },
            );
        } else {
            state.dropped_totals = state.dropped_totals.saturating_add(1);
        }

        // Reserve for the fixed fields and up to six output bytes per input byte.
        let estimate = site
            .name
            .len()
            .saturating_add(site.file.len())
            .saturating_mul(6)
            .saturating_add(256);
        let total_bytes = state.trace_bytes.saturating_add(estimate);
        if state.spans.len() >= self.sample_limit || total_bytes > self.trace_limit {
            state.dropped_spans = state.dropped_spans.saturating_add(1);
            return;
        }
        state.trace_bytes = total_bytes;
        state.spans.push(Span {
            site,
            depth: sample.depth,
            start: sample.start,
            elapsed: sample.elapsed,
        });
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

    /// Write the profile files and the terminal summary.
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
        let mut spans: Vec<_> = state.spans.iter().collect();
        spans.sort_unstable_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then_with(|| b.elapsed.cmp(&a.elapsed))
                .then_with(|| a.depth.cmp(&b.depth))
                .then_with(|| {
                    b.site
                        .kind
                        .is_eval_phase()
                        .cmp(&a.site.kind.is_eval_phase())
                })
                .then_with(|| a.site.kind.cmp(&b.site.kind))
                .then_with(|| a.site.cmp(&b.site))
        });

        let graph = flamegraph::Graph::from_spans(&spans);
        let folded = graph.folded(self.trace_limit).ok_or_else(|| {
            HostError::unsupported("Folded profile stacks exceed the size limit.")
        })?;
        host.create_dir_all(directory)?;
        host.write(
            &directory.join("trace.json"),
            &trace::render(&spans, host.system().pid),
            WriteMode::Truncate,
        )?;
        host.write(
            &directory.join("stacks.folded"),
            folded.as_bytes(),
            WriteMode::Truncate,
        )?;
        host.write(
            &directory.join("flamegraph.html"),
            graph.html().as_bytes(),
            WriteMode::Truncate,
        )?;
        host.write_out(Stream::Stderr, terminal::render(&state, wall).as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rcmake_host::{PureHost, ReadRange};
    use rcmake_json::{ReadOptions, parse};

    use super::*;

    const RANGE: ReadRange = ReadRange::All { limit: 100_000 };

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
        assert!(html.contains("&lt;macro&gt;&amp;&quot;&#39;"));
        let stderr = host.stream_bytes(Stream::Stderr);
        let stderr = String::from_utf8_lossy(stderr.as_slice());
        assert!(stderr.contains("4.00 s"));
        assert!(stderr.contains("src/<test>.cmake"));
        assert!(stderr.contains("<macro>"));
        Ok(())
    }

    #[test]
    fn keeps_totals_when_the_trace_reaches_its_limit() -> HostResult<()> {
        let host = PureHost::default();
        let recorder = ProfileRecorder::with_limits(1, 4096);
        let entry = sample(ProfileKind::Builtin, b"set");
        recorder.record(entry);
        recorder.record(entry);
        let directory = host.initial_cwd().join("limited");
        recorder.write(&host, &directory, Duration::from_secs(5))?;
        let trace = host.read(&directory.join("trace.json"), RANGE)?;
        assert!(
            matches!(parse(&trace, &ReadOptions::default()), Ok(Value::Array(events)) if events.len() == 1)
        );
        let stderr = host.stream_bytes(Stream::Stderr);
        let stderr = String::from_utf8_lossy(stderr.as_slice());
        assert!(stderr.contains("2 commands"));
        assert!(stderr.contains("Samples omitted from trace and flamegraph: 1"));
        Ok(())
    }
}
