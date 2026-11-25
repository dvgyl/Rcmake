use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{Interp, RunOutcome, Services};
use rcmake_host::Host;

use crate::hermetic::HermeticRuntime;

#[cfg(all(feature = "native", any(unix, windows)))]
mod native;

/// Host and directories for one configure session.
pub struct ConfigureRuntime {
    host: Arc<dyn Host>,
    source: PathBuf,
    build: PathBuf,
    hermetic: Option<HermeticRuntime>,
    #[cfg(all(feature = "native", any(unix, windows)))]
    native: Option<native::NativeSession>,
    evaluated: bool,
}

impl ConfigureRuntime {
    /// Prepare a configure session with the requested services.
    #[must_use]
    pub fn new(
        host: Arc<dyn Host>,
        source: &Path,
        build: &Path,
        services: &Services,
        allow_persistent: bool,
    ) -> Self {
        let hermetic = HermeticRuntime::new(Arc::clone(&host), source, build, services);
        let evaluated_host = hermetic
            .as_ref()
            .map_or_else(|| Arc::clone(&host), HermeticRuntime::host);
        #[cfg(all(feature = "native", any(unix, windows)))]
        let native = if allow_persistent && host.supports_process_execution() {
            native::NativeSession::new(host, Arc::clone(&evaluated_host), source, build, services)
        } else {
            None
        };
        #[cfg(all(feature = "native", any(unix, windows)))]
        let evaluated_host = native
            .as_ref()
            .map_or(evaluated_host, native::NativeSession::host);
        #[cfg(not(all(feature = "native", any(unix, windows))))]
        let _ = allow_persistent;
        Self {
            host: evaluated_host,
            source: source.to_owned(),
            build: build.to_owned(),
            hermetic,
            #[cfg(all(feature = "native", any(unix, windows)))]
            native,
            evaluated: false,
        }
    }

    /// Clone the host for this session.
    #[must_use]
    pub fn host(&self) -> Arc<dyn Host> {
        Arc::clone(&self.host)
    }

    /// Attach this session's command services before the evaluator runs listfiles.
    pub fn attach(&self, interp: &mut Interp) {
        if let Some(hermetic) = &self.hermetic {
            let cmake_root = interp.cx().cmake_root().to_owned();
            hermetic.allow_root(&cmake_root);
            hermetic.install(interp);
        }
        #[cfg(all(feature = "native", any(unix, windows)))]
        if let Some(native) = &self.native {
            native.attach(interp);
        }
    }

    /// Return whether strict input checks rejected an operation.
    #[must_use]
    pub fn denied(&self) -> bool {
        self.hermetic.as_ref().is_some_and(HermeticRuntime::denied)
    }

    /// Evaluate the listfiles for this session.
    pub fn run(&mut self, interp: &mut Interp, preloads: &[PathBuf]) -> RunOutcome {
        self.attach(interp);
        #[cfg(all(feature = "native", any(unix, windows)))]
        let outcome = if let Some(native) = &mut self.native {
            native.run(interp, &self.source, &self.build, preloads)
        } else {
            interp.run_configure(&self.source, &self.build, preloads)
        };
        #[cfg(not(all(feature = "native", any(unix, windows))))]
        let outcome = interp.run_configure(&self.source, &self.build, preloads);
        self.evaluated = outcome.exit_code == 0
            && interp.stop_reason().is_none()
            && interp.requested_exit_code().is_none()
            && !self.denied();
        if self.denied() {
            return RunOutcome { exit_code: 1 };
        }
        outcome
    }

    /// Publish session records after successful evaluation and generation.
    ///
    /// Return the publication error, if any.
    pub fn finish(self, complete: bool) -> Option<Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(all(feature = "native", any(unix, windows)))]
        let result = self
            .native
            .map_or(Ok(()), |native| native.finish(complete && self.evaluated))
            .err()
            .map(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>);
        #[cfg(not(all(feature = "native", any(unix, windows))))]
        let result = {
            let _ = complete;
            None
        };
        if let Some(hermetic) = self.hermetic {
            hermetic.finish();
        }
        result
    }
}

#[cfg(all(test, feature = "native", any(unix, windows)))]
mod tests;
