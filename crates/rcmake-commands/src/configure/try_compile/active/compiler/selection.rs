// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_host::Host;
use rcmake_store::{Digest, TraceMemo};

use super::super::super::plan::CachedEnv;
use super::{CompilerEvidence, SessionEvidence};

/// Process evidence for one probe source.
pub(in super::super) struct ProbeCompiler {
    pub(in super::super) verifier: Arc<CompilerEvidence>,
    pub(in super::super) identity: BTreeMap<BString, Digest>,
}

impl ProbeCompiler {
    /// Select a compiler with verified toolchain evidence.
    #[allow(clippy::too_many_arguments)]
    pub(in super::super) fn select(
        host: Arc<dyn Host>,
        cx: &mut Cx<'_>,
        session: &SessionEvidence,
        name: &[u8],
        memo: &Arc<TraceMemo>,
        external_source: Option<(PathBuf, Digest)>,
        probe_cwd: &Path,
        env: &CachedEnv,
    ) -> Option<Self> {
        let compiler = CompilerEvidence::new(
            host,
            cx,
            session,
            name,
            memo,
            external_source,
            probe_cwd,
            env,
        )?;
        Some(Self {
            identity: compiler.identity(),
            verifier: Arc::new(compiler),
        })
    }

    /// Select a compiler for a speculative probe without mutating foreground
    /// variable-access tracking or session caches.
    pub(in super::super) fn select_speculative(
        host: Arc<dyn Host>,
        cx: &Cx<'_>,
        session: &SessionEvidence,
        name: &[u8],
        memo: &Arc<TraceMemo>,
        reference: &CompilerEvidence,
        env: &CachedEnv,
    ) -> Option<Self> {
        let compiler =
            CompilerEvidence::new_speculative(host, cx, session, name, memo, reference, env)?;
        Some(Self {
            identity: compiler.identity(),
            verifier: Arc::new(compiler),
        })
    }
}
