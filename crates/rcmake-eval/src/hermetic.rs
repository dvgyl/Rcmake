// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configure input rules and diagnostics for hermetic mode.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use rcmake_base::BString;
use rcmake_base::unicode::is_unicode_format;
use rcmake_host::{FileKind, Follow, Host, Stream, path::to_native};

use crate::services::HermeticMode;

mod context;

pub use context::{HermeticContext, HermeticSiteGuard, HermeticSources};

const BASE_ENV: &[&[u8]] = &[
    b"PATH",
    b"TMPDIR",
    b"TEMP",
    b"TMP",
    b"HOME",
    b"USERPROFILE",
    b"SYSTEMROOT",
    b"SOURCE_DATE_EPOCH",
    b"CMAKE_BUILD_TYPE",
    b"CMAKE_MAXIMUM_RECURSION_DEPTH",
    b"CMAKE_POLICY_VERSION_MINIMUM",
    b"CMAKE_TLS_VERIFY",
    b"CMAKE_TLS_VERSION",
    b"RCMAKE_FETCH_OFFLINE",
    b"RCMAKE_FETCH_GIT_READ_ONLY",
];
const USER_RECORD_LIMIT: usize = 256;
const BUNDLED_RECORD_LIMIT: usize = 64;
const MAX_DISPLAY_CHARS: usize = 256;

/// Allowed inputs to one configure. The caller supplies roots and exact executable paths.
#[derive(Clone, Debug)]
pub struct HermeticPolicy {
    env: BTreeSet<BString>,
    case_insensitive_env: bool,
    case_insensitive_roots: bool,
    roots: BTreeSet<PathBuf>,
    exact_paths: BTreeSet<PathBuf>,
    bundled_roots: BTreeSet<PathBuf>,
    programs: BTreeSet<PathBuf>,
    warnings: Vec<String>,
}

