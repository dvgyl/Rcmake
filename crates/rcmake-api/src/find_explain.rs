//! Report, suggestions, and output for `rcmake explain --find <package>`.

use std::fmt::Write as _;

use rcmake_base::BString;
use rcmake_eval::find_trace::{FindMode, FindOutcome, FindPackageTrace, Rejection};
use rcmake_json::Value;

use crate::find_trace_json::call_value;

/// Schema of `rcmake explain --find --json`.
pub const EXPLAIN_FIND_SCHEMA: &str = "rcmake-explain-find-v1";
const MAX_SUGGESTIONS: usize = 3;

/// Origin of the records in a find report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindSource {
    /// Saved by the last native configure, and its stamps match the tree.
    Saved,
    /// Saved by a past run whose stamps do not match the tree.
    Past,
    /// Produced by a new overlay evaluation in `rcmake`.
    Reevaluated,
}

impl FindSource {
    /// Stable name used in reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Saved => "saved",
            Self::Past => "past",
            Self::Reevaluated => "re-evaluated",
        }
    }
}

/// Kind of one suggestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    /// Set `<Pkg>_DIR` to a directory with a package file.
    SetDir,
    /// Add a prefix to `CMAKE_PREFIX_PATH`.
    AddPrefix,
    /// Remove a cached `<Pkg>_DIR` that names no package file.
    ClearStaleDir,
    /// Install missing components.
    InstallComponents,
    /// Accept the found version or install the requested one.
    RelaxVersion,
    /// Use a package built for this architecture.
    MatchArch,
    /// Remove `MODULE` or `CONFIG` from the call.
    ChangeMode,
    /// Enable a disabled search root.
    ReenableRoot,
}

impl SuggestionKind {
    /// Stable name used in reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SetDir => "set_dir",
            Self::AddPrefix => "add_prefix",
            Self::ClearStaleDir => "clear_stale_dir",
            Self::InstallComponents => "install_components",
            Self::RelaxVersion => "relax_version",
            Self::MatchArch => "match_arch",
            Self::ChangeMode => "change_mode",
            Self::ReenableRoot => "reenable_root",
        }
    }
}

/// One suggested change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindSuggestion {
    /// Kind of change.
    pub kind: SuggestionKind,
    /// Exact command-line text, for example `-DBoost_DIR=/opt/x`.
    pub define: Option<BString>,
    /// One short sentence.
    pub reason: BString,
}

/// Bounded read-only file checks for near-miss suggestions.
pub trait NearMissProbe {
    /// Return the names in a directory. Implementations cap names and calls.
    fn list_dir(&mut self, path: &[u8]) -> Vec<BString>;
    /// Return an environment value.
    fn env(&mut self, name: &[u8]) -> Option<BString>;
}

/// Report for one package.
#[derive(Clone, Debug)]
pub struct FindReport {
    /// Requested package name.
    pub package: BString,
    /// Origin of the records.
    pub source: FindSource,
    /// Why saved records are evidence from a past run, when they are.
    pub stale_reason: Option<&'static str>,
    /// Calls for the package, in call order, with their suggestions.
    pub calls: Vec<(FindPackageTrace, Vec<FindSuggestion>)>,
    /// Calls of the run that the record limits left out. Their packages are unknown.
    pub omitted_calls: u32,
    /// The configure ran to its end. Calls after a failure are unknown.
    pub completed: bool,
    /// The evaluation was partial, for example because a subprocess was denied.
    pub provisional: bool,
}

impl FindReport {
    /// Build the report from all calls of a run. `call` selects one 1-based call.
    pub fn new(
        package: &[u8],
        calls: &[FindPackageTrace],
        call: Option<usize>,
        probe: &mut dyn NearMissProbe,
    ) -> Self {
        let matching = calls
            .iter()
            .filter(|trace| trace.name == package)
            .enumerate()
            .filter(|(index, _)| call.is_none_or(|wanted| wanted == index + 1))
            .map(|(_, trace)| (trace.clone(), suggest(trace, probe)))
            .collect();
        Self {
            package: package.into(),
            source: FindSource::Saved,
            stale_reason: None,
            calls: matching,
            omitted_calls: 0,
            completed: true,
            provisional: false,
        }
    }

