// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configure host adapter for hermetic input checks.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rcmake_base::path_lex::Lexical;
use rcmake_diag::rich::{RichOptions, SourceLookup, render_rich};
use rcmake_diag::{
    Diagnostic, Effect, Location, MessageKind, RenderOptions, SourceFile, SourceId, SourceMap,
    render,
};
use rcmake_eval::hermetic::{
    AuditDecision, AuditOutcome, HermeticAuditor, HermeticContext, HermeticPolicy, HermeticSite,
    HermeticSources, HermeticViolation, ViolationKind,
};
use rcmake_eval::{CmdError, Cx, HermeticMode, Interp, Services};
use rcmake_host::{
    FileKind, Follow, Host, HostError, HostResult, Stream, discover_cmake_root, path,
};

mod host;
mod report;
mod transfer;

use report::{InputKind, Observations};

const MAX_WARN_USER_VIOLATIONS: usize = 20;
const SIBLING_ENTRYPOINTS: &[&str] = &["rcmake", "cmake", "ctest", "cpack", "rcmake-lsp"];
const TOOLCHAIN_TOOLS: &[&str] = &[
    "ninja",
    "ninja-build",
    "samu",
    "make",
    "gmake",
    "smake",
    "nmake",
    "cc",
    "CC",
    "c89",
    "c99",
    "c++",
    "clang",
    "clang++",
    "gcc",
    "g++",
    "cl",
    "clang-cl",
    "ar",
    "gcc-ar",
    "llvm-ar",
    "ranlib",
    "gcc-ranlib",
    "llvm-ranlib",
    "ld",
    "lld",
    "ld.lld",
    "lld-link",
    "ld.gold",
    "ld.bfd",
    "mold",
    "ld_classic",
    "strip",
    "llvm-strip",
    "nm",
    "gcc-nm",
    "llvm-nm",
    "objdump",
    "llvm-objdump",
    "objcopy",
    "llvm-objcopy",
    "readelf",
    "llvm-readelf",
    "dlltool",
    "llvm-dlltool",
    "addr2line",
    "llvm-addr2line",
    "clang-scan-deps",
    "tapi",
    "install_name_tool",
    "llvm-install-name-tool",
    "lipo",
    "otool",
    "dsymutil",
    "libtool",
    "codesign_allocate",
    "lib",
    "link",
    "mt",
    "rc",
];

pub(crate) struct HermeticRuntime {
    host: Arc<HermeticHost>,
    build: PathBuf,
}

impl HermeticRuntime {
    pub(crate) fn new(
        host: Arc<dyn Host>,
        source: &Path,
        build: &Path,
        services: &Services,
    ) -> Option<Self> {
        let mode = services.speed.hermetic;
        if mode == HermeticMode::Off {
            return None;
        }
        let sysname = host.system().sysname;
        let windows = sysname == b"Windows";
        let (exe, modules) = discover_cmake_root(host.as_ref(), None);
        let roots = [source, build].into_iter().chain(modules.as_deref());
        let mut policy = HermeticPolicy::from_host(host.as_ref(), roots);
        policy.allow_program(host.as_ref(), &exe);
        policy.allow_exact_path(host.as_ref(), &exe);
        allow_sibling_entrypoints(&mut policy, host.as_ref(), &exe, windows);
        allow_host_toolchain(&mut policy, host.as_ref(), sysname.as_slice());
        if let Some(modules) = modules.as_deref() {
            for child in ["Modules", "Templates"] {
                let dir = modules.join(child);
                policy.allow_root(host.as_ref(), &dir);
                policy.allow_bundled_root(host.as_ref(), &dir);
            }
        }
        let context = HermeticContext::new(HermeticSite::startup());
        let inputs = Observations::new(host.as_ref(), source, build, modules.as_deref());
        Some(Self {
            host: Arc::new(HermeticHost {
                windows,
                inner: host,
                mode,
                context,
                sources: HermeticSources::default(),
                state: Mutex::new(AuditState {
                    auditor: HermeticAuditor::new(mode, policy),
                    inputs,
                    rich: None,
                    reported_summary: false,
                }),
            }),
            build: build.into(),
        })
    }

    pub(crate) fn host(&self) -> Arc<dyn Host> {
        self.host.clone()
    }

