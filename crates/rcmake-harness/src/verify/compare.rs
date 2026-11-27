//! Semantic normalization and comparison for `rcmake-verify`.
//!
//! Normalizes only known engine executable/resource identities and staged scratch roots
//! at path boundaries, preserving ordinary paths, case, define values, list order, and
//! duplicate arguments.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, path};

use super::collect::{
    ArtifactState, CacheEntry, CodemodelEvidence, CompDbInvocation, CompileDbEvidence,
    CompileGroupModel, ConfigModel, IncludeEntry, LinkFragmentModel, NinjaDryRunEvidence,
    NinjaManifestEvidence, ProcessOutcome, RunEvidence, TargetModel,
};

const MAX_EXAMPLES: usize = 25;

/// Documented volatile cache entry exceptions (ignored during cache comparison).
pub(crate) const VOLATILE_CACHE_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "CMAKE_EDIT_COMMAND",
        "Optional interactive TUI executable (ccmake) may or may not be bundled beside the engine.",
    ),
    (
        "*_GUID_CMAKE",
        "Visual Studio generator assigns a fresh random project GUID on each configure.",
    ),
];

/// Outcome of one comparison section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionStatus {
    Equal,
    Different,
    Unavailable,
    Skipped,
    Inapplicable,
}

impl SectionStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Different => "different",
            Self::Unavailable => "unavailable",
            Self::Skipped => "skipped",
            Self::Inapplicable => "inapplicable",
        }
    }
}

/// Overall verification verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Parity,
    Divergent,
    MatchedFailure,
    IncompleteEvidence,
    InvalidInput,
}

impl Verdict {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Parity => "parity",
            Self::Divergent => "divergent",
            Self::MatchedFailure => "matched_failure",
            Self::IncompleteEvidence => "incomplete_evidence",
            Self::InvalidInput => "invalid_input",
        }
    }

    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Parity => 0,
            Self::Divergent => 1,
            Self::MatchedFailure | Self::IncompleteEvidence | Self::InvalidInput => 2,
        }
    }
}

/// Field-level report for one verification section.
#[derive(Clone, Debug)]
pub(crate) struct SectionReport {
    pub(crate) status: SectionStatus,
    pub(crate) compared: usize,
    pub(crate) differences: usize,
    pub(crate) examples: Vec<String>,
    pub(crate) notes: Vec<String>,
}

impl SectionReport {
    pub(crate) fn equal(compared: usize) -> Self {
        Self {
            status: SectionStatus::Equal,
            compared,
            differences: 0,
            examples: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            status: SectionStatus::Unavailable,
            compared: 0,
            differences: 0,
            examples: Vec::new(),
            notes: vec![reason.into()],
        }
    }

    pub(crate) fn skipped(reason: impl Into<String>) -> Self {
        Self {
            status: SectionStatus::Skipped,
            compared: 0,
            differences: 0,
            examples: Vec::new(),
            notes: vec![reason.into()],
        }
    }

    pub(crate) fn inapplicable(reason: impl Into<String>) -> Self {
        Self {
            status: SectionStatus::Inapplicable,
            compared: 0,
            differences: 0,
            examples: Vec::new(),
            notes: vec![reason.into()],
        }
    }

    fn push_diff(&mut self, msg: String) {
        self.differences += 1;
        self.status = SectionStatus::Different;
        if self.examples.len() < MAX_EXAMPLES {
            self.examples.push(msg);
        }
    }
}

/// Paired single-observation timing record.
#[derive(Clone, Debug)]
pub(crate) struct PairedTimings {
    pub(crate) run_order: [&'static str; 2],
    pub(crate) oracle_ns: u64,
    pub(crate) rcmake_ns: u64,
    /// `oracle_ns / rcmake_ns` only when both configures succeeded and `rcmake_ns > 0`.
    pub(crate) speedup: Option<f64>,
}

/// Complete verification comparison report.
#[derive(Clone, Debug)]
pub(crate) struct VerifyReport {
    pub(crate) verdict: Verdict,
    pub(crate) oracle_exe: String,
    pub(crate) oracle_version: String,
    pub(crate) rcmake_exe: String,
    pub(crate) rcmake_version: String,
    pub(crate) generator: Option<String>,
    pub(crate) compile_commands_injected: bool,
    pub(crate) external_inputs: Vec<String>,
    pub(crate) timings: PairedTimings,
    pub(crate) configure: SectionReport,
    pub(crate) codemodel: SectionReport,
    pub(crate) cache: SectionReport,
    pub(crate) compile_commands: SectionReport,
    pub(crate) ninja_dry_run: SectionReport,
    pub(crate) error: Option<String>,
    pub(crate) cleanup_warning: Option<String>,
}

/// Boundary-aware path normalizer for one engine's run.
#[derive(Clone, Debug)]
pub(crate) struct PathNormalizer {
    rules: Vec<(BString, &'static [u8])>,
}

impl PathNormalizer {
    /// Construct normalization rules for one engine run.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_run(
        host: &dyn Host,
        exe: &Path,
        cmake_root: Option<&[u8]>,
        staged_src: &Path,
        staged_bin: &Path,
        staged_home: &Path,
        staged_tmp: &Path,
    ) -> Self {
        let mut pairs: BTreeMap<BString, &'static [u8]> = BTreeMap::new();
        let mut add_path = |p: &Path, token: &'static [u8]| {
            let fwd = trim_trailing_sep(path::from_native_cmake(p));
            if !fwd.is_empty() {
                pairs.insert(fwd.clone(), token);
                // Handle macOS `/private/var` <-> `/var` and `/private/tmp` <-> `/tmp` aliases.
                if let Some(stripped) = fwd.strip_prefix(b"/private/") {
                    let mut alias = BString::from("/");
                    alias.extend_from_slice(stripped);
                    pairs.insert(alias, token);
                } else if fwd.starts_with(b"/var/") || fwd.starts_with(b"/tmp/") {
                    let mut alias = BString::from("/private");
                    alias.extend_from_slice(&fwd);
                    pairs.insert(alias, token);
                }
                if fwd.contains(&b'/') {
                    let back: Vec<u8> = fwd
                        .iter()
                        .map(|&b| if b == b'/' { b'\\' } else { b })
                        .collect();
                    pairs.insert(BString::from(back), token);
                }
            }
            if let Ok(canon) = host.canonicalize(p) {
                let cfwd = trim_trailing_sep(path::from_native_cmake(&canon));
                if !cfwd.is_empty() {
                    pairs.insert(cfwd.clone(), token);
                    if cfwd.contains(&b'/') {
                        let back: Vec<u8> = cfwd
                            .iter()
                            .map(|&b| if b == b'/' { b'\\' } else { b })
                            .collect();
                        pairs.insert(BString::from(back), token);
                    }
                }
            }
        };

