// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, HostResult, Input, Output, ProcessResult, RunOutput,
    RunRequest, path,
};
use rcmake_store::{Digest, InputStamp, Trace, TraceMemo};

use super::wrapper::Wrapper;
use crate::configure::concurrent::{self, Job};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Family {
    AppleClang,
    Clang,
    Gnu,
    Msvc,
}

impl Family {
    pub(super) fn from_id(id: &[u8]) -> Option<Self> {
        match id {
            b"AppleClang" => Some(Self::AppleClang),
            b"Clang" => Some(Self::Clang),
            b"GNU" => Some(Self::Gnu),
            b"MSVC" => Some(Self::Msvc),
            _ => None,
        }
    }
}

pub(super) struct ToolchainEvidence {
    pub(super) family: Family,
    pub(super) driver: PathBuf,
    pub(super) sdk: Option<PathBuf>,
    pub(super) sealed: Vec<PathBuf>,
    pub(super) tools: Trace,
    pub(super) telemetry: Vec<BString>,
    pub(super) prefixes: Vec<Vec<BString>>,
    pub(super) link_trace: bool,
    pub(super) identities: Vec<(BString, Digest)>,
    memo: Arc<TraceMemo>,
}

impl ToolchainEvidence {
    pub(super) fn discover(
        host: &dyn Host,
        family: Family,
        program: &Path,
        sdk: &Path,
        cxx: bool,
        memo: Arc<TraceMemo>,
    ) -> Option<Self> {
        if !program.is_absolute() {
            return None;
        }
        let mut evidence = Self {
            family,
            driver: program.to_owned(),
            sdk: None,
            sealed: Vec::new(),
            tools: Vec::new(),
            telemetry: Vec::new(),
            prefixes: vec![Vec::new()],
            link_trace: false,
            identities: Vec::new(),
            memo,
        };
        for depth in 0..=2 {
            let driver = evidence.driver.clone();
            if excluded_launcher(&driver) {
                return None;
            }
            evidence.stamp(host, &driver, false)?;
            let Some(wrapper) = Wrapper::read(host, &driver) else {
                if starts_with_shebang(host, &driver)? {
                    return None;
                }
                break;
            };
            if depth == 2 {
                return None;
            }
            evidence.stamp(host, &wrapper.interpreter, false)?;
            if let Some(interpreter) = wrapper.env_program {
                evidence.stamp(host, &interpreter, false)?;
            }
            evidence.telemetry.extend(wrapper.telemetry);
            if evidence
                .prefixes
                .len()
                .saturating_mul(wrapper.prefixes.len())
                > 8
            {
                return None;
            }
            evidence.prefixes = wrapper
                .prefixes
                .into_iter()
                .flat_map(|prefix| {
                    evidence
                        .prefixes
                        .iter()
                        .map(move |outer| prefix.iter().chain(outer).cloned().collect())
                })
                .collect();
            evidence.driver = wrapper.target;
        }

        match family {
            Family::AppleClang => evidence.apple(host, sdk, cxx)?,
            Family::Clang => evidence.clang(host, cxx)?,
            Family::Gnu => evidence.gnu(host, cxx)?,
            Family::Msvc => evidence.msvc(host)?,
        }
        Some(evidence)
    }

