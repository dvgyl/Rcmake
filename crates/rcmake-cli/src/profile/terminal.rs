//! Report phase, file, command, and category timing on stderr.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use super::{Coverage, ProfileKind, ProfilePhase, Site, Span, State};

const TOP_COUNT: usize = 10;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Category {
    Check,
    Compile,
    Run,
    Find,
    Process,
    Download,
}

impl Category {
    fn of(site: &Site) -> Option<Self> {
        if !site.kind.is_command() {
            return None;
        }
        let name = site.name.as_ref();
        let starts_with = |prefix: &[u8]| {
            name.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        };
        if starts_with(b"check_") {
            return Some(Self::Check);
        }
        if starts_with(b"fetchcontent_") {
            return Some(Self::Download);
        }
        [
            (b"try_compile".as_slice(), Self::Compile),
            (b"try_run", Self::Run),
            (b"find_package", Self::Find),
            (b"execute_process", Self::Process),
            (b"download", Self::Download),
            (b"file(download)", Self::Download),
            (b"externalproject_add_step_download", Self::Download),
        ]
        .into_iter()
        .find_map(|(command, category)| name.eq_ignore_ascii_case(command).then_some(category))
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Check => "check_*",
            Self::Compile => "try_compile",
            Self::Run => "try_run",
            Self::Find => "find_package",
            Self::Process => "execute_process",
            Self::Download => "FetchContent / downloads",
        }
    }
}

#[derive(Default)]
struct Timing {
    inclusive: Coverage,
    self_time: Duration,
    calls: usize,
    measured: bool,
}

impl Timing {
    fn add_span(&mut self, span: &Span) {
        self.inclusive.add(span.start, span.end());
        self.self_time = self.self_time.saturating_add(span.self_time);
        self.measured = true;
    }
}

#[derive(Default)]
struct Summary<'a> {
    phases: BTreeMap<ProfilePhase, Timing>,
    files: BTreeMap<&'a [u8], Timing>,
    commands: BTreeMap<&'a Site, Timing>,
    categories: BTreeMap<Category, Timing>,
}

impl<'a> Summary<'a> {
    fn rows(&mut self, site: &'a Site) -> impl Iterator<Item = &mut Timing> {
        let (row, category) = match site.kind {
            ProfileKind::Phase(phase) => (self.phases.entry(phase).or_default(), None),
            ProfileKind::ListFile => {
                let file = if site.file.is_empty() {
                    &site.name
                } else {
                    &site.file
                };
                (self.files.entry(file.as_ref()).or_default(), None)
            }
            _ => (
                self.commands.entry(site).or_default(),
                Category::of(site).map(|kind| self.categories.entry(kind).or_default()),
            ),
        };
        std::iter::once(row).chain(category)
    }
}

/// Format inclusive wall time, self time, and call counts for measured operations.
pub(super) fn render(state: &State, spans: &[Span], wall: Duration, total: Duration) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Rcmake profile: {} invocation wall time, {} commands",
        duration(wall),
        state.command_count,
    );
    if state.dropped_spans > 0 {
        let _ = writeln!(
            out,
            "Omitted samples at the recording limit ({}). Timing uses the retained samples.",
            state.dropped_spans
        );
    }
    if state.dropped_totals > 0 {
        let _ = writeln!(
            out,
            "The call site limit omitted {} calls from the breakdown.",
            state.dropped_totals
        );
    }
    if state.clipped_labels > 0 {
        let _ = writeln!(
            out,
            "The label limit replaced {} names or paths.",
            state.clipped_labels
        );
    }
    let _ = writeln!(
        out,
        "% of total uses {} of recorded wall time. Self time is summed across calls.",
        duration(total)
    );

    let mut summary = Summary::default();
    for (site, &count) in &state.sites {
        for row in summary.rows(site) {
            row.calls = row.calls.saturating_add(count);
        }
    }
    for span in spans {
        if let Some((site, _)) = state.sites.get_key_value(&span.site) {
            for row in summary.rows(site) {
                row.add_span(span);
            }
        }
    }

    heading(&mut out, "Phase", "Phases can overlap");
    for phase in ProfilePhase::ALL {
        let name = phase.name();
        let stats = summary.phases.get(&phase);
        row(&mut out, total, name, stats);
    }

    if !summary.categories.is_empty() {
        heading(&mut out, "Category", "Command categories");
        for (category, stats) in summary.categories {
            row(&mut out, total, category.name(), Some(&stats));
        }
    }
    if !summary.files.is_empty() {
        heading(&mut out, "List file", "Top list files by self time");
        for (file, stats) in top(summary.files, |stats| stats.self_time) {
            row(&mut out, total, &display(file), Some(&stats));
        }
    }
    if !summary.commands.is_empty() {
        heading(
            &mut out,
            "Command and source",
            "Top command call sites by inclusive time",
        );
        for (site, stats) in top(summary.commands, |stats| stats.inclusive.elapsed) {
            let mut label = format!("{:<8}  {}", site.kind.category(), display(&site.name));
            if !site.file.is_empty() {
                let _ = write!(label, "  {}:{}", display(&site.file), site.line);
            }
            row(&mut out, total, &label, Some(&stats));
        }
    }
    out
}

