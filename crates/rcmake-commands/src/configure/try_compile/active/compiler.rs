use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_host::{FileKind, Follow, Host, ProcessResult, ReadRange, RunOutput, RunRequest, path};
use rcmake_store::{Digest, InputStamp, Trace, TraceMemo, VerifyOutcome};

use crate::configure::build_tool_facts::{BuildToolFacts, VerifiedNinja};
use crate::configure::platform_queries::{PlatformQueries, RecordedQueries};

use super::super::journal::{DependencyClosure, ProcessVerifier, SearchObservation, depfile};
use super::super::plan::{process_environment, process_environment_fingerprint};

mod flags;
mod hazards;
mod link;
mod search;
mod selection;
mod toolchain;
mod wrapper;

use flags::{Flags, attached_search, library_file, private_path, pure_flag};
use hazards::source_hazards;
use link::link_dependencies;
pub(super) use selection::ProbeCompiler;
use toolchain::{Family, ToolchainEvidence, query_in};

const LINK_TRACE: &str = ".rcmake-link-dependencies";
const GNU_LINK_TRACE: &str = "-Wl,--dependency-file=.rcmake-link-dependencies";
const COMPILE_TRACE: &str = ".rcmake-compile-dependencies";
const MSVC_TRACE: &str = ".rcmake-source-dependencies.json";
const FAILURE_TRACE: &str = ".rcmake-failed-dependencies";

pub(super) fn admit_source(name: &[u8], source: &[u8]) -> bool {
    flags::source_language(name).is_some()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(byte))
        && !source.contains(&0)
        && source_hazards(source)
}

enum NinjaEvidence {
    /// Non-Ninja generator: no Ninja version verification applies.
    NotRequired,
    /// `CMAKE_MAKE_PROGRAM` verified as a certified native Ninja >= 1.3 binary
    /// under an admitted environment, eligible for probe caching and replay.
    Certified(VerifiedNinja),
    /// Uncertified (for example a script wrapper or non-admitted environment) or
    /// invalid Ninja: bypass probe cache lookup and speculation, falling back to
    /// live child execution.
    Uncertified,
}

/// Verify the active Ninja executable and return replayable evidence only when it is a
/// certified native binary under an admitted environment. Uncertified wrappers decline
/// probe-cache lookup and speculation so the child executes live in its own directory.
fn ninja_snapshot(host: &dyn Host, cx: &mut Cx<'_>, memo: &TraceMemo) -> NinjaEvidence {
    if !matches!(
        cx.project().generator,
        Some(rcmake_model::GeneratorKind::Ninja | rcmake_model::GeneratorKind::NinjaMultiConfig)
    ) {
        return NinjaEvidence::NotRequired;
    }
    let program = path::to_native(&cx.var_or_empty(b"CMAKE_MAKE_PROGRAM"));
    if !program.is_absolute() {
        return NinjaEvidence::Uncertified;
    }
    let env = host.env_all();
    let native = admit_environment(&env)
        && host
            .canonicalize(&program)
            .ok()
            .is_some_and(|canonical| super::native_ninja(host, &canonical));
    if !native {
        return NinjaEvidence::Uncertified;
    }
    let Ok(digest) = memo.file_digest(host, &program) else {
        return NinjaEvidence::Uncertified;
    };
    let verified = if let Some(facts) = cx.session_data::<Arc<BuildToolFacts>>() {
        facts.sample_ninja(host, &program, digest, env, cx.current_binary_dir())
    } else {
        BuildToolFacts::default().sample_ninja(host, &program, digest, env, cx.current_binary_dir())
    };
    match verified {
        Some(verified) => NinjaEvidence::Certified(verified),
        None => NinjaEvidence::Uncertified,
    }
}