    pub(crate) fn install(&self, interp: &mut Interp) {
        if let Some(&rich) = interp.cx().session_data::<RichOptions>() {
            self.host.state().rich = Some(rich);
        }
        self.host.sources.extend_from(interp.diagnostic_sources());
        interp.add_session_data(self.host.context.clone());
        interp.add_session_data(self.host.sources.clone());
        interp.add_session_data(self.host.clone());
    }

    pub(crate) fn allow_root(&self, path: &Path) {
        let mut state = self.host.state();
        let inner = self.host.inner.as_ref();
        state.auditor.allow_root(inner, path);
        for child in ["Modules", "Templates"] {
            let dir = path.join(child);
            state.auditor.allow_root(inner, &dir);
            state.auditor.allow_bundled_root(inner, &dir);
        }
        state.inputs.add_module_root(inner, path);
    }

    pub(crate) fn denied(&self) -> bool {
        self.host.context.denied()
    }

    pub(crate) fn finish(&self) {
        let (emit_warn, user_violations, summary, rich, contents) = {
            let mut state = self.host.state();
            let emit_warn = self.host.mode == HermeticMode::Warn && !state.reported_summary;
            let user_violations = if emit_warn {
                state
                    .auditor
                    .records()
                    .filter(|record| !record.bundled)
                    .take(MAX_WARN_USER_VIOLATIONS)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let summary = if emit_warn {
                state.reported_summary = true;
                Some(state.auditor.summary())
            } else {
                None
            };
            let rich = state.rich;
            let contents = report::render(&state, self.host.mode, self.denied());
            (emit_warn, user_violations, summary, rich, contents)
        };
        let host = self.host.inner.as_ref();
        if emit_warn {
            for violation in &user_violations {
                write_violation(host, &self.host.sources, violation, false, rich);
            }
            if let Some(summary) = summary {
                let user_total = summary.user.saturating_add(summary.omitted_user);
                let bundled_total = summary.bundled.saturating_add(summary.omitted_bundled);
                if user_total != 0 || bundled_total != 0 {
                    let report_path = self.build.join("CMakeFiles/rcmake/inputs.v1.json");
                    let line = format!(
                        "{user_total} hermetic violations in your project, {bundled_total} in bundled CMake modules. Full list: {}\n",
                        report_path.display()
                    );
                    host.write_out(Stream::Stderr, line.as_bytes());
                }
                if let Some(names) = summary.allow_env {
                    host.write_out(
                        Stream::Stderr,
                        format!("RCMAKE_HERMETIC_ALLOW_ENV={names}\n").as_bytes(),
                    );
                }
            }
        }
        if let Err(error) = report::publish(host, &self.build, &contents) {
            host.write_out(
                Stream::Stderr,
                format!(
                    "Warning: Cannot write CMakeFiles/rcmake/inputs.v1.json ({:?}).\n",
                    error.kind
                )
                .as_bytes(),
            );
        }
    }
}

fn allow_sibling_entrypoints(
    policy: &mut HermeticPolicy,
    host: &dyn Host,
    exe: &Path,
    windows: bool,
) {
    let suffix = if windows { ".exe" } else { "" };
    let canon_exe = host.canonicalize(exe).ok();
    let canon_dir = canon_exe
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_owned);
    let mut parents = Vec::new();
    if let Some(parent) = exe.parent() {
        parents.push(parent.to_owned());
    }
    if let Some(parent) = canon_dir.as_ref()
        && !parents.iter().any(|existing| existing == parent)
    {
        parents.push(parent.clone());
    }
    for parent in parents {
        for name in SIBLING_ENTRYPOINTS {
            let expected_name = format!("{name}{suffix}");
            let candidate = parent.join(&expected_name);
            match host.metadata(&candidate, Follow::No) {
                Ok(_) => {}
                Err(err) if err.kind == std::io::ErrorKind::NotFound => {
                    // Allow startup existence probes for absent sibling names without
                    // approving execution or trusting any symlink.
                    policy.allow_exact_path(host, &candidate);
                    continue;
                }
                Err(_) => continue,
            }
            let Ok(target) = host.canonicalize(&candidate) else {
                continue;
            };
            let is_exec = host.metadata(&target, Follow::No).is_ok_and(|meta| {
                meta.kind == FileKind::File && (windows || meta.mode & 0o111 != 0)
            });
            if !is_exec {
                continue;
            }
            let same_binary = canon_exe.as_deref() == Some(target.as_path());
            let same_stage_tool = canon_dir.as_deref().is_some_and(|dir| {
                target.parent() == Some(dir)
                    && target
                        .file_name()
                        .and_then(|file| file.to_str())
                        .is_some_and(|file| {
                            if windows {
                                file.eq_ignore_ascii_case(&expected_name)
                            } else {
                                file == expected_name
                            }
                        })
            });
            if same_binary || same_stage_tool {
                policy.allow_exact_path(host, &candidate);
                policy.allow_program(host, &candidate);
            }
        }
    }
}