        add_path(staged_bin, b"<BIN>");
        add_path(staged_src, b"<SRC>");
        add_path(staged_home, b"<HOME>");
        add_path(staged_tmp, b"<TMP>");

        add_path(exe, b"<CMAKE_EXE>");
        let is_win = host.system().sysname.eq_ignore_ascii_case(b"Windows");
        let suffix = if is_win { ".exe" } else { "" };
        for (sib, tok) in [
            ("cmake", b"<CMAKE_EXE>".as_slice()),
            ("ctest", b"<CTEST_EXE>".as_slice()),
            ("cpack", b"<CPACK_EXE>".as_slice()),
            ("ccmake", b"<CCMAKE_EXE>".as_slice()),
        ] {
            let leaf = format!("{sib}{suffix}");
            add_path(&exe.with_file_name(&leaf), tok);
            if let Ok(canon) = host.canonicalize(exe) {
                add_path(&canon.with_file_name(&leaf), tok);
            }
        }

        if let Some(root) = cmake_root
            && !root.is_empty()
        {
            add_path(&path::to_native(root), b"<CMAKE_ROOT>");
        }

        // Longest pattern first so nested paths (`<BIN>` inside scratch) win deterministically.
        let mut rules: Vec<(BString, &'static [u8])> = pairs.into_iter().collect();
        rules.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Self { rules }
    }

    /// Normalize known engine and staged paths at path boundaries only.
    pub(crate) fn normalize(&self, input: &[u8]) -> BString {
        if input.is_empty() || self.rules.is_empty() {
            return BString::from(input);
        }
        let mut out = Vec::with_capacity(input.len());
        let mut pos = 0usize;
        while pos < input.len() {
            let rem = input.get(pos..).unwrap_or(&[]);
            let mut matched = None;
            for (pat, repl) in &self.rules {
                if rem.starts_with(pat.as_slice())
                    && left_boundary_ok(input, pos)
                    && right_boundary_ok(input, pos + pat.len())
                {
                    matched = Some((pat.len(), *repl));
                    break;
                }
            }
            if let Some((len, repl)) = matched {
                out.extend_from_slice(repl);
                pos += len;
            } else if let Some(&b) = input.get(pos) {
                out.push(b);
                pos += 1;
            } else {
                break;
            }
        }
        BString::from(out)
    }

    fn normalize_opt(&self, input: &Option<BString>) -> Option<BString> {
        input.as_deref().map(|v| self.normalize(v))
    }
}

fn trim_trailing_sep(mut b: BString) -> BString {
    while b.len() > 1 && (b.ends_with(b"/") || b.ends_with(b"\\")) {
        b.pop();
    }
    b
}

fn left_boundary_ok(buf: &[u8], start: usize) -> bool {
    if start == 0 {
        return true;
    }
    let Some(&prev) = buf.get(start - 1) else {
        return true;
    };
    if is_left_delim(prev) {
        return true;
    }
    // Allow attached compiler flag prefixes like `-I<path>`, `-L<path>`, `-isystem<path>`,
    // `/I<path>`, `/Fo<path>`, while rejecting ordinary path prefixes like `/other<path>`.
    let mut tok_start = start;
    while tok_start > 0 {
        if let Some(&b) = buf.get(tok_start - 1)
            && !is_left_delim(b)
        {
            tok_start -= 1;
        } else {
            break;
        }
    }
    let prefix = buf.get(tok_start..start).unwrap_or(&[]);
    if prefix.starts_with(b"-") && !prefix.contains(&b'/') && !prefix.contains(&b'\\') {
        return true;
    }
    matches!(
        prefix,
        b"/I" | b"/D" | b"/Fo" | b"/Fd" | b"/Fp" | b"/Fi" | b"/FI" | b"/Fe" | b"/Fa"
    )
}

fn is_left_delim(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b';'
            | b','
            | b'='
            | b':'
            | b'"'
            | b'\''
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'@'
    )
}

fn right_boundary_ok(buf: &[u8], end: usize) -> bool {
    let Some(&next) = buf.get(end) else {
        return true;
    };
    matches!(
        next,
        b'/' | b'\\'
            | b' '
            | b'\t'
            | b'\n'
            | b'\r'
            | b';'
            | b','
            | b':'
            | b'"'
            | b'\''
            | b')'
            | b'>'
    )
}

