// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configure host adapter for hermetic input checks.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rcmake_eval::hermetic::{
    AuditDecision, AuditOutcome, HermeticAuditor, HermeticContext, HermeticPolicy, HermeticSite,
    ViolationKind,
};
use rcmake_eval::{CmdError, Cx, HermeticMode, Interp, Services};
use rcmake_host::{Host, HostError, HostResult, Stream, discover_cmake_root};

mod host;
mod report;
mod transfer;

use report::{InputKind, Observations};

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
        let (exe, modules) = discover_cmake_root(host.as_ref(), None);
        let roots = [source, build].into_iter().chain(modules.as_deref());
        let mut policy = HermeticPolicy::from_host(host.as_ref(), roots);
        policy.allow_program(host.as_ref(), &exe);
        let context = HermeticContext::new(HermeticSite::new(&source.join("CMakeLists.txt"), 0));
        let inputs = Observations::new(host.as_ref(), source, build, modules.as_deref());
        Some(Self {
            host: Arc::new(HermeticHost {
                windows: host.system().sysname == b"Windows",
                inner: host,
                mode,
                context,
                state: Mutex::new(AuditState {
                    auditor: HermeticAuditor::new(mode, policy),
                    inputs,
                }),
            }),
            build: build.into(),
        })
    }

    pub(crate) fn host(&self) -> Arc<dyn Host> {
        self.host.clone()
    }

    pub(crate) fn install(&self, interp: &mut Interp) {
        interp.add_session_data(self.host.context.clone());
        interp.add_session_data(self.host.clone());
    }

    pub(crate) fn allow_root(&self, path: &Path) {
        self.host
            .state()
            .auditor
            .allow_root(self.host.inner.as_ref(), path);
    }

    pub(crate) fn denied(&self) -> bool {
        self.host.context.denied()
    }

    pub(crate) fn finish(&self) {
        let (summary, contents) = {
            let mut state = self.host.state();
            let summary = state.auditor.take_omission_summary();
            let contents = report::render(&state, self.host.mode, self.denied());
            (summary, contents)
        };
        let host = self.host.inner.as_ref();
        if let Some(summary) = summary {
            host.write_out(Stream::Stderr, format!("{summary}\n").as_bytes());
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

struct HermeticHost {
    inner: Arc<dyn Host>,
    mode: HermeticMode,
    context: HermeticContext,
    state: Mutex<AuditState>,
    windows: bool,
}

struct AuditState {
    auditor: HermeticAuditor,
    inputs: Observations,
}

impl HermeticHost {
    fn state(&self) -> MutexGuard<'_, AuditState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn audit(
        &self,
        check: impl for<'a> FnOnce(&'a mut HermeticAuditor) -> AuditOutcome<'a>,
    ) -> AuditDecision {
        let (decision, diagnostic) = {
            let mut state = self.state();
            let AuditOutcome {
                decision,
                diagnostic,
            } = check(&mut state.auditor);
            (decision, diagnostic.map(ToString::to_string))
        };
        if decision == AuditDecision::Deny {
            self.context.deny();
        }
        if let Some(diagnostic) = diagnostic {
            let level = if decision == AuditDecision::Deny {
                "Error"
            } else {
                "Warning"
            };
            self.inner.write_out(
                Stream::Stderr,
                format!("{diagnostic} [{level}]\n").as_bytes(),
            );
        }
        decision
    }

    fn guard_path<'path>(&self, path: &'path Path) -> HostResult<Cow<'path, Path>> {
        let site = self.context.site();
        let fetch_cache = self.context.fetch_cache();
        let mut resolved = None;
        let decision = self.audit(|audit| {
            let (outcome, checked_path) =
                audit.check_read_path(self.inner.as_ref(), &site, path, fetch_cache.as_deref());
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

pub(crate) fn inherit(cx: &Cx<'_>, interp: &mut Interp) {
    if let Some(host) = cx.session_data::<Arc<HermeticHost>>() {
        interp.add_session_data(host.context.clone());
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