fn allow_host_toolchain(policy: &mut HermeticPolicy, host: &dyn Host, sysname: &[u8]) {
    let windows = sysname == b"Windows";
    let darwin = sysname == b"Darwin";
    let linux = sysname == b"Linux";
    let suffix = if windows { ".exe" } else { "" };

    let mut search_dirs: Vec<PathBuf> = Vec::new();
    let mut trusted_roots: Vec<PathBuf> = Vec::new();
    let mut push_dir = |dir: PathBuf| {
        if dir.is_absolute() && !search_dirs.iter().any(|existing| existing == &dir) {
            search_dirs.push(dir);
        }
    };
    let mut push_root = |root: PathBuf| {
        if root.is_absolute() && !trusted_roots.iter().any(|existing| existing == &root) {
            trusted_roots.push(root);
        }
    };

    if !windows {
        for dir in ["/usr/bin", "/bin"] {
            push_dir(PathBuf::from(dir));
        }
        for root in ["/usr/bin", "/bin", "/usr/sbin"] {
            push_root(PathBuf::from(root));
        }
    }

    if darwin {
        for dir in [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/Library/Developer/CommandLineTools/usr/bin",
            "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin",
        ] {
            push_dir(PathBuf::from(dir));
        }
        for root in [
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
            "/opt/homebrew",
            "/usr/local",
        ] {
            push_root(PathBuf::from(root));
        }
    } else if linux {
        push_dir(PathBuf::from("/usr/local/bin"));
        for root in [
            "/usr/lib",
            "/usr/lib64",
            "/lib",
            "/lib64",
            "/usr/libexec",
            "/usr/local",
        ] {
            push_root(PathBuf::from(root));
        }
    }

    let trusted_root_refs: Vec<&Path> = trusted_roots.iter().map(PathBuf::as_path).collect();

    if !windows {
        for probe in ["/bin/uname", "/usr/bin/uname", "/usr/bin/arch", "/bin/arch"] {
            let path = Path::new(probe);
            if policy.allow_program_in_roots(host, path, trusted_root_refs.iter().copied()) {
                policy.allow_exact_path(host, path);
            }
        }
    }

    if darwin {
        for probe in [
            "/usr/sbin/sysctl",
            "/usr/bin/sw_vers",
            "/usr/bin/xcode-select",
            "/usr/bin/xcrun",
            "/opt/homebrew/bin/brew",
            "/usr/local/bin/brew",
        ] {
            let path = Path::new(probe);
            if policy.allow_program_in_roots(host, path, trusted_root_refs.iter().copied()) {
                policy.allow_exact_path(host, path);
            }
        }
        for sdk_root in [
            "/Applications/Xcode.app/Contents/Developer",
            "/Library/Developer/CommandLineTools",
        ] {
            let root = Path::new(sdk_root);
            if host
                .metadata(root, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
            {
                policy.allow_root(host, root);
            }
        }
        for exact in [
            "/Applications",
            "/Applications/Xcode.app/Contents/Applications",
            "/Applications/Xcode.app/Contents/Library/Frameworks",
            "/Library/Developer/Applications",
            "/opt/homebrew",
            "/usr/local",
            "/usr/lib",
            "/usr/include",
            "/usr/local/lib",
            "/usr/local/include",
            "/Library/Frameworks",
            "/System/Library/Frameworks",
        ] {
            let path = Path::new(exact);
            if host.metadata(path, Follow::Yes).is_ok() {
                policy.allow_exact_path(host, path);
            }
        }
        if let Some(home) = host
            .env_get(b"HOME")
            .map(|value| path::to_native(&value))
            .filter(|dir| dir.is_absolute())
        {
            let user_apps = home.join("Applications");
            if host.metadata(&user_apps, Follow::Yes).is_ok() {
                policy.allow_exact_path(host, &user_apps);
            }
        }
    } else if linux {
        for toolchain_root in ["/usr/lib/gcc", "/usr/lib64/gcc", "/usr/lib/clang"] {
            let root = Path::new(toolchain_root);
            if host
                .metadata(root, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::Dir)
            {
                policy.allow_root(host, root);
            }
        }
        for exact in [
            "/etc/debian_version",
            "/etc/arch-release",
            "/usr",
            "/usr/lib",
            "/usr/lib64",
            "/usr/include",
            "/usr/local/lib",
            "/usr/local/include",
        ] {
            let path = Path::new(exact);
            if host.metadata(path, Follow::Yes).is_ok() {
                policy.allow_exact_path(host, path);
            }
        }
    }

    for dir in &search_dirs {
        let mut matched_tool = false;
        for tool in TOOLCHAIN_TOOLS {
            let candidate = dir.join(format!("{tool}{suffix}"));
            if policy.allow_program_in_roots(host, &candidate, trusted_root_refs.iter().copied()) {
                policy.allow_exact_path(host, &candidate);
                matched_tool = true;
            }
        }
        if matched_tool {
            policy.allow_exact_path(host, dir);
        }
    }
}

struct HermeticHost {
    inner: Arc<dyn Host>,
    mode: HermeticMode,
    context: HermeticContext,
    sources: HermeticSources,
    state: Mutex<AuditState>,
    windows: bool,
}

struct AuditState {
    auditor: HermeticAuditor,
    inputs: Observations,
    rich: Option<RichOptions>,
    reported_summary: bool,
}

impl HermeticHost {
    fn state(&self) -> MutexGuard<'_, AuditState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_current_site_bundled(&self) -> bool {
        let mut state = self.state();
        let site = self.context.site();
        state.auditor.is_bundled_site(&site)
    }

    fn audit(
        &self,
        check: impl for<'a> FnOnce(&'a mut HermeticAuditor, &HermeticSite) -> AuditOutcome<'a>,
    ) -> AuditDecision {
        let (decision, violation, rich) = {
            let mut state = self.state();
            let site = self.context.site();
            let AuditOutcome {
                decision,
                diagnostic,
            } = check(&mut state.auditor, &site);
            (decision, diagnostic.cloned(), state.rich)
        };
        if decision == AuditDecision::Deny {
            self.context.deny();
            if let Some(violation) = violation {
                write_violation(self.inner.as_ref(), &self.sources, &violation, true, rich);
            }
        }
        decision
    }

    fn audit_at(
        &self,
        site: &HermeticSite,
        check: impl for<'a> FnOnce(&'a mut HermeticAuditor) -> AuditOutcome<'a>,
    ) -> AuditDecision {
        let (decision, violation, rich) = {
            let mut state = self.state();
            let AuditOutcome {
                decision,
                diagnostic,
            } = check(&mut state.auditor);
            (decision, diagnostic.cloned(), state.rich)
        };
        let _ = site;
        if decision == AuditDecision::Deny {
            self.context.deny();
            if let Some(violation) = violation {
                write_violation(self.inner.as_ref(), &self.sources, &violation, true, rich);
            }
        }
        decision
    }

    fn guard_path<'path>(&self, path: &'path Path) -> HostResult<Cow<'path, Path>> {
        let fetch_cache = self.context.fetch_cache();
        let mut resolved = None;
        let decision = self.audit(|audit, site| {
            let (outcome, checked_path) =
                audit.check_read_path(self.inner.as_ref(), site, path, fetch_cache.as_deref());
            resolved = checked_path;
            outcome
        });
        require_allowed(decision, ViolationKind::OutOfTreePathRead)?;
        if let Some(resolved) = resolved {
            self.state().inputs.path(InputKind::Path, &resolved);
            if self.mode == HermeticMode::Strict {
                return Ok(Cow::Owned(resolved));
            }
        }
        Ok(Cow::Borrowed(path))
    }
}

