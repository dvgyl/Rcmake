// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rcmake_base::BString;
use rcmake_host::{Host, Input, Output, ProcessResult, ReadRange, RunOutput, RunRequest, path};
use rcmake_resolve::query_ninja_cxx_dyndep;
use rcmake_store::Digest;

use super::enable_language::{ninja_version_supported, parse_ninja_version};
use super::try_compile::active::admit_environment;
use super::try_compile::plan::{process_environment, process_environment_fingerprint};

/// Return whether `path` satisfies the native Ninja format check (canonical stem
/// `"ninja"` with ELF, Mach-O, or PE executable magic) rather than a shell or script
/// wrapper.
///
/// This certifies file format and canonical filename to exclude interpreted wrappers
/// whose `--version` output may depend on working directory or external files; it does
/// not inspect arbitrary compiled binaries that impersonate the same name and magic.
pub(super) fn is_native_ninja(host: &dyn Host, path: &Path) -> bool {
    if !path.file_stem().is_some_and(|name| name == "ninja") {
        return false;
    }
    host.read(
        path,
        ReadRange::Slice {
            offset: 0,
            len: Some(4),
        },
    )
    .is_ok_and(|bytes| {
        matches!(
            bytes.as_slice(),
            b"\x7fELF"
                | [b'M', b'Z', ..]
                | b"\xcf\xfa\xed\xfe"
                | b"\xfe\xed\xfa\xcf"
                | b"\xca\xfe\xba\xbe"
                | b"\xca\xfe\xba\xbf"
        )
    })
}

#[derive(Clone)]
pub(super) struct VerifiedNinja {
    pub(super) program: PathBuf,
    pub(super) env: Digest,
    pub(super) output: RunOutput,
    pub(super) version: BString,
}

impl VerifiedNinja {
    pub(super) fn matches(&self, request: &RunRequest) -> bool {
        let [command] = request.pipeline.as_slice() else {
            return false;
        };
        if path::to_native(&command.program) != self.program
            || command.args.as_slice() != [BString::from("--version")]
            || request.stdin != Input::Null
            || request.stdout != Output::Capture
            || request.stderr != Output::Capture
            || request.tee
        {
            return false;
        }
        let mut env = request.env.clone();
        process_environment(&mut env);
        process_environment_fingerprint(&env) == self.env
    }

    pub(super) fn supports_cxx_dyndep(&self) -> bool {
        rcmake_base::version::compare(&self.version, b"1.11") != std::cmp::Ordering::Less
    }
}

#[derive(Default)]
pub(super) struct BuildToolFacts {
    ninja: Mutex<BTreeMap<(PathBuf, Digest, Digest), Option<bool>>>,
    verified_ninja: Mutex<BTreeMap<(PathBuf, Digest, Digest), VerifiedNinja>>,
}

impl BuildToolFacts {
    pub(super) fn record_ninja(
        &self,
        program: PathBuf,
        digest: Digest,
        env: Digest,
        output: RunOutput,
        version: BString,
    ) -> VerifiedNinja {
        let entry = VerifiedNinja {
            program: program.clone(),
            env,
            output,
            version,
        };
        let mut ninja = self.ninja.lock().unwrap_or_else(PoisonError::into_inner);
        ninja.insert(
            (program.clone(), digest, env),
            Some(entry.supports_cxx_dyndep()),
        );
        drop(ninja);
        let mut verified = self
            .verified_ninja
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        verified.insert((program, digest, env), entry.clone());
        entry
    }

    pub(super) fn get_verified_ninja(
        &self,
        program: &Path,
        digest: Digest,
        env: Digest,
    ) -> Option<VerifiedNinja> {
        let verified = self
            .verified_ninja
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        verified.get(&(program.to_path_buf(), digest, env)).cloned()
    }

    pub(super) fn sample_ninja(
        &self,
        host: &dyn Host,
        program: &Path,
        digest: Digest,
        mut env: Vec<(BString, BString)>,
        cwd: PathBuf,
    ) -> Option<VerifiedNinja> {
        if !program.is_absolute() || !admit_environment(&env) {
            return None;
        }
        let canonical = host.canonicalize(program).ok()?;
        if !is_native_ninja(host, &canonical) {
            return None;
        }
        process_environment(&mut env);
        let env_digest = process_environment_fingerprint(&env);
        if let Some(found) = self.get_verified_ninja(program, digest, env_digest) {
            return Some(found);
        }
        let request = RunRequest {
            pipeline: vec![rcmake_host::CommandSpec {
                program: path::from_native(program),
                args: vec!["--version".into()],
            }],
            cwd,
            env,
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(std::time::Duration::from_secs(5)),
            tee: false,
        };
        let output = host.run_bounded(&request, 4096).ok()?;
        if output.results != [ProcessResult::Exited(0)] {
            return None;
        }
        let version = parse_ninja_version(&output.stdout)?;
        if !ninja_version_supported(&version) {
            return None;
        }
        if rcmake_store::file_digest(host, program).ok()? != digest {
            return None;
        }
        Some(self.record_ninja(program.to_path_buf(), digest, env_digest, output, version))
    }

    pub(super) fn ninja_cxx_dyndep(
        &self,
        host: &dyn Host,
        request: &RunRequest,
        program: PathBuf,
        binary: Digest,
        environment: Digest,
    ) -> Option<bool> {
        if let Some(verified) = self.get_verified_ninja(&program, binary, environment) {
            return Some(verified.supports_cxx_dyndep());
        }
        let mut ninja = self.ninja.lock().unwrap_or_else(PoisonError::into_inner);
        *ninja
            .entry((program, binary, environment))
            .or_insert_with(|| query_ninja_cxx_dyndep(host, request))
    }
}