/// Copy the platform queries that a try-compile child can reuse.
fn platform_snapshot(host: &dyn Host, cx: &Cx<'_>) -> RecordedQueries {
    let mut environment = host.env_all();
    process_environment(&mut environment);
    cx.session_data::<Arc<PlatformQueries>>()
        .and_then(|memo| {
            memo.refresh(host, &environment);
            memo.snapshot(host, &environment)
        })
        .unwrap_or_default()
}

pub(in crate::configure) fn admit_environment(environment: &[(BString, BString)]) -> bool {
    !environment.iter().any(|(name, value)| {
        let name = name.to_ascii_uppercase();
        !value.is_empty()
            && (matches!(
                name.as_slice(),
                b"COMPILER_PATH"
                    | b"CCC_OVERRIDE_OPTIONS"
                    | b"GCC_EXEC_PREFIX"
                    | b"TOOLCHAINS"
                    | b"DEPENDENCIES_OUTPUT"
                    | b"SUNPRO_DEPENDENCIES"
                    | b"VS_UNICODE_OUTPUT"
                    | b"LD_PRELOAD"
                    | b"LD_AUDIT"
                    | b"LD_LIBRARY_PATH"
                    | b"BASH_ENV"
                    | b"ENV"
                    | b"SHELLOPTS"
                    | b"BASHOPTS"
            ) || [
                b"DYLD_".as_slice(),
                b"LD_TRACE_",
                b"CLANG_CONFIG_FILE_",
                b"BASH_FUNC_",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix)))
    })
}

pub(super) fn admit_cmake_flag(flag: &[u8]) -> bool {
    // The option changes only warnings about unused cache entries.
    if flag.is_empty() || flag == b"--no-warn-unused-cli" {
        return true;
    }
    let Some(define) = flag.strip_prefix(b"-D") else {
        return false;
    };
    let mut parts = define.splitn(2, |&byte| byte == b'=');
    let (Some(name), Some(value)) = (parts.next(), parts.next()) else {
        return false;
    };
    let key = name.split(|&byte| byte == b':').next().unwrap_or_default();
    if key == b"CMAKE_SKIP_RPATH" {
        return rcmake_base::truth::is_on(value) || rcmake_base::truth::is_off(value);
    }
    matches!(
        key,
        b"COMPILE_DEFINITIONS"
            | b"INCLUDE_DIRECTORIES"
            | b"LINK_DIRECTORIES"
            | b"CMAKE_C_FLAGS"
            | b"CMAKE_CXX_FLAGS"
            | b"CMAKE_C_STANDARD_LIBRARIES"
            | b"CMAKE_CXX_STANDARD_LIBRARIES"
            | b"EXE_LINKER_FLAGS"
    )
}

/// Admit one `COMPILE_DEFINITIONS` item.
///
/// The probe project splits each item into command words. Simple words split
/// at white space in the same way.
pub(super) fn admit_definition(flag: &[u8]) -> bool {
    if flag.iter().any(u8::is_ascii_whitespace) {
        return !flag.iter().any(|byte| b"\"'\\".contains(byte))
            && flag
                .split(u8::is_ascii_whitespace)
                .filter(|word| !word.is_empty())
                .all(admit_definition_word);
    }
    admit_definition_word(flag)
}

fn admit_definition_word(flag: &[u8]) -> bool {
    matches!(flag, b"-v" | b"-Wl,-v")
        || pure_flag(flag)
        || attached_search(flag).is_some()
        || matches!(
            flag,
            b"-I" | b"-isystem" | b"-iquote" | b"-idirafter" | b"-F" | b"-iframework"
        )
}

pub(super) fn admit_link(flag: &[u8]) -> bool {
    flag.starts_with(b"-l")
        || flag.starts_with(b"-L")
        || matches!(
            flag,
            b"-framework"
                | b"-Wl,--as-needed"
                | b"-Wl,-search_paths_first"
                | b"-Wl,-headerpad_max_install_names"
        )
        || flag
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+.-:".contains(byte))
        || (path::to_native(flag).is_absolute() && library_file(flag))
}