struct MapLookup<'a> {
    sources: &'a SourceMap,
}

impl SourceLookup for MapLookup<'_> {
    fn source(&self, id: SourceId) -> Option<&SourceFile> {
        self.sources.get(id)
    }

    fn source_by_path(&self, raw: &[u8]) -> Option<&SourceFile> {
        if raw.is_empty() {
            return None;
        }
        if let Some(id) = self.sources.latest_file(raw) {
            return self.sources.get(id);
        }
        let normalized = Lexical::new(&path::from_native_cmake(&path::to_native(raw))).normalize();
        self.sources
            .latest_file(&normalized)
            .and_then(|id| self.sources.get(id))
    }
}

fn write_violation(
    host: &dyn Host,
    sources: &HermeticSources,
    violation: &HermeticViolation,
    deny: bool,
    rich: Option<RichOptions>,
) {
    let file_bytes = if violation.site.line() == 0 {
        violation.site.file().to_string_lossy().into_owned().into()
    } else {
        path::from_native_cmake(violation.site.file())
    };
    let diag = Diagnostic {
        kind: if deny {
            MessageKind::Error
        } else {
            MessageKind::Warning
        },
        effect: if deny { Effect::Fatal } else { Effect::None },
        category: None,
        policy: None,
        location: Location::FileLine {
            file: Arc::from(file_bytes.as_slice()),
            line: violation.site.line(),
        },
        body: format!(
            "{}: {} ({}).\n{}",
            violation.kind.code(),
            violation.kind.description(),
            violation.value,
            violation.hint()
        )
        .into(),
        annotations: None,
    };
    let rendered = match rich {
        Some(opts) => sources.with_map(|map| {
            let lookup = MapLookup { sources: map };
            render_rich(&diag, None, &lookup, opts)
        }),
        None => render(&diag, &RenderOptions::default()),
    };
    host.write_out(Stream::Stderr, &rendered);
}

