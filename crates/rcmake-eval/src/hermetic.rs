// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configure input rules and diagnostics for hermetic mode.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, Stream, path::to_native};

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
    case_insensitive_roots: bool,
    roots: BTreeSet<PathBuf>,
    programs: BTreeSet<PathBuf>,
    pinned_network: bool,
    warnings: Vec<String>,
}

impl HermeticPolicy {
    /// Read the baseline and user allowlists through the host.
    ///
    /// Use semicolon-separated names in `RCMAKE_HERMETIC_ALLOW_ENV` and
    /// semicolon-separated absolute paths in `RCMAKE_HERMETIC_ALLOW_PATHS`.
    /// Pass source, build, module, toolchain, sysroot, and staging roots in
    /// `roots`. Use [Self::allow_program] for known tools in those roots or elsewhere.
    pub fn from_host<'a>(host: &dyn Host, roots: impl IntoIterator<Item = &'a Path>) -> Self {
        let system = host.system();
        let mut policy = Self {
            env: BTreeSet::new(),
            case_insensitive_env: system.sysname == "Windows",
            case_insensitive_roots: matches!(system.sysname.as_slice(), b"Darwin" | b"Windows"),
            roots: BTreeSet::new(),
            programs: BTreeSet::new(),
            pinned_network: true,
            warnings: Vec::new(),
        };
        for name in BASE_ENV {
            policy.allow_env(name);
        }
        for root in roots {
            policy.allow_root(host, root);
        }
        for (setting, is_env) in [
            ("RCMAKE_HERMETIC_ALLOW_ENV", true),
            ("RCMAKE_HERMETIC_ALLOW_PATHS", false),
        ] {
            let Some(value) = host
                .env_get(setting.as_bytes())
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            for (index, entry) in value.split(|byte| *byte == b';').enumerate() {
                let valid = if is_env {
                    policy.allow_env(entry.trim_ascii())
                } else {
                    policy.allow_root(host, &to_native(entry))
                };
                if !valid {
                    let expected = if is_env {
                        "a variable name"
                    } else {
                        "a resolvable absolute path"
                    };
                    let warning = format!(
                        "Warning: Ignored entry {} in {setting}. Specify {expected}.\n",
                        index + 1
                    );
                    host.write_out(Stream::Stderr, warning.as_bytes());
                    policy.warnings.push(warning);
                }
            }
        }
        policy
    }

    /// Return warnings for invalid allowlist entries.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
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

    /// Permit path aliases on hosts with case-insensitive filesystems.
    ///
    /// This still requires the host to report the same directory identity for
    /// an alias. A distinct directory on a case-sensitive volume stays outside
    /// the allowed root even when the names differ only by case.
    pub fn allow_case_insensitive_roots(&mut self, allow: bool) {
        self.case_insensitive_roots = allow;
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
        resolve_path(host, path).is_some_and(|path| {
            self.roots.iter().any(|root| {
                path.starts_with(root)
                    || (self.case_insensitive_roots && same_root(host, &path, root))
            })
        })
    }

    fn allows_program(&self, host: &dyn Host, path: &Path) -> bool {
        path.is_absolute()
            && host
                .canonicalize(path)
                .is_ok_and(|path| self.programs.contains(&path))
    }
}

fn same_root(host: &dyn Host, path: &Path, root: &Path) -> bool {
    let Some(depth) = path
        .components()
        .count()
        .checked_sub(root.components().count())
    else {
        return false;
    };
    let Some(ancestor) = path.ancestors().nth(depth) else {
        return false;
    };
    let (Ok(actual), Ok(allowed)) = (
        host.metadata(ancestor, Follow::No),
        host.metadata(root, Follow::No),
    ) else {
        return false;
    };
    actual.kind == FileKind::Dir
        && allowed.kind == FileKind::Dir
        && actual.ino != 0
        && (actual.dev, actual.ino) == (allowed.dev, allowed.ino)
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

/// Source location of the project command or expression that caused an input read.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceLocation {
    file: PathBuf,
    line: u32,
}