/// Compare paired oracle and rcmake evidence into one structured [`VerifyReport`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare_runs(
    host: &dyn Host,
    oracle_exe: &Path,
    oracle_version: String,
    rcmake_exe: &Path,
    rcmake_version: String,
    staged_src: &Path,
    staged_bin: &Path,
    staged_home: &Path,
    staged_tmp: &Path,
    compile_commands_injected: bool,
    compile_commands_user_disabled: bool,
    external_inputs: Vec<String>,
    oracle: &RunEvidence,
    rcmake: &RunEvidence,
) -> VerifyReport {
    let oracle_norm = PathNormalizer::for_run(
        host,
        oracle_exe,
        oracle.cmake_root.as_ref().map(|r| r.as_slice()),
        staged_src,
        staged_bin,
        staged_home,
        staged_tmp,
    );
    let rcmake_norm = PathNormalizer::for_run(
        host,
        rcmake_exe,
        rcmake.cmake_root.as_ref().map(|r| r.as_slice()),
        staged_src,
        staged_bin,
        staged_home,
        staged_tmp,
    );

    let both_succeeded = matches!(oracle.configure.status, ProcessOutcome::Exited(0))
        && matches!(rcmake.configure.status, ProcessOutcome::Exited(0));
    let speedup = if both_succeeded && rcmake.configure.elapsed_ns > 0 {
        Some(oracle.configure.elapsed_ns as f64 / rcmake.configure.elapsed_ns as f64)
    } else {
        None
    };
    let timings = PairedTimings {
        run_order: ["oracle", "rcmake"],
        oracle_ns: oracle.configure.elapsed_ns,
        rcmake_ns: rcmake.configure.elapsed_ns,
        speedup,
    };

    let generator = oracle
        .generator
        .as_deref()
        .or(rcmake.generator.as_deref())
        .map(|g| String::from_utf8_lossy(g).into_owned());

    let (configure_report, early_verdict) = compare_configure(oracle, rcmake);
    if let Some(verdict) = early_verdict {
        let skipped =
            SectionReport::skipped("Configure did not complete successfully on both engines.");
        return VerifyReport {
            verdict,
            oracle_exe: oracle_exe.display().to_string(),
            oracle_version,
            rcmake_exe: rcmake_exe.display().to_string(),
            rcmake_version,
            generator,
            compile_commands_injected,
            external_inputs,
            timings,
            configure: configure_report,
            codemodel: skipped.clone(),
            cache: skipped.clone(),
            compile_commands: skipped.clone(),
            ninja_dry_run: skipped,
            error: None,
            cleanup_warning: None,
        };
    }

    let codemodel_report = compare_codemodel(
        &oracle.codemodel,
        &rcmake.codemodel,
        &oracle_norm,
        &rcmake_norm,
    );
    let cache_report = compare_cache(&oracle.cache, &rcmake.cache, &oracle_norm, &rcmake_norm);
    let compdb_report = compare_compile_commands(
        oracle,
        rcmake,
        &oracle_norm,
        &rcmake_norm,
        compile_commands_user_disabled,
    );
    let ninja_report = compare_ninja_dry_run(
        &oracle.ninja_dry_run,
        &rcmake.ninja_dry_run,
        &oracle_norm,
        &rcmake_norm,
    );

    let sections = [
        &configure_report,
        &codemodel_report,
        &cache_report,
        &compdb_report,
        &ninja_report,
    ];
    let verdict = if sections
        .iter()
        .any(|s| s.status == SectionStatus::Different)
    {
        Verdict::Divergent
    } else if sections
        .iter()
        .any(|s| s.status == SectionStatus::Unavailable)
    {
        Verdict::IncompleteEvidence
    } else {
        Verdict::Parity
    };

    VerifyReport {
        verdict,
        oracle_exe: oracle_exe.display().to_string(),
        oracle_version,
        rcmake_exe: rcmake_exe.display().to_string(),
        rcmake_version,
        generator,
        compile_commands_injected,
        external_inputs,
        timings,
        configure: configure_report,
        codemodel: codemodel_report,
        cache: cache_report,
        compile_commands: compdb_report,
        ninja_dry_run: ninja_report,
        error: None,
        cleanup_warning: None,
    }
}

fn compare_configure(
    oracle: &RunEvidence,
    rcmake: &RunEvidence,
) -> (SectionReport, Option<Verdict>) {
    match (&oracle.configure.status, &rcmake.configure.status) {
        (ProcessOutcome::Exited(0), ProcessOutcome::Exited(0)) => (SectionReport::equal(1), None),
        (ProcessOutcome::Exited(a), ProcessOutcome::Exited(b)) if a == b => {
            let mut rep = SectionReport::unavailable(format!(
                "Both oracle and rcmake failed configure with matching exit status {a}; no parity claim is made."
            ));
            if !oracle.configure.stderr_tail.trim().is_empty() {
                rep.notes.push(format!(
                    "oracle stderr tail: {}",
                    first_line_excerpt(&oracle.configure.stderr_tail)
                ));
            }
            if !rcmake.configure.stderr_tail.trim().is_empty() {
                rep.notes.push(format!(
                    "rcmake stderr tail: {}",
                    first_line_excerpt(&rcmake.configure.stderr_tail)
                ));
            }
            (rep, Some(Verdict::MatchedFailure))
        }
        (ProcessOutcome::Exited(a), ProcessOutcome::Exited(b)) => {
            let mut rep = SectionReport {
                status: SectionStatus::Different,
                compared: 1,
                differences: 1,
                examples: vec![format!(
                    "configure exit status differs: oracle exited {a}, rcmake exited {b}"
                )],
                notes: Vec::new(),
            };
            if *a != 0 && !oracle.configure.stderr_tail.trim().is_empty() {
                rep.notes.push(format!(
                    "oracle stderr tail: {}",
                    first_line_excerpt(&oracle.configure.stderr_tail)
                ));
            }
            if *b != 0 && !rcmake.configure.stderr_tail.trim().is_empty() {
                rep.notes.push(format!(
                    "rcmake stderr tail: {}",
                    first_line_excerpt(&rcmake.configure.stderr_tail)
                ));
            }
            (rep, Some(Verdict::Divergent))
        }
        (o, r) => {
            let rep = SectionReport::unavailable(format!(
                "Configure did not exit cleanly (oracle: {}, rcmake: {}).",
                describe_outcome(o),
                describe_outcome(r)
            ));
            (rep, Some(Verdict::IncompleteEvidence))
        }
    }
}

fn describe_outcome(o: &ProcessOutcome) -> String {
    match o {
        ProcessOutcome::Exited(c) => format!("exited {c}"),
        ProcessOutcome::Signaled(s) => format!("signaled {s}"),
        ProcessOutcome::TimedOut => "timed out".to_owned(),
        ProcessOutcome::SpawnFailed(e) => format!("spawn failed ({e})"),
    }
}