type ToolKey = (PathBuf, PathBuf, BString, bool, Digest);
type SearchKey = (PathBuf, Vec<BString>, Digest);
type SearchMemo = Mutex<BTreeMap<SearchKey, (Vec<PathBuf>, Vec<bool>)>>;

#[derive(Default)]
pub(super) struct SessionEvidence {
    tools: Mutex<BTreeMap<ToolKey, Arc<ToolchainEvidence>>>,
    searches: Arc<SearchMemo>,
}

pub(super) struct CompilerEvidence {
    host: Arc<dyn Host>,
    program: PathBuf,
    memo: Arc<TraceMemo>,
    toolchain: Arc<ToolchainEvidence>,
    archive: BTreeMap<PathBuf, InputStamp>,
    searches: Arc<SearchMemo>,
    cxx: bool,
    external_source: Option<(PathBuf, Digest)>,
    platform: RecordedQueries,
    ninja: Option<VerifiedNinja>,
}

impl CompilerEvidence {
    pub(super) fn new(
        host: Arc<dyn Host>,
        cx: &mut Cx<'_>,
        session: &SessionEvidence,
        name: &[u8],
        memo: &Arc<TraceMemo>,
        external_source: Option<(PathBuf, Digest)>,
    ) -> Option<Self> {
        let cxx = flags::source_language(name)?;
        let language: &[u8] = if cxx { b"CXX" } else { b"C" };
        let compiler_var = |suffix: &[u8]| cx.var_or_empty(&[b"CMAKE_", language, suffix].concat());
        if !compiler_var(b"_COMPILER_LAUNCHER").is_empty() {
            return None;
        }
        let id = compiler_var(b"_COMPILER_ID");
        let family = Family::from_id(&id)?;
        if family == Family::Msvc && !msvc_dependency_version(&compiler_var(b"_COMPILER_VERSION")) {
            return None;
        }
        let program = path::to_native(&compiler_var(b"_COMPILER"));
        let sdk = path::to_native(&cx.var_or_empty(b"_CMAKE_OSX_SYSROOT_PATH"));
        let sdk = if sdk.is_absolute() {
            sdk
        } else {
            path::to_native(&cx.var_or_empty(b"CMAKE_OSX_SYSROOT"))
        };
        let key = (
            program.clone(),
            sdk.clone(),
            id,
            cxx,
            process_environment_fingerprint(&host.env_all()),
        );
        let mut tools = session.tools.lock().ok()?;
        if tools.get(&key).is_some_and(|toolchain| {
            memo.verify(host.as_ref(), &toolchain.tools) != VerifyOutcome::Hit
        }) {
            tools.remove(&key);
            session.searches.lock().ok()?.clear();
        }
        let toolchain = if let Some(toolchain) = tools.get(&key) {
            Arc::clone(toolchain)
        } else {
            let toolchain = Arc::new(ToolchainEvidence::discover(
                host.as_ref(),
                family,
                &program,
                &sdk,
                cxx,
                Arc::clone(memo),
            )?);
            if tools.len() < 32 {
                tools.insert(key, Arc::clone(&toolchain));
            }
            toolchain
        };
        drop(tools);
        let mut archive = BTreeMap::new();
        if cx.var_or_empty(b"CMAKE_TRY_COMPILE_TARGET_TYPE") == "STATIC_LIBRARY" {
            for variable in [b"CMAKE_AR".as_slice(), b"CMAKE_RANLIB"] {
                let file = path::to_native(&cx.var_or_empty(variable));
                if file.as_os_str().is_empty() {
                    continue;
                }
                if !file.is_absolute() {
                    return None;
                }
                archive.insert(
                    file.clone(),
                    InputStamp::FileHash {
                        digest: memo.file_digest(host.as_ref(), &file).ok()?,
                        path: file,
                    },
                );
            }
        }
        let platform = platform_snapshot(host.as_ref(), cx);
        let ninja = match ninja_snapshot(host.as_ref(), cx, memo) {
            NinjaEvidence::NotRequired => None,
            NinjaEvidence::Certified(verified) => Some(verified),
            NinjaEvidence::Uncertified => return None,
        };
        Some(Self {
            host,
            program,
            memo: Arc::clone(memo),
            toolchain,
            archive,
            searches: Arc::clone(&session.searches),
            cxx,
            external_source,
            platform,
            ninja,
        })
    }

