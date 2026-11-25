use std::collections::BTreeMap;

use super::super::plan::{Admission, RcmakeBuildId};
use super::super::template::PathBindings;
use super::*;

/// Create an eligible recipe for a test probe.
pub(crate) fn recipe(host: &dyn Host, root: &Path) -> HostResult<ProbeRecipe> {
    let eligibility = Admission {
        store_mode: StoreMode::ReadWrite,
        debug_try_compile: false,
        trace: false,
        profiling: false,
        sources_signature: true,
        private_scratch: true,
        verified_execution: true,
    }
    .eligibility();
    let digest = Digest::hash(b"test");
    let bindings = PathBindings::new(
        &root.join("scratch"),
        &root.join("build"),
        &root.join("source"),
        &root.join("cmake"),
    )
    .and_then(|bindings| bindings.with_target(b"cmTC_1"))
    .ok_or_else(|| HostError::unsupported("Missing path bindings."))?;
    Ok(ProbeRecipe {
        eligibility,
        rcmake: RcmakeBuildId(digest),
        cmake_root: digest,
        host: digest,
        generator: digest,
        files: BTreeMap::new(),
        tools: BTreeMap::new(),
        flags: Vec::new(),
        definitions: Vec::new(),
        link_libraries: Vec::new(),
        target: "arm64".into(),
        sysroot: "/sdk".into(),
        seed: digest,
        imports: digest,
        environment: environment_fingerprint(&host.env_all()),
        scope: ReuseScope::VerifiedRelocation,
        bindings,
        try_run: false,
        speculative_run: None,
    })
}
