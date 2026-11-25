// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rcmake_base::BString;
use rcmake_eval::{Cx, journal::ReplayReceipts};
use rcmake_host::{
    FileKind, Follow, Host, HostResult, Input, Output, ProcessResult, RunOutput, RunRequest,
    Stream, path,
};
use rcmake_store::{Digest, Domain, InputStamp, KeyHasher, file_digest};

use super::try_compile::plan::environment_fingerprint;

mod prefetch;
mod receipts;

/// Shortest process bound that can reuse a completed sample. The try-compile
/// child configure bounds every process at 120 seconds. A system query that
/// completed without a bound gives the same result under a long bound.
const MIN_REPLAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Query {
    program: PathBuf,
    args: Vec<BString>,
    env: Digest,
    stderr: u8,
    missing_programs: Vec<PathBuf>,
}

#[derive(Clone)]
struct Observation {
    executable: Digest,
    result: RunOutput,
    /// The sampled request. A probe samples it again in a new environment.
    request: RunRequest,
}

#[derive(Clone, Default)]
pub(super) struct RecordedQueries(BTreeMap<Query, Observation>);

#[derive(Default)]
pub(super) struct PlatformQueries(Mutex<RecordedQueries>, prefetch::Prefetched);

impl PlatformQueries {
    /// Sample each recorded query again in the environment of a try-compile
    /// child.
    ///
    /// Compiler identification can change the environment after the platform
    /// modules run. For example, it restores `LC_ALL` as an empty value, and
    /// policy `CMP0132` OLD exports `CC`. The child sees that environment.
    pub(super) fn refresh(&self, host: &dyn Host, env: &[(BString, BString)]) {
        let Ok(mut memo) = self.0.lock() else {
            return;
        };
        let digest = environment_fingerprint(env);
        let mut pending: Vec<_> = memo
            .0
            .iter()
            .filter(|(query, _)| query.env != digest)
            .map(|(_, observation)| RunRequest {
                env: env.to_vec(),
                ..observation.request.clone()
            })
            .collect();
        // `xcrun` needs the developer directory sample for the same environment.
        pending.sort_by_key(|request| {
            request
                .pipeline
                .first()
                .is_some_and(|command| command.program.ends_with(b"xcrun"))
        });
        for request in pending {
            let Some(key) = query(host, &request) else {
                continue;
            };
            if memo.0.contains_key(&key) || !admits_xcrun(host, &memo, &key, &request) {
                continue;
            }
            if let Ok(executable) = file_digest(host, &key.program) {
                let output = launch(host, &key, &request);
                let _ = record(host, &mut memo, key, executable, &request, output, |_| {});
            }
        }
    }

    /// Copy the samples that a try-compile child in this environment can use.
    pub(super) fn snapshot(
        &self,
        host: &dyn Host,
        env: &[(BString, BString)],
    ) -> Option<RecordedQueries> {
        let digest = environment_fingerprint(env);
        let mut snapshot = self.0.lock().ok()?.clone();
        snapshot.0.retain(|query, _| {
            query.env == digest
                && query
                    .missing_programs
                    .iter()
                    .all(|file| is_missing(host, file))
        });
        Some(snapshot)
    }
}

/// Sample a system query when a bundled Darwin module reaches it.
pub(crate) fn run(cx: &Cx<'_>, request: &RunRequest) -> Option<HostResult<RunOutput>> {
    let memo_queries = cx.session_data::<std::sync::Arc<PlatformQueries>>()?;
    let host = cx.host();
    let key = query(host, request)?;
    let backtrace = cx.backtrace();
    let frame = backtrace.top()?;
    let file = path::to_native(&frame.file);
    let canonical = host.canonicalize(&file).ok()?;
    if ![
        "Modules/CMakeDetermineSystem.cmake",
        "Modules/Platform/Darwin-Initialize.cmake",
        "Modules/Platform/Darwin.cmake",
    ]
    .iter()
    .any(|relative| host.canonicalize(&cx.cmake_root().join(relative)).as_ref() == Ok(&canonical))
    {
        return None;
    }
    let executable = file_digest(host, &key.program).ok()?;
    let mut memo = memo_queries.0.lock().ok()?;
    if !admits_xcrun(host, &memo, &key, request) {
        return None;
    }
    if let Some(observed) = memo.0.get(&key)
        && observed.executable == executable
    {
        return Some(Ok(observed.result.clone()));
    }
    let receipt = cx
        .session_data::<std::sync::Arc<ReplayReceipts>>()
        .and_then(|receipts| {
            let (digest, result) = receipts::verify(host, &key, executable, &canonical, request)?;
            Some((receipts, digest, result))
        });
    if let Some((receipts, digest, result)) = &receipt
        && receipts.contains(*digest)
    {
        receipts.record(*digest);
        if memo.0.len() < 64 {
            memo.0.insert(
                key,
                Observation {
                    executable,
                    result: result.clone(),
                    request: request.clone(),
                },
            );
        }
        return Some(Ok(result.clone()));
    }
    let output = memo_queries
        .1
        .take(&key)
        .unwrap_or_else(|| launch(host, &key, request));
    if let Ok(result) = &output
        && request.stderr == Output::Inherit
        && !result.stderr.is_empty()
    {
        host.write_out(Stream::Stderr, &result.stderr);
    }
    Some(record(
        host,
        &mut memo,
        key,
        executable,
        request,
        output,
        |result| {
            if let Some((receipts, digest, expected)) = &receipt
                && result == expected
            {
                receipts.record(*digest);
            }
        },
    ))
}