    pub(super) fn identity(&self) -> BTreeMap<BString, Digest> {
        let mut identity: BTreeMap<_, _> = self
            .toolchain
            .tools
            .iter()
            .chain(self.archive.values())
            .filter_map(|stamp| match stamp {
                InputStamp::FileHash { path: file, digest } => {
                    Some((path::from_native(file), *digest))
                }
                InputStamp::Missing { path: file } => {
                    Some((path::from_native(file), Digest::hash(b"missing file")))
                }
                _ => None,
            })
            .chain(self.toolchain.identities.iter().cloned())
            .collect();
        identity.insert("platform-queries".into(), self.platform.identity());
        if let Some((file, digest)) = &self.external_source {
            identity.insert(path::from_native(file), *digest);
        }
        identity
    }

    fn classify(&self, request: &RunRequest) -> Option<Flags> {
        let [cmd] = request.pipeline.as_slice() else {
            return None;
        };
        if path::to_native(&cmd.program) != self.program || !admit_environment(&request.env) {
            return None;
        }
        if !self.check_telemetry(request) {
            return None;
        }
        let options;
        let args = if self.toolchain.family == Family::Msvc {
            options = msvc_environment_args(request, &cmd.args)?;
            &options
        } else {
            &cmd.args
        };
        let flags = flags::classify(
            args,
            &request.cwd,
            self.toolchain.family,
            self.external_source
                .as_ref()
                .map(|(file, _)| file.as_path()),
        )?;
        if flags.source.is_none() && !self.toolchain.link_trace {
            return None;
        }
        if let Some(sdk) = &self.toolchain.sdk {
            for pair in flags.search.windows(2) {
                if let [key, value] = pair
                    && matches!(key.as_slice(), b"-isysroot" | b"--sysroot")
                    && self.host.canonicalize(&path::to_native(value)).as_ref() != Ok(sdk)
                {
                    return None;
                }
            }
        }
        Some(flags)
    }

    fn check_telemetry(&self, request: &RunRequest) -> bool {
        self.toolchain.telemetry.iter().all(|value| {
            let expanded = value
                .strip_prefix(b"\"$")
                .and_then(|word| word.strip_suffix(b"\""));
            let value = match expanded {
                Some(name) => {
                    let name = name
                        .strip_prefix(b"{")
                        .and_then(|word| word.strip_suffix(b"}"))
                        .unwrap_or(name);
                    let Some((_, value)) =
                        request.env.iter().find(|(key, _)| key.as_slice() == name)
                    else {
                        return false;
                    };
                    value
                }
                None => value,
            };
            let output = path::to_native(value);
            let Some(parent) = output
                .parent()
                .and_then(|directory| self.host.canonicalize(directory).ok())
            else {
                return false;
            };
            let Some(cwd) = self.host.canonicalize(&request.cwd).ok() else {
                return false;
            };
            output.is_absolute()
                && !parent.starts_with(cwd)
                && (self
                    .host
                    .metadata(&output, Follow::No)
                    .is_ok_and(|stat| stat.kind == FileKind::File && stat.nlink == 1)
                    || self
                        .host
                        .metadata(&output, Follow::No)
                        .is_err_and(|error| error.kind == ErrorKind::NotFound))
        })
    }