    fn apple(&mut self, host: &dyn Host, sdk: &Path, cxx: bool) -> Option<()> {
        if host.system().sysname != "Darwin" {
            return None;
        }
        let name: &[u8] = if cxx { b"clang++" } else { b"clang" };
        let jobs: Vec<Job<'_, Option<PathBuf>>> = vec![
            Box::new(|| {
                if sdk.is_absolute() {
                    return host.canonicalize(sdk).ok();
                }
                let output = query(
                    host,
                    Path::new("/usr/bin/xcrun"),
                    &["--show-sdk-path".into()],
                )?;
                host.canonicalize(&path::to_native(output.stdout.trim_ascii()))
                    .ok()
            }),
            Box::new(|| xcrun_find(host, name)),
            Box::new(|| xcrun_find(host, b"ld")),
        ];
        let [Some(Some(sdk)), Some(Some(driver)), Some(Some(linker))] =
            <[_; 3]>::try_from(concurrent::run_all(jobs)).ok()?
        else {
            return None;
        };
        if !vendor_sdk(&sdk) {
            return None;
        }
        if !vendor_tools(&driver) || driver.parent() != linker.parent() {
            return None;
        }
        let shim = if cxx { "/usr/bin/c++" } else { "/usr/bin/cc" };
        let clang = if cxx {
            "/usr/bin/clang++"
        } else {
            "/usr/bin/clang"
        };
        if self.driver != Path::new(shim)
            && self.driver != Path::new(clang)
            && host.canonicalize(&self.driver).ok()? != driver
        {
            return None;
        }
        let settings = sdk.join("SDKSettings.json");
        let files = [
            driver.as_path(),
            linker.as_path(),
            Path::new("/usr/bin/xcrun"),
            settings.as_path(),
        ];
        // Hash the compiler files while the compiler answers its queries.
        let memo = Arc::clone(&self.memo);
        let (digests, queries) = concurrent::join(
            move || files.map(|file| digest(&memo, host, file)),
            || ClangQueries::run(host, &driver),
        );
        for (file, digest) in files.into_iter().zip(digests?) {
            self.record(host, file, false, digest)?;
        }
        self.sealed.push(sdk.clone());
        self.sdk = Some(sdk);
        self.link_trace = true;
        self.sealed
            .push(driver.parent()?.parent()?.parent()?.to_path_buf());
        self.clang_configuration(host, &driver, cxx, queries?)
    }

    fn clang(&mut self, host: &dyn Host, cxx: bool) -> Option<()> {
        let driver = self.driver.clone();
        let version = query(host, &driver, &["--version".into()])?;
        if !version.stdout.starts_with(b"clang version ") {
            return None;
        }
        let linker = program_query(host, &driver, "ld")?;
        self.stamp(host, &linker, false)?;
        self.link_trace = linker_dependency_file(host, &linker);
        let queries = ClangQueries::run(host, &driver)?;
        self.clang_configuration(host, &driver, cxx, queries)
    }

    fn clang_configuration(
        &mut self,
        host: &dyn Host,
        driver: &Path,
        cxx: bool,
        queries: ClangQueries,
    ) -> Option<()> {
        let ClangQueries {
            resource,
            triple,
            verbose,
        } = queries;
        if !resource.is_absolute() {
            return None;
        }
        self.identities.push((
            "resource directory".into(),
            Digest::hash(&path::from_native(&resource)),
        ));
        let triple = std::str::from_utf8(triple.stdout.trim_ascii()).ok()?;
        if triple.is_empty()
            || !triple
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return None;
        }
        let mut directories = vec![
            driver.parent()?.to_path_buf(),
            self.driver.parent()?.to_path_buf(),
        ];
        for line in verbose.stderr.split(|&byte| byte == b'\n') {
            let line = line.trim_ascii();
            for label in [
                b"System configuration file directory: ".as_slice(),
                b"User configuration file directory: ",
                b"System configuration file directory:".as_slice(),
                b"User configuration file directory:",
            ] {
                if let Some(value) = line.strip_prefix(label).map(<[u8]>::trim_ascii) {
                    let directory = path::to_native(value);
                    if directory.is_absolute() {
                        directories.push(directory);
                    }
                }
            }
            if let Some(value) = line.strip_prefix(b"Configuration file: ") {
                self.stamp(host, &path::to_native(value), false)?;
            }
        }
        directories.sort_unstable();
        directories.dedup();
        let mode = if cxx { "clang++" } else { "clang" };
        for directory in directories {
            for candidate in [
                format!("{triple}-{mode}.cfg"),
                format!("{triple}.cfg"),
                format!("{mode}.cfg"),
                "clang.cfg".into(),
            ] {
                self.stamp(host, &directory.join(candidate), true)?;
            }
        }
        Some(())
    }

    fn gnu(&mut self, host: &dyn Host, cxx: bool) -> Option<()> {
        let driver = self.driver.clone();
        for flag in ["-dumpfullversion", "-dumpmachine", "-print-sysroot"] {
            let result = query(host, &driver, &[flag.into()])?;
            self.identities
                .push((flag.into(), Digest::hash(result.stdout.trim_ascii())));
        }
        let frontend = if cxx { "cc1plus" } else { "cc1" };
        for name in [frontend, "as", "collect2", "ld"] {
            self.stamp(host, &program_query(host, &driver, name)?, false)?;
        }
        self.link_trace = linker_dependency_file(host, &program_query(host, &driver, "ld")?);
        let libgcc = query_path(host, &driver, "-print-libgcc-file-name")?;
        self.stamp(host, &libgcc, false)?;
        let specs = query_path(host, &driver, "-print-file-name=specs")?;
        if specs.is_absolute() {
            self.stamp(host, &specs, false)?;
        } else if specs == Path::new("specs") {
            self.stamp(host, &libgcc.parent()?.join("specs"), true)?;
        } else {
            return None;
        }
        Some(())
    }

    fn msvc(&mut self, host: &dyn Host) -> Option<()> {
        if host.system().sysname != "Windows"
            || !self
                .driver
                .file_name()?
                .to_str()?
                .eq_ignore_ascii_case("cl.exe")
        {
            return None;
        }
        let directory = self.driver.parent()?.to_owned();
        for name in [
            "c1.dll",
            "c1xx.dll",
            "c2.dll",
            "link.exe",
            "lib.exe",
            "mspdbcore.dll",
        ] {
            self.stamp(host, &directory.join(name), false)?;
        }
        for entry in host.read_dir(&directory).ok()? {
            if entry.name.starts_with(b"msobj") && entry.name.ends_with(b".dll") {
                self.stamp(host, &directory.join(path::to_native(&entry.name)), false)?;
            }
        }
        let vc = path::to_native(&host.env_get(b"VCToolsInstallDir")?);
        let sdk = path::to_native(&host.env_get(b"WindowsSdkDir")?);
        let version = path::to_native(&host.env_get(b"WindowsSDKVersion")?);
        let sdk_version = sdk.join("Include").join(&version);
        self.stamp(host, &sdk_version.join("SDKManifest.xml"), false)?;
        self.stamp(
            host,
            &vc.parent()?
                .parent()?
                .parent()?
                .join("Auxiliary/Build/Microsoft.VCToolsVersion.default.txt"),
            false,
        )?;
        self.sealed.extend([
            host.canonicalize(&vc).ok()?,
            host.canonicalize(&sdk_version).ok()?,
        ]);
        Some(())
    }

    fn stamp(&mut self, host: &dyn Host, path: &Path, optional: bool) -> Option<()> {
        let digest = digest(&self.memo, host, path);
        self.record(host, path, optional, digest)
    }

    fn record(
        &mut self,
        host: &dyn Host,
        path: &Path,
        optional: bool,
        digest: Option<HostResult<Digest>>,
    ) -> Option<()> {
        let stamp = match digest? {
            Ok(digest) => InputStamp::FileHash {
                path: path.to_owned(),
                digest,
            },
            Err(error)
                if optional
                    && error.kind == ErrorKind::NotFound
                    && host
                        .metadata(path, Follow::No)
                        .is_err_and(|error| error.kind == ErrorKind::NotFound) =>
            {
                InputStamp::Missing {
                    path: path.to_owned(),
                }
            }
            Err(_) => return None,
        };
        if !self.tools.contains(&stamp) {
            self.tools.push(stamp);
        }
        Some(())
    }
}