/// Source location supplied by the caller. Use the project site for generated code.
pub type HermeticSite = SourceLocation;

impl SourceLocation {
    /// Create a location with a 1-based line number.
    pub fn new(file: &Path, line: u32) -> Self {
        Self {
            file: file.into(),
            line,
        }
    }

    /// Return the original source path.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Return the 1-based source line.
    pub const fn line(&self) -> u32 {
        self.line
    }
}

impl fmt::Display for SourceLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for ch in self.file.to_string_lossy().chars() {
            write_display_char(formatter, ch)?;
        }
        write!(formatter, ":{}", self.line)
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
    hint: String,
}

impl HermeticViolation {
    /// Return an action the project or caller can take.
    pub fn hint(&self) -> &str {
        &self.hint
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
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuditSummary {
    /// Number of distinct records retained.
    pub recorded: usize,
    /// Number of observations omitted after the limit. This can include repeats.
    pub omitted: usize,
    /// Combined rule for distinct, valid environment names in retained records.
    pub allow_env: Option<String>,
}

impl fmt::Display for AuditSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.omitted != 0 {
            write!(
                formatter,
                "Additional hermetic violations omitted: {}.",
                self.omitted
            )?;
        }
        if let Some(names) = &self.allow_env {
            if self.omitted != 0 {
                formatter.write_char('\n')?;
            }
            write!(formatter, "RCMAKE_HERMETIC_ALLOW_ENV={names}")?;
        }
        Ok(())
    }
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum InputIdentity {
    Env(BString),
    Path(PathBuf),
    Remote(String, Option<String>),
}

/// Checks inputs and retains a bounded set of distinct records for one configure.
pub struct HermeticAuditor {
    mode: HermeticMode,
    policy: HermeticPolicy,
    records: BTreeMap<(HermeticSite, ViolationKind, InputIdentity), HermeticViolation>,
    reported_omissions: bool,
    record_limit: usize,
    omitted: usize,
}

impl HermeticAuditor {
    /// Start an audit with the default limit of 256 diagnostic records.
    pub fn new(mode: HermeticMode, policy: HermeticPolicy) -> Self {
        Self {
            mode,
            policy,
            records: BTreeMap::new(),
            reported_omissions: false,
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
        let (value, hint) = if valid_env_name(name) {
            let name = String::from_utf8_lossy(name);
            (
                display_text(&name),
                format!(
                    "Set RCMAKE_HERMETIC_ALLOW_ENV={name}. For a non-secret value, you can pass -D{name}=... and read ${{{name}}}. Keep secrets out of the CMake cache and command arguments."
                ),
            )
        } else {
            (
                "<invalid variable name>".into(),
                "Use a valid name in RCMAKE_HERMETIC_ALLOW_ENV. Keep secrets out of -D variables."
                    .into(),
            )
        };
        self.record(
            site,
            ViolationKind::UndeclaredEnvRead,
            InputIdentity::Env(self.policy.env_key(name)),
            value,
            hint,
        )
    }

    /// Check `DEFINED ENV{...}`. In warning mode the caller treats the variable as undefined.
    pub fn check_env_defined(&mut self, site: &HermeticSite, name: &[u8]) -> AuditOutcome<'_> {
        self.check_env(site, name)
    }

    /// Check an absolute path, including symlink traversal and parent components.
    ///
    /// The caller must enforce containment at open time if the filesystem can
    /// change between this check and the read. A separate canonicalization cannot
    /// prevent a symlink or ancestor from changing before access.
    pub fn check_path(
        &mut self,
        host: &dyn Host,
        site: &HermeticSite,
        path: &Path,
    ) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || self.policy.allows_path(host, path) {
            return allowed();
        }
        let directory = path.parent().filter(|parent| parent.is_absolute());
        let directory = directory.map_or_else(|| "<DIR>".into(), |dir| dir.to_string_lossy());
        let directory = if directory
            .chars()
            .any(|ch| ch.is_control() || is_format(ch) || ch == ';')
        {
            "<DIR>"
        } else {
            &directory
        };
        self.record(
            site,
            ViolationKind::OutOfTreePathRead,
            InputIdentity::Path(path.into()),
            display_input_path(path),
            format!("Declare the input root or set RCMAKE_HERMETIC_ALLOW_PATHS={directory}."),
        )
    }