    fn archive_inputs(&self, request: &RunRequest) -> Option<Vec<PathBuf>> {
        let [cmd] = request.pipeline.as_slice() else {
            return None;
        };
        let program = path::to_native(&cmd.program);
        self.archive.get(&program)?;
        let name = program.file_stem()?.to_str()?;
        if name.eq_ignore_ascii_case("lib") && self.toolchain.family == Family::Msvc {
            if request.env.iter().any(|(name, value)| {
                !value.is_empty()
                    && [b"LINK".as_slice(), b"_LINK_"]
                        .iter()
                        .any(|key| name.eq_ignore_ascii_case(key))
            }) {
                return None;
            }
            let mut output = false;
            let mut inputs = Vec::new();
            for arg in &cmd.args {
                if let Some(file) = arg
                    .strip_prefix(b"/OUT:")
                    .or_else(|| arg.strip_prefix(b"/out:"))
                {
                    output = private_path(&request.cwd, &path::to_native(file));
                } else if arg == "/nologo" {
                    continue;
                } else if arg.ends_with(b".obj")
                    && private_path(&request.cwd, &path::to_native(arg))
                {
                    inputs.push(request.cwd.join(path::to_native(arg)));
                } else {
                    return None;
                }
            }
            return (output && !inputs.is_empty()).then_some(inputs);
        }
        let inputs = if matches!(name, "ar" | "llvm-ar" | "gcc-ar") {
            let [operation, output, inputs @ ..] = cmd.args.as_slice() else {
                return None;
            };
            if !matches!(
                operation.as_slice(),
                b"qc" | b"q" | b"rc" | b"rcs" | b"-qc" | b"qcD"
            ) || !private_path(&request.cwd, &path::to_native(output))
                || inputs.is_empty()
            {
                return None;
            }
            inputs
        } else if name == "libtool" && self.toolchain.family == Family::AppleClang {
            let [mode, option, output, inputs @ ..] = cmd.args.as_slice() else {
                return None;
            };
            if mode != "-static"
                || option != "-o"
                || inputs.is_empty()
                || !private_path(&request.cwd, &path::to_native(output))
            {
                return None;
            }
            inputs
        } else if matches!(name, "ranlib" | "llvm-ranlib" | "gcc-ranlib") && cmd.args.len() == 1 {
            &cmd.args
        } else {
            return None;
        };
        inputs
            .iter()
            .map(|input| {
                let file = path::to_native(input);
                (private_path(&request.cwd, &file)
                    && (input.ends_with(b".o") || input.ends_with(b".a")))
                .then(|| request.cwd.join(file))
            })
            .collect()
    }
}

fn msvc_dependency_version(version: &[u8]) -> bool {
    let number = |part| std::str::from_utf8(part).ok()?.parse::<u64>().ok();
    let mut parts = version.split(|&byte| byte == b'.').filter_map(number);
    matches!(
        (parts.next(), parts.next()),
        (Some(19), Some(27..)) | (Some(20..), Some(_))
    )
}

fn msvc_environment_args(request: &RunRequest, args: &[BString]) -> Option<Vec<BString>> {
    let parse = |key: &[u8]| {
        let Some((_, value)) = request
            .env
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
        else {
            return Some(Vec::new());
        };
        if value
            .iter()
            .any(|byte| matches!(byte, b'"' | b'#' | 0 | b'\r' | b'\n'))
        {
            return None;
        }
        Some(
            value
                .split(u8::is_ascii_whitespace)
                .filter(|word| !word.is_empty())
                .map(BString::from)
                .collect(),
        )
    };
    let mut options = parse(b"CL")?;
    options.extend_from_slice(args);
    options.extend(parse(b"_CL_")?);
    Some(options)
}

impl ProcessVerifier for CompilerEvidence {
    fn recorded_platform_query(&self, request: &RunRequest) -> Option<RunOutput> {
        if let Some(output) = self.platform.get(self.host.as_ref(), request) {
            return Some(output);
        }
        if let Some(ninja) = &self.ninja
            && admit_environment(&request.env)
            && ninja.matches(request)
        {
            return Some(ninja.output.clone());
        }
        None
    }