/// Admit `xcrun` only with a standard SDK and developer directory.
fn admits_xcrun(
    host: &dyn Host,
    memo: &RecordedQueries,
    key: &Query,
    request: &RunRequest,
) -> bool {
    key.program != Path::new("/usr/bin/xcrun")
        || (sdk_is_standard(host, request)
            && memo.0.iter().any(|(sample, result)| {
                sample.program == Path::new("/usr/bin/xcode-select")
                    && sample.env == key.env
                    && host
                        .canonicalize(&path::to_native(result.result.stdout.trim_ascii()))
                        .is_ok_and(|developer| {
                            developer == Path::new("/Applications/Xcode.app/Contents/Developer")
                                || developer == Path::new("/Library/Developer/CommandLineTools")
                        })
            }))
}

/// Run an admitted query with captured output and no input.
fn launch(host: &dyn Host, key: &Query, request: &RunRequest) -> HostResult<RunOutput> {
    let mut captured = request.clone();
    if let Some(command) = captured.pipeline.first_mut() {
        command.program = path::from_native(&key.program);
    }
    captured.stdin = Input::Null;
    if captured.stderr == Output::Inherit {
        captured.stderr = Output::Capture;
    }
    host.run(&captured, &mut |_, _| {})
}

/// Record a successful query result with bounded output.
fn record(
    host: &dyn Host,
    memo: &mut RecordedQueries,
    key: Query,
    executable: Digest,
    request: &RunRequest,
    output: HostResult<RunOutput>,
    on_record: impl FnOnce(&RunOutput),
) -> HostResult<RunOutput> {
    let inherited_error = request.stderr == Output::Inherit;
    if let Ok(result) = &output
        && result.results.as_slice() == [ProcessResult::Exited(0)]
        && result.stdout.len().saturating_add(result.stderr.len()) <= 64 * 1024
        && (!inherited_error || result.stderr.is_empty())
        && memo.0.len() < 64
        && file_digest(host, &key.program) == Ok(executable)
    {
        on_record(result);
        memo.0.insert(
            key,
            Observation {
                executable,
                result: result.clone(),
                request: request.clone(),
            },
        );
    }
    output.map(|mut result| {
        if inherited_error {
            result.stderr.clear();
        }
        result
    })
}

impl RecordedQueries {
    pub(super) fn identity(&self) -> Digest {
        let mut key = KeyHasher::new(Domain::ProbeV1);
        key.field(1, b"Darwin platform queries");
        for (query, observation) in &self.0 {
            key.field(2, query.program.as_os_str().as_encoded_bytes());
            key.field(3, observation.executable.as_bytes());
            key.field(4, query.env.as_bytes());
            key.field(5, &[query.stderr]);
            for arg in &query.args {
                key.field(6, arg);
            }
            for result in &observation.result.results {
                if let ProcessResult::Exited(code) = result {
                    key.field(7, &code.to_le_bytes());
                }
            }
            key.field(8, Digest::hash(&observation.result.stdout).as_bytes());
            key.field(9, Digest::hash(&observation.result.stderr).as_bytes());
            for missing in &query.missing_programs {
                key.field(10, missing.as_os_str().as_encoded_bytes());
            }
        }
        key.finish()
    }

    pub(super) fn stamps(&self) -> impl Iterator<Item = InputStamp> + '_ {
        self.0.iter().flat_map(|(query, observation)| {
            std::iter::once(InputStamp::FileHash {
                path: query.program.clone(),
                digest: observation.executable,
            })
            .chain(
                query
                    .missing_programs
                    .iter()
                    .map(|path| InputStamp::Missing { path: path.clone() }),
            )
        })
    }

    pub(super) fn get(&self, host: &dyn Host, request: &RunRequest) -> Option<RunOutput> {
        // The try-compile child captures an inherited error stream and
        // forwards it with `tee`. A sample of an inherited error stream has no
        // error output, so the forwarded stream is also empty.
        let forwarded;
        let request = if request.tee && request.stderr == Output::Capture {
            forwarded = RunRequest {
                tee: false,
                stderr: Output::Inherit,
                ..request.clone()
            };
            &forwarded
        } else {
            request
        };
        let key = query(host, request)?;
        self.0
            .get(&key)
            .filter(|observation| key.stderr != 1 || observation.result.stderr.is_empty())
            .map(|observation| observation.result.clone())
    }
}

