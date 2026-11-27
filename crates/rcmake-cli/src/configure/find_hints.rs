//! Native find trace collection, persistence, and trailing `hint:` lines.
//!
//! Only an effective native configure collects or writes anything. Compatibility
//! personas, `RCMAKE_ORACLE_MODE=1`, and `-L` view runs keep every byte unchanged.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_api::{FIND_TRACE_MAX_BYTES, FIND_TRACE_PATH, FileStamp, FindTraceDocument, Freshness};
use rcmake_base::{BString, path_lex::Lexical};
use rcmake_buildinfo::{ArtifactReader, ReadLimits};
use rcmake_eval::find_trace::{FindOutcome, FindPackageTrace, Rejection};
use rcmake_eval::{FindTraceLevel, FindTraceLimits, Interp};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Stream, WriteMode, path};

use crate::options::Options;
use crate::persona::{Persona, oracle_mode};

const MAX_HINT_PACKAGES: usize = 3;
const MAX_INPUT_STAMPS: usize = 4096;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Return the trace level for a configure with these options.
pub(super) fn level(opts: &Options, host: &dyn Host) -> FindTraceLevel {
    if opts.persona == Persona::Native && !oracle_mode(host) && !opts.view {
        FindTraceLevel::Collect(FindTraceLimits::default())
    } else {
        FindTraceLevel::Off
    }
}

/// Return the current state of a path, or `None` when it cannot be checked.
///
/// A missing path gives an absent stamp. A directory stamp keeps only the
/// modification time, because directory sizes differ between file systems.
pub(crate) fn stamp(host: &dyn Host, file: &[u8]) -> Option<FileStamp> {
    let meta = match host.metadata(&path::to_native(file), Follow::Yes) {
        Ok(meta) => meta,
        Err(error) if error.kind == ErrorKind::NotFound => return Some(FileStamp::absent(file)),
        Err(_) => return None,
    };
    let directory = match meta.kind {
        FileKind::File => false,
        FileKind::Dir => true,
        _ => return None,
    };
    Some(FileStamp {
        path: file.into(),
        exists: true,
        directory,
        size: if directory { 0 } else { meta.len },
        mtime_ns: meta
            .mtime
            .secs
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(meta.mtime.nanos)),
    })
}

/// A saved trace and its freshness for the current build tree.
pub(crate) struct SavedTrace {
    /// Parsed document.
    pub(crate) document: FindTraceDocument,
    /// Result of the shared freshness check.
    pub(crate) freshness: Freshness,
}

/// Read the saved trace of `build` through the artifact reader and check it.
///
/// `explain --find` and `diff-config` both use this function, so they apply the
/// same containment, size, schema, and freshness rules. The cache stamp is bound to
/// the `CMakeCache.txt` of `build`. Return `None` when no valid trace exists.
pub(crate) fn load_saved(host: &dyn Host, build: &Path) -> Option<SavedTrace> {
    let limit = u64::try_from(FIND_TRACE_MAX_BYTES).unwrap_or(u64::MAX);
    let mut reader = ArtifactReader::new(
        host,
        build,
        ReadLimits {
            max_file_bytes: limit,
            max_total_bytes: limit,
        },
    )?;
    let bytes = reader
        .read(&path::to_native(FIND_TRACE_PATH.as_bytes()))
        .ok()?;
    let mut document = FindTraceDocument::parse(&bytes)?;
    let root = path::from_native_cmake(reader.root());
    let source = crate::cache::load(host, reader.root())
        .ok()
        .and_then(|cache| {
            cache
                .get(b"CMAKE_HOME_DIRECTORY")
                .map(|entry| Lexical::new(&entry.value).normalize())
        })
        .unwrap_or_default();
    document.build = Lexical::new(&document.build).normalize();
    document.source = Lexical::new(&document.source).normalize();
    let cache = cache_path(&root);
    let freshness = document.freshness(&Lexical::new(&root).normalize(), &source, &cache, |file| {
        stamp(host, file)
    });
    Some(SavedTrace {
        document,
        freshness,
    })
}

fn cache_path(build: &[u8]) -> BString {
    let mut cache = BString::from(build);
    if !cache.ends_with(b"/") {
        cache.push(b'/');
    }
    cache.extend_from_slice(b"CMakeCache.txt");
    cache
}

