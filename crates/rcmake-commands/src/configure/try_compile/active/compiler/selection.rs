// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_host::Host;
use rcmake_store::{Digest, TraceMemo};

use super::super::super::journal::ProcessVerifier;
use super::{CompilerEvidence, SessionEvidence};

/// Process evidence for one probe source.
pub(in super::super) struct ProbeCompiler {
    pub(in super::super) verifier: Arc<dyn ProcessVerifier>,
    pub(in super::super) identity: BTreeMap<BString, Digest>,
}

impl ProbeCompiler {
    /// Select a compiler with verified toolchain evidence.
    pub(in super::super) fn select(
        host: Arc<dyn Host>,
        cx: &mut Cx<'_>,
        session: &SessionEvidence,
        name: &[u8],
        memo: &Arc<TraceMemo>,
        external_source: Option<(PathBuf, Digest)>,
    ) -> Option<Self> {
        let compiler = CompilerEvidence::new(host, cx, session, name, memo, external_source)?;
        Some(Self {
            identity: compiler.identity(),
            verifier: Arc::new(compiler),
        })
    }
}
