// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configure input rules and diagnostics for hermetic mode.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    fmt,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use rcmake_base::BString;
use rcmake_host::{Follow, Host, path::to_native};

use crate::services::HermeticMode;

const BASE_ENV: &[&[u8]] = &[
    b"PATH",
    b"TMPDIR",
    b"TEMP",
    b"TMP",
    b"HOME",
    b"USERPROFILE",
    b"SYSTEMROOT",
    b"SOURCE_DATE_EPOCH",
];
const DEFAULT_RECORD_LIMIT: usize = 256;
const MAX_DISPLAY_CHARS: usize = 256;

/// Allowed inputs to one configure. The caller supplies roots and exact executable paths.
#[derive(Clone, Debug)]
pub struct HermeticPolicy {
    env: BTreeSet<BString>,
    case_insensitive_env: bool,
    roots: BTreeSet<PathBuf>,
    programs: BTreeSet<PathBuf>,
    pinned_network: bool,
}

impl HermeticPolicy {
    /// Read the baseline and user allowlists through the host.
    ///
    /// Use semicolon-separated names in `RCMAKE_HERMETIC_ALLOW_ENV` and
    /// semicolon-separated absolute paths in `RCMAKE_HERMETIC_ALLOW_PATHS`.
    /// Pass source, build, module, toolchain, sysroot, and staging roots in
    /// `roots`. Use [Self::allow_program] for known tools in those roots or elsewhere.
    pub fn from_host<'a>(host: &dyn Host, roots: impl IntoIterator<Item = &'a Path>) -> Self {
        let mut policy = Self {
            env: BTreeSet::new(),
            case_insensitive_env: host.system().sysname == "Windows",
            roots: BTreeSet::new(),
            programs: BTreeSet::new(),
            pinned_network: true,
        };
        for name in BASE_ENV {
            policy.allow_env(name);
        }
        for root in roots {
            policy.allow_root(host, root);
        }
        if let Some(names) = host.env_get(b"RCMAKE_HERMETIC_ALLOW_ENV") {
            for name in names.split(|byte| *byte == b';') {
                policy.allow_env(name.trim_ascii());
            }
        }
        if let Some(paths) = host.env_get(b"RCMAKE_HERMETIC_ALLOW_PATHS") {
            for root in paths
                .split(|byte| *byte == b';')
                .filter(|root| !root.is_empty())
            {
                policy.allow_root(host, &to_native(root));
            }
        }
        policy
    }

    /// Allow one environment variable. Return false for an invalid name.
    pub fn allow_env(&mut self, name: &[u8]) -> bool {
        if !valid_env_name(name) {
            return false;
        }
        self.env.insert(self.env_key(name));
        true
    }

    /// Allow reads below one absolute root. Return false if the root cannot be resolved.
    pub fn allow_root(&mut self, host: &dyn Host, root: &Path) -> bool {
        let Some(root) = resolve_path(host, root) else {
            return false;
        };
        self.roots.insert(root);
        true
    }

    /// Allow one existing, absolute executable path. The caller resolves tool names first.
    pub fn allow_program(&mut self, host: &dyn Host, program: &Path) -> bool {
        if !program.is_absolute() {
            return false;
        }
        let Ok(path) = host.canonicalize(program) else {
            return false;
        };
        self.programs.insert(path);
        true
    }

    /// Set whether a transfer may use network access when its fetch pin is enforced.
    pub fn allow_pinned_network(&mut self, allow: bool) {
        self.pinned_network = allow;
    }

    /// Return whether the variable can be read from the environment.
    pub fn allows_env(&self, name: &[u8]) -> bool {
        if self.case_insensitive_env {
            self.env.contains(&self.env_key(name))
        } else {
            self.env.contains(name)
        }
    }

    /// Keep allowed variables for a child process and preserve their order.
    pub fn filter_process_env(&self, env: Vec<(BString, BString)>) -> Vec<(BString, BString)> {
        env.into_iter()
            .filter(|(name, _)| self.allows_env(name))
            .collect()
    }

    fn env_key(&self, name: &[u8]) -> BString {
        if self.case_insensitive_env {
            name.to_ascii_uppercase().into()
        } else {
            name.into()
        }
    }

    fn allows_path(&self, host: &dyn Host, path: &Path) -> bool {
        resolve_path(host, path)
            .is_some_and(|path| self.roots.iter().any(|root| path.starts_with(root)))
    }

    fn allows_program(&self, host: &dyn Host, path: &Path) -> bool {
        path.is_absolute()
            && host
                .canonicalize(path)
                .is_ok_and(|path| self.programs.contains(&path))
    }
}

