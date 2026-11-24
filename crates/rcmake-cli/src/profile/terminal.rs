//! Report the longest operations and phase durations on stderr.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::{ProfileKind, ProfilePhase, State, Totals};

const TOP_COUNT: usize = 10;

/// Format phase durations, slow list files, and slow command call sites.
pub(super) fn render(state: &State, wall: Duration) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Rcmake profile: {:.2} s wall time, {} commands",
        wall.as_secs_f64(),
        state.command_count
    );
    if state.dropped_spans > 0 {
        let _ = writeln!(
            out,
            "Samples omitted from trace and flamegraph: {}",
            state.dropped_spans
        );
    }
    if state.dropped_totals > 0 {
        let _ = writeln!(
            out,
            "Samples omitted from call site totals: {}",
            state.dropped_totals
        );
    }

    let mut phases = BTreeMap::<ProfilePhase, Totals>::new();
    let mut files = BTreeMap::<&[u8], Totals>::new();
    let mut commands = Vec::new();
    for (site, totals) in &state.sites {
        match site.kind {
            ProfileKind::Phase(phase) => phases.entry(phase).or_default().add(*totals),
            ProfileKind::ListFile => {
                let file = if site.file.is_empty() {
                    &site.name
                } else {
                    &site.file
                };
                files.entry(file.as_ref()).or_default().add(*totals);
            }
            ProfileKind::Builtin | ProfileKind::Function | ProfileKind::Macro => {
                commands.push((site, totals))
            }
        }
    }

    out.push_str("\nPhase durations (can overlap)\n");
    for phase in ProfilePhase::ALL {
        let elapsed = phases
            .get(&phase)
            .map_or(Duration::ZERO, |stats| stats.elapsed);
        let _ = writeln!(out, "  {:>8.2} s  {}", elapsed.as_secs_f64(), phase.name());
    }

    if !files.is_empty() {
        out.push_str("\n    Time (s)     Runs  List file\n");
        for (file, totals) in top(
            files.into_iter().collect(),
            |entry| entry.1.elapsed,
            |entry| entry.0,
        ) {
            let _ = writeln!(
                out,
                "    {:>8.2} {:>8}  {}",
                totals.elapsed.as_secs_f64(),
                totals.count,
                display(file)
            );
        }
    }

    if !commands.is_empty() {
        out.push_str("\n    Time (s)    Calls  Kind      Command and source\n");
        for (site, totals) in top(commands, |entry| entry.1.elapsed, |entry| entry.0) {
            let _ = write!(
                out,
                "    {:>8.2} {:>8}  {:<8}  {}",
                totals.elapsed.as_secs_f64(),
                totals.count,
                site.kind.category(),
                display(&site.name)
            );
            if !site.file.is_empty() {
                let _ = write!(out, "  {}:{}", display(&site.file), site.line);
            }
            out.push('\n');
        }
    }
    out
}

fn top<T, K: Ord>(
    mut entries: Vec<T>,
    elapsed: impl Fn(&T) -> Duration,
    key: impl Fn(&T) -> K,
) -> impl Iterator<Item = T> {
    entries.sort_unstable_by(|a, b| {
        elapsed(b)
            .cmp(&elapsed(a))
            .then_with(|| key(a).cmp(&key(b)))
    });
    entries.into_iter().take(TOP_COUNT)
}

fn display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| if ch.is_control() { '\u{fffd}' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use rcmake_host::{Host, HostResult, PureHost, Stream};

    use super::*;
    use crate::profile::{ProfileRecorder, ProfileSample};

    #[test]
    fn sorts_ties_and_escapes_terminal_controls() -> HostResult<()> {
        let host = PureHost::default();
        let recorder = ProfileRecorder::new();
        for name in [b"z\x1b".as_slice(), b"a\n"] {
            recorder.record(ProfileSample {
                name,
                file: b"test.cmake",
                line: 10,
                depth: 0,
                start: Duration::ZERO,
                elapsed: Duration::from_secs(1),
                kind: ProfileKind::Macro,
            });
        }
        recorder.write(
            &host,
            &host.initial_cwd().join("terminal"),
            Duration::from_secs(2),
        )?;
        let report = host.stream_bytes(Stream::Stderr);
        let report = String::from_utf8_lossy(report.as_slice());
        assert!(report.contains("macro     a\u{fffd}  test.cmake:10"));
        assert!(report.contains("macro     z\u{fffd}  test.cmake:10"));
        assert!(report.find("a\u{fffd}") < report.find("z\u{fffd}"));
        assert!(!report.contains('\x1b'));
        Ok(())
    }
}