fn heading(out: &mut String, name: &str, title: &str) {
    let _ = writeln!(
        out,
        "\n{title}\n{:>11} {:>11} {:>10} {:>8}  {name}",
        "Inclusive", "Self", "% of total", "Calls",
    );
}

fn row(out: &mut String, total: Duration, name: &str, timing: Option<&Timing>) {
    let missing = || "—".to_owned();
    let calls = timing.map_or_else(missing, |stats| stats.calls.to_string());
    let measured = timing.filter(|stats| stats.measured);
    let inclusive = measured.map_or_else(missing, |stats| duration(stats.inclusive.elapsed));
    let self_time = measured.map_or_else(missing, |stats| duration(stats.self_time));
    let percent = measured
        .filter(|_| !total.is_zero())
        .map_or_else(missing, |stats| {
            format!(
                "{:.1}%",
                100.0 * stats.inclusive.elapsed.as_secs_f64() / total.as_secs_f64()
            )
        });
    let _ = writeln!(
        out,
        "{inclusive:>11} {self_time:>11} {percent:>10} {calls:>8}  {name}"
    );
}

fn top<K: Ord>(
    timings: BTreeMap<K, Timing>,
    elapsed: impl Fn(&Timing) -> Duration,
) -> impl Iterator<Item = (K, Timing)> {
    let mut entries: Vec<_> = timings.into_iter().collect();
    entries.sort_unstable_by(|(a, a_time), (b, b_time)| {
        b_time
            .measured
            .cmp(&a_time.measured)
            .then_with(|| elapsed(b_time).cmp(&elapsed(a_time)))
            .then_with(|| a.cmp(b))
    });
    entries.into_iter().take(TOP_COUNT)
}

fn duration(elapsed: Duration) -> String {
    if elapsed >= Duration::from_secs(1) {
        format!("{:.2} s", elapsed.as_secs_f64())
    } else if !elapsed.is_zero() && elapsed < Duration::from_micros(50) {
        "<0.1 ms".to_owned()
    } else {
        format!("{:.1} ms", elapsed.as_secs_f64() * 1000.0)
    }
}

/// Format a filesystem path without terminal control characters.
pub(super) fn display_path(path: &Path) -> String {
    display(path.to_string_lossy().as_bytes())
}

fn display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| if ch.is_control() { '\u{fffd}' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{ProfileRecorder, ProfileSample, resolve_spans};

    #[test]
    fn reports_subsecond_operations_and_unmeasured_phases() -> Result<(), &'static str> {
        let recorder = ProfileRecorder::new();
        for name in [b"z\x1b".as_slice(), b"a\n", b"check_type_size"] {
            recorder.record(ProfileSample {
                name,
                file: b"test.cmake",
                line: 10,
                depth: 0,
                start: Duration::ZERO,
                elapsed: Duration::from_micros(4200),
                kind: ProfileKind::Macro,
            });
        }
        let state = recorder
            .state
            .lock()
            .map_err(|_| "Cannot read the profile samples.")?;
        let mut spans = state.spans.clone().into_sorted_vec();
        let coverage = resolve_spans(&mut spans);
        let report = render(&state, &spans, Duration::from_secs(2), coverage);
        assert!(
            report
                .lines()
                .any(|line| line.contains('—') && line.ends_with("eval"))
        );
        assert!(report.contains("4.2 ms of recorded wall time"));
        assert!(report.contains("100.0%"));
        assert!(report.contains("check_*"));
        assert!(report.contains("macro     a\u{fffd}  test.cmake:10"));
        assert!(report.contains("macro     z\u{fffd}  test.cmake:10"));
        assert!(report.find("a\u{fffd}") < report.find("z\u{fffd}"));
        assert!(!report.contains('\x1b'));
        Ok(())
    }
}