fn valid_env_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn resolve_path(host: &dyn Host, path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }

    // A build output can be absent. Resolve its nearest existing ancestor first.
    // Reject broken symlinks and missing ancestors followed by a parent component.
    for ancestor in path.ancestors() {
        match host.canonicalize(ancestor) {
            Ok(canonical) => {
                let suffix = path.strip_prefix(ancestor).ok()?;
                if suffix
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                {
                    return Some(canonical.join(suffix));
                }
                return None;
            }
            Err(error) if error.kind == ErrorKind::NotFound => {
                if !host
                    .metadata(ancestor, Follow::No)
                    .is_err_and(|error| error.kind == ErrorKind::NotFound)
                {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// Source location supplied by the command or expression that reads an input.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HermeticSite {
    file: String,
    line: u32,
}

impl HermeticSite {
    /// Create a location with a 1-based line number.
    pub fn new(file: &Path, line: u32) -> Self {
        Self {
            file: display_path(file),
            line,
        }
    }

    /// Return the file name as it appears in diagnostics.
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Return the 1-based source line.
    pub const fn line(&self) -> u32 {
        self.line
    }
}

impl fmt::Display for HermeticSite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.file, self.line)
    }
}

/// An input that falls outside the configure allowlists.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ViolationKind {
    /// A read of an environment variable outside the allowlist.
    UndeclaredEnvRead,
    /// A filesystem read outside the allowed roots.
    OutOfTreePathRead,
    /// Network access without an enforced fetch pin or policy permission.
    NetworkAccess,
    /// A process with an executable outside the allowlist.
    UnapprovedProcess,
}

impl ViolationKind {
    /// Return the stable code for this kind of violation.
    pub const fn code(self) -> &'static str {
        match self {
            Self::UndeclaredEnvRead => "RCMAKE_HERMETIC_ENV",
            Self::OutOfTreePathRead => "RCMAKE_HERMETIC_READ",
            Self::NetworkAccess => "RCMAKE_HERMETIC_NETWORK",
            Self::UnapprovedProcess => "RCMAKE_HERMETIC_PROCESS",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::UndeclaredEnvRead => "Read of an undeclared environment variable",
            Self::OutOfTreePathRead => "Read outside the allowed roots",
            Self::NetworkAccess => "Network access outside the fetch policy",
            Self::UnapprovedProcess => "Use of an unapproved executable",
        }
    }
}

/// A bounded diagnostic record. It contains no environment values or process arguments.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HermeticViolation {
    /// Kind of input.
    pub kind: ViolationKind,
    /// Source of the input read or operation.
    pub site: HermeticSite,
    /// Input name, path, or remote host with credentials removed.
    pub value: String,
}