    /// Return whether the last selected call found the package.
    #[must_use]
    pub fn found(&self) -> bool {
        self.calls
            .last()
            .is_some_and(|(trace, _)| trace.outcome == FindOutcome::Found)
    }

    /// Return whether the absence of a call can mean an unrecorded call.
    #[must_use]
    pub fn absence_unknown(&self) -> bool {
        self.calls.is_empty() && (self.omitted_calls > 0 || !self.completed)
    }
}

fn dir_of(path: &[u8]) -> &[u8] {
    path.iter()
        .rposition(|&byte| byte == b'/')
        .and_then(|end| path.get(..end))
        .unwrap_or(path)
}

fn cat(parts: &[&[u8]]) -> BString {
    let mut out = BString::default();
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// Suggest at most three changes for a failed call.
pub fn suggest(trace: &FindPackageTrace, probe: &mut dyn NearMissProbe) -> Vec<FindSuggestion> {
    let mut out = Vec::new();
    if matches!(trace.outcome, FindOutcome::Found | FindOutcome::Disabled) {
        return out;
    }
    let name = trace.name.as_slice();
    let dir_var = trace.request.dir_var.as_slice();
    let rejected = |reason| trace.candidates.iter().find(|c| c.reason == reason);

    if let Some(before) = &trace.request.dir_value_before
        && !before.ends_with(b"-NOTFOUND")
        && before.as_slice() != b"NOTFOUND"
        && trace.candidates.iter().any(|candidate| {
            candidate.reason == Rejection::NoFile && dir_of(&candidate.path) == before.as_slice()
        })
    {
        out.push(FindSuggestion {
            kind: SuggestionKind::ClearStaleDir,
            define: Some(cat(&[b"-U ", dir_var])),
            reason: cat(&[dir_var, b" names ", before, b", which has no package file."]),
        });
    }
    if let Some(candidate) = rejected(Rejection::VersionMismatch) {
        let found = candidate
            .found_version
            .as_deref()
            .map_or(b"an unknown version".as_slice(), |v| v.as_slice());
        let wanted = trace
            .request
            .version
            .as_deref()
            .map_or(b"another version".as_slice(), |v| v.as_slice());
        out.push(FindSuggestion {
            kind: SuggestionKind::RelaxVersion,
            define: Some(cat(&[b"-D", dir_var, b"=", dir_of(&candidate.path)])),
            reason: cat(&[
                b"Found ",
                found,
                b" at ",
                &candidate.path,
                b". The call requires ",
                wanted,
                b". Use this only if that version is acceptable.",
            ]),
        });
    }
    if let Some(candidate) = rejected(Rejection::VersionUnsuitable) {
        let found = candidate
            .found_version
            .as_deref()
            .map_or(b"unknown".as_slice(), |v| v.as_slice());
        out.push(FindSuggestion {
            kind: SuggestionKind::MatchArch,
            define: None,
            reason: cat(&[
                b"The package at ",
                &candidate.path,
                b" reports ",
                found,
                b" and is unsuitable for this build. Install a build for the target architecture.",
            ]),
        });
    }
    if let Some(candidate) = rejected(Rejection::ComponentsMissing) {
        let missing: Vec<&[u8]> = candidate
            .missing_components
            .iter()
            .map(|c| c.as_slice())
            .collect();
        let missing = if missing.is_empty() {
            b"the requested components".to_vec()
        } else {
            missing.join(b", ".as_slice())
        };
        out.push(FindSuggestion {
            kind: SuggestionKind::InstallComponents,
            define: None,
            reason: cat(&[
                b"Install the missing components of ",
                name,
                b": ",
                &missing,
                b".",
            ]),
        });
    }
    let disabled: Vec<&[u8]> = trace
        .roots
        .iter()
        .filter(|root| !root.enabled)
        .map(|root| root.category.as_slice())
        .collect();
    let any_file = trace.candidates.iter().any(|candidate| {
        candidate.reason != Rejection::NoFile && candidate.reason != Rejection::Ignored
    });
    if !any_file {
        let hits = near_miss(trace, probe);
        if trace.request.mode == Some(FindMode::Module) && !hits.is_empty() {
            out.push(FindSuggestion {
                kind: SuggestionKind::ChangeMode,
                define: None,
                reason: cat(&[
                    b"No Find",
                    name,
                    b".cmake module exists, but a package file does. Remove MODULE from the call.",
                ]),
            });
        }
        match hits.first() {
            Some((prefix, dir)) => {
                out.push(FindSuggestion {
                    kind: SuggestionKind::SetDir,
                    define: Some(cat(&[b"-D", dir_var, b"=", dir])),
                    reason: cat(&[b"A package file for ", name, b" exists in ", dir, b"."]),
                });
                out.push(FindSuggestion {
                    kind: SuggestionKind::AddPrefix,
                    define: Some(cat(&[b"-DCMAKE_PREFIX_PATH=", prefix])),
                    reason: cat(&[b"Add the install prefix ", prefix, b" to the search."]),
                });
            }
            None => {
                let files: Vec<&[u8]> = trace
                    .request
                    .config_files
                    .iter()
                    .map(|f| f.as_slice())
                    .collect();
                out.push(FindSuggestion {
                    kind: SuggestionKind::AddPrefix,
                    define: Some(cat(&[
                        b"-DCMAKE_PREFIX_PATH=<install-prefix-of-",
                        name,
                        b">",
                    ])),
                    reason: cat(&[
                        b"Install ",
                        name,
                        b", then add its prefix. The search looked for ",
                        &files.join(b", ".as_slice()),
                        b".",
                    ]),
                });
            }
        }
        if !disabled.is_empty() {
            out.push(FindSuggestion {
                kind: SuggestionKind::ReenableRoot,
                define: None,
                reason: cat(&[
                    b"These search roots were disabled: ",
                    &disabled.join(b", ".as_slice()),
                    b".",
                ]),
            });
        }
    }
    out.truncate(MAX_SUGGESTIONS);
    out
}

fn near_miss(trace: &FindPackageTrace, probe: &mut dyn NearMissProbe) -> Vec<(BString, BString)> {
    let name = trace.name.as_slice();
    let lower = name.to_ascii_lowercase();
    let mut prefixes: Vec<BString> = Vec::new();
    if let Some(paths) = probe.env(b"CMAKE_PREFIX_PATH") {
        let separator = if paths.contains(&b';') { b';' } else { b':' };
        prefixes.extend(
            paths
                .split(|&byte| byte == separator)
                .filter(|p| !p.is_empty())
                .map(BString::from),
        );
    }
    if let Some(root) = probe.env(b"VCPKG_ROOT") {
        let installed = cat(&[&root, b"/installed"]);
        for triplet in probe.list_dir(&installed) {
            prefixes.push(cat(&[&installed, b"/", &triplet]));
        }
    }
    prefixes.extend(probe.env(b"CONDA_PREFIX"));
    prefixes.push(
        probe
            .env(b"HOMEBREW_PREFIX")
            .unwrap_or_else(|| "/opt/homebrew".into()),
    );
    prefixes.push("/usr/local".into());
    prefixes.push(cat(&[b"/opt/", &lower]));
    let mut hits = Vec::new();
    for prefix in prefixes {
        for (parent, child) in [
            (cat(&[&prefix, b"/lib/cmake"]), None),
            (cat(&[&prefix, b"/lib64/cmake"]), None),
            (cat(&[&prefix, b"/share"]), Some(b"/cmake".as_slice())),
        ] {
            let wanted: &[u8] = if child.is_some() { &lower } else { name };
            for entry in probe.list_dir(&parent) {
                let starts = if child.is_some() {
                    entry.to_ascii_lowercase().starts_with(wanted)
                } else {
                    entry.starts_with(wanted)
                };
                if starts {
                    let dir = cat(&[&parent, b"/", &entry, child.unwrap_or_default()]);
                    let files = probe.list_dir(&dir);
                    if trace
                        .request
                        .config_files
                        .iter()
                        .any(|file| files.iter().any(|name| name == file))
                    {
                        hits.push((prefix.clone(), dir));
                    }
                }
            }
        }
        if !hits.is_empty() {
            break;
        }
    }
    hits
}

fn display(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn request_text(trace: &FindPackageTrace) -> String {
    let request = &trace.request;
    let mut text = format!("find_package({}", display(&trace.name));
    if let Some(version) = &request.version {
        let _ = write!(text, " {}", display(version));
    }
    if request.exact {
        text.push_str(" EXACT");
    }
    match request.mode {
        Some(FindMode::Module) => text.push_str(" MODULE"),
        Some(FindMode::Config) => text.push_str(" CONFIG"),
        _ => {}
    }
    if request.quiet {
        text.push_str(" QUIET");
    }
    if request.required {
        text.push_str(" REQUIRED");
    }
    if !request.components.is_empty() {
        let names: Vec<String> = request.components.iter().map(|c| display(c)).collect();
        let _ = write!(text, " COMPONENTS {}", names.join(" "));
    }
    text.push(')');
    text
}

fn candidate_reason(
    candidate: &rcmake_eval::find_trace::FindCandidate,
    trace: &FindPackageTrace,
) -> String {
    let version = candidate.found_version.as_deref().map(|v| display(v));
    match candidate.reason {
        Rejection::VersionMismatch => format!(
            "version {}, requires {}",
            version.unwrap_or_else(|| "unknown".to_owned()),
            trace
                .request
                .version
                .as_deref()
                .map_or_else(|| "another version".to_owned(), |v| display(v))
        ),
        Rejection::VersionUnsuitable => format!(
            "version {} is unsuitable for this build",
            version.unwrap_or_else(|| "unknown".to_owned())
        ),
        Rejection::ComponentsMissing => {
            let names: Vec<String> = candidate
                .missing_components
                .iter()
                .map(|c| display(c))
                .collect();
            format!("missing components {}", names.join(", "))
        }
        Rejection::Ignored => "ignored by CMAKE_IGNORE_PATH or CMAKE_IGNORE_PREFIX_PATH".to_owned(),
        Rejection::NotFound => candidate.detail.as_deref().map_or_else(
            || "the package file reported the package as not found".to_owned(),
            |detail| format!("the package reported: {}", display(detail)),
        ),
        Rejection::Error => "the package file failed".to_owned(),
        Rejection::NoFile => "no package file".to_owned(),
    }
}

/// Render the report for a terminal.
#[must_use]
pub fn render_find_terminal(report: &FindReport, all_candidates: bool) -> String {
    let mut out = String::new();
    let package = display(&report.package);
    if report.calls.is_empty() {
        if report.absence_unknown() {
            let _ = writeln!(
                out,
                "No record of find_package({package}). The record of this run is incomplete, so the project can still call it."
            );
        } else {
            let _ = writeln!(out, "The project never called find_package({package}).");
        }
    }
    for (trace, suggestions) in &report.calls {
        let state = match trace.outcome {
            FindOutcome::Found => "was found",
            FindOutcome::NotFound => "was not found",
            FindOutcome::Disabled => "was disabled by CMAKE_DISABLE_FIND_PACKAGE",
            FindOutcome::Error => "failed with an error",
        };
        let site = trace.call.first().map_or_else(String::new, |frame| {
            format!("  {}:{}", display(&frame.file), frame.line)
        });
        let _ = writeln!(out, "{package} {state}.{site}  {}", request_text(trace));
        if let Some(selected) = &trace.selected {
            let _ = writeln!(
                out,
                "  Selected {} ({})",
                display(&selected.file),
                selected.mode.label()
            );
        }
        if let Some(version) = &trace.version {
            let _ = writeln!(out, "  Version {}", display(version));
        }
        let shown: Vec<_> = trace
            .candidates
            .iter()
            .filter(|candidate| all_candidates || candidate.reason != Rejection::NoFile)
            .collect();
        if !shown.is_empty() {
            let _ = writeln!(
                out,
                "\n{}",
                if all_candidates { "Tested" } else { "Rejected" }
            );
            for candidate in shown {
                let _ = writeln!(
                    out,
                    "  {}\n      {}",
                    display(&candidate.path),
                    candidate_reason(candidate, trace)
                );
            }
        }
        if trace.candidates_omitted > 0 {
            let _ = writeln!(
                out,
                "  {} more candidates were not recorded.",
                trace.candidates_omitted
            );
        }
        let tested = trace
            .candidates
            .iter()
            .filter(|c| c.reason == Rejection::NoFile)
            .count();
        if tested > 0 && !all_candidates {
            let _ = writeln!(
                out,
                "\nChecked {tested} locations with no package file. Use --all-candidates to list them."
            );
        }
        let disabled: Vec<String> = trace
            .roots
            .iter()
            .filter(|root| !root.enabled)
            .map(|root| display(&root.category))
            .collect();
        if !disabled.is_empty() {
            let _ = writeln!(out, "Skipped roots: {}", disabled.join(", "));
        }
        if !suggestions.is_empty() {
            let _ = writeln!(out, "\nTry");
            for suggestion in suggestions {
                match &suggestion.define {
                    Some(define) => {
                        let _ = writeln!(
                            out,
                            "  {}  {}",
                            display(define),
                            display(&suggestion.reason)
                        );
                    }
                    None => {
                        let _ = writeln!(out, "  {}", display(&suggestion.reason));
                    }
                }
            }
        }
        out.push('\n');
    }
    match report.source {
        FindSource::Reevaluated => out.push_str("(re-evaluated by rcmake)\n"),
        FindSource::Past => {
            let _ = writeln!(
                out,
                "(evidence from a past run. {})",
                report.stale_reason.unwrap_or("The tree changed.")
            );
        }
        FindSource::Saved => {}
    }
    if report.provisional {
        out.push_str("The evaluation was partial, so this result is provisional.\n");
    }
    out
}

/// Render the report as `rcmake-explain-find-v1` JSON.
#[must_use]
pub fn render_find_json(report: &FindReport) -> BString {
    let calls = report.calls.iter().map(|(trace, suggestions)| {
        let mut value = call_value(trace);
        if let Value::Object(map) = &mut value {
            map.insert(
                "suggestions".into(),
                Value::array_from(suggestions.iter().map(|suggestion| {
                    Value::object_from([
                        ("kind", Value::string(suggestion.kind.label())),
                        (
                            "define",
                            suggestion.define.clone().map_or(Value::Null, Value::String),
                        ),
                        ("reason", Value::String(suggestion.reason.clone())),
                    ])
                })),
            );
        }
        value
    });
    let value = Value::object_from([
        ("schema", Value::string(EXPLAIN_FIND_SCHEMA)),
        ("package", Value::String(report.package.clone())),
        ("source", Value::string(report.source.label())),
        (
            "stale_reason",
            report.stale_reason.map_or(Value::Null, Value::string),
        ),
        ("calls", Value::array_from(calls)),
        (
            "omitted_calls",
            Value::UInt(u64::from(report.omitted_calls)),
        ),
        ("completed", Value::Bool(report.completed)),
        ("absence_unknown", Value::Bool(report.absence_unknown())),
        ("provisional", Value::Bool(report.provisional)),
    ]);
    let mut out = rcmake_json::write_styled(&value, "  ");
    out.push(b'\n');
    out
}

#[cfg(test)]
#[path = "find_explain_tests.rs"]
mod tests;