fn first_line_excerpt(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

fn compare_codemodel(
    oracle: &ArtifactState<CodemodelEvidence>,
    rcmake: &ArtifactState<CodemodelEvidence>,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
) -> SectionReport {
    match (oracle, rcmake) {
        (ArtifactState::Present(o), ArtifactState::Present(r)) => {
            let mut rep = SectionReport::equal(0);
            let o_src = o_norm.normalize(&o.paths_source);
            let r_src = r_norm.normalize(&r.paths_source);
            if o_src != r_src {
                rep.push_diff(format!(
                    "codemodel paths.source: oracle='{}' vs rcmake='{}'",
                    lossy(&o_src),
                    lossy(&r_src)
                ));
            }
            let o_bin = o_norm.normalize(&o.paths_build);
            let r_bin = r_norm.normalize(&r.paths_build);
            if o_bin != r_bin {
                rep.push_diff(format!(
                    "codemodel paths.build: oracle='{}' vs rcmake='{}'",
                    lossy(&o_bin),
                    lossy(&r_bin)
                ));
            }

            let all_cfgs: BTreeSet<&BString> = o
                .configurations
                .keys()
                .chain(r.configurations.keys())
                .collect();
            for cfg in all_cfgs {
                let cfg_label = if cfg.is_empty() {
                    "<default>".to_owned()
                } else {
                    lossy(cfg)
                };
                match (o.configurations.get(cfg), r.configurations.get(cfg)) {
                    (Some(oc), Some(rc)) => {
                        compare_config_model(&cfg_label, oc, rc, o_norm, r_norm, &mut rep);
                    }
                    (Some(_), None) => {
                        rep.compared += 1;
                        rep.push_diff(format!(
                            "configuration '{cfg_label}' present in oracle, missing in rcmake"
                        ));
                    }
                    (None, Some(_)) => {
                        rep.compared += 1;
                        rep.push_diff(format!(
                            "configuration '{cfg_label}' missing in oracle, present in rcmake"
                        ));
                    }
                    (None, None) => {}
                }
            }
            rep
        }
        (ArtifactState::Present(_), ArtifactState::Missing) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff(
                "Required File API codemodel-v2 reply present in oracle but missing in rcmake."
                    .to_owned(),
            );
            rep
        }
        (ArtifactState::Missing, ArtifactState::Present(_)) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff(
                "Required File API codemodel-v2 reply missing in oracle but present in rcmake."
                    .to_owned(),
            );
            rep
        }
        (ArtifactState::Missing, ArtifactState::Missing) => SectionReport::unavailable(
            "Required File API codemodel-v2 reply is missing on both engines.",
        ),
        (ArtifactState::Unavailable(e), _) => {
            SectionReport::unavailable(format!("Oracle codemodel unavailable: {e}"))
        }
        (_, ArtifactState::Unavailable(e)) => {
            SectionReport::unavailable(format!("rcmake codemodel unavailable: {e}"))
        }
    }
}

fn compare_config_model(
    cfg_label: &str,
    oc: &ConfigModel,
    rc: &ConfigModel,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
    rep: &mut SectionReport,
) {
    // Projects
    let proj_keys: BTreeSet<&BString> = oc.projects.keys().chain(rc.projects.keys()).collect();
    for key in proj_keys {
        rep.compared += 1;
        match (oc.projects.get(key), rc.projects.get(key)) {
            (Some(op), Some(rp)) => {
                if op.parent != rp.parent {
                    rep.push_diff(format!(
                        "[{cfg_label}] project '{}' parent differs: oracle={:?} vs rcmake={:?}",
                        lossy(key),
                        op.parent.as_ref().map(|v| lossy(v)),
                        rp.parent.as_ref().map(|v| lossy(v))
                    ));
                }
                let o_dirs: BTreeSet<BString> =
                    op.directories.iter().map(|d| o_norm.normalize(d)).collect();
                let r_dirs: BTreeSet<BString> =
                    rp.directories.iter().map(|d| r_norm.normalize(d)).collect();
                if o_dirs != r_dirs {
                    rep.push_diff(format!(
                        "[{cfg_label}] project '{}' directories differ",
                        lossy(key)
                    ));
                }
                if op.targets != rp.targets {
                    rep.push_diff(format!(
                        "[{cfg_label}] project '{}' target set differs",
                        lossy(key)
                    ));
                }
            }
            (Some(_), None) => rep.push_diff(format!(
                "[{cfg_label}] project '{}' present only in oracle",
                lossy(key)
            )),
            (None, Some(_)) => rep.push_diff(format!(
                "[{cfg_label}] project '{}' present only in rcmake",
                lossy(key)
            )),
            (None, None) => {}
        }
    }

    // Directories
    let o_dirs: BTreeMap<BString, _> = oc
        .directories
        .iter()
        .map(|(k, v)| (o_norm.normalize(k), v))
        .collect();
    let r_dirs: BTreeMap<BString, _> = rc
        .directories
        .iter()
        .map(|(k, v)| (r_norm.normalize(k), v))
        .collect();
    let dir_keys: BTreeSet<&BString> = o_dirs.keys().chain(r_dirs.keys()).collect();
    for key in dir_keys {
        rep.compared += 1;
        match (o_dirs.get(key), r_dirs.get(key)) {
            (Some(od), Some(rd)) => {
                let o_src = o_norm.normalize(&od.source);
                let r_src = r_norm.normalize(&rd.source);
                let o_par = o_norm.normalize_opt(&od.parent_build);
                let r_par = r_norm.normalize_opt(&rd.parent_build);
                if o_src != r_src
                    || o_par != r_par
                    || od.project != rd.project
                    || od.minimum_cmake_version != rd.minimum_cmake_version
                {
                    rep.push_diff(format!(
                        "[{cfg_label}] directory '{}' metadata differs",
                        lossy(key)
                    ));
                }
            }
            (Some(_), None) => rep.push_diff(format!(
                "[{cfg_label}] directory '{}' present only in oracle",
                lossy(key)
            )),
            (None, Some(_)) => rep.push_diff(format!(
                "[{cfg_label}] directory '{}' present only in rcmake",
                lossy(key)
            )),
            (None, None) => {}
        }
    }

    // Targets
    let target_keys: BTreeSet<&BString> = oc.targets.keys().chain(rc.targets.keys()).collect();
    for key in target_keys {
        rep.compared += 1;
        match (oc.targets.get(key), rc.targets.get(key)) {
            (Some(ot), Some(rt)) => {
                compare_target_model(cfg_label, ot, rt, o_norm, r_norm, rep);
            }
            (Some(_), None) => rep.push_diff(format!(
                "[{cfg_label}] target '{}' present only in oracle",
                lossy(key)
            )),
            (None, Some(_)) => rep.push_diff(format!(
                "[{cfg_label}] target '{}' present only in rcmake",
                lossy(key)
            )),
            (None, None) => {}
        }
    }
}