/// Collect the input stamps of a run. Return the stamps and whether they are complete.
fn input_stamps(
    host: &dyn Host,
    interp: &mut Interp,
    calls: &[FindPackageTrace],
) -> (Vec<FileStamp>, bool) {
    let mut paths = BTreeSet::new();
    let mut complete = true;
    let add = |path: &[u8], paths: &mut BTreeSet<BString>| {
        if paths.len() >= MAX_INPUT_STAMPS {
            return false;
        }
        let trimmed = path
            .strip_suffix(b"/")
            .filter(|rest| !rest.is_empty())
            .unwrap_or(path);
        paths.insert(Lexical::new(trimmed).normalize());
        true
    };
    {
        let cx = interp.cx();
        for (_, dir) in cx.project_ref().dirs() {
            for file in &dir.listfiles {
                complete &= add(file, &mut paths);
            }
        }
    }
    for call in calls {
        complete &= call.candidates_omitted == 0 && call.items_omitted == 0;
        if let Some(selected) = &call.selected {
            complete &= add(&selected.file, &mut paths);
        }
        for candidate in &call.candidates {
            complete &= add(&candidate.path, &mut paths);
        }
    }
    let mut stamps = Vec::with_capacity(paths.len());
    for file in &paths {
        match stamp(host, file) {
            Some(stamp) => stamps.push(stamp),
            None => complete = false,
        }
    }
    (stamps, complete)
}

/// Drain the session's records at the final configure attempt, publish them, and
/// print hints. `ran` reports whether the project listfiles started to run.
///
/// A run that did not start, or a trace that cannot be published, removes the
/// earlier trace. An earlier trace never stands for a newer run.
pub(super) fn finish(
    host: &dyn Host,
    opts: &Options,
    src: &Path,
    bin: &Path,
    interp: &mut Interp,
    ran: bool,
) {
    if level(opts, host) == FindTraceLevel::Off {
        return;
    }
    if !ran {
        let _ = remove_saved(host, bin);
        return;
    }
    let store = interp.take_find_traces();
    let canonical = host.canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    let build = Lexical::new(&path::from_native_cmake(&canonical)).normalize();
    let source = path::from_native_cmake(src);
    let (inputs, inputs_complete) = input_stamps(host, interp, &store.calls);
    let completed = !interp.fatal_occurred() && interp.exit_code() == 0;
    let document = FindTraceDocument {
        producer: format!("rcmake {}", env!("CARGO_PKG_VERSION")).into(),
        cache: stamp(host, &cache_path(&build)).filter(|stamp| stamp.exists),
        inputs,
        inputs_complete: inputs_complete && store.all_calls_kept(),
        build,
        source,
        completed,
        store,
    };
    if publish(host, bin, &document.to_bytes()).is_err() {
        let _ = remove_saved(host, bin);
    }
    if host
        .env_get(b"RCMAKE_NO_HINTS")
        .is_some_and(|value| value.as_slice() == b"1")
    {
        return;
    }
    let text = hints(&document.store.calls, &display_build(host, bin));
    if !text.is_empty() {
        host.write_out(Stream::Stderr, text.as_bytes());
    }
}