    fn instrument(&self, request: &mut RunRequest) {
        let Some(flags) = self.classify(request) else {
            return;
        };
        let Some(command) = request.pipeline.first_mut() else {
            return;
        };
        if flags.source.is_some() {
            // A verbose driver prints its commands, so it gets no private
            // option. Without a build depfile, the probe is not published.
            if flags.depfile.is_none() && !flags.verbose {
                if self.toolchain.family == Family::Msvc {
                    command
                        .args
                        .extend(["/sourceDependencies".into(), MSVC_TRACE.into()]);
                } else {
                    command
                        .args
                        .extend(["-MD".into(), "-MF".into(), COMPILE_TRACE.into()]);
                }
            }
        } else if flags.verbose {
            // The driver prints a verbose link command. A second link in
            // `dependencies` collects the evidence.
        } else if self.toolchain.family == Family::AppleClang {
            command.args.extend([
                "-Xlinker".into(),
                "-dependency_info".into(),
                "-Xlinker".into(),
                LINK_TRACE.into(),
            ]);
        } else {
            command.args.push(GNU_LINK_TRACE.into());
        }
    }

    fn preflight(&self, request: &RunRequest) -> Option<Trace> {
        let mut tools = self.toolchain.tools.clone();
        tools.extend(self.platform.stamps());
        if let Some((path, digest)) = &self.external_source {
            tools.push(InputStamp::FileHash {
                path: path.clone(),
                digest: *digest,
            });
        }
        if self.archive_inputs(request).is_some() {
            tools.extend(self.archive.values().cloned());
            return Some(tools);
        }
        let original = strip_instrumentation(request);
        let flags = self.classify(&original)?;
        for directory in self.search_directories(&original, &flags)? {
            if let Some(stamp) = self.coverage(&directory, &request.cwd, flags.source.is_some())? {
                tools.push(stamp);
            }
        }
        Some(tools)
    }

    fn confines_writes(&self, request: &RunRequest) -> bool {
        // A compiler wrapper can write telemetry to an external path.
        self.toolchain.telemetry.is_empty()
            && request
                .pipeline
                .first()
                .is_some_and(|command| path::to_native(&command.program) == self.program)
    }

    fn is_probe_compile(&self, request: &RunRequest) -> bool {
        self.classify(&strip_instrumentation(request))
            .is_some_and(|flags| flags.source.is_some())
    }

    fn dependencies(
        &self,
        request: &RunRequest,
        output: &RunOutput,
        preflight: &[InputStamp],
    ) -> Option<DependencyClosure> {
        let mut closure = self.closure(request, output)?;
        if self.archive_inputs(request).is_some() {
            return Some(closure);
        }
        let original = strip_instrumentation(request);
        let flags = self.classify(&original)?;
        if let Some(source) = &flags.source {
            let source = request.cwd.join(path::to_native(source));
            if !closure.files.iter().all(|file| {
                file == &source || self.header_is_covered(file, &request.cwd, preflight)
            }) {
                return None;
            }
        } else {
            closure
                .searches
                .retain(|search| !self.in_sealed_root(&search.root));
        }
        Some(closure)
    }
}