    /// Check a direct network operation. Set `enforced_pin` only for a verified transfer.
    pub fn check_network(
        &mut self,
        site: &HermeticSite,
        target: &str,
        enforced_pin: bool,
    ) -> AuditOutcome<'_> {
        self.check_transfer(site, target, enforced_pin, None)
    }

    /// Check a FetchContent transfer at the project call site with its dependency name.
    pub fn check_fetch_network(
        &mut self,
        site: &HermeticSite,
        target: &str,
        enforced_pin: bool,
        dependency: &str,
    ) -> AuditOutcome<'_> {
        self.check_transfer(site, target, enforced_pin, Some(dependency))
    }

    fn check_transfer(
        &mut self,
        site: &HermeticSite,
        target: &str,
        enforced_pin: bool,
        dependency: Option<&str>,
    ) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || (enforced_pin && self.policy.pinned_network) {
            return allowed();
        }
        let hint = match dependency {
            Some(name) => {
                let name = if name.len() <= 256
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
                    && !name.is_empty()
                {
                    name.to_ascii_uppercase()
                } else {
                    "<NAME>".into()
                };
                format!(
                    "Verify the dependency and run --fetch-lock=update, or set -DFETCHCONTENT_SOURCE_DIR_{name}=... to a prepared source directory."
                )
            }
            None => "Use a local input or configure a transfer that verifies its content.".into(),
        };
        self.record(
            site,
            ViolationKind::NetworkAccess,
            InputIdentity::Remote(target.into(), dependency.map(str::to_owned)),
            display_remote(target),
            hint,
        )
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
            display_text(&program.to_string_lossy())
        } else {
            "<unresolved executable>".into()
        };
        self.record(
            site,
            ViolationKind::UnapprovedProcess,
            InputIdentity::Path(program.into()),
            identity,
            "Declare the executable by absolute path or remove the process call.".into(),
        )
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
        self.records.values()
    }

    /// Return the counts and a combined rule for multiple environment names.
    pub fn summary(&self) -> AuditSummary {
        let names: BTreeSet<_> = self
            .records
            .keys()
            .filter_map(|(_, _, identity)| match identity {
                InputIdentity::Env(name) if valid_env_name(name) => std::str::from_utf8(name).ok(),
                _ => None,
            })
            .collect();
        AuditSummary {
            recorded: self.records.len(),
            omitted: self.omitted,
            allow_env: (names.len() > 1).then(|| names.into_iter().collect::<Vec<_>>().join(";")),
        }
    }

    /// Return the final summary once if it has omissions or an environment rule.
    pub fn take_omission_summary(&mut self) -> Option<AuditSummary> {
        let summary = self.summary();
        if (summary.omitted == 0 && summary.allow_env.is_none()) || self.reported_omissions {
            return None;
        }
        self.reported_omissions = true;
        Some(summary)
    }

    fn record(
        &mut self,
        site: &HermeticSite,
        kind: ViolationKind,
        identity: InputIdentity,
        value: String,
        hint: String,
    ) -> AuditOutcome<'_> {
        let decision = match self.mode {
            HermeticMode::Off => return allowed(),
            HermeticMode::Warn => AuditDecision::Warn,
            HermeticMode::Strict => AuditDecision::Deny,
        };
        let full = self.records.len() >= self.record_limit;
        let diagnostic = match self.records.entry((site.clone(), kind, identity)) {
            std::collections::btree_map::Entry::Occupied(_) => None,
            std::collections::btree_map::Entry::Vacant(_) if full => {
                self.omitted = self.omitted.saturating_add(1);
                None
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                Some(&*slot.insert(HermeticViolation {
                    kind,
                    site: site.clone(),
                    value,
                    hint,
                }))
            }
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

fn display_input_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !path.is_absolute() && text.contains("://") {
        display_remote(&text)
    } else {
        display_text(&text)
    }
}

