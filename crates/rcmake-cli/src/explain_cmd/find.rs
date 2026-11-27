//! `rcmake explain --find <package>` and `rcmake explain why-not <package>`.

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::Arc;

use rcmake_api::{
    ApiError, FIND_TRACE_MAX_BYTES, FIND_TRACE_PATH, FindReport, FindSource, FindTraceDocument,
    Freshness, NearMissProbe, render_find_json, render_find_terminal, run_configure,
};
use rcmake_base::{BString, path_lex::Lexical};
use rcmake_eval::{FindTraceLevel, FindTraceLimits, FindTraceStore};
use rcmake_host::{Host, ReadRange, Stream, path};

use super::args::FindOptions;
use super::{error, request};
use crate::{cache, configure::find_hints, resolve_cli_path};

pub(super) const HELP: &str = "\
Usage: rcmake explain --find <package> [-B <build-dir>] [--call <n>]
                      [--all-candidates] [--fresh] [--json]
                      [--color=auto|always|never]
       rcmake explain why-not <package> [options]

Show how find_package() searched for a package and why it failed.

  -B <build-dir>      Read the build directory. Default: current directory.
  --call <n>          Show only call <n> when the project calls find_package
                      for this package more than once.
  --all-candidates    Show every tested path, not only rejected files.
  --fresh             Run the search again. Do not use the saved trace.
  --json              Write one JSON report to standard output.
  --color=<mode>      Set terminal color. Default: auto.
  -h, --help          Show this help.

A native rcmake configure saves the search in CMakeFiles/rcmake/find-trace.v1.json.
When that file is missing or out of date, rcmake evaluates the project again
without running subprocesses or writing to the build directory.

Examples
  rcmake explain --find Boost
  rcmake explain why-not Qt6 -B build --json

Exit status
  0  The package was found.
  1  The package was not found, or the project never asked for it.
  2  Invalid arguments or an unavailable configured build directory.
";

const MAX_PROBE_CALLS: usize = 48;
const MAX_PROBE_NAMES: usize = 256;

struct HostProbe<'a> {
    host: &'a dyn Host,
    calls: usize,
}

impl NearMissProbe for HostProbe<'_> {
    fn list_dir(&mut self, dir: &[u8]) -> Vec<BString> {
        if self.calls >= MAX_PROBE_CALLS {
            return Vec::new();
        }
        self.calls += 1;
        self.host
            .read_dir(&path::to_native(dir))
            .map(|entries| {
                entries
                    .into_iter()
                    .take(MAX_PROBE_NAMES)
                    .map(|entry| entry.name)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn env(&mut self, name: &[u8]) -> Option<BString> {
        self.host.env_get(name).filter(|value| !value.is_empty())
    }
}

struct Evidence {
    store: FindTraceStore,
    source: FindSource,
    stale_reason: Option<&'static str>,
    completed: bool,
    provisional: bool,
}

fn normalized(value: &[u8]) -> BString {
    Lexical::new(value).normalize()
}

fn saved(host: &dyn Host, build: &Path) -> Option<(FindTraceDocument, Freshness)> {
    let file = build.join(path::to_native(FIND_TRACE_PATH.as_bytes()));
    let bytes = host
        .read(
            &file,
            ReadRange::All {
                limit: u64::try_from(FIND_TRACE_MAX_BYTES).unwrap_or(u64::MAX),
            },
        )
        .ok()?;
    let mut document = FindTraceDocument::parse(&bytes)?;
    let cache = cache::load(host, build).ok()?;
    let source = cache
        .get(b"CMAKE_HOME_DIRECTORY")
        .map(|entry| normalized(&entry.value))
        .unwrap_or_default();
    document.build = normalized(&document.build);
    document.source = normalized(&document.source);
    let build = normalized(&path::from_native_cmake(build));
    let freshness = document.freshness(&build, &source, |file| find_hints::stamp(host, file));
    Some((document, freshness))
}

fn reevaluate(
    options: &FindOptions,
    argv0: Option<&BString>,
    host: &Arc<dyn Host>,
) -> Result<Evidence, (String, i32)> {
    let level = FindTraceLevel::Collect(FindTraceLimits::default());
    let request =
        request(options.build.as_ref(), argv0, host, level).map_err(|message| (message, 2))?;
    match run_configure(request) {
        Ok(result) => {
            super::report_partial(host.as_ref(), &result, false);
            Ok(Evidence {
                store: result.facts.find_traces.unwrap_or_default(),
                source: FindSource::Reevaluated,
                stale_reason: None,
                completed: true,
                provisional: result.completion.provisional_outside(
                    super::module_root(host.as_ref(), argv0)
                        .as_deref()
                        .map(|root| root.as_slice()),
                ),
            })
        }
        Err(ApiError::ConfigureFailed {
            find_traces: Some(store),
            ..
        }) => Ok(Evidence {
            store: *store,
            source: FindSource::Reevaluated,
            stale_reason: None,
            completed: false,
            provisional: true,
        }),
        Err(err) => Err((err.to_string(), 1)),
    }
}

pub(super) fn run(argv0: Option<&BString>, options: &FindOptions, host: &Arc<dyn Host>) -> i32 {
    let build = options.build.as_ref().map_or_else(
        || host.initial_cwd(),
        |dir| resolve_cli_path(host.as_ref(), dir),
    );
    let previous = if options.fresh {
        None
    } else {
        saved(host.as_ref(), &build)
    };
    let evidence = match previous {
        Some((document, Freshness::Current)) => Evidence {
            store: document.store,
            source: FindSource::Saved,
            stale_reason: None,
            completed: document.completed,
            provisional: false,
        },
        previous => match reevaluate(options, argv0, host) {
            Ok(evidence) => evidence,
            Err((message, code)) => match previous {
                Some((document, Freshness::Past(reason))) if code != 2 => {
                    host.write_out(
                        Stream::Stderr,
                        format!("rcmake explain: {message} Showing evidence from a past run.\n")
                            .as_bytes(),
                    );
                    Evidence {
                        store: document.store,
                        source: FindSource::Past,
                        stale_reason: Some(reason),
                        completed: document.completed,
                        provisional: true,
                    }
                }
                _ => return error(host.as_ref(), &message, code),
            },
        },
    };
    let mut probe = HostProbe {
        host: host.as_ref(),
        calls: 0,
    };
    let mut report = FindReport::new(
        &options.package,
        &evidence.store.calls,
        options.call,
        &mut probe,
    );
    if let Some(call) = options.call
        && report.calls.is_empty()
        && evidence
            .store
            .calls
            .iter()
            .any(|trace| trace.name == options.package)
    {
        return error(
            host.as_ref(),
            &format!(
                "The project calls find_package({}) fewer than {call} times.",
                options.package
            ),
            2,
        );
    }
    report.source = evidence.source;
    report.stale_reason = evidence.stale_reason;
    report.omitted_calls = evidence.store.omitted_calls;
    report.completed = evidence.completed;
    report.provisional = evidence.provisional;
    if options.json {
        host.write_out(Stream::Stdout, &render_find_json(&report));
    } else {
        let text = render_find_terminal(&report, options.all_candidates);
        host.write_out(Stream::Stdout, text.as_bytes());
    }
    if report.found() { 0 } else { 1 }
}