/// Hash one toolchain file. Return `None` for a relative path.
fn digest(memo: &TraceMemo, host: &dyn Host, path: &Path) -> Option<HostResult<Digest>> {
    if !path.is_absolute() {
        return None;
    }
    let binary = path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("dll")
    });
    Some(if binary {
        memo.tool_digest(host, path)
    } else {
        memo.file_digest(host, path)
    })
}

/// Outputs of the Clang driver queries that identify its configuration.
struct ClangQueries {
    resource: PathBuf,
    triple: RunOutput,
    verbose: RunOutput,
}

impl ClangQueries {
    /// Run the independent driver queries at the same time.
    fn run(host: &dyn Host, driver: &Path) -> Option<Self> {
        let outputs = concurrent::run_all(
            ["-print-resource-dir", "-print-target-triple", "-v"]
                .into_iter()
                .map(|flag| -> Job<'_, Option<RunOutput>> {
                    Box::new(move || query(host, driver, &[flag.into()]))
                })
                .collect(),
        );
        let [
            Some(Some(resource)),
            Some(Some(triple)),
            Some(Some(verbose)),
        ] = <[_; 3]>::try_from(outputs).ok()?
        else {
            return None;
        };
        Some(Self {
            resource: path::to_native(resource.stdout.trim_ascii()),
            triple,
            verbose,
        })
    }
}