impl HermeticPolicy {
    /// Read the baseline and user allowlists through the host.
    ///
    /// Use semicolon-separated names in `RCMAKE_HERMETIC_ALLOW_ENV` and
    /// semicolon-separated absolute paths in `RCMAKE_HERMETIC_ALLOW_PATHS`
    /// and `RCMAKE_HERMETIC_ALLOW_PROGRAMS`.
    /// Pass source, build, module, toolchain, sysroot, and staging roots in
    /// `roots`. Use [Self::allow_program] for known tools in those roots or elsewhere.
    pub fn from_host<'a>(host: &dyn Host, roots: impl IntoIterator<Item = &'a Path>) -> Self {
        let system = host.system();
        let mut policy = Self {
            env: BTreeSet::new(),
            case_insensitive_env: system.sysname == "Windows",
            case_insensitive_roots: matches!(system.sysname.as_slice(), b"Darwin" | b"Windows"),
            roots: BTreeSet::new(),
            exact_paths: BTreeSet::new(),
            bundled_roots: BTreeSet::new(),
            programs: BTreeSet::new(),
            warnings: Vec::new(),
        };
        for name in BASE_ENV {
            policy.allow_env(name);
        }
        for root in roots {
            policy.allow_root(host, root);
        }
        let settings = [
            ("RCMAKE_HERMETIC_ALLOW_ENV", "a variable name"),
            ("RCMAKE_HERMETIC_ALLOW_PATHS", "a resolvable absolute path"),
            (
                "RCMAKE_HERMETIC_ALLOW_PROGRAMS",
                "an absolute executable path",
            ),
        ];
        for (setting, expected) in settings {
            let Some(value) = host
                .env_get(setting.as_bytes())
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            for (index, entry) in value.split(|byte| *byte == b';').enumerate() {
                let valid = match setting {
                    "RCMAKE_HERMETIC_ALLOW_ENV" => policy.allow_env(entry.trim_ascii()),
                    "RCMAKE_HERMETIC_ALLOW_PATHS" => policy.allow_root(host, &to_native(entry)),
                    _ => policy.allow_program(host, &to_native(entry)),
                };
                if !valid {
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

    /// Allow reads of one exact absolute path, such as a sibling entry point.
    pub fn allow_exact_path(&mut self, host: &dyn Host, path: &Path) -> bool {
        let Some(resolved) = resolve_path(host, path) else {
            return false;
        };
        self.exact_paths.insert(resolved);
        true
    }

    /// Mark one directory root as belonging to bundled CMake modules or templates.
    pub fn allow_bundled_root(&mut self, host: &dyn Host, root: &Path) -> bool {
        if !root.is_absolute() {
            return false;
        }
        let cleaned = clean_path(root);
        let mut added = self.bundled_roots.insert(cleaned);
        if let Some(resolved) = resolve_path(host, root) {
            added |= self.bundled_roots.insert(clean_path(&resolved));
        }
        added || !self.bundled_roots.is_empty()
    }

    /// Replace the set of bundled CMake module and template roots.
    pub fn set_bundled_roots<'a>(
        &mut self,
        host: &dyn Host,
        roots: impl IntoIterator<Item = &'a Path>,
    ) {
        self.bundled_roots.clear();
        for root in roots {
            self.allow_bundled_root(host, root);
        }
    }

    /// Return whether `site` originates inside one of the bundled module or template roots.
    #[must_use]
    pub fn is_bundled_site(&self, site: &HermeticSite) -> bool {
        if self.bundled_roots.is_empty() || !site.file().is_absolute() {
            return false;
        }
        let cleaned = clean_path(site.file());
        let file = windows_path_spelling(&cleaned);
        self.bundled_roots.iter().any(|root| {
            let root = windows_path_spelling(root);
            path_has_prefix(file.as_ref(), root.as_ref(), self.case_insensitive_roots)
        })
    }

    /// Allow one explicitly approved, existing, absolute executable path.
    ///
    /// Use [Self::allow_program_in_roots] for automatic host toolchain discovery so
    /// symlinks cannot escape verified system directories.
    pub fn allow_program(&mut self, host: &dyn Host, program: &Path) -> bool {
        if !program.is_absolute()
            || !host.metadata(program, Follow::Yes).is_ok_and(|metadata| {
                metadata.kind == FileKind::File
                    && (self.case_insensitive_env || metadata.mode & 0o111 != 0)
            })
        {
            return false;
        }
        let Ok(path) = host.canonicalize(program) else {
            return false;
        };
        self.programs.insert(path);
        true
    }

    /// Allow one existing, absolute executable path only when its canonical target
    /// is a regular executable file inside one of `trusted_roots`.
    pub fn allow_program_in_roots<'a>(
        &mut self,
        host: &dyn Host,
        program: &Path,
        trusted_roots: impl IntoIterator<Item = &'a Path>,
    ) -> bool {
        if !program.is_absolute() {
            return false;
        }
        let Ok(canonical) = host.canonicalize(program) else {
            return false;
        };
        if !host.metadata(&canonical, Follow::No).is_ok_and(|metadata| {
            metadata.kind == FileKind::File
                && (self.case_insensitive_env || metadata.mode & 0o111 != 0)
        }) {
            return false;
        }
        let canon_spelled = windows_path_spelling(&canonical);
        let in_trusted_root = trusted_roots.into_iter().any(|root| {
            if !root.is_absolute() {
                return false;
            }
            let Ok(resolved_root) = host.canonicalize(root) else {
                return false;
            };
            if !host
                .metadata(&resolved_root, Follow::No)
                .is_ok_and(|metadata| metadata.kind == FileKind::Dir)
            {
                return false;
            }
            let root_spelled = windows_path_spelling(&resolved_root);
            canon_spelled.as_ref().starts_with(root_spelled.as_ref())
        });
        if !in_trusted_root {
            return false;
        }
        self.programs.insert(canonical);
        true
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

    fn resolve_allowed_path(
        &self,
        host: &dyn Host,
        path: &Path,
        fetch_cache: Option<&Path>,
    ) -> Option<PathBuf> {
        resolve_path(host, path).filter(|path| {
            let contains = |root: &Path| {
                let (path, root) = (windows_path_spelling(path), windows_path_spelling(root));
                path.starts_with(root.as_ref())
                    || (self.case_insensitive_roots && same_root(host, &path, &root))
            };
            self.roots.iter().any(|root| contains(root))
                || self.programs.contains(path)
                || self.exact_paths.contains(path)
                || fetch_cache
                    .and_then(|root| resolve_path(host, root))
                    .is_some_and(|root| contains(&root))
        })
    }

    fn allows_program(&self, host: &dyn Host, path: &Path) -> bool {
        path.is_absolute()
            && host
                .canonicalize(path)
                .is_ok_and(|path| self.programs.contains(&path))
    }
}

/// Return a drive or network path without its verbatim Windows prefix.
pub fn windows_path_spelling(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::borrow::Cow;
    if cfg!(windows)
        && let Some(text) = path.to_str().and_then(|text| text.strip_prefix(r"\\?\"))
    {
        if let Some(network) = text.strip_prefix(r"UNC\") {
            return Cow::Owned(PathBuf::from(format!(r"\\{network}")));
        }
        if matches!(text.as_bytes(), [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic()) {
            return Cow::Borrowed(Path::new(text));
        }
    }
    Cow::Borrowed(path)
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

fn clean_path(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                cleaned.pop();
            }
            other => cleaned.push(other),
        }
    }
    cleaned
}

fn path_has_prefix(path: &Path, root: &Path, case_insensitive: bool) -> bool {
    if path.starts_with(root) {
        return true;
    }
    if !case_insensitive {
        return false;
    }
    let mut path_components = path.components();
    for root_part in root.components() {
        let Some(path_part) = path_components.next() else {
            return false;
        };
        if !path_part
            .as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(root_part.as_os_str().as_encoded_bytes())
        {
            return false;
        }
    }
    true
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
                if suffix.as_os_str().is_empty() {
                    return Some(canonical);
                }
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

    /// Location for input reads performed during startup before any listfile command runs.
    #[must_use]
    pub fn startup() -> Self {
        Self {
            file: PathBuf::from("<rcmake startup>"),
            line: 0,
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
        if self.line != 0 {
            write!(formatter, ":{}", self.line)?;
        }
        Ok(())
    }
}

/// An input that falls outside the configure allowlists.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ViolationKind {
    /// A read of an environment variable outside the allowlist.
    UndeclaredEnvRead,
    /// A read of the Windows registry.
    RegistryRead,
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
            Self::RegistryRead => "RCMAKE_HERMETIC_REGISTRY",
            Self::OutOfTreePathRead => "RCMAKE_HERMETIC_READ",
            Self::NetworkAccess => "RCMAKE_HERMETIC_NETWORK",
            Self::UnapprovedProcess => "RCMAKE_HERMETIC_PROCESS",
        }
    }

    /// Return the human-readable description for this kind of violation.
    pub const fn description(self) -> &'static str {
        match self {
            Self::UndeclaredEnvRead => "Read of an undeclared environment variable",
            Self::RegistryRead => "Read of the Windows registry",
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
    /// Number of times this `(kind, identity)` violation occurred.
    pub occurrences: usize,
    /// Whether every observed occurrence came from a bundled CMake module.
    pub bundled: bool,
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
    /// Continue and withhold the external value without recording a violation.
    Withhold,
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
    /// Number of retained records attributed to user project code.
    pub user: usize,
    /// Number of retained records attributed to bundled CMake modules.
    pub bundled: usize,
    /// Number of user observations omitted after the user record limit.
    pub omitted_user: usize,
    /// Number of bundled observations omitted after the bundled record limit.
    pub omitted_bundled: usize,
    /// Combined rule for distinct, valid environment names in retained records.
    pub allow_env: Option<String>,
}

impl fmt::Display for AuditSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(names) = &self.allow_env {
            write!(formatter, "RCMAKE_HERMETIC_ALLOW_ENV={names}")?;
        }
        Ok(())
    }
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
enum InputIdentity {
    Registry,
    Env(BString),
    Path(PathBuf),
    Remote(String, Option<String>),
}

/// Checks inputs and retains a bounded set of distinct records for one configure.
pub struct HermeticAuditor {
    mode: HermeticMode,
    policy: HermeticPolicy,
    records: BTreeMap<(ViolationKind, InputIdentity), HermeticViolation>,
    site_bundled_cache: BTreeMap<PathBuf, bool>,
    reported_omissions: bool,
    user_limit: usize,
    bundled_limit: usize,
    user_records: usize,
    bundled_records: usize,
    omitted_user: usize,
    omitted_bundled: usize,
}

impl HermeticAuditor {
    /// Start an audit with the default limits of 256 user and 64 bundled records.
    pub fn new(mode: HermeticMode, policy: HermeticPolicy) -> Self {
        Self {
            mode,
            policy,
            records: BTreeMap::new(),
            site_bundled_cache: BTreeMap::new(),
            reported_omissions: false,
            user_limit: USER_RECORD_LIMIT,
            bundled_limit: BUNDLED_RECORD_LIMIT,
            user_records: 0,
            bundled_records: 0,
            omitted_user: 0,
            omitted_bundled: 0,
        }
    }

    /// Use a lower record limit. A zero limit still enforces decisions.
    pub fn with_record_limit(mut self, limit: usize) -> Self {
        self.user_limit = limit.min(USER_RECORD_LIMIT);
        self.bundled_limit = limit.min(BUNDLED_RECORD_LIMIT);
        self
    }

    /// Add an input root from the configure request.
    pub fn allow_root(&mut self, host: &dyn Host, path: &Path) -> bool {
        self.policy.allow_root(host, path)
    }

    /// Mark a directory root as belonging to bundled CMake modules or templates.
    pub fn allow_bundled_root(&mut self, host: &dyn Host, path: &Path) -> bool {
        self.site_bundled_cache.clear();
        self.policy.allow_bundled_root(host, path)
    }

    /// Return whether `site` originates inside one of the bundled module or template roots.
    pub fn is_bundled_site(&mut self, site: &HermeticSite) -> bool {
        if let Some(&cached) = self.site_bundled_cache.get(site.file()) {
            return cached;
        }
        let bundled = self.policy.is_bundled_site(site);
        self.site_bundled_cache
            .insert(site.file().to_owned(), bundled);
        bundled
    }

    /// Record a registry read as an external input.
    pub fn check_registry(&mut self, site: &HermeticSite) -> AuditOutcome<'_> {
        let bundled = self.is_bundled_site(site);
        self.record(
            site,
            ViolationKind::RegistryRead,
            InputIdentity::Registry,
            "Windows registry".into(),
            "Pass the path as a configure input.".into(),
            bundled,
        )
    }

    /// Check an environment read. In warning mode the caller supplies an empty value.
    pub fn check_env(&mut self, site: &HermeticSite, name: &[u8]) -> AuditOutcome<'_> {
        if self.mode == HermeticMode::Off || self.policy.allows_env(name) {
            return allowed();
        }
        if self.is_bundled_site(site) {
            return AuditOutcome {
                decision: AuditDecision::Withhold,
                diagnostic: None,
            };
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
            false,
        )
    }

    /// Return whether `path` is permitted for reading without recording a violation.
    #[must_use]
    pub fn allows_read_path(
        &self,
        host: &dyn Host,
        path: &Path,
        fetch_cache: Option<&Path>,
    ) -> bool {
        self.mode == HermeticMode::Off
            || self
                .policy
                .resolve_allowed_path(host, path, fetch_cache)
                .is_some()
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
        self.check_read_path(host, site, path, None).0
    }

    /// Check a read and return the resolved path when the policy allows it.
    ///
    /// Use the resolved path for reads that follow symbolic links. The host must
    /// still constrain lookup if directory entries can change before access.
    pub fn check_read_path(
        &mut self,
        host: &dyn Host,
        site: &HermeticSite,
        path: &Path,
        fetch_cache: Option<&Path>,
    ) -> (AuditOutcome<'_>, Option<PathBuf>) {
        if self.mode == HermeticMode::Off {
            return (allowed(), None);
        }
        if let Some(path) = self.policy.resolve_allowed_path(host, path, fetch_cache) {
            return (allowed(), Some(path));
        }
        let bundled = self.is_bundled_site(site);
        // Never suggest the filesystem root. For a child of the root, suggest the child.
        let directory = path
            .parent()
            .filter(|parent| parent.is_absolute())
            .map(|parent| {
                if parent.parent().is_none() {
                    path
                } else {
                    parent
                }
            });
        let directory = directory.map_or_else(|| "<DIR>".into(), |dir| dir.to_string_lossy());
        let directory = if directory
            .chars()
            .any(|ch| ch.is_control() || is_unicode_format(ch) || ch == ';')
        {
            "<DIR>"
        } else {
            &directory
        };
        let is_executable = path.is_absolute()
            && host.metadata(path, Follow::Yes).is_ok_and(|meta| {
                meta.kind == FileKind::File
                    && (self.policy.case_insensitive_env || meta.mode & 0o111 != 0)
            });
        let hint = if is_executable {
            format!(
                "Allow this tool with RCMAKE_HERMETIC_ALLOW_PROGRAMS={} or set RCMAKE_HERMETIC_ALLOW_PATHS={directory}.",
                display_input_path(path)
            )
        } else {
            format!("Declare the input root or set RCMAKE_HERMETIC_ALLOW_PATHS={directory}.")
        };
        let outcome = self.record(
            site,
            ViolationKind::OutOfTreePathRead,
            InputIdentity::Path(path.into()),
            display_input_path(path),
            hint,
            bundled,
        );
        (outcome, None)
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
        if self.mode == HermeticMode::Off || enforced_pin {
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
            false,
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
        let bundled = self.is_bundled_site(site);
        let identity = if program.is_absolute() {
            display_text(&program.to_string_lossy())
        } else {
            format!(
                "<unresolved executable {}>",
                display_text(&program.to_string_lossy())
            )
        };
        let hint = if program.is_absolute() {
            format!("Allow this tool with RCMAKE_HERMETIC_ALLOW_PROGRAMS={identity}.")
        } else {
            "Set RCMAKE_HERMETIC_ALLOW_PROGRAMS to the absolute executable path.".into()
        };
        self.record(
            site,
            ViolationKind::UnapprovedProcess,
            InputIdentity::Path(program.into()),
            identity,
            hint,
            bundled,
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

    /// Return whether a variable may pass to a child process.
    pub fn allows_process_env(&self, name: &[u8]) -> bool {
        self.mode == HermeticMode::Off || self.policy.allows_env(name)
    }

    /// Return retained diagnostic records in a stable order: user records first, then bundled.
    pub fn records(&self) -> impl ExactSizeIterator<Item = &HermeticViolation> {
        let mut ordered: Vec<&HermeticViolation> = self.records.values().collect();
        ordered.sort_by(|left, right| {
            left.bundled
                .cmp(&right.bundled)
                .then_with(|| left.site.file().cmp(right.site.file()))
                .then_with(|| left.site.line().cmp(&right.site.line()))
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.value.cmp(&right.value))
        });
        ordered.into_iter()
    }

    /// Return the counts and a combined rule for multiple environment names.
    pub fn summary(&self) -> AuditSummary {
        let names: BTreeSet<_> = self
            .records
            .iter()
            .filter(|(_, record)| !record.bundled)
            .filter_map(|((_, identity), _)| match identity {
                InputIdentity::Env(name) if valid_env_name(name) => std::str::from_utf8(name).ok(),
                _ => None,
            })
            .collect();
        AuditSummary {
            recorded: self.records.len(),
            omitted: self.omitted_user.saturating_add(self.omitted_bundled),
            user: self.user_records,
            bundled: self.bundled_records,
            omitted_user: self.omitted_user,
            omitted_bundled: self.omitted_bundled,
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
        bundled: bool,
    ) -> AuditOutcome<'_> {
        let decision = match self.mode {
            HermeticMode::Off => return allowed(),
            HermeticMode::Warn => AuditDecision::Warn,
            HermeticMode::Strict => AuditDecision::Deny,
        };
        let user_full = self.user_records >= self.user_limit;
        let bundled_full = self.bundled_records >= self.bundled_limit;
        let diagnostic = match self.records.entry((kind, identity)) {
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let record = slot.get_mut();
                record.occurrences = record.occurrences.saturating_add(1);
                if record.bundled && !bundled {
                    if user_full {
                        self.omitted_user = self.omitted_user.saturating_add(1);
                        None
                    } else {
                        record.site = site.clone();
                        record.bundled = false;
                        record.hint = hint;
                        self.bundled_records = self.bundled_records.saturating_sub(1);
                        self.user_records = self.user_records.saturating_add(1);
                        Some(&*slot.into_mut())
                    }
                } else {
                    None
                }
            }
            std::collections::btree_map::Entry::Vacant(_) if bundled && bundled_full => {
                self.omitted_bundled = self.omitted_bundled.saturating_add(1);
                None
            }
            std::collections::btree_map::Entry::Vacant(_) if !bundled && user_full => {
                self.omitted_user = self.omitted_user.saturating_add(1);
                None
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                if bundled {
                    self.bundled_records = self.bundled_records.saturating_add(1);
                } else {
                    self.user_records = self.user_records.saturating_add(1);
                }
                Some(&*slot.insert(HermeticViolation {
                    kind,
                    site: site.clone(),
                    value,
                    occurrences: 1,
                    bundled,
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
    if is_unicode_format(ch) {
        write!(writer, "<U+{:04X}>", u32::from(ch))
    } else {
        writer.write_char(if ch.is_control() { '?' } else { ch })
    }
}

/// Return the scheme and host of a remote destination, without credentials or parameters.
pub fn display_remote(value: &str) -> String {
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
mod tests;