fn compare_target_model(
    cfg_label: &str,
    ot: &TargetModel,
    rt: &TargetModel,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
    rep: &mut SectionReport,
) {
    let tname = lossy(&ot.name);
    if ot.target_type != rt.target_type {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' type: oracle='{}' vs rcmake='{}'",
            lossy(&ot.target_type),
            lossy(&rt.target_type)
        ));
    }
    if ot.name_on_disk != rt.name_on_disk {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' nameOnDisk: oracle={:?} vs rcmake={:?}",
            ot.name_on_disk.as_ref().map(|v| lossy(v)),
            rt.name_on_disk.as_ref().map(|v| lossy(v))
        ));
    }
    if o_norm.normalize(&ot.directory_source) != r_norm.normalize(&rt.directory_source)
        || o_norm.normalize(&ot.directory_build) != r_norm.normalize(&rt.directory_build)
        || ot.project != rt.project
    {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' directory/project ownership differs"
        ));
    }

    let o_arts: Vec<BString> = ot.artifacts.iter().map(|a| o_norm.normalize(a)).collect();
    let r_arts: Vec<BString> = rt.artifacts.iter().map(|a| r_norm.normalize(a)).collect();
    if o_arts != r_arts {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' artifacts differ: oracle={:?} vs rcmake={:?}",
            o_arts.iter().map(|v| lossy(v)).collect::<Vec<_>>(),
            r_arts.iter().map(|v| lossy(v)).collect::<Vec<_>>()
        ));
    }
    if ot.dependencies != rt.dependencies {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' dependencies differ: oracle={:?} vs rcmake={:?}",
            ot.dependencies.iter().map(|v| lossy(v)).collect::<Vec<_>>(),
            rt.dependencies.iter().map(|v| lossy(v)).collect::<Vec<_>>()
        ));
    }

    // Compiled sources & per-source compile-group membership.
    let o_srcs: BTreeMap<BString, Vec<CompileGroupModel>> = ot
        .compiled_sources
        .iter()
        .map(|(k, v)| {
            (
                o_norm.normalize(k),
                v.iter().map(|g| normalize_group(g, o_norm)).collect(),
            )
        })
        .collect();
    let r_srcs: BTreeMap<BString, Vec<CompileGroupModel>> = rt
        .compiled_sources
        .iter()
        .map(|(k, v)| {
            (
                r_norm.normalize(k),
                v.iter().map(|g| normalize_group(g, r_norm)).collect(),
            )
        })
        .collect();
    let src_keys: BTreeSet<&BString> = o_srcs.keys().chain(r_srcs.keys()).collect();
    for src in src_keys {
        match (o_srcs.get(src), r_srcs.get(src)) {
            (Some(og), Some(rg)) => {
                if og != rg {
                    let detail = describe_group_diff(og, rg);
                    rep.push_diff(format!(
                        "[{cfg_label}] target '{tname}' source '{}' compile settings differ: {detail}",
                        lossy(src)
                    ));
                }
            }
            (Some(_), None) => rep.push_diff(format!(
                "[{cfg_label}] target '{tname}' compiled source '{}' present only in oracle",
                lossy(src)
            )),
            (None, Some(_)) => rep.push_diff(format!(
                "[{cfg_label}] target '{tname}' compiled source '{}' present only in rcmake",
                lossy(src)
            )),
            (None, None) => {}
        }
    }

    let o_uncomp: BTreeSet<BString> = ot
        .uncompiled_sources
        .iter()
        .map(|s| o_norm.normalize(s))
        .collect();
    let r_uncomp: BTreeSet<BString> = rt
        .uncompiled_sources
        .iter()
        .map(|s| r_norm.normalize(s))
        .collect();
    if o_uncomp != r_uncomp {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' non-compiled source set differs"
        ));
    }

    if ot.link_language != rt.link_language {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' link language: oracle={:?} vs rcmake={:?}",
            ot.link_language.as_ref().map(|v| lossy(v)),
            rt.link_language.as_ref().map(|v| lossy(v))
        ));
    }
    let o_link: Vec<LinkFragmentModel> = ot
        .link_fragments
        .iter()
        .map(|f| LinkFragmentModel {
            role: f.role.clone(),
            fragment: o_norm.normalize(&f.fragment),
        })
        .collect();
    let r_link: Vec<LinkFragmentModel> = rt
        .link_fragments
        .iter()
        .map(|f| LinkFragmentModel {
            role: f.role.clone(),
            fragment: r_norm.normalize(&f.fragment),
        })
        .collect();
    if o_link != r_link {
        rep.push_diff(format!(
            "[{cfg_label}] target '{tname}' link/archive commandFragments differ"
        ));
    }
}

fn normalize_group(g: &CompileGroupModel, norm: &PathNormalizer) -> CompileGroupModel {
    CompileGroupModel {
        language: g.language.clone(),
        standard: g.standard.clone(),
        defines: g.defines.iter().map(|d| norm.normalize(d)).collect(),
        includes: g
            .includes
            .iter()
            .map(|i| IncludeEntry {
                path: norm.normalize(&i.path),
                is_system: i.is_system,
            })
            .collect(),
        frameworks: g
            .frameworks
            .iter()
            .map(|f| IncludeEntry {
                path: norm.normalize(&f.path),
                is_system: f.is_system,
            })
            .collect(),
        fragments: g.fragments.iter().map(|f| norm.normalize(f)).collect(),
        sysroot: norm.normalize_opt(&g.sysroot),
    }
}

fn describe_group_diff(og: &[CompileGroupModel], rg: &[CompileGroupModel]) -> String {
    if og.len() != rg.len() {
        return format!("group count oracle={} vs rcmake={}", og.len(), rg.len());
    }
    if let (Some(o), Some(r)) = (og.first(), rg.first()) {
        let mut fields = Vec::new();
        if o.language != r.language {
            fields.push(format!(
                "language ({} vs {})",
                lossy(&o.language),
                lossy(&r.language)
            ));
        }
        if o.standard != r.standard {
            fields.push("standard".to_owned());
        }
        if o.defines != r.defines {
            fields.push(format!(
                "defines (oracle={:?} vs rcmake={:?})",
                o.defines.iter().map(|v| lossy(v)).collect::<Vec<_>>(),
                r.defines.iter().map(|v| lossy(v)).collect::<Vec<_>>()
            ));
        }
        if o.includes != r.includes {
            fields.push("includes (order, path, or isSystem)".to_owned());
        }
        if o.frameworks != r.frameworks {
            fields.push("frameworks".to_owned());
        }
        if o.fragments != r.fragments {
            fields.push(format!(
                "fragments (oracle={:?} vs rcmake={:?})",
                o.fragments.iter().map(|v| lossy(v)).collect::<Vec<_>>(),
                r.fragments.iter().map(|v| lossy(v)).collect::<Vec<_>>()
            ));
        }
        if o.sysroot != r.sysroot {
            fields.push("sysroot".to_owned());
        }
        if !fields.is_empty() {
            return fields.join(", ");
        }
    }
    "compile group contents differ".to_owned()
}

fn is_volatile_cache_key(key: &[u8]) -> bool {
    key == b"CMAKE_EDIT_COMMAND" || key.ends_with(b"_GUID_CMAKE")
}