impl CompilerEvidence {
    fn closure(&self, request: &RunRequest, output: &RunOutput) -> Option<DependencyClosure> {
        let success = output.results.as_slice() == [ProcessResult::Exited(0)];
        if let Some(inputs) = self.archive_inputs(request) {
            return success.then_some(DependencyClosure {
                files: inputs.into_iter().collect(),
                searches: Vec::new(),
            });
        }
        let original = strip_instrumentation(request);
        let flags = self.classify(&original)?;
        if let Some(source) = &flags.source {
            if !success {
                return self.failure_dependencies(&original, source);
            }
            let trace = flags.depfile.unwrap_or_else(|| {
                request.cwd.join(if self.toolchain.family == Family::Msvc {
                    MSVC_TRACE
                } else {
                    COMPILE_TRACE
                })
            });
            let bytes = self
                .host
                .read(&trace, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            let files = if self.toolchain.family == Family::Msvc {
                depfile::msvc_dependencies(&bytes, &request.cwd)?
            } else {
                depfile::make_dependencies(&bytes, &request.cwd)?
            };
            if !files
                .iter()
                .all(|file| self.admit_dependency_source(file, &request.cwd))
            {
                return None;
            }
            return Some(DependencyClosure {
                files,
                searches: Vec::new(),
            });
        }
        if flags.verbose {
            return (success && self.toolchain.family == Family::AppleClang)
                .then(|| self.verbose_apple_link(&original, &flags))
                .flatten();
        }
        let bytes = match self.host.read(
            &request.cwd.join(LINK_TRACE),
            ReadRange::All { limit: 1 << 20 },
        ) {
            Ok(bytes) => bytes,
            Err(error)
                if !success
                    && error.kind == ErrorKind::NotFound
                    && self.toolchain.family == Family::AppleClang =>
            {
                return self.failed_apple_link(&original, &flags, output);
            }
            Err(_) => return None,
        };
        let mut closure = if self.toolchain.family == Family::AppleClang {
            link_dependencies(&bytes, &request.cwd)?
        } else {
            DependencyClosure {
                files: depfile::make_dependencies(&bytes, &request.cwd)?,
                searches: Vec::new(),
            }
        };
        closure.files.extend(flags.inputs);
        Some(closure)
    }
}

impl CompilerEvidence {
    fn failure_dependencies(
        &self,
        request: &RunRequest,
        source: &[u8],
    ) -> Option<DependencyClosure> {
        if self.toolchain.family == Family::Msvc {
            return None;
        }
        let command = request.pipeline.first()?;
        let mut args = Vec::new();
        let mut arguments = command.args.iter();
        while let Some(arg) = arguments.next() {
            match arg.as_slice() {
                b"-MF" | b"-MQ" | b"-MT" | b"-o" => {
                    arguments.next()?;
                }
                b"-MD" | b"-MMD" | b"-c" => {}
                _ if arg.as_slice() == source => {}
                _ => args.push(arg.clone()),
            }
        }
        args.extend([
            "-E".into(),
            "-M".into(),
            "-MG".into(),
            "-MF".into(),
            FAILURE_TRACE.into(),
            "-o".into(),
            if cfg!(windows) {
                "NUL".into()
            } else {
                "/dev/null".into()
            },
            source.into(),
        ]);
        let trace = request.cwd.join(FAILURE_TRACE);
        let mut files = BTreeSet::new();
        let mut searches = Vec::new();
        for prefix in &self.toolchain.prefixes {
            let options: Vec<_> = prefix.iter().chain(&args).cloned().collect();
            if self.host.metadata(&trace, Follow::No).is_ok() {
                self.host.remove_file(&trace).ok()?;
            }
            query_in(
                self.host.as_ref(),
                &self.toolchain.driver,
                &options,
                &request.cwd,
                request.env.clone(),
            )?;
            let bytes = self
                .host
                .read(&trace, ReadRange::All { limit: 1 << 20 })
                .ok()?;
            for file in depfile::make_dependencies(&bytes, &request.cwd)? {
                match self.host.metadata(&file, Follow::Yes) {
                    Ok(stat) if stat.kind == FileKind::File => {
                        if !self.admit_dependency_source(&file, &request.cwd) {
                            return None;
                        }
                        files.insert(file);
                    }
                    Err(error) if error.kind == ErrorKind::NotFound => {
                        let relative = file.strip_prefix(&request.cwd).ok();
                        let (root, relative) = if let Some(relative) = relative {
                            (request.cwd.clone(), relative.to_owned())
                        } else {
                            (file.parent()?.to_owned(), PathBuf::from(file.file_name()?))
                        };
                        if !relative
                            .components()
                            .all(|component| matches!(component, Component::Normal(_)))
                        {
                            return None;
                        }
                        searches.push(SearchObservation {
                            root,
                            relative,
                            found: false,
                        });
                    }
                    _ => return None,
                }
            }
        }
        Some(DependencyClosure { files, searches })
    }