fn query(host: &dyn Host, request: &RunRequest) -> Option<Query> {
    let [command] = request.pipeline.as_slice() else {
        return None;
    };
    if !cfg!(target_os = "macos")
        || !matches!(request.stdin, Input::Inherit | Input::Null)
        || request.stdout != Output::Capture
        || request.tee
        || request
            .timeout
            .is_some_and(|limit| limit < MIN_REPLAY_TIMEOUT)
        || request.env.iter().any(|(name, value)| {
            !value.is_empty()
                && (name.starts_with(b"DYLD_") || name.starts_with(b"LD_") || name == b"TOOLCHAINS")
        })
    {
        return None;
    }
    let stderr = match request.stderr {
        Output::Inherit => 1,
        Output::Capture => 2,
        Output::Null => 3,
        _ => return None,
    };
    let requested = path::to_native(&command.program);
    let name = requested.file_name()?.to_str()?;
    // Homebrew installs its launcher under a relocatable prefix, so `brew`
    // has no fixed path. The parent run binds its exit status and output.
    let fixed = match (name, command.args.as_slice()) {
        ("xcode-select", [flag]) if flag == "-print-path" => Some("/usr/bin/xcode-select"),
        ("sw_vers", [flag]) if flag == "-productVersion" => Some("/usr/bin/sw_vers"),
        ("sysctl", [flag, field])
            if matches!(flag.as_slice(), b"-q" | b"-n") && field == "hw.optional.arm64" =>
        {
            Some("/usr/sbin/sysctl")
        }
        ("xcrun", [flag])
            if matches!(flag.as_slice(), b"--show-sdk-path" | b"--show-sdk-version") =>
        {
            Some("/usr/bin/xcrun")
        }
        ("xcrun", [flag, tool]) if flag == "--find" && plain_name(tool) => Some("/usr/bin/xcrun"),
        ("xcrun", [sdk, name, flag])
            if matches!(sdk.as_slice(), b"-sdk" | b"--sdk")
                && plain_name(name)
                && matches!(flag.as_slice(), b"--show-sdk-path" | b"--show-sdk-version") =>
        {
            Some("/usr/bin/xcrun")
        }
        ("brew", [flag]) if flag == "--prefix" => None,
        _ => return None,
    };
    let fixed = fixed.map(Path::new);
    let mut missing_programs = Vec::new();
    let selected = if requested.is_absolute() {
        requested
    } else {
        if requested != Path::new(name) {
            return None;
        }
        let value = &request.env.iter().find(|(key, _)| key == "PATH")?.1;
        let directories = path::to_native(value);
        let mut selected = None;
        for directory in std::env::split_paths(&directories).take(128) {
            if !directory.is_absolute() {
                return None;
            }
            let candidate = directory.join(name);
            if fixed.is_none_or(|fixed| candidate == fixed) && is_executable(host, &candidate) {
                selected = Some(candidate);
                break;
            }
            if !is_missing(host, &candidate) {
                return None;
            }
            missing_programs.push(candidate);
        }
        selected?
    };
    // Require a system query to select the sealed system executable. A program
    // with the same name earlier in PATH can have different effects.
    if fixed.is_some_and(|fixed| selected != fixed) || !is_executable(host, &selected) {
        return None;
    }
    Some(Query {
        program: selected,
        args: command.args.clone(),
        env: environment_fingerprint(&request.env),
        stderr,
        missing_programs,
    })
}

fn is_executable(host: &dyn Host, program: &Path) -> bool {
    host.metadata(program, Follow::Yes)
        .is_ok_and(|info| info.kind == FileKind::File && info.mode & 0o111 != 0)
}

fn is_missing(host: &dyn Host, program: &Path) -> bool {
    host.metadata(program, Follow::No)
        .is_err_and(|error| error.kind == ErrorKind::NotFound)
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
}

