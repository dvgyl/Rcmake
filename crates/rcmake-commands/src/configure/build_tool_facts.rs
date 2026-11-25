// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use rcmake_host::{Host, RunRequest};
use rcmake_resolve::query_ninja_cxx_dyndep;
use rcmake_store::Digest;

#[derive(Default)]
pub(super) struct BuildToolFacts {
    ninja: Mutex<BTreeMap<(PathBuf, Digest, Digest), Option<bool>>>,
}

impl BuildToolFacts {
    pub(super) fn ninja_cxx_dyndep(
        &self,
        host: &dyn Host,
        request: &RunRequest,
        program: PathBuf,
        binary: Digest,
        environment: Digest,
    ) -> Option<bool> {
        let mut ninja = self.ninja.lock().unwrap_or_else(PoisonError::into_inner);
        *ninja
            .entry((program, binary, environment))
            .or_insert_with(|| query_ninja_cxx_dyndep(host, request))
    }
}