fn compare_cache(
    oracle: &ArtifactState<BTreeMap<BString, CacheEntry>>,
    rcmake: &ArtifactState<BTreeMap<BString, CacheEntry>>,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
) -> SectionReport {
    match (oracle, rcmake) {
        (ArtifactState::Present(o), ArtifactState::Present(r)) => {
            let mut rep = SectionReport::equal(0);
            let mut ignored = 0usize;
            let keys: BTreeSet<&BString> = o.keys().chain(r.keys()).collect();
            for key in keys {
                if is_volatile_cache_key(key) {
                    ignored += 1;
                    continue;
                }
                rep.compared += 1;
                match (o.get(key), r.get(key)) {
                    (Some(oe), Some(re)) => {
                        let o_val = o_norm.normalize(&oe.value);
                        let r_val = r_norm.normalize(&re.value);
                        if oe.ty != re.ty || o_val != r_val {
                            rep.push_diff(format!(
                                "cache '{}': oracle={}:{} vs rcmake={}:{}",
                                lossy(key),
                                lossy(&oe.ty),
                                lossy(&o_val),
                                lossy(&re.ty),
                                lossy(&r_val)
                            ));
                        }
                    }
                    (Some(oe), None) => rep.push_diff(format!(
                        "cache '{}' ({}:{}) present only in oracle",
                        lossy(key),
                        lossy(&oe.ty),
                        lossy(&o_norm.normalize(&oe.value))
                    )),
                    (None, Some(re)) => rep.push_diff(format!(
                        "cache '{}' ({}:{}) present only in rcmake",
                        lossy(key),
                        lossy(&re.ty),
                        lossy(&r_norm.normalize(&re.value))
                    )),
                    (None, None) => {}
                }
            }
            if ignored > 0 {
                rep.notes.push(format!(
                    "Ignored {ignored} documented volatile cache entries (CMAKE_EDIT_COMMAND, *_GUID_CMAKE)."
                ));
            }
            rep
        }
        (ArtifactState::Present(_), ArtifactState::Missing) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff("CMakeCache.txt present in oracle but missing in rcmake.".to_owned());
            rep
        }
        (ArtifactState::Missing, ArtifactState::Present(_)) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff("CMakeCache.txt missing in oracle but present in rcmake.".to_owned());
            rep
        }
        (ArtifactState::Missing, ArtifactState::Missing) => {
            SectionReport::unavailable("CMakeCache.txt is missing on both engines.")
        }
        (ArtifactState::Unavailable(e), _) => {
            SectionReport::unavailable(format!("Oracle cache unavailable: {e}"))
        }
        (_, ArtifactState::Unavailable(e)) => {
            SectionReport::unavailable(format!("rcmake cache unavailable: {e}"))
        }
    }
}

fn generator_supports_compat_compdb(generator_name: Option<&[u8]>) -> bool {
    let Some(g) = generator_name else { return true };
    g == b"Ninja" || g == b"Ninja Multi-Config" || g.ends_with(b"Makefiles")
}

fn compare_compile_commands(
    oracle: &RunEvidence,
    rcmake: &RunEvidence,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
    user_disabled: bool,
) -> SectionReport {
    if user_disabled {
        return SectionReport::skipped(
            "Compile database comparison skipped because CMAKE_EXPORT_COMPILE_COMMANDS was disabled by caller.",
        );
    }

    let generator_name = oracle
        .generator
        .as_ref()
        .or(rcmake.generator.as_ref())
        .map(|g| g.as_slice());
    if !generator_supports_compat_compdb(generator_name) {
        return SectionReport::inapplicable(format!(
            "Generator '{}' does not export compile_commands.json in compatibility mode.",
            generator_name.map_or_else(|| "<unknown>".to_owned(), lossy)
        ));
    }

    let any_compiled = matches!(&oracle.codemodel, ArtifactState::Present(cm) if cm.has_compiled_sources)
        || matches!(&rcmake.codemodel, ArtifactState::Present(cm) if cm.has_compiled_sources);
    let both_codemodels_present = matches!(oracle.codemodel, ArtifactState::Present(_))
        && matches!(rcmake.codemodel, ArtifactState::Present(_));

    match (&oracle.compile_commands, &rcmake.compile_commands) {
        (ArtifactState::Present(o), ArtifactState::Present(r)) => {
            compare_compdb_entries(o, r, o_norm, r_norm)
        }
        (ArtifactState::Missing, ArtifactState::Missing) => {
            if both_codemodels_present && !any_compiled {
                SectionReport::inapplicable(
                    "Project has no compiled sources; compile_commands.json was not generated.",
                )
            } else if both_caches_disable_compdb(oracle, rcmake) {
                SectionReport::skipped(
                    "Project disabled CMAKE_EXPORT_COMPILE_COMMANDS in both build trees.",
                )
            } else {
                SectionReport::unavailable(
                    "compile_commands.json is missing on both engines despite compiled sources.",
                )
            }
        }
        (ArtifactState::Present(_), ArtifactState::Missing) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff(
                "compile_commands.json present in oracle but missing in rcmake.".to_owned(),
            );
            rep
        }
        (ArtifactState::Missing, ArtifactState::Present(_)) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff(
                "compile_commands.json missing in oracle but present in rcmake.".to_owned(),
            );
            rep
        }
        (ArtifactState::Unavailable(e), _) => {
            SectionReport::unavailable(format!("Oracle compile_commands.json unavailable: {e}"))
        }
        (_, ArtifactState::Unavailable(e)) => {
            SectionReport::unavailable(format!("rcmake compile_commands.json unavailable: {e}"))
        }
    }
}

fn both_caches_disable_compdb(oracle: &RunEvidence, rcmake: &RunEvidence) -> bool {
    let off = |ev: &RunEvidence| {
        matches!(
            &ev.cache,
            ArtifactState::Present(map)
                if map
                    .get(b"CMAKE_EXPORT_COMPILE_COMMANDS".as_slice())
                    .is_some_and(|e| !rcmake_base::truth::is_on(&e.value))
        )
    };
    off(oracle) && off(rcmake)
}

type CompDbKey = (Option<BString>, BString, BString, Option<BString>);