fn require_allowed(decision: AuditDecision, kind: ViolationKind) -> HostResult<()> {
    if decision == AuditDecision::Deny {
        Err(HostError::from_io(
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, kind.code()),
            None,
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn check_network(cx: &Cx<'_>, url: &str, pinned: bool) -> Result<(), CmdError> {
    transfer::check(cx, url, pinned, None)
}

#[cfg(all(feature = "native", any(unix, windows)))]
pub(crate) use transfer::local_file;

/// Return `true` if an audit host checks the filesystem reads of this run.
pub(crate) fn audits_reads(cx: &Cx<'_>) -> bool {
    cx.session_data::<Arc<HermeticHost>>().is_some()
}

pub(crate) fn should_probe_speculative_path(cx: &Cx<'_>, path: &Path) -> bool {
    let Some(host) = cx.session_data::<Arc<HermeticHost>>() else {
        return true;
    };
    if !path.is_absolute() {
        return true;
    }
    let fetch_cache = host.context.fetch_cache();
    let allowed = {
        let state = host.state();
        state
            .auditor
            .allows_read_path(host.inner.as_ref(), path, fetch_cache.as_deref())
    };
    if allowed {
        return true;
    }
    match host.mode {
        HermeticMode::Off => true,
        HermeticMode::Strict => false,
        HermeticMode::Warn => !host
            .inner
            .metadata(path, Follow::No)
            .is_err_and(|error| error.kind == std::io::ErrorKind::NotFound),
    }
}

pub(crate) fn inherit(cx: &Cx<'_>, interp: &mut Interp) {
    if let Some(host) = cx.session_data::<Arc<HermeticHost>>() {
        interp.add_session_data(host.context.clone());
        interp.add_session_data(host.sources.clone());
        interp.add_session_data(host.clone());
    }
}

#[cfg(all(feature = "native", any(unix, windows)))]
pub(crate) fn allow_fetch_cache(
    cx: &Cx<'_>,
    cache_root: &Path,
) -> Option<rcmake_eval::hermetic::HermeticSiteGuard> {
    let context = cx.session_data::<HermeticContext>()?;
    cache_root
        .is_absolute()
        .then(|| context.enter_fetch_cache(cache_root.into()))
}

#[cfg(all(feature = "native", any(unix, windows)))]
pub(crate) fn check_fetch_network(
    cx: &Cx<'_>,
    url: &str,
    pinned: bool,
    dependency: &str,
) -> Result<(), CmdError> {
    transfer::check(cx, url, pinned, Some(dependency))
}

#[cfg(test)]
mod tests;