fn starts_with_shebang(host: &dyn Host, path: &Path) -> Option<bool> {
    let magic = host
        .read(
            path,
            rcmake_host::ReadRange::Slice {
                offset: 0,
                len: Some(2),
            },
        )
        .ok()?;
    Some(magic == b"#!")
}

fn excluded_launcher(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            stem.eq_ignore_ascii_case("ccache") || stem.eq_ignore_ascii_case("sccache")
        })
}

fn linker_dependency_file(host: &dyn Host, linker: &Path) -> bool {
    query(host, linker, &["--help".into()]).is_some_and(|output| {
        output
            .stdout
            .windows(b"--dependency-file".len())
            .any(|word| word == b"--dependency-file")
    })
}

pub(super) fn query(host: &dyn Host, program: &Path, args: &[BString]) -> Option<RunOutput> {
    query_in(host, program, args, &host.initial_cwd(), host.env_all())
}

pub(super) fn query_in(
    host: &dyn Host,
    program: &Path,
    args: &[BString],
    cwd: &Path,
    mut env: Vec<(BString, BString)>,
) -> Option<RunOutput> {
    super::super::super::plan::process_environment(&mut env);
    let output = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: path::from_native(program),
                    args: args.to_vec(),
                }],
                cwd: cwd.to_owned(),
                env,
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(4)),
                tee: false,
            },
            1 << 20,
        )
        .ok()?;
    (output.results.as_slice() == [ProcessResult::Exited(0)]).then_some(output)
}

fn query_path(host: &dyn Host, program: &Path, flag: &str) -> Option<PathBuf> {
    let output = query(host, program, &[flag.into()])?;
    Some(path::to_native(output.stdout.trim_ascii()))
}

fn program_query(host: &dyn Host, driver: &Path, name: &str) -> Option<PathBuf> {
    let program = query_path(host, driver, &format!("-print-prog-name={name}"))?;
    if program.is_absolute() {
        return Some(program);
    }
    if program.as_os_str() != name {
        return None;
    }
    let search = path::to_native(&host.env_get(b"PATH")?);
    std::env::split_paths(&search).find_map(|directory| {
        let file = directory.join(&program);
        (file.is_absolute()
            && host
                .metadata(&file, Follow::Yes)
                .is_ok_and(|stat| stat.kind == FileKind::File && stat.mode & 0o111 != 0))
        .then_some(file)
    })
}

fn xcrun_find(host: &dyn Host, name: &[u8]) -> Option<PathBuf> {
    let program = Path::new("/usr/bin/xcrun");
    let output = query(host, program, &["--find".into(), name.into()])?;
    host.canonicalize(&path::to_native(output.stdout.trim_ascii()))
        .ok()
}

fn vendor_tools(path: &Path) -> bool {
    path.starts_with(
        "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin",
    ) || path.starts_with("/Library/Developer/CommandLineTools/usr/bin")
}