fn normalize_compdb_map(
    db: &CompileDbEvidence,
    norm: &PathNormalizer,
) -> BTreeMap<CompDbKey, BTreeMap<CompDbInvocation, usize>> {
    let mut map: BTreeMap<CompDbKey, BTreeMap<CompDbInvocation, usize>> = BTreeMap::new();
    for entry in &db.entries {
        let key: CompDbKey = (
            entry.config.clone(),
            norm.normalize(&entry.directory),
            norm.normalize(&entry.file),
            norm.normalize_opt(&entry.output),
        );
        let inv = match &entry.invocation {
            CompDbInvocation::Tokens(toks) => {
                CompDbInvocation::Tokens(toks.iter().map(|t| norm.normalize(t)).collect())
            }
            CompDbInvocation::Opaque { raw, reason } => CompDbInvocation::Opaque {
                raw: norm.normalize(raw),
                reason: reason.clone(),
            },
        };
        *map.entry(key).or_default().entry(inv).or_insert(0) += 1;
    }
    map
}

fn compare_compdb_entries(
    o: &CompileDbEvidence,
    r: &CompileDbEvidence,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
) -> SectionReport {
    let o_map = normalize_compdb_map(o, o_norm);
    let r_map = normalize_compdb_map(r, r_norm);
    let mut rep = SectionReport::equal(0);
    let mut uncomparable_mixed = false;

    let keys: BTreeSet<&CompDbKey> = o_map.keys().chain(r_map.keys()).collect();
    for key in keys {
        rep.compared += 1;
        let label = format_compdb_key(key);
        match (o_map.get(key), r_map.get(key)) {
            (Some(oi), Some(ri)) => {
                if oi != ri {
                    // Check if one side is Tokens and the other is Opaque (uncomparable encoding).
                    let o_has_opaque = oi
                        .keys()
                        .any(|k| matches!(k, CompDbInvocation::Opaque { .. }));
                    let r_has_opaque = ri
                        .keys()
                        .any(|k| matches!(k, CompDbInvocation::Opaque { .. }));
                    if o_has_opaque != r_has_opaque {
                        uncomparable_mixed = true;
                        rep.notes.push(format!(
                            "Uncomparable compile command encoding for {label} (tokenized on one side, opaque on the other)."
                        ));
                    } else {
                        rep.push_diff(format!(
                            "compile_commands entry {label} differs (oracle={} invocation(s), rcmake={} invocation(s))",
                            oi.values().sum::<usize>(),
                            ri.values().sum::<usize>()
                        ));
                    }
                }
            }
            (Some(oi), None) => rep.push_diff(format!(
                "compile_commands entry {label} (x{}) present only in oracle",
                oi.values().sum::<usize>()
            )),
            (None, Some(ri)) => rep.push_diff(format!(
                "compile_commands entry {label} (x{}) present only in rcmake",
                ri.values().sum::<usize>()
            )),
            (None, None) => {}
        }
    }

    if o.opaque_count > 0 || r.opaque_count > 0 {
        rep.notes.push(format!(
            "Opaque unparsed compile commands prevented complete token comparison: oracle={}, rcmake={}.",
            o.opaque_count, r.opaque_count
        ));
        if rep.status == SectionStatus::Equal {
            rep.status = SectionStatus::Unavailable;
        }
    }
    if uncomparable_mixed && rep.status == SectionStatus::Equal {
        rep.status = SectionStatus::Unavailable;
    }
    rep
}

fn format_compdb_key(key: &CompDbKey) -> String {
    let mut parts = Vec::new();
    if let Some(cfg) = &key.0 {
        parts.push(format!("[{}]", lossy(cfg)));
    }
    parts.push(lossy(&key.2));
    if let Some(out) = &key.3 {
        parts.push(format!("-> {}", lossy(out)));
    }
    parts.join(" ")
}

fn compare_ninja_dry_run(
    oracle: &NinjaDryRunEvidence,
    rcmake: &NinjaDryRunEvidence,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
) -> SectionReport {
    match (oracle, rcmake) {
        (NinjaDryRunEvidence::Inapplicable(msg), NinjaDryRunEvidence::Inapplicable(_)) => {
            SectionReport::inapplicable(msg.clone())
        }
        (NinjaDryRunEvidence::Unavailable(msg), _) => {
            SectionReport::unavailable(format!("Oracle Ninja dry-run unavailable: {msg}"))
        }
        (_, NinjaDryRunEvidence::Unavailable(msg)) => {
            SectionReport::unavailable(format!("rcmake Ninja dry-run unavailable: {msg}"))
        }
        (NinjaDryRunEvidence::Present(_), NinjaDryRunEvidence::Inapplicable(msg))
        | (NinjaDryRunEvidence::Inapplicable(msg), NinjaDryRunEvidence::Present(_)) => {
            let mut rep = SectionReport::equal(1);
            rep.push_diff(format!(
                "Ninja dry-run applicability differs between engines: {msg}"
            ));
            rep
        }
        (NinjaDryRunEvidence::Present(o_map), NinjaDryRunEvidence::Present(r_map)) => {
            let mut rep = SectionReport::equal(0);
            let manifests: BTreeSet<&String> = o_map.keys().chain(r_map.keys()).collect();
            for name in manifests {
                match (o_map.get(name), r_map.get(name)) {
                    (Some(om), Some(rm)) => {
                        compare_ninja_manifest(name, om, rm, o_norm, r_norm, &mut rep);
                    }
                    (Some(_), None) => {
                        rep.compared += 1;
                        rep.push_diff(format!("Ninja manifest '{name}' present only in oracle"));
                    }
                    (None, Some(_)) => {
                        rep.compared += 1;
                        rep.push_diff(format!("Ninja manifest '{name}' present only in rcmake"));
                    }
                    (None, None) => {}
                }
            }
            rep
        }
    }
}

