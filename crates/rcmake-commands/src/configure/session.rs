use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{Interp, RunOutcome, Services};
use rcmake_host::Host;

/// Host and directories for one configure session.
pub struct ConfigureRuntime {
    host: Arc<dyn Host>,
    source: PathBuf,
    build: PathBuf,
}

impl ConfigureRuntime {
    /// Prepare a configure session. Wave 1A uses the service and persistence settings.
    #[must_use]
    pub fn new(
        host: Arc<dyn Host>,
        source: &Path,
        build: &Path,
        _services: &Services,
        _allow_persistent: bool,
    ) -> Self {
        Self {
            host,
            source: source.to_owned(),
            build: build.to_owned(),
        }
    }

    /// Clone the host for this session.
    #[must_use]
    pub fn host(&self) -> Arc<dyn Host> {
        Arc::clone(&self.host)
    }

    /// Evaluate the listfiles for this session.
    pub fn run(&mut self, interp: &mut Interp, preloads: &[PathBuf]) -> RunOutcome {
        interp.run_configure(&self.source, &self.build, preloads)
    }

    /// Finish this session. Wave 1A uses the completion state.
    pub fn finish(self, _complete: bool) {}
}