fn vendor_sdk(path: &Path) -> bool {
    path.starts_with("/Library/Developer/CommandLineTools/SDKs")
        || path.starts_with(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs",
        )
}

#[cfg(test)]
mod tests {
    use rcmake_host::{HostError, HostResult, PureHost};
    use rcmake_store::{VerifyOutcome, verify_trace};

    use super::*;

    fn fixture(family: Family) -> HostResult<(PureHost, PathBuf, PathBuf)> {
        let root = PathBuf::from(if cfg!(windows) {
            "C:/work/tools"
        } else {
            "/work/tools"
        });
        let driver = root.join(if family == Family::Gnu {
            "gcc"
        } else {
            "clang"
        });
        let changed = root.join(if family == Family::Gnu {
            "specs"
        } else {
            "clang.cfg"
        });
        let host = PureHost::new(root.clone(), driver.clone());
        for file in ["clang", "gcc", "ld", "cc1", "as", "collect2", "libgcc.a"] {
            host.seed_file(&root.join(file), b"fixture tool")?;
        }
        host.set_command_handler(move |host, request, _| {
            let [command] = request.pipeline.as_slice() else {
                return Err(HostError::unsupported("Expected one tool command."));
            };
            let [flag] = command.args.as_slice() else {
                return Err(HostError::unsupported("Expected one tool option."));
            };
            let absolute =
                |name: &[u8]| path::from_native(&root.join(path::to_native(name))).to_vec();
            let stdout = match flag.as_slice() {
                b"--version" => b"clang version 21.0.0\n".to_vec(),
                b"--help" => b"--dependency-file FILE\n".to_vec(),
                b"-dumpfullversion" => b"14.1.0\n".to_vec(),
                b"-dumpmachine" | b"-print-target-triple" => b"arm64-linux-gnu\n".to_vec(),
                b"-v" | b"-print-sysroot" => Vec::new(),
                b"-print-resource-dir" => absolute(b"resource"),
                b"-print-libgcc-file-name" => absolute(b"libgcc.a"),
                b"-print-file-name=specs"
                    if host.metadata(&root.join("specs"), Follow::No).is_ok() =>
                {
                    absolute(b"specs")
                }
                b"-print-file-name=specs" => b"specs\n".to_vec(),
                value if value.starts_with(b"-print-prog-name=") => {
                    absolute(value.strip_prefix(b"-print-prog-name=").unwrap_or_default())
                }
                _ => return Err(HostError::unsupported("Unexpected tool option.")),
            };
            Ok(RunOutput {
                stdout,
                results: vec![ProcessResult::Exited(0)],
                ..Default::default()
            })
        })?;
        Ok((host, driver, changed))
    }

    fn assert_invalidated(family: Family) -> HostResult<()> {
        let (host, driver, changed) = fixture(family)?;
        let discover = || {
            ToolchainEvidence::discover(
                &host,
                family,
                &driver,
                Path::new(""),
                false,
                Arc::default(),
            )
            .ok_or_else(|| HostError::unsupported("Expected toolchain evidence."))
        };
        let original = discover()?;
        assert!(original.tools.contains(&InputStamp::Missing {
            path: changed.clone()
        }));
        host.seed_file(
            &changed,
            if family == Family::Gnu {
                b"*cc1:\n"
            } else {
                b"-DNEW=1\n"
            },
        )?;
        assert_ne!(verify_trace(&host, &original.tools), VerifyOutcome::Hit);
        assert!(discover()?.tools.iter().any(|stamp| matches!(stamp,
            InputStamp::FileHash { path, .. } if path == &changed)));
        Ok(())
    }

    #[test]
    fn discovers_a_new_driver_configuration_file() -> HostResult<()> {
        assert_invalidated(Family::Clang)
    }

    #[test]
    fn discovers_a_new_gcc_specs_file() -> HostResult<()> {
        assert_invalidated(Family::Gnu)
    }
}
