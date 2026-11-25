use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_host::{
    CommandSpec, Host, Input, Output, ProcessResult, ReadRange, RunOutput, RunRequest, path,
};
use rcmake_store::{Digest, InputStamp, Trace, file_digest};

use super::super::journal::{DependencyClosure, ProcessVerifier, depfile};

const LINK_TRACE: &str = ".rcmake-link-dependencies";
const COMPILE_TRACE: &str = ".rcmake-compile-dependencies";

pub(super) fn admit_source(name: &[u8], source: &[u8]) -> bool {
    name.ends_with(b".c")
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
        && source.is_ascii()
        && !source.iter().any(|byte| b"#?\\\0".contains(byte))
        && !source.windows(2).any(|word| matches!(word, b"__" | b"%:"))
        && !source.windows(3).any(|word| word == b"asm")
}

pub(super) fn admit_environment(environment: &[(BString, BString)]) -> bool {
    !environment.iter().any(|(name, value)| {
        !value.is_empty()
            && (matches!(
                name.as_slice(),
                b"CPATH"
                    | b"C_INCLUDE_PATH"
                    | b"CPLUS_INCLUDE_PATH"
                    | b"OBJC_INCLUDE_PATH"
                    | b"LIBRARY_PATH"
                    | b"COMPILER_PATH"
                    | b"CCC_OVERRIDE_OPTIONS"
                    | b"GCC_EXEC_PREFIX"
                    | b"CLANG_CONFIG_FILE_SYSTEM_DIR"
                    | b"CLANG_CONFIG_FILE_USER_DIR"
                    | b"TOOLCHAINS"
                    | b"LD_TRACE_ARCHIVES"
                    | b"LD_TRACE_DYLIBS"
                    | b"LD_TRACE_FILE"
            ) || name.starts_with(b"DYLD_")
                || name.starts_with(b"LD_"))
    })
}

pub(super) fn plain_define(flag: &[u8]) -> bool {
    matches!(flag.strip_prefix(b"-D").and_then(<[u8]>::split_first), Some((first, rest))
        if (first.is_ascii_alphabetic() || *first == b'_')
            && rest.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_'))
}

#[derive(Clone)]
pub(super) struct AppleToolchain {
    developer_dir: Option<BString>,
    tools: Trace,
    driver: PathBuf,
}

impl AppleToolchain {
    fn discover(host: &dyn Host) -> Option<Self> {
        let driver = xcrun_find(host, b"clang")?;
        let linker = xcrun_find(host, b"ld")?;
        if !vendor_tools(&driver) || driver.parent() != linker.parent() {
            return None;
        }
        let tools = [
            driver.as_path(),
            linker.as_path(),
            Path::new("/usr/bin/xcrun"),
        ]
        .into_iter()
        .map(|file| {
            let digest = file_digest(host, file).ok()?;
            Some(InputStamp::FileHash {
                path: file.to_owned(),
                digest,
            })
        })
        .collect::<Option<Trace>>()?;
        Some(Self {
            developer_dir: host.env_get(b"DEVELOPER_DIR"),
            driver,
            tools,
        })
    }
}

pub(super) struct CompilerEvidence {
    host: Arc<dyn Host>,
    program: PathBuf,
    sdk: PathBuf,
    toolchain: AppleToolchain,
}

impl CompilerEvidence {
    pub(super) fn new(
        host: Arc<dyn Host>,
        cx: &Cx<'_>,
        session: &OnceLock<Option<AppleToolchain>>,
    ) -> Option<Self> {
        if host.system().sysname != "Darwin"
            || cx.var_or_empty(b"CMAKE_C_COMPILER_ID") != "AppleClang"
            || cx.var_or_empty(b"CMAKE_TRY_COMPILE_TARGET_TYPE") == "STATIC_LIBRARY"
            || !cx.var_or_empty(b"CMAKE_C_COMPILER_LAUNCHER").is_empty()
        {
            return None;
        }
        let program = path::to_native(&cx.var_or_empty(b"CMAKE_C_COMPILER"));
        let sdk = path::to_native(&cx.var_or_empty(b"_CMAKE_OSX_SYSROOT_PATH"));
        let sdk = if sdk.is_absolute() {
            sdk
        } else {
            path::to_native(&cx.var_or_empty(b"CMAKE_OSX_SYSROOT"))
        };
        if !vendor_sdk(&host.canonicalize(&sdk).ok()?) {
            return None;
        }
        let toolchain = session
            .get_or_init(|| AppleToolchain::discover(host.as_ref()))
            .clone()?;
        if toolchain.developer_dir != host.env_get(b"DEVELOPER_DIR")
            || (program != Path::new("/usr/bin/cc")
                && program != Path::new("/usr/bin/clang")
                && host.canonicalize(&program).ok()? != toolchain.driver)
        {
            return None;
        }
        Some(Self {
            host,
            program,
            sdk,
            toolchain,
        })
    }

    pub(super) fn identity(&self) -> BTreeMap<BString, Digest> {
        self.toolchain
            .tools
            .iter()
            .filter_map(|stamp| match stamp {
                InputStamp::FileHash { path: file, digest } => {
                    Some((path::from_native(file), *digest))
                }
                _ => None,
            })
            .collect()
    }
}

fn xcrun_find(host: &dyn Host, name: &[u8]) -> Option<PathBuf> {
    let result = host
        .run_bounded(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "/usr/bin/xcrun".into(),
                    args: vec!["--find".into(), name.into()],
                }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(2)),
                tee: false,
            },
            4096,
        )
        .ok()?;
    if result.results.as_slice() != [ProcessResult::Exited(0)] {
        return None;
    }
    host.canonicalize(&path::to_native(result.stdout.trim_ascii()))
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

