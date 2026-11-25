// SPDX-License-Identifier: MIT OR Apache-2.0

//! Start the Darwin platform queries when a configure session starts.
//!
//! `Darwin-Initialize.cmake` and `Darwin.cmake` run these queries in sequence
//! in every configure. A started query runs once, with the session
//! environment. The module uses its result only for an equal query key:
//! the same program, earlier `PATH` misses, arguments, environment, and error
//! stream. Otherwise the module runs its own query.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rcmake_host::{CommandSpec, Host, HostResult, Input, Output, RunOutput, RunRequest};

use super::{PlatformQueries, Query, launch, query};

type Started = (Query, JoinHandle<HostResult<RunOutput>>);

/// Queries that started before the platform modules reached them.
#[derive(Default)]
pub(super) struct Prefetched(Mutex<Vec<Started>>);

impl Prefetched {
    /// Wait for a started query with the same key.
    pub(super) fn take(&self, key: &Query) -> Option<HostResult<RunOutput>> {
        let handle = {
            let mut pending = self.0.lock().ok()?;
            let index = pending.iter().position(|(started, _)| started == key)?;
            pending.swap_remove(index).1
        };
        handle.join().ok()
    }
}

impl Drop for Prefetched {
    fn drop(&mut self) {
        if let Ok(pending) = self.0.get_mut() {
            for (_, handle) in pending.drain(..) {
                let _ = handle.join();
            }
        }
    }
}

impl PlatformQueries {
    /// Start the queries that the Darwin platform modules run.
    pub(in crate::configure) fn prefetch(&self, host: &Arc<dyn Host>) {
        let env = host.env_all();
        let cwd = host.initial_cwd();
        let Ok(mut pending) = self.1.0.lock() else {
            return;
        };
        for (program, args, stderr) in [
            ("xcode-select", ["-print-path"].as_slice(), Output::Capture),
            ("sysctl", &["-q", "hw.optional.arm64"], Output::Capture),
            ("xcrun", &["--show-sdk-path"], Output::Capture),
            ("sw_vers", &["-productVersion"], Output::Inherit),
            ("brew", &["--prefix"], Output::Inherit),
        ] {
            let request = RunRequest {
                pipeline: vec![CommandSpec {
                    program: program.into(),
                    args: args.iter().map(|&arg| arg.into()).collect(),
                }],
                cwd: cwd.clone(),
                env: env.clone(),
                stdin: Input::Inherit,
                stdout: Output::Capture,
                stderr,
                timeout: None,
                tee: false,
            };
            let Some(key) = query(host.as_ref(), &request) else {
                continue;
            };
            let worker = Arc::clone(host);
            let started = key.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name("rcmake-platform-query".into())
                .spawn(move || launch(worker.as_ref(), &started, &request))
            {
                pending.push((key, handle));
            }
        }
    }
}