impl HermeticViolation {
    /// Return an action the project or caller can take.
    pub fn hint(&self) -> String {
        match self.kind {
            ViolationKind::UndeclaredEnvRead if valid_env_name(self.value.as_bytes()) => format!(
                "Pass -D{0}=... and read ${{{0}}}, or add {0} to RCMAKE_HERMETIC_ALLOW_ENV.",
                self.value
            ),
            ViolationKind::UndeclaredEnvRead => {
                "Use a cache variable or add a valid name to RCMAKE_HERMETIC_ALLOW_ENV.".into()
            }
            ViolationKind::OutOfTreePathRead => {
                "Declare the input root or add its path to RCMAKE_HERMETIC_ALLOW_PATHS.".into()
            }
            ViolationKind::NetworkAccess => {
                "Pin the dependency in rcmake-fetch.lock and use the fetch cache.".into()
            }
            ViolationKind::UnapprovedProcess => {
                "Declare the executable by absolute path or remove the process call.".into()
            }
        }
    }
}

impl fmt::Display for HermeticViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {}: {} ({}). {}",
            self.site,
            self.kind.code(),
            self.kind.description(),
            self.value,
            self.hint()
        )
    }
}

/// Action for the caller after a policy check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditDecision {
    /// Continue without a violation.
    Allow,
    /// Continue and apply the warning-mode input rules.
    Warn,
    /// Reject the input or operation.
    Deny,
}

/// Result of a policy check.
#[derive(Debug)]
pub struct AuditOutcome<'a> {
    /// Action for this operation, including repeated violations.
    pub decision: AuditDecision,
    /// Diagnostic for a new violation that fits within the record limit.
    pub diagnostic: Option<&'a HermeticViolation>,
}

/// Count of retained diagnostics and observations beyond the record limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuditSummary {
    /// Number of distinct records retained.
    pub recorded: usize,
    /// Number of observations omitted after the limit. This can include repeats.
    pub omitted: usize,
}

impl fmt::Display for AuditSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Hermetic checks recorded {} violation(s) and omitted {} observation(s).",
            self.recorded, self.omitted
        )
    }
}

/// Checks inputs and retains a bounded set of distinct records for one configure.
pub struct HermeticAuditor {
    mode: HermeticMode,
    policy: HermeticPolicy,
    records: BTreeSet<HermeticViolation>,
    record_limit: usize,
    omitted: usize,
}

impl HermeticAuditor {
    /// Start an audit with the default limit of 256 diagnostic records.
    pub fn new(mode: HermeticMode, policy: HermeticPolicy) -> Self {
        Self {
            mode,
            policy,
            records: BTreeSet::new(),
            record_limit: DEFAULT_RECORD_LIMIT,
            omitted: 0,
        }
    }

    /// Use a lower record limit. A zero limit still enforces decisions.
    pub fn with_record_limit(mut self, limit: usize) -> Self {
        self.record_limit = limit.min(DEFAULT_RECORD_LIMIT);
        self
    }