fn sdk_is_standard(host: &dyn Host, request: &RunRequest) -> bool {
    let Some((_, sdk)) = request.env.iter().find(|(name, _)| name == "SDKROOT") else {
        return true;
    };
    sdk.is_empty()
        || plain_name(sdk)
        || host.canonicalize(&path::to_native(sdk)).is_ok_and(|sdk| {
            sdk.starts_with("/Library/Developer/CommandLineTools/SDKs")
                || sdk.starts_with("/Applications/Xcode.app/Contents/Developer/Platforms")
        })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use rcmake_host::{CommandSpec, PureHost};

    use super::*;

    #[test]
    fn rejects_mutating_arguments_and_path_shadows() -> HostResult<()> {
        let host = PureHost::default();
        let shadow = Path::new("/work/bin/sysctl");
        host.seed_file(shadow, b"other program")?;
        host.set_permissions(shadow, 0o755)?;
        let fixed = CommandSpec {
            program: "/usr/sbin/sysctl".into(),
            args: vec!["-q".into(), "hw.optional.arm64".into()],
        };
        let system = Path::new("/usr/sbin/sysctl");
        host.seed_file(system, b"system program")?;
        host.set_permissions(system, 0o755)?;
        let request = |command| RunRequest {
            pipeline: vec![command],
            cwd: PathBuf::from("/work"),
            env: vec![("PATH".into(), "/work/bin:/usr/sbin".into())],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: None,
            tee: false,
        };
        assert!(query(&host, &request(fixed.clone())).is_some());
        let mut bounded = request(fixed.clone());
        bounded.timeout = Some(std::time::Duration::from_secs(120));
        assert!(query(&host, &bounded).is_some());
        bounded.timeout = Some(std::time::Duration::from_secs(1));
        assert!(query(&host, &bounded).is_none());
        assert!(
            query(
                &host,
                &request(CommandSpec {
                    program: "sysctl".into(),
                    ..fixed.clone()
                })
            )
            .is_none()
        );
        let mut bare = request(CommandSpec {
            program: "sysctl".into(),
            ..fixed.clone()
        });
        bare.env = vec![("PATH".into(), "/work/missing:/usr/sbin".into())];
        let observed = query(&host, &bare)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the system program."))?;
        let missing = PathBuf::from("/work/missing/sysctl");
        assert_eq!(
            observed.missing_programs.as_slice(),
            std::slice::from_ref(&missing)
        );
        let unused = vec!["/work/unused"; 150].join(":");
        bare.env = vec![(
            "PATH".into(),
            format!("/work/missing:/usr/sbin:{unused}").into(),
        )];
        let observed = query(&host, &bare)
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the system program."))?;
        assert_eq!(
            observed.missing_programs.as_slice(),
            std::slice::from_ref(&missing)
        );
        host.seed_file(&missing, b"shadow")?;
        assert!(query(&host, &bare).is_none());
        bare.env = vec![("PATH".into(), format!("{unused}:/usr/sbin").into())];
        assert!(query(&host, &bare).is_none());
        assert!(
            query(
                &host,
                &request(CommandSpec {
                    args: vec!["-w".into(), "hw.optional.arm64=0".into()],
                    ..fixed
                })
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn homebrew_prefix_binds_the_selected_launcher_and_earlier_candidates() -> HostResult<()> {
        let host = PureHost::default();
        let launcher = Path::new("/opt/homebrew/bin/brew");
        host.seed_file(launcher, b"launcher")?;
        host.set_permissions(launcher, 0o755)?;
        let request = |program: &str, args: Vec<BString>, search: &str| RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args,
            }],
            cwd: PathBuf::from("/work"),
            env: vec![("PATH".into(), search.into())],
            stdin: Input::Inherit,
            stdout: Output::Capture,
            stderr: Output::Inherit,
            timeout: None,
            tee: false,
        };
        let prefix = || vec![BString::from("--prefix")];
        let search = "/work/missing:/opt/homebrew/bin:/usr/bin";
        let observed = query(&host, &request("brew", prefix(), search))
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the launcher."))?;
        assert_eq!(observed.program, launcher);
        assert_eq!(
            observed.missing_programs,
            vec![PathBuf::from("/work/missing/brew")]
        );
        for args in [
            vec![BString::from("--prefix"), BString::from("openssl")],
            vec![BString::from("--repository")],
            vec![BString::from("update")],
        ] {
            assert!(query(&host, &request("brew", args, search)).is_none());
        }
        assert!(query(&host, &request("bin/brew", prefix(), search)).is_none());
        assert!(query(&host, &request("brew", prefix(), "/work/missing:/usr/bin")).is_none());
        let shadow = Path::new("/work/missing/brew");
        host.seed_file(shadow, b"shadow")?;
        assert!(query(&host, &request("brew", prefix(), search)).is_none());
        host.set_permissions(shadow, 0o755)?;
        let shadowed = query(&host, &request("brew", prefix(), search))
            .ok_or_else(|| rcmake_host::HostError::unsupported("Expected the shadow."))?;
        assert_eq!(shadowed.program, shadow);
        assert_ne!(shadowed.program, observed.program);
        Ok(())
    }
}
