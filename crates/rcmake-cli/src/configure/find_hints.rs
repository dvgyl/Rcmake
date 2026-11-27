//! Native find trace collection, persistence, and trailing `hint:` lines.
//!
//! Only an effective native configure collects or writes anything. Compatibility
//! personas, `RCMAKE_ORACLE_MODE=1`, and `-L` view runs keep every byte unchanged.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_api::{FIND_TRACE_PATH, FileStamp, FindTraceDocument};
use rcmake_eval::find_trace::{FindOutcome, FindPackageTrace, Rejection};
use rcmake_eval::{FindTraceLevel, FindTraceLimits, Interp};
use rcmake_host::{FileKind, Follow, Host, HostError, HostResult, Stream, WriteMode, path};

use crate::options::Options;
use crate::persona::{Persona, oracle_mode};

const MAX_HINT_PACKAGES: usize = 3;
const MAX_INPUT_STAMPS: usize = 256;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Return the trace level for a configure with these options.
pub(super) fn level(opts: &Options, host: &dyn Host) -> FindTraceLevel {
    if opts.persona == Persona::Native && !oracle_mode(host) && !opts.view {
        FindTraceLevel::Collect(FindTraceLimits::default())
    } else {
        FindTraceLevel::Off
    }
}

/// Return the stamp of a file, or `None` when it cannot be read.
pub(crate) fn stamp(host: &dyn Host, file: &[u8]) -> Option<FileStamp> {
    let meta = host.metadata(&path::to_native(file), Follow::Yes).ok()?;
    (meta.kind == FileKind::File).then(|| FileStamp {
        path: file.into(),
        size: meta.len,
        mtime_ns: meta
            .mtime
            .secs
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(meta.mtime.nanos)),
    })
}

/// Drain the session's records at the final configure attempt, publish them, and
/// print hints. `ran` reports whether the project listfiles started to run.
pub(super) fn finish(
    host: &dyn Host,
    opts: &Options,
    src: &Path,
    bin: &Path,
    interp: &mut Interp,
    ran: bool,
) {
    if level(opts, host) == FindTraceLevel::Off || !ran {
        return;
    }
    let store = interp.take_find_traces();
    let build = path::from_native_cmake(bin);
    let source = path::from_native_cmake(src);
    let mut inputs = BTreeSet::new();
    for call in &store.calls {
        if let Some(selected) = &call.selected
            && inputs.len() < MAX_INPUT_STAMPS
        {
            inputs.insert(selected.file.clone());
        }
    }
    let completed = !interp.fatal_occurred() && interp.exit_code() == 0;
    let mut cache = build.clone();
    cache.extend_from_slice(b"/CMakeCache.txt");
    let document = FindTraceDocument {
        producer: format!("rcmake {}", env!("CARGO_PKG_VERSION")).into(),
        cache: stamp(host, &cache),
        inputs: inputs.iter().filter_map(|file| stamp(host, file)).collect(),
        build,
        source,
        completed,
        store,
    };
    let _ = publish(host, bin, &document.to_bytes());
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
}