/// Remove the saved trace of `bin`. Links in the trace path are not followed.
fn remove_saved(host: &dyn Host, bin: &Path) -> HostResult<()> {
    let mut directory = host.canonicalize(bin)?;
    for part in ["CMakeFiles", "rcmake"] {
        directory.push(part);
        match host.metadata(&directory, Follow::No) {
            Ok(meta) if meta.kind == FileKind::Dir => {}
            Ok(_) => {
                return Err(HostError::unsupported(
                    "The trace path must be a directory.",
                ));
            }
            Err(error) if error.kind == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    let target = directory.join("find-trace.v1.json");
    match host.remove_file(&target) {
        Err(error) if error.kind != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn display_build(host: &dyn Host, bin: &Path) -> String {
    let cwd = host.initial_cwd();
    let shown = bin.strip_prefix(&cwd).unwrap_or(bin);
    let shown = if shown.as_os_str().is_empty() {
        Path::new(".")
    } else {
        shown
    };
    String::from_utf8_lossy(&path::from_native_cmake(shown)).into_owned()
}

/// Render at most two hint lines for each of the first three failed calls.
pub(super) fn hints(calls: &[FindPackageTrace], build: &str) -> String {
    let mut out = String::new();
    let mut shown = BTreeSet::new();
    for call in calls {
        if shown.len() >= MAX_HINT_PACKAGES {
            break;
        }
        if call.outcome != FindOutcome::NotFound
            || !(call.request.required || !call.request.quiet)
            || !shown.insert(call.name.clone())
        {
            continue;
        }
        let name = String::from_utf8_lossy(&call.name);
        let version = call
            .request
            .version
            .as_ref()
            .map(|version| format!(" {}", String::from_utf8_lossy(version)))
            .unwrap_or_default();
        let rejected = call
            .candidates
            .iter()
            .find(|candidate| candidate.reason != Rejection::NoFile);
        let detail = match rejected {
            Some(candidate) => {
                let why = match (&candidate.reason, &candidate.found_version) {
                    (Rejection::VersionMismatch | Rejection::VersionUnsuitable, Some(found)) => {
                        format!("version {}", String::from_utf8_lossy(found))
                    }
                    (reason, _) => reason.label().replace('_', " "),
                };
                format!(
                    " Rejected {} ({why}).",
                    String::from_utf8_lossy(&candidate.path)
                )
            }
            None => String::new(),
        };
        out.push_str(&format!("hint: {name}{version} not found.{detail}\n"));
        let quoted = if build.contains(' ') {
            format!("\"{build}\"")
        } else {
            build.to_owned()
        };
        out.push_str(&format!(
            "hint: Run `rcmake explain --find {name} -B {quoted}` for the full search.\n"
        ));
    }
    out
}

fn publish(host: &dyn Host, build: &Path, bytes: &[u8]) -> HostResult<()> {
    let mut directory = host.canonicalize(build)?;
    for part in ["CMakeFiles", "rcmake"] {
        directory.push(part);
        match host.metadata(&directory, Follow::No) {
            Ok(meta) if meta.kind == FileKind::Dir => {}
            Ok(_) => {
                return Err(HostError::unsupported(
                    "The trace path must be a directory.",
                ));
            }
            Err(error) if error.kind == ErrorKind::NotFound => host.create_dir_all(&directory)?,
            Err(error) => return Err(error),
        }
    }
    let target = build.join(path::to_native(FIND_TRACE_PATH.as_bytes()));
    let pid = host.system().pid;
    for _ in 0..32 {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = directory.join(format!(".find-trace.v1.{pid}.{sequence}.tmp"));
        match host.write(&temp, bytes, WriteMode::CreateNew) {
            Ok(()) => {
                let result = host.rename(&temp, &target);
                if result.is_err() {
                    let _ = host.remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(HostError::unsupported(
        "Cannot reserve a temporary trace file.",
    ))
}

#[cfg(test)]
mod tests {
    use rcmake_eval::find_trace::{FindCandidate, FindMode, FindRequestSummary};

    use super::*;

    fn call(name: &str, outcome: FindOutcome, quiet: bool) -> FindPackageTrace {
        FindPackageTrace {
            ordinal: 1,
            package_call: Some(1),
            items_omitted: 0,
            name: name.into(),
            call: Vec::new(),
            request: FindRequestSummary {
                version: Some("1.84".into()),
                quiet,
                ..FindRequestSummary::default()
            },
            roots: Vec::new(),
            root_path_mode: "BOTH".into(),
            candidates: vec![FindCandidate {
                path: "/opt/BoostConfig.cmake".into(),
                mode: FindMode::Config,
                reason: Rejection::VersionMismatch,
                found_version: Some("1.83.0".into()),
                missing_components: Vec::new(),
                detail: None,
            }],
            candidates_omitted: 0,
            fields_cut: 0,
            selected: None,
            outcome,
            version: None,
        }
    }

    #[test]
    fn hints_cover_failed_visible_calls_only() {
        let calls = [
            call("Boost", FindOutcome::NotFound, false),
            call("Quiet", FindOutcome::NotFound, true),
            call("Fine", FindOutcome::Found, false),
            call("Boost", FindOutcome::NotFound, false),
        ];
        assert_eq!(
            hints(&calls, "build"),
            "hint: Boost 1.84 not found. Rejected /opt/BoostConfig.cmake (version 1.83.0).\n\
             hint: Run `rcmake explain --find Boost -B build` for the full search.\n"
        );
    }

    #[test]
    fn only_an_effective_native_configure_collects() {
        let host = rcmake_host::PureHost::default();
        let native = Options {
            persona: Persona::Native,
            ..Options::default()
        };
        assert!(matches!(level(&native, &host), FindTraceLevel::Collect(_)));
        let compat = Options {
            persona: Persona::Compat,
            ..Options::default()
        };
        assert_eq!(level(&compat, &host), FindTraceLevel::Off);
        let view = Options {
            persona: Persona::Native,
            view: true,
            ..Options::default()
        };
        assert_eq!(level(&view, &host), FindTraceLevel::Off);
        host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
        assert_eq!(level(&native, &host), FindTraceLevel::Off);
    }

    #[test]
    fn hints_stop_after_three_packages() {
        let calls: Vec<_> = ["A", "B", "C", "D"]
            .into_iter()
            .map(|name| call(name, FindOutcome::NotFound, false))
            .collect();
        assert_eq!(hints(&calls, "b").lines().count(), 6);
    }

    fn configure(host: &std::sync::Arc<rcmake_host::PureHost>, argv0: &str) -> i32 {
        let argv: Vec<rcmake_base::BString> = [argv0, "-S", ".", "-B", "out", "-G", "Ninja"]
            .into_iter()
            .map(rcmake_base::BString::from)
            .collect();
        crate::run(&argv, host.clone())
    }

    fn freshness(host: &rcmake_host::PureHost) -> Option<Freshness> {
        load_saved(host, Path::new("/work/out")).map(|saved| saved.freshness)
    }

    fn widget_project(version: &str) -> String {
        format!(
            "cmake_minimum_required(VERSION 3.20)\nproject(example NONE)\nunset(Widget_DIR CACHE)\nfind_package(Widget {version} CONFIG QUIET NO_DEFAULT_PATH PATHS /empty /pkg)\n"
        )
    }

    #[test]
    fn saved_trace_freshness_follows_listfiles_search_paths_and_cache() {
        let host = crate::driver_tests::project_host();
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            widget_project("1.0").as_bytes(),
        )
        .expect("seed");
        host.seed_file(
            Path::new("/pkg/WidgetConfig.cmake"),
            b"set(Widget_VERSION 1.0)\n",
        )
        .expect("seed");
        assert_eq!(
            configure(&host, "rcmake"),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        assert_eq!(freshness(&host), Some(Freshness::Current));

        // A listfile edit leaves every selected package file untouched.
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            widget_project("2.0").as_bytes(),
        )
        .expect("seed");
        assert!(matches!(freshness(&host), Some(Freshness::Past(_))));
        assert_eq!(configure(&host, "rcmake"), 0);
        assert_eq!(freshness(&host), Some(Freshness::Current));

        // A package in a formerly empty search location changes the search.
        host.seed_file(
            Path::new("/empty/WidgetConfig.cmake"),
            b"set(Widget_VERSION 3.0)\n",
        )
        .expect("seed");
        assert!(matches!(freshness(&host), Some(Freshness::Past(_))));
        assert_eq!(configure(&host, "rcmake"), 0);
        assert_eq!(freshness(&host), Some(Freshness::Current));

        // A compatibility configure that changes the cache never touches the trace.
        let argv: Vec<rcmake_base::BString> =
            ["cmake", "-S", ".", "-B", "out", "-G", "Ninja", "-DMARK=1"]
                .into_iter()
                .map(rcmake_base::BString::from)
                .collect();
        assert_eq!(crate::run(&argv, host.clone()), 0);
        assert!(matches!(freshness(&host), Some(Freshness::Past(_))));
    }

    #[test]
    fn a_run_that_does_not_start_removes_the_earlier_trace() {
        let host = crate::driver_tests::project_host();
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            widget_project("1.0").as_bytes(),
        )
        .expect("seed");
        assert_eq!(configure(&host, "rcmake"), 0);
        assert!(freshness(&host).is_some());
        let options = Options {
            persona: Persona::Native,
            ..Options::default()
        };
        let mut interp = rcmake_eval::Interp::new(
            rcmake_eval::InterpConfig::new(
                host.clone(),
                rcmake_eval::RunMode::Configure,
                "/share/cmake",
                "/bin/rcmake",
                "/work",
            ),
            |_| {},
        );
        finish(
            host.as_ref(),
            &options,
            Path::new("/work"),
            Path::new("/work/out"),
            &mut interp,
            false,
        );
        assert!(freshness(&host).is_none());
    }
}