    /// Check an environment read. In warning mode the caller supplies an empty value.
    pub fn check_env(&mut self, site: &HermeticSite, name: &[u8]) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || self.policy.allows_env(name) {
            return allowed();
        }
        let value = if valid_env_name(name) {
            display_text(&String::from_utf8_lossy(name))
        } else {
            "<invalid variable name>".into()
        };
        self.record(site, ViolationKind::UndeclaredEnvRead, value)
    }

    /// Check an absolute path, including symlink traversal and parent components.
    pub fn check_path(
        &mut self,
        host: &dyn Host,
        site: &HermeticSite,
        path: &Path,
    ) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || self.policy.allows_path(host, path) {
            return allowed();
        }
        self.record(site, ViolationKind::OutOfTreePathRead, display_path(path))
    }

    /// Check network access. Set `enforced_pin` only for a transfer that verifies its content.
    pub fn check_network(
        &mut self,
        site: &HermeticSite,
        target: &str,
        enforced_pin: bool,
    ) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || (enforced_pin && self.policy.pinned_network) {
            return allowed();
        }
        self.record(site, ViolationKind::NetworkAccess, display_remote(target))
    }

    /// Check the resolved executable path of one stage of a process pipeline.
    pub fn check_process(
        &mut self,
        host: &dyn Host,
        site: &HermeticSite,
        program: &Path,
    ) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || self.policy.allows_program(host, program) {
            return allowed();
        }
        let identity = if program.is_absolute() {
            display_path(program)
        } else {
            "<unresolved executable>".into()
        };
        self.record(site, ViolationKind::UnapprovedProcess, identity)
    }

    /// Supply the environment for a child process under the current mode.
    pub fn process_env(&self, env: Vec<(BString, BString)>) -> Vec<(BString, BString)> {
        if self.mode == HermeticMode::Off {
            env
        } else {
            self.policy.filter_process_env(env)
        }
    }

    /// Return retained diagnostic records in a stable order.
    pub fn records(&self) -> impl ExactSizeIterator<Item = &HermeticViolation> {
        self.records.iter()
    }

    /// Return the number of retained and omitted diagnostics.
    pub fn summary(&self) -> AuditSummary {
        AuditSummary {
            recorded: self.records.len(),
            omitted: self.omitted,
        }
    }

    fn record(
        &mut self,
        site: &HermeticSite,
        kind: ViolationKind,
        value: String,
    ) -> AuditOutcome<'_> {
        let decision = match self.mode {
            HermeticMode::Off => return allowed(),
            HermeticMode::Warn => AuditDecision::Warn,
            HermeticMode::Strict => AuditDecision::Deny,
        };
        let record = HermeticViolation {
            kind,
            site: site.clone(),
            value,
        };
        let diagnostic = if self.records.contains(&record) {
            None
        } else if self.records.len() == self.record_limit {
            self.omitted = self.omitted.saturating_add(1);
            None
        } else {
            self.records.insert(record.clone());
            self.records.get(&record)
        };
        AuditOutcome {
            decision,
            diagnostic,
        }
    }
}

fn allowed() -> AuditOutcome<'static> {
    AuditOutcome {
        decision: AuditDecision::Allow,
        diagnostic: None,
    }
}

fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path.contains("://") {
        display_remote(&path)
    } else {
        display_text(path.split(['?', '#']).next().unwrap_or_default())
    }
}

fn display_text(value: &str) -> String {
    let mut chars = value.chars();
    let mut text: String = chars
        .by_ref()
        .take(MAX_DISPLAY_CHARS)
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect();
    if chars.next().is_some() {
        text.push_str("...");
    }
    text
}