impl ProcessVerifier for CompilerEvidence {
    fn instrument(&self, request: &mut RunRequest) {
        let Some((compiles, has_depfile)) = self.classify(request) else {
            return;
        };
        let Some(command) = request.pipeline.first_mut() else {
            return;
        };
        if compiles && !has_depfile {
            command
                .args
                .extend(["-MD".into(), "-MF".into(), COMPILE_TRACE.into()]);
        } else if !compiles {
            command.args.extend([
                "-Xlinker".into(),
                "-dependency_info".into(),
                "-Xlinker".into(),
                LINK_TRACE.into(),
            ]);
        }
    }

    fn preflight(&self, request: &RunRequest) -> Option<Trace> {
        let original = strip_link_trace(request);
        self.classify(&original)?;
        let mut tools = self.toolchain.tools.clone();
        let settings = self.sdk.join("SDKSettings.json");
        tools.push(InputStamp::FileHash {
            digest: file_digest(self.host.as_ref(), &settings).ok()?,
            path: settings,
        });
        Some(tools)
    }

    fn dependencies(&self, request: &RunRequest, output: &RunOutput) -> Option<DependencyClosure> {
        if output.results.as_slice() != [ProcessResult::Exited(0)] {
            return None;
        }
        let cmd = request.pipeline.first()?;
        if cmd.args.iter().any(|arg| arg == "-c") {
            let dep = cmd.args.windows(2).find_map(|pair| match pair {
                [option, file] if option == "-MF" => Some(path::to_native(file)),
                _ => None,
            })?;
            let dep = request.cwd.join(dep);
            if !dep.starts_with(&request.cwd) {
                return None;
            }
            let bytes = self
                .host
                .read(&dep, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            let files = depfile::make_dependencies(&bytes, &request.cwd)?;
            return Some(DependencyClosure {
                files,
                searches: Vec::new(),
            });
        }
        let bytes = self
            .host
            .read(
                &request.cwd.join(LINK_TRACE),
                ReadRange::All { limit: 1 << 20 },
            )
            .ok()?;
        link_dependencies(&bytes, &request.cwd)
    }
}

impl CompilerEvidence {
    fn classify(&self, request: &RunRequest) -> Option<(bool, bool)> {
        let [cmd] = request.pipeline.as_slice() else {
            return None;
        };
        if path::to_native(&cmd.program) != self.program || !admit_environment(&request.env) {
            return None;
        }
        let mut args = cmd.args.iter();
        let mut compiles = false;
        let mut depfile = false;
        let mut input = false;
        let mut output = false;
        let mut sysroot = false;
        while let Some(arg) = args.next() {
            match arg.as_slice() {
                b"-c" => compiles = true,
                b"-o" | b"-MF" | b"-MT" | b"-MQ" => {
                    if !private_path(&request.cwd, &path::to_native(args.next()?)) {
                        return None;
                    }
                    output |= arg == "-o";
                    depfile |= arg == "-MF";
                }
                b"-isysroot" => {
                    let actual = path::to_native(args.next()?);
                    if self.host.canonicalize(&actual).ok()?
                        != self.host.canonicalize(&self.sdk).ok()?
                    {
                        return None;
                    }
                    sysroot = true;
                }
                b"-arch" => {
                    if !matches!(args.next()?.as_slice(), b"arm64" | b"x86_64") {
                        return None;
                    }
                }
                b"-g"
                | b"-g0"
                | b"-O0"
                | b"-O1"
                | b"-O2"
                | b"-O3"
                | b"-Os"
                | b"-DNDEBUG"
                | b"-MD"
                | b"-MMD"
                | b"-fcolor-diagnostics"
                | b"-fno-color-diagnostics"
                | b"-fPIC"
                | b"-fPIE"
                | b"-Wl,-search_paths_first"
                | b"-Wl,-headerpad_max_install_names" => {}
                value if plain_define(value) => {}
                value
                    if value.starts_with(b"-mmacosx-version-min=")
                        && value
                            .strip_prefix(b"-mmacosx-version-min=")?
                            .iter()
                            .all(|b| b.is_ascii_digit() || *b == b'.') => {}
                b"-std=c99" | b"-std=c11" | b"-std=c17" | b"-std=c23" | b"-std=gnu99"
                | b"-std=gnu11" | b"-std=gnu17" | b"-std=gnu23" => {}
                value
                    if !value.starts_with(b"-")
                        && (value.ends_with(b".c") || value.ends_with(b".o"))
                        && private_path(&request.cwd, &path::to_native(value)) =>
                {
                    input = true;
                }
                _ => return None,
            }
        }
        (input && output && sysroot).then_some((compiles, depfile))
    }
}

fn strip_link_trace(request: &RunRequest) -> std::borrow::Cow<'_, RunRequest> {
    let Some(command) = request.pipeline.first() else {
        return std::borrow::Cow::Borrowed(request);
    };
    if !matches!(command.args.as_slice(), [.., a, b, c, d] if a == "-Xlinker" && b == "-dependency_info" && c == "-Xlinker" && d == LINK_TRACE)
    {
        return std::borrow::Cow::Borrowed(request);
    }
    let mut request = request.clone();
    if let Some(command) = request.pipeline.first_mut() {
        command.args.truncate(command.args.len().saturating_sub(4));
    }
    std::borrow::Cow::Owned(request)
}

fn private_path(cwd: &Path, file: &Path) -> bool {
    cwd.join(file).strip_prefix(cwd).is_ok_and(|suffix| {
        !suffix.as_os_str().is_empty()
            && suffix
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    })
}

fn link_dependencies(bytes: &[u8], cwd: &Path) -> Option<DependencyClosure> {
    use super::super::journal::SearchObservation;
    let bytes = bytes.strip_suffix(&[0])?;
    let mut files = BTreeSet::new();
    let mut searches = Vec::new();
    // The version record begins with a zero opcode, which is also the record separator.
    let version = bytes.strip_prefix(&[0])?;
    let mut records = version.split(|&byte| byte == 0);
    records.next()?;
    for record in records {
        match record.split_first()? {
            (0x10, name) => {
                files.insert(cwd.join(path::to_native(name)));
            }
            (0x11, name) => {
                let path = cwd.join(path::to_native(name));
                let parent = path.parent()?.to_owned();
                let name = PathBuf::from(path.file_name()?);
                searches.push(SearchObservation {
                    root: parent,
                    relative: name,
                    found: false,
                });
            }
            (0x40, _) => {}
            _ => return None,
        }
    }
    (!files.is_empty()).then_some(DependencyClosure { files, searches })
}