fn compare_ninja_manifest(
    name: &str,
    om: &NinjaManifestEvidence,
    rm: &NinjaManifestEvidence,
    o_norm: &PathNormalizer,
    r_norm: &PathNormalizer,
    rep: &mut SectionReport,
) {
    let o_cmds: Vec<BString> = om.commands.iter().map(|c| o_norm.normalize(c)).collect();
    let r_cmds: Vec<BString> = rm.commands.iter().map(|c| r_norm.normalize(c)).collect();
    rep.compared += o_cmds.len().max(r_cmds.len());

    if o_cmds == r_cmds {
        return;
    }

    // Compare command multisets first (preserving multiplicity).
    let mut o_counts: BTreeMap<&BString, usize> = BTreeMap::new();
    for c in &o_cmds {
        *o_counts.entry(c).or_insert(0) += 1;
    }
    let mut r_counts: BTreeMap<&BString, usize> = BTreeMap::new();
    for c in &r_cmds {
        *r_counts.entry(c).or_insert(0) += 1;
    }

    if o_counts != r_counts {
        let all: BTreeSet<&BString> = o_counts.keys().chain(r_counts.keys()).copied().collect();
        for cmd in all {
            let oc = o_counts.get(cmd).copied().unwrap_or(0);
            let rc = r_counts.get(cmd).copied().unwrap_or(0);
            if oc != rc {
                rep.push_diff(format!(
                    "[{name}] Ninja command count differs (oracle={oc}, rcmake={rc}): {}",
                    lossy(cmd)
                ));
            }
        }
        return;
    }

    // Exact same command multiset, different order: check whether any inverted pair
    // is dependency-ordered in either Ninja manifest's build graph.
    let o_edges = normalize_dep_edges(&om.dep_edges, o_norm);
    let r_edges = normalize_dep_edges(&rm.dep_edges, r_norm);
    let dep_flips = find_dependency_order_inversions(&o_cmds, &r_cmds, &o_edges, &r_edges);
    if !dep_flips.is_empty() {
        for (a, b) in dep_flips {
            rep.push_diff(format!(
                "[{name}] Dependency-relevant Ninja command order reversed between '{}' and '{}'",
                excerpt_cmd(&a),
                excerpt_cmd(&b)
            ));
        }
    } else {
        let inverted_count = count_reordered_positions(&o_cmds, &r_cmds);
        rep.notes.push(format!(
            "[{name}] Harmless independent-edge reordering across {inverted_count} dry-run step(s) (identical command multiset and dependency order)."
        ));
    }
}

fn normalize_dep_edges(
    edges: &BTreeMap<BString, BTreeSet<BString>>,
    norm: &PathNormalizer,
) -> BTreeMap<BString, BTreeSet<BString>> {
    edges
        .iter()
        .map(|(k, deps)| {
            (
                norm.normalize(k),
                deps.iter().map(|d| norm.normalize(d)).collect(),
            )
        })
        .collect()
}

fn find_dependency_order_inversions(
    o_cmds: &[BString],
    r_cmds: &[BString],
    o_edges: &BTreeMap<BString, BTreeSet<BString>>,
    r_edges: &BTreeMap<BString, BTreeSet<BString>>,
) -> Vec<(BString, BString)> {
    // Map each command occurrence to its index in r_cmds.
    let mut r_positions: BTreeMap<&BString, Vec<usize>> = BTreeMap::new();
    for (idx, cmd) in r_cmds.iter().enumerate() {
        r_positions.entry(cmd).or_default().push(idx);
    }
    let mut used_occ: BTreeMap<&BString, usize> = BTreeMap::new();
    let mut perm: Vec<usize> = Vec::with_capacity(o_cmds.len());
    for cmd in o_cmds {
        let occ = used_occ.entry(cmd).or_insert(0);
        let pos = r_positions
            .get(cmd)
            .and_then(|v| v.get(*occ).copied())
            .unwrap_or(0);
        *occ += 1;
        perm.push(pos);
    }

    // Collect tokens mentioned in each command that match known graph outputs.
    let known_outputs: BTreeSet<&BString> = o_edges.keys().chain(r_edges.keys()).collect();
    let cmd_outputs: Vec<Vec<&BString>> = o_cmds
        .iter()
        .map(|cmd| {
            known_outputs
                .iter()
                .copied()
                .filter(|out| !out.is_empty() && contains_token(cmd, out))
                .collect()
        })
        .collect();

    let mut flips = Vec::new();
    let limit = o_cmds.len().min(2048);
    for i in 0..limit {
        for j in (i + 1)..limit {
            let (Some(&pi), Some(&pj)) = (perm.get(i), perm.get(j)) else {
                continue;
            };
            if pi <= pj {
                continue;
            }
            // i preceded j in oracle, but j precedes i in rcmake.
            let outs_i = cmd_outputs.get(i).map(Vec::as_slice).unwrap_or(&[]);
            let outs_j = cmd_outputs.get(j).map(Vec::as_slice).unwrap_or(&[]);
            if commands_dependent(outs_i, outs_j, o_edges)
                || commands_dependent(outs_i, outs_j, r_edges)
                || commands_dependent(outs_j, outs_i, o_edges)
                || commands_dependent(outs_j, outs_i, r_edges)
            {
                if let (Some(ci), Some(cj)) = (o_cmds.get(i), o_cmds.get(j)) {
                    flips.push((ci.clone(), cj.clone()));
                }
                if flips.len() >= MAX_EXAMPLES {
                    return flips;
                }
            }
        }
    }
    flips
}

fn commands_dependent(
    producer_outs: &[&BString],
    consumer_outs: &[&BString],
    edges: &BTreeMap<BString, BTreeSet<BString>>,
) -> bool {
    if producer_outs.is_empty() || consumer_outs.is_empty() {
        return false;
    }
    let prod_set: BTreeSet<&BString> = producer_outs.iter().copied().collect();
    // Bounded BFS from consumer_outs back through dependency inputs.
    let mut queue: Vec<&BString> = consumer_outs.to_vec();
    let mut seen: BTreeSet<&BString> = consumer_outs.iter().copied().collect();
    let mut steps = 0usize;
    while let Some(node) = queue.pop() {
        steps += 1;
        if steps > 512 {
            break;
        }
        let Some(deps) = edges.get(node) else {
            continue;
        };
        for dep in deps {
            if prod_set.contains(dep) {
                return true;
            }
            if seen.insert(dep) {
                queue.push(dep);
            }
        }
    }
    false
}

fn contains_token(cmd: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > cmd.len() {
        return false;
    }
    for (idx, win) in cmd.windows(needle.len()).enumerate() {
        if win == needle {
            let left_ok = idx == 0
                || cmd.get(idx - 1).is_some_and(|&b| {
                    b.is_ascii_whitespace() || b == b'"' || b == b'\'' || b == b'='
                });
            let right_ok = cmd
                .get(idx + needle.len())
                .is_none_or(|&b| b.is_ascii_whitespace() || b == b'"' || b == b'\'' || b == b';');
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

fn count_reordered_positions(a: &[BString], b: &[BString]) -> usize {
    a.iter().zip(b.iter()).filter(|(x, y)| x != y).count()
}

fn excerpt_cmd(cmd: &[u8]) -> String {
    let s = lossy(cmd);
    if s.len() <= 120 {
        s
    } else {
        format!("{}...", s.chars().take(117).collect::<String>())
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}