    fn admit_dependency_source(&self, file: &Path, scratch: &Path) -> bool {
        if private_path(scratch, file)
            || file
                .file_name()
                .is_some_and(|name| name == "SDKSettings.json")
        {
            return true;
        }
        // Installed toolchain headers use the same trust boundary as the compiler.
        if self.in_sealed_root(file) {
            return true;
        }
        self.host
            .read(file, ReadRange::All { limit: 1 << 20 })
            .is_ok_and(|bytes| source_hazards(&bytes))
    }

    fn in_sealed_root(&self, file: &Path) -> bool {
        let sealed = |file: &Path| {
            self.toolchain
                .sealed
                .iter()
                .any(|root| file.starts_with(root))
        };
        sealed(file)
            || self
                .host
                .canonicalize(file)
                .is_ok_and(|canonical| sealed(&canonical))
    }
}

fn strip_instrumentation(request: &RunRequest) -> std::borrow::Cow<'_, RunRequest> {
    let Some(command) = request.pipeline.first() else {
        return std::borrow::Cow::Borrowed(request);
    };
    let strip = match command.args.as_slice() {
        [.., a, b, c, d]
            if a == "-Xlinker" && b == "-dependency_info" && c == "-Xlinker" && d == LINK_TRACE =>
        {
            4
        }
        [.., a, b, c] if a == "-MD" && b == "-MF" && c == COMPILE_TRACE => 3,
        [.., a, b] if a == "/sourceDependencies" && b == MSVC_TRACE => 2,
        [.., a] if a == GNU_LINK_TRACE => 1,
        _ => return std::borrow::Cow::Borrowed(request),
    };
    let mut request = request.clone();
    if let Some(command) = request.pipeline.first_mut() {
        command
            .args
            .truncate(command.args.len().saturating_sub(strip));
    }
    std::borrow::Cow::Owned(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_probe_options_are_admitted_with_their_words() {
        assert!(admit_definition(b"-v -Wl,-v"));
        assert!(admit_definition(b"-DNAME=1"));
        assert!(!admit_definition(b"-DNAME=\"a b\""));
        assert!(!admit_definition(b"-v -Wl,-force_load,/lib.a"));
        assert!(admit_cmake_flag(b"--no-warn-unused-cli"));
        assert!(admit_cmake_flag(b"-DCMAKE_C_STANDARD_LIBRARIES="));
        assert!(admit_cmake_flag(b"-DEXE_LINKER_FLAGS=-v -Wl,-v"));
        assert!(!admit_cmake_flag(b"-DCMAKE_TOOLCHAIN_FILE=/x.cmake"));
        assert!(!admit_cmake_flag(b"--trace"));
        let cwd = Path::new("/work/scratch");
        let link = ["-v", "-Wl,-v", "probe.o", "-o", "probe"].map(BString::from);
        let flags = flags::classify(&link, cwd, Family::AppleClang, None);
        assert!(flags.is_some_and(|flags| flags.verbose && flags.source.is_none()));
    }

    #[test]
    fn search_environment_remains_in_the_probe_key() {
        let first = [(BString::from("INCLUDE"), BString::from("C:/sdk/Include"))];
        let second = [(BString::from("INCLUDE"), BString::from("C:/vc/include"))];
        assert!(admit_environment(&first));
        assert!(admit_environment(&second));
        assert_ne!(
            process_environment_fingerprint(&first),
            process_environment_fingerprint(&second)
        );
    }
}
