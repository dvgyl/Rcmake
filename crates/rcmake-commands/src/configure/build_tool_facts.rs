// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use rcmake_base::BString;
use rcmake_host::{Host, Input, Output, ProcessResult, ReadRange, RunOutput, RunRequest, path};
use rcmake_resolve::query_ninja_cxx_dyndep;
use rcmake_store::{Digest, TraceMemo};

use super::enable_language::{ninja_version_supported, parse_ninja_version};
#[cfg(test)]
use super::try_compile::active::admit_environment;
use super::try_compile::plan::{process_environment, process_environment_fingerprint};

/// Return whether `path` satisfies the native Ninja format check (canonical stem
/// `"ninja"` with ELF, Mach-O, or PE executable magic) rather than a shell or script
/// wrapper.
///
/// Format screening keeps script wrappers off the probe-cache path. Soundness against
/// compiled binaries is enforced by running `--version` live in the exact target working
/// directory (`cwd`) and binding the binary digest and version in probe identity and
/// preflight stamps rather than replaying caller-directory observations into child dirs.
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
    pub(super) digest: Digest,
    pub(super) env: Digest,
    pub(super) cwd: PathBuf,
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
            || request.cwd != self.cwd
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

type NinjaKey = (PathBuf, Digest, Digest, PathBuf);

#[derive(Default)]
pub(super) struct BuildToolFacts {
    ninja: Mutex<BTreeMap<NinjaKey, Option<bool>>>,
    verified_ninja: Mutex<BTreeMap<NinjaKey, VerifiedNinja>>,
}

impl BuildToolFacts {
    pub(super) fn record_ninja(
        &self,
        program: PathBuf,
        digest: Digest,
        env: Digest,
        cwd: PathBuf,
        output: RunOutput,
        version: BString,
    ) -> VerifiedNinja {
        let entry = VerifiedNinja {
            program: program.clone(),
            digest,
            env,
            cwd: cwd.clone(),
            output,
            version,
        };
        let key = (program, digest, env, cwd);
        let mut ninja = self.ninja.lock().unwrap_or_else(PoisonError::into_inner);
        ninja.insert(key.clone(), Some(entry.supports_cxx_dyndep()));
        drop(ninja);
        let mut verified = self
            .verified_ninja
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        verified.insert(key, entry.clone());
        entry
    }

    pub(super) fn get_verified_ninja(
        &self,
        program: &Path,
        digest: Digest,
        env: Digest,
        cwd: &Path,
    ) -> Option<VerifiedNinja> {
        let verified = self
            .verified_ninja
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        verified
            .get(&(program.to_path_buf(), digest, env, cwd.to_path_buf()))
            .cloned()
    }

    #[cfg(test)]
    pub(super) fn sample_ninja(
        &self,
        host: &dyn Host,
        program: &Path,
        digest: Digest,
        env: Vec<(BString, BString)>,
        cwd: PathBuf,
    ) -> Option<VerifiedNinja> {
        if !program.is_absolute() || !admit_environment(&env) {
            return None;
        }
        let canonical = host.canonicalize(program).ok()?;
        if !is_native_ninja(host, &canonical) {
            return None;
        }
        self.sample_screened_ninja(host, None, program, digest, env, cwd)
    }

    /// Run `program --version` live in `cwd` and record the verified result.
    ///
    /// The caller must already require an absolute `program` that passes
    /// [`is_native_ninja`] and an environment that passes `admit_environment`.
    /// The binary digest after the run must equal `digest`. `memo` reuses the
    /// digest only while the file metadata and change time are unchanged.
    pub(super) fn sample_screened_ninja(
        &self,
        host: &dyn Host,
        memo: Option<&TraceMemo>,
        program: &Path,
        digest: Digest,
        mut env: Vec<(BString, BString)>,
        cwd: PathBuf,
    ) -> Option<VerifiedNinja> {
        process_environment(&mut env);
        let env_digest = process_environment_fingerprint(&env);
        if let Some(found) = self.get_verified_ninja(program, digest, env_digest, &cwd) {
            return Some(found);
        }
        let request = RunRequest {
            pipeline: vec![rcmake_host::CommandSpec {
                program: path::from_native(program),
                args: vec!["--version".into()],
            }],
            cwd: cwd.clone(),
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
        let after = memo.map_or_else(
            || rcmake_store::file_digest(host, program),
            |memo| memo.file_digest(host, program),
        );
        if after.ok()? != digest {
            return None;
        }
        Some(self.record_ninja(
            program.to_path_buf(),
            digest,
            env_digest,
            cwd,
            output,
            version,
        ))
    }

    pub(super) fn ninja_cxx_dyndep(
        &self,
        host: &dyn Host,
        request: &RunRequest,
        program: PathBuf,
        binary: Digest,
        environment: Digest,
    ) -> Option<bool> {
        if let Some(verified) = self.get_verified_ninja(&program, binary, environment, &request.cwd)
        {
            return Some(verified.supports_cxx_dyndep());
        }
        let mut ninja = self.ninja.lock().unwrap_or_else(PoisonError::into_inner);
        *ninja
            .entry((program, binary, environment, request.cwd.clone()))
            .or_insert_with(|| query_ninja_cxx_dyndep(host, request))
    }
}