fn display_remote(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return "<remote destination>".into();
    };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if matches!(
        scheme,
        "https" | "http" | "ftp" | "ftps" | "ssh" | "git" | "s3"
    ) && !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    {
        display_text(&format!("{scheme}://{host}"))
    } else {
        "<remote destination>".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{path}"))
        } else {
            PathBuf::from(path)
        }
    }

    fn host() -> PureHost {
        PureHost::new(abs("/src"), abs("/bin/rcmake"))
    }

    fn site() -> HermeticSite {
        HermeticSite::new(&abs("/src/CMakeLists.txt"), 19)
    }

    #[test]
    fn off_warn_and_strict_apply_to_each_observation() {
        let host = host();
        for (mode, expected) in [
            (HermeticMode::Off, AuditDecision::Allow),
            (HermeticMode::Warn, AuditDecision::Warn),
            (HermeticMode::Strict, AuditDecision::Deny),
        ] {
            let mut auditor = HermeticAuditor::new(mode, HermeticPolicy::from_host(&host, []));
            let site = site();
            let outcome = auditor.check_env(&site, b"TOKEN");
            assert_eq!(outcome.decision, expected);
            assert_eq!(outcome.diagnostic.is_some(), mode != HermeticMode::Off);
            let duplicate = auditor.check_env(&site, b"TOKEN");
            assert_eq!(duplicate.decision, expected);
            assert!(duplicate.diagnostic.is_none());
        }
    }

    #[test]
    fn applies_environment_and_process_allowlists() -> Result<(), Box<dyn std::error::Error>> {
        let host = host();
        host.env_set(b"RCMAKE_HERMETIC_ALLOW_ENV", Some(b"SDKROOT; CUSTOM"));
        let compiler = abs("/src/bin/cc");
        host.seed_file(&compiler, b"")?;
        let mut policy = HermeticPolicy::from_host(&host, []);
        assert!(policy.allow_program(&host, &compiler));
        for name in [
            b"PATH".as_slice(),
            b"HOME",
            b"USERPROFILE",
            b"TMP",
            b"SDKROOT",
            b"CUSTOM",
        ] {
            assert!(policy.allows_env(name));
        }
        let mut auditor = HermeticAuditor::new(HermeticMode::Strict, policy);
        let env = vec![
            ("PATH".into(), "bin".into()),
            ("TOKEN".into(), "secret".into()),
        ];
        assert_eq!(
            auditor.process_env(env),
            vec![("PATH".into(), "bin".into())]
        );
        assert_eq!(
            auditor.check_process(&host, &site(), &compiler).decision,
            AuditDecision::Allow
        );
        assert_eq!(
            auditor
                .check_process(&host, &site(), Path::new("cc"))
                .decision,
            AuditDecision::Deny
        );
        Ok(())
    }

    #[test]
    fn checks_roots_by_component_and_blocks_symlink_escape()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = host();
        for path in [
            "/src/nested/input",
            "/src-other/input",
            "/sdk/include/input",
            "/outside/input",
        ] {
            host.seed_file(&abs(path), b"")?;
        }
        host.symlink(&abs("/outside"), &abs("/src/link"))?;
        let root = abs("/sdk");
        host.env_set(
            b"RCMAKE_HERMETIC_ALLOW_PATHS",
            Some(&rcmake_host::path::from_native(&root)),
        );
        let src = abs("/src");
        let policy = HermeticPolicy::from_host(&host, [src.as_path()]);
        assert!(policy.allows_path(&host, &abs("/src/nested/../nested/input")));
        assert!(policy.allows_path(&host, &abs("/src/new/output")));
        assert!(policy.allows_path(&host, &abs("/sdk/include/input")));
        assert!(!policy.allows_path(&host, &abs("/src/../src-other/input")));
        assert!(!policy.allows_path(&host, &abs("/src-other/input")));
        assert!(!policy.allows_path(&host, &abs("/src/link/input")));
        Ok(())
    }

    #[test]
    fn bounds_records_and_formats_clickable_diagnostics() -> Result<(), Box<dyn std::error::Error>>
    {
        let host = host();
        let mut auditor =
            HermeticAuditor::new(HermeticMode::Strict, HermeticPolicy::from_host(&host, []))
                .with_record_limit(2);
        let site = site();
        let record = auditor
            .check_env(&site, b"FOO")
            .diagnostic
            .ok_or("missing diagnostic")?;
        assert!(record.to_string().starts_with(&format!("{site}: ")));
        assert!(record.hint().contains("-DFOO=..."));
        assert!(record.hint().contains("${FOO}"));
        let url = "https://user:password@example.org/path?token=secret";
        assert_eq!(
            auditor.check_network(&site, url, true).decision,
            AuditDecision::Allow
        );
        let denied = auditor.check_network(&site, url, false);
        assert_eq!(denied.decision, AuditDecision::Deny);
        let record = denied.diagnostic.ok_or("missing network diagnostic")?;
        assert_eq!(record.value, "https://example.org");
        assert!(record.hint().contains("rcmake-fetch.lock"));
        assert!(auditor.check_env(&site, b"FOO").diagnostic.is_none());
        assert_eq!(
            auditor.check_env(&site, b"BAR").decision,
            AuditDecision::Deny
        );
        assert_eq!(
            auditor.summary(),
            AuditSummary {
                recorded: 2,
                omitted: 1
            }
        );
        assert_eq!(auditor.records().len(), 2);
        Ok(())
    }
}
