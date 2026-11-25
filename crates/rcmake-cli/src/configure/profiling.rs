use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_eval::Interp;
use rcmake_host::Host;

use crate::options::{Options, extensions::ProfileRequest};
use crate::persona::{Persona, oracle_mode};
use crate::profile::{self, ProfilePhase, ProfileRecorder, Recorder};

use super::Exit;

pub(super) struct Profilers {
    legacy: Option<(Arc<Recorder>, PathBuf)>,
    native: Option<(Arc<ProfileRecorder>, PathBuf)>,
}

impl Profilers {
    pub(super) fn new(host: &dyn Host, opts: &Options, build: &Path) -> Self {
        let legacy = opts
            .profile_output
            .as_ref()
            .map(|path| (Arc::new(Recorder::new(host)), path.clone()));
        let native = if opts.persona == Persona::Native && !oracle_mode(host) && !opts.view {
            opts.extensions
                .profile
                .clone()
                .or_else(|| ProfileRequest::from_env(host))
                .map(|req| {
                    (
                        Arc::new(ProfileRecorder::new()),
                        req.output_directory(build, host),
                    )
                })
        } else {
            None
        };
        Self { legacy, native }
    }

    pub(super) fn active(&self) -> bool {
        self.legacy.is_some() || self.native.is_some()
    }

    pub(super) fn attach(&self, interp: &mut Interp) {
        profile::attach_profilers(
            interp,
            self.legacy.as_ref().map(|(recorder, _)| recorder),
            self.native.as_ref().map(|(recorder, _)| recorder),
        );
    }

    pub(super) fn measure<T>(
        &self,
        host: &dyn Host,
        phase: ProfilePhase,
        run: impl FnOnce() -> T,
    ) -> T {
        let Some((recorder, _)) = &self.native else {
            return run();
        };
        let start = host.monotonic();
        let result = run();
        recorder.record_phase(phase, start, host.monotonic());
        result
    }

    pub(super) fn measure_listfile<T>(
        &self,
        host: &dyn Host,
        file: &Path,
        run: impl FnOnce() -> T,
    ) -> T {
        let Some((recorder, _)) = &self.native else {
            return run();
        };
        let start = host.monotonic();
        let output = run();
        let elapsed = host.monotonic().saturating_sub(start);
        let file = rcmake_host::path::from_native_cmake(file);
        recorder.record(profile::ProfileSample {
            name: &file,
            file: &file,
            start,
            elapsed,
            line: 0,
            depth: 0,
            kind: profile::ProfileKind::ListFile,
        });
        output
    }

    pub(super) fn write(&self, host: &dyn Host, start: Duration) -> Result<(), Exit> {
        let error = |err| Exit::Message(format!("Could not write profiling output: {err}"));
        if let Some((recorder, path)) = &self.legacy {
            recorder.write(host, path).map_err(error)?;
        }
        if let Some((recorder, dir)) = &self.native {
            recorder
                .write(host, dir, host.monotonic().saturating_sub(start))
                .map_err(error)?;
        }
        Ok(())
    }
}
