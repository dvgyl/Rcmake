// SPDX-License-Identifier: MIT OR Apache-2.0

//! Process evidence for in-memory speculation with an unverified compiler.
//!
//! With the persistent probe cache off, a speculative result lives only in
//! the current configure. The lookahead batches only consecutive literal
//! `try_compile` commands, so no command between a speculative run and its
//! adoption can change the environment or a compile input. Adoption also
//! requires the same probe key: compiler path and file hash, rendered project
//! and source bytes, environment, seed, and bindings. The compiler can be an
//! opaque wrapper in this mode. Persistent publication still requires the full
//! toolchain evidence in `CompilerEvidence`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_host::{Host, ProcessResult, RunOutput, RunRequest, path};
use rcmake_store::{Digest, InputStamp, Trace, TraceMemo};

use super::super::super::journal::{DependencyClosure, ProcessVerifier};
use super::{
    CompilerEvidence, RecordedQueries, SessionEvidence, flags, platform_snapshot, private_path,
};

/// Process evidence for one probe source.
pub(in super::super) struct ProbeCompiler {
    pub(in super::super) verifier: Arc<dyn ProcessVerifier>,
    pub(in super::super) identity: BTreeMap<BString, Digest>,
}

impl ProbeCompiler {
    /// Use the full toolchain evidence, or opaque evidence when `in_memory`.
    pub(in super::super) fn select(
        host: Arc<dyn Host>,
        cx: &Cx<'_>,
        session: &SessionEvidence,
        name: &[u8],
        memo: &Arc<TraceMemo>,
        external_source: Option<(PathBuf, Digest)>,
        in_memory: bool,
    ) -> Option<Self> {
        let opaque = in_memory && external_source.is_none();
        if let Some(compiler) =
            CompilerEvidence::new(Arc::clone(&host), cx, session, name, memo, external_source)
        {
            return Some(Self {
                identity: compiler.identity(),
                verifier: Arc::new(compiler),
            });
        }
        if !opaque {
            return None;
        }
        let compiler = OpaqueCompiler::new(host, cx, name, memo)?;
        Some(Self {
            identity: compiler.identity(),
            verifier: Arc::new(compiler),
        })
    }
}

/// A compiler with no verified dependency evidence.
struct OpaqueCompiler {
    host: Arc<dyn Host>,
    program: PathBuf,
    digest: Digest,
    platform: RecordedQueries,
}

impl OpaqueCompiler {
    fn new(host: Arc<dyn Host>, cx: &Cx<'_>, name: &[u8], memo: &TraceMemo) -> Option<Self> {
        let language: &[u8] = if flags::source_language(name)? {
            b"CXX"
        } else {
            b"C"
        };
        let variable = |suffix: &[u8]| cx.var_or_empty(&[b"CMAKE_", language, suffix].concat());
        if !variable(b"_COMPILER_LAUNCHER").is_empty() {
            return None;
        }
        let program = path::to_native(&variable(b"_COMPILER"));
        if !program.is_absolute() {
            return None;
        }
        let digest = memo.file_digest(host.as_ref(), &program).ok()?;
        let platform = platform_snapshot(host.as_ref(), cx);
        Some(Self {
            host,
            program,
            digest,
            platform,
        })
    }

    fn identity(&self) -> BTreeMap<BString, Digest> {
        BTreeMap::from([
            (BString::from("opaque-compiler"), Digest::hash(b"in-memory")),
            (path::from_native(&self.program), self.digest),
            (BString::from("platform-queries"), self.platform.identity()),
        ])
    }
}

impl ProcessVerifier for OpaqueCompiler {
    fn recorded_platform_query(&self, request: &RunRequest) -> Option<RunOutput> {
        self.platform.get(self.host.as_ref(), request)
    }

    fn preflight(&self, request: &RunRequest) -> Option<Trace> {
        let [command] = request.pipeline.as_slice() else {
            return None;
        };
        (path::to_native(&command.program) == self.program).then(|| {
            std::iter::once(InputStamp::FileHash {
                path: self.program.clone(),
                digest: self.digest,
            })
            .chain(self.platform.stamps())
            .collect()
        })
    }

    /// Name the private sources and objects of a successful command.
    fn dependencies(&self, request: &RunRequest, output: &RunOutput) -> Option<DependencyClosure> {
        let [command] = request.pipeline.as_slice() else {
            return None;
        };
        if output.results.as_slice() != [ProcessResult::Exited(0)] {
            return None;
        }
        let files: BTreeSet<_> = command
            .args
            .iter()
            .filter(|arg| !arg.starts_with(b"-"))
            .map(|arg| request.cwd.join(path::to_native(arg)))
            .filter(|file| private_path(&request.cwd, file))
            .collect();
        (!files.is_empty()).then_some(DependencyClosure {
            files,
            searches: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_evidence_accepts_only_the_compiler_and_successful_private_inputs() {
        let compiler = OpaqueCompiler {
            host: Arc::new(rcmake_host::PureHost::default()),
            program: PathBuf::from("/work/observe-CC"),
            digest: Digest::hash(b"wrapper"),
            platform: RecordedQueries::default(),
        };
        let request = |program: &str| RunRequest {
            pipeline: vec![rcmake_host::CommandSpec {
                program: program.into(),
                args: vec!["-c".into(), "probe.c".into(), "-o".into(), "probe.o".into()],
            }],
            cwd: PathBuf::from("/work/build/CMakeFiles/CMakeScratch/Spec-0"),
            env: Vec::new(),
            stdin: rcmake_host::Input::Null,
            stdout: rcmake_host::Output::Capture,
            stderr: rcmake_host::Output::Capture,
            timeout: None,
            tee: false,
        };
        assert!(compiler.preflight(&request("/usr/bin/cc")).is_none());
        let trace = compiler.preflight(&request("/work/observe-CC"));
        assert_eq!(trace.map(|trace| trace.len()), Some(1));
        let success = RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        };
        let closure = compiler.dependencies(&request("/work/observe-CC"), &success);
        assert_eq!(closure.map(|closure| closure.files.len()), Some(2));
        let failure = RunOutput {
            results: vec![ProcessResult::Exited(1)],
            ..RunOutput::default()
        };
        assert!(
            compiler
                .dependencies(&request("/work/observe-CC"), &failure)
                .is_none()
        );
    }
}