fn display_text(value: &str) -> String {
    let mut text = String::new();
    let mut chars = value.chars();
    for ch in chars.by_ref().take(MAX_DISPLAY_CHARS) {
        let _ = write_display_char(&mut text, ch);
    }
    if chars.next().is_some() {
        text.push_str("...");
    }
    text
}

fn write_display_char(writer: &mut impl fmt::Write, ch: char) -> fmt::Result {
    if is_format(ch) {
        write!(writer, "<U+{:04X}>", u32::from(ch))
    } else {
        writer.write_char(if ch.is_control() { '?' } else { ch })
    }
}

fn is_format(ch: char) -> bool {
    // Unicode 17.0 General_Category=Cf.
    matches!(ch,
        '\u{00ad}' | '\u{0600}'..='\u{0605}' | '\u{061c}' | '\u{06dd}' | '\u{070f}' |
        '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{180e}' | '\u{200b}'..='\u{200f}' |
        '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' |
        '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{110bd}' | '\u{110cd}' |
        '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' |
        '\u{e0001}' | '\u{e0020}'..='\u{e007f}'
    )
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
    fn keeps_untruncated_paths_for_deduplication() -> Result<(), &'static str> {
        let host = host();
        let mut auditor =
            HermeticAuditor::new(HermeticMode::Strict, HermeticPolicy::from_host(&host, []));
        let prefix = "a".repeat(200);
        let path = abs(&format!("/{prefix}/{prefix}/CMakeLists#dev?.txt"));
        let site = HermeticSite::new(&path, 12);
        assert_eq!(site.file(), path.as_path());
        assert!(site.to_string().ends_with("CMakeLists#dev?.txt:12"));
        for suffix in ["#one", "?two"] {
            let input = abs(&format!("/{prefix}/{prefix}/{suffix}"));
            let violation = auditor
                .check_path(&host, &site, &input)
                .diagnostic
                .ok_or("Missing diagnostic.")?;
            assert_eq!(violation.site, site);
        }
        assert_eq!(auditor.records().len(), 2);
        Ok(())
    }

    #[test]
    fn reports_invalid_entries_and_combines_env_names() -> Result<(), &'static str> {
        let host = host();
        host.env_set(b"RCMAKE_HERMETIC_ALLOW_ENV", Some(b"OK;BAD=private"));
        host.env_set(b"RCMAKE_HERMETIC_ALLOW_PATHS", Some(b"relative;another"));
        let policy = HermeticPolicy::from_host(&host, []);
        assert_eq!(policy.warnings().len(), 3);
        assert!(
            policy
                .warnings()
                .iter()
                .all(|warning| !warning.contains("private"))
        );
        let mut auditor = HermeticAuditor::new(HermeticMode::Warn, policy);
        for name in [b"B", b"A", b"C", b"B"] {
            auditor.check_env(&site(), name);
        }
        let summary = auditor
            .take_omission_summary()
            .ok_or("Missing environment rule.")?;
        assert_eq!(summary.to_string(), "RCMAKE_HERMETIC_ALLOW_ENV=A;B;C");
        assert!(auditor.take_omission_summary().is_none());
        assert_eq!(display_text("test\u{202e}"), "test<U+202E>");
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
        let denied = auditor.check_fetch_network(&site, url, false, "widget");
        assert_eq!(denied.decision, AuditDecision::Deny);
        let record = denied.diagnostic.ok_or("missing network diagnostic")?;
        assert_eq!(record.value, "https://example.org");
        assert!(record.hint().contains("--fetch-lock=update"));
        assert!(auditor.check_env(&site, b"FOO").diagnostic.is_none());
        assert_eq!(
            auditor.check_env(&site, b"BAR").decision,
            AuditDecision::Deny
        );
        assert_eq!(
            auditor.summary(),
            AuditSummary {
                recorded: 2,
                omitted: 1,
                allow_env: None,
            }
        );
        assert_eq!(auditor.records().len(), 2);
        assert!(auditor.take_omission_summary().is_some());
        assert!(auditor.take_omission_summary().is_none());
        Ok(())
    }
}
