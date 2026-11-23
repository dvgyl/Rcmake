use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use rcmake_base::{BString, list, truth};
use rcmake_build::{BuildOptions, BuildOutcome};
use rcmake_eval::{CacheDefine, CmdError, CmdResult, Cx, Flow, Interp, InterpConfig, RunMode};
use rcmake_expr::ExpandedArg;
use rcmake_genex::ConfigId;
use rcmake_host::{
    CommandSpec, CopyOptions, Host, HostError, Input, Output, ProcessResult, RunRequest, WriteMode,
    path,
};
use rcmake_model::{CacheEntry, CacheType, DirId, ProjectModel, Target, TargetKind, UsageField};
use rcmake_resolve::{
    BuildGraph, BuildStep, GraphTarget, Invocation, LinkKind, LinkStep, ResolveError,
};

thread_local! {
    static PROBE_DEPTH: Cell<u8> = const { Cell::new(0) };
}

const PROBE_TARGET: &str = "cmTryCompile";
const TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn try_compile(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    probe(cx, args, false)
}

pub(super) fn try_run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    probe(cx, args, true)
}

#[derive(Clone, Copy)]
enum Source<'a> {
    File(&'a [u8]),
    Content(&'a [u8], &'a [u8]),
    Variable(&'a [u8], &'a [u8]),
    Copy(&'a [u8], &'a [u8]),
}

enum Signature<'a> {
    Sources {
        base: Option<&'a [u8]>,
        sources: Vec<Source<'a>>,
    },
    Project {
        source: &'a [u8],
        binary: Option<&'a [u8]>,
        target: Option<&'a [u8]>,
    },
}

#[derive(Default)]
struct Options<'a> {
    values: BTreeMap<&'a [u8], Vec<&'a [u8]>>,
}

impl<'a> Options<'a> {
    fn has(&self, key: &[u8]) -> bool {
        self.values.contains_key(key)
    }

    fn many(&self, key: &[u8]) -> &[&'a [u8]] {
        self.values.get(key).map_or(&[], Vec::as_slice)
    }

    fn one(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.many(key).first().copied()
    }
}

fn status(text: impl Into<BString>) -> CmdError {
    CmdError::Status(text.into())
}

fn error_text(error: impl std::fmt::Display) -> CmdError {
    status(error.to_string())
}

fn keyword(arg: &[u8]) -> bool {
    option_arity(arg).is_some()
        || matches!(
            arg,
            b"SOURCES" | b"SOURCE_FROM_CONTENT" | b"SOURCE_FROM_VAR" | b"SOURCE_FROM_FILE"
        )
}

#[derive(Clone, Copy)]
enum Arity {
    Flag,
    One,
    List,
}

fn option_arity(arg: &[u8]) -> Option<Arity> {
    Some(match arg {
        b"NO_CACHE" | b"NO_LOG" => Arity::Flag,
        b"CMAKE_FLAGS" | b"COMPILE_DEFINITIONS" | b"LINK_OPTIONS" | b"LINK_LIBRARIES" | b"ARGS" => {
            Arity::List
        }
        b"OUTPUT_VARIABLE"
        | b"COPY_FILE"
        | b"COPY_FILE_ERROR"
        | b"LOG_DESCRIPTION"
        | b"LINKER_LANGUAGE"
        | b"__CMAKE_INTERNAL"
        | b"SOURCE_DIR"
        | b"BINARY_DIR"
        | b"TARGET"
        | b"SOURCES_TYPE"
        | b"COMPILE_OUTPUT_VARIABLE"
        | b"RUN_OUTPUT_VARIABLE"
        | b"RUN_OUTPUT_STDOUT_VARIABLE"
        | b"RUN_OUTPUT_STDERR_VARIABLE"
        | b"WORKING_DIRECTORY"
        | b"C_STANDARD"
        | b"C_STANDARD_REQUIRED"
        | b"C_EXTENSIONS"
        | b"CXX_STANDARD"
        | b"CXX_STANDARD_REQUIRED"
        | b"CXX_EXTENSIONS" => Arity::One,
        _ => return None,
    })
}

fn required<'a>(
    args: &mut impl Iterator<Item = &'a [u8]>,
    name: &[u8],
) -> Result<&'a [u8], CmdError> {
    args.next().ok_or_else(|| {
        status(format!(
            "{} requires a value.",
            String::from_utf8_lossy(name)
        ))
    })
}

fn parse<'a>(
    args: impl Iterator<Item = &'a [u8]>,
    run: bool,
) -> Result<(Signature<'a>, Options<'a>), CmdError> {
    let mut args = args.peekable();
    let first = required(&mut args, b"Source or project")?;
    let mut options = Options::default();
    let mut sources = Vec::new();
    let mut base = None;
    let mut project = None;
    if first == b"PROJECT" && !run {
        required(&mut args, b"PROJECT")?;
        project = Some((None, None, None));
    } else if !keyword(first) {
        base = Some(first);
        let source = required(&mut args, b"Source or project")?;
        if source == b"SOURCES" {
            sources
                .extend(std::iter::from_fn(|| args.next_if(|arg| !keyword(arg))).map(Source::File));
        } else if !run && args.peek().is_some_and(|arg| !keyword(arg)) {
            args.next();
            let target = args.next_if(|arg| !keyword(arg));
            project = Some((Some(source), base, target));
        } else {
            sources.push(Source::File(source));
        }
    } else {
        parse_item(first, &mut args, &mut options, &mut sources)?;
    }
    while let Some(arg) = args.next() {
        parse_item(arg, &mut args, &mut options, &mut sources)?;
    }
    if options.has(b"COPY_FILE_ERROR") && !options.has(b"COPY_FILE") {
        return Err(status("COPY_FILE_ERROR requires COPY_FILE."));
    }
    if run
        && (options.has(b"RUN_OUTPUT_VARIABLE") || options.has(b"OUTPUT_VARIABLE"))
        && (options.has(b"RUN_OUTPUT_STDOUT_VARIABLE")
            || options.has(b"RUN_OUTPUT_STDERR_VARIABLE"))
    {
        return Err(status(
            "Use either combined or separate run output variables.",
        ));
    }
    let signature = if let Some((source, binary, target)) = project {
        let source = source
            .or_else(|| options.one(b"SOURCE_DIR"))
            .ok_or_else(|| status("PROJECT requires SOURCE_DIR."))?;
        if !sources.is_empty() || options.has(b"COPY_FILE") {
            return Err(status(
                "Source files and COPY_FILE require the source file signature.",
            ));
        }
        Signature::Project {
            source,
            binary: binary.or_else(|| options.one(b"BINARY_DIR")),
            target: target.or_else(|| options.one(b"TARGET")),
        }
    } else {
        if sources.is_empty() {
            return Err(status("Specify at least one source file."));
        }
        Signature::Sources { base, sources }
    };
    Ok((signature, options))
}

fn parse_item<'a, I: Iterator<Item = &'a [u8]>>(
    arg: &'a [u8],
    args: &mut std::iter::Peekable<I>,
    opts: &mut Options<'a>,
    sources: &mut Vec<Source<'a>>,
) -> Result<(), CmdError> {
    match arg {
        b"SOURCES" => {
            sources.extend(std::iter::from_fn(|| args.next_if(|v| !keyword(v))).map(Source::File))
        }
        b"SOURCE_FROM_CONTENT" | b"SOURCE_FROM_VAR" | b"SOURCE_FROM_FILE" => {
            let name = required(args, arg)?;
            let value = required(args, arg)?;
            let native = path::to_native(name);
            let mut parts = native.components();
            if !matches!(parts.next(), Some(Component::Normal(_)))
                || parts.next().is_some()
                || name.contains(&b'/')
                || name.contains(&b'\\')
                || name.eq_ignore_ascii_case(b"CMakeLists.txt")
            {
                return Err(status(
                    "SOURCE_FROM_* requires a file name without directory components.",
                ));
            }
            sources.push(match arg {
                b"SOURCE_FROM_CONTENT" => Source::Content(name, value),
                b"SOURCE_FROM_VAR" => Source::Variable(name, value),
                _ => Source::Copy(name, value),
            });
        }
        _ => {
            let values = match option_arity(arg) {
                Some(Arity::Flag) => Vec::new(),
                Some(Arity::One) => vec![required(args, arg)?],
                Some(Arity::List) => std::iter::from_fn(|| args.next_if(|v| !keyword(v))).collect(),
                None => {
                    return Err(status(format!(
                        "Unknown argument: {}",
                        String::from_utf8_lossy(arg)
                    )));
                }
            };
            opts.values.entry(arg).or_default().extend(values);
        }
    }
    Ok(())
}

struct ProbeDirs {
    source: PathBuf,
    binary: PathBuf,
    scratch: Option<PathBuf>,
}

impl ProbeDirs {
    fn create(cx: &Cx<'_>, signature: &Signature<'_>) -> Result<Self, CmdError> {
        match signature {
            Signature::Project {
                source,
                binary: Some(binary),
                ..
            } => {
                let binary = explicit_binary_dir(binary)?;
                let source = absolute(&binary, source);
                if binary == path::to_native(&cx.var_or_empty(b"CMAKE_BINARY_DIR")) {
                    return Err(status(
                        "The test project cannot use the parent build directory.",
                    ));
                }
                Ok(Self {
                    source,
                    binary,
                    scratch: None,
                })
            }
            Signature::Sources {
                base: Some(base), ..
            } => {
                let dir = explicit_binary_dir(base)?.join("CMakeFiles/CMakeTmp");
                make_scratch(cx.host(), &dir)?;
                Ok(Self {
                    source: dir.clone(),
                    binary: dir.clone(),
                    scratch: Some(dir),
                })
            }
            _ => {
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let root = path::to_native(&cx.var_or_empty(b"CMAKE_BINARY_DIR"));
                let parent = root.join("CMakeFiles/CMakeScratch");
                let pid = cx.host().system().pid;
                for _ in 0..64 {
                    let index = NEXT.fetch_add(1, Ordering::Relaxed);
                    let dir = parent.join(format!("TryCompile-{pid:x}-{index:x}"));
                    match cx.host().metadata(&dir, rcmake_host::Follow::No) {
                        Ok(_) => continue,
                        Err(error) if error.kind == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error_text(error)),
                    }
                    make_scratch(cx.host(), &dir)?;
                    let source = match signature {
                        Signature::Project { source, .. } => absolute(&dir, source),
                        Signature::Sources { .. } => dir.clone(),
                    };
                    return Ok(Self {
                        source,
                        binary: dir.clone(),
                        scratch: Some(dir),
                    });
                }
                Err(status(
                    "Cannot allocate a scratch directory for the test build.",
                ))
            }
        }
    }
}

fn make_scratch(host: &dyn Host, dir: &Path) -> Result<(), CmdError> {
    host.create_dir_all(dir).map_err(error_text)?;
    if host
        .metadata(dir, rcmake_host::Follow::No)
        .map_err(error_text)?
        .kind
        != rcmake_host::FileKind::Dir
    {
        return Err(status("The scratch path must be a directory."));
    }
    Ok(())
}

fn explicit_binary_dir(value: &[u8]) -> Result<PathBuf, CmdError> {
    let dir = path::to_native(value);
    if !dir.is_absolute() {
        return Err(status(format!(
            "The test build directory must be an absolute path: {}",
            dir.display()
        )));
    }
    Ok(absolute(Path::new("/"), value))
}

fn absolute(base: &Path, value: &[u8]) -> PathBuf {
    let joined = path::from_native(&base.join(path::to_native(value)));
    path::to_native(&rcmake_base::path_lex::Lexical::new(&joined).normalize())
}

fn probe(cx: &mut Cx<'_>, args: &[ExpandedArg], run: bool) -> CmdResult {
    let mut args = args.iter().map(|arg| arg.value.as_slice());
    let run_var = if run {
        Some(required(&mut args, b"Run result")?)
    } else {
        None
    };
    let build_var = required(&mut args, b"Compile result")?;
    let (signature, options) = parse(args, run)?;
    let depth = PROBE_DEPTH.get();
    if depth >= 2 {
        return Err(status("The try_compile nesting limit is two."));
    }
    let dirs = ProbeDirs::create(cx, &signature)?;
    PROBE_DEPTH.set(depth + 1);
    let result = build_probe(cx, &signature, &options, &dirs, build_var, run_var);
    PROBE_DEPTH.set(depth);
    if let Some(scratch) = &dirs.scratch
        && !cx.var_is_on(b"CMAKE_DEBUG_TRY_COMPILE")
    {
        let cleanup = cx.host().remove_dir_all(scratch).map_err(error_text);
        result?;
        cleanup?;
    } else {
        result?;
    }
    Ok(Flow::Normal)
}

fn build_probe(
    cx: &mut Cx<'_>,
    signature: &Signature<'_>,
    options: &Options<'_>,
    dirs: &ProbeDirs,
    build_var: &[u8],
    run_var: Option<&[u8]>,
) -> Result<(), CmdError> {
    let target = match signature {
        Signature::Sources { sources, .. } => {
            write_project(cx, options, sources, &dirs.source, run_var.is_some())?;
            Some(PROBE_TARGET.as_bytes())
        }
        Signature::Project { target, .. } => *target,
    };
    let seed = TryCompileSeed::new(cx, options);
    let cmake_root = path::to_native(&cx.var_or_empty(b"CMAKE_ROOT"));
    let cmake_command = path::to_native(&cx.var_or_empty(b"CMAKE_COMMAND"));
    let limits = cx.limits().clone();
    let trace = cx.trace_config().cloned();
    let generator = cx.project().generator;
    let imports = match signature {
        Signature::Sources { .. } => probe_imports(cx, options),
        Signature::Project { .. } => Vec::new(),
    };
    let (build, mut output) = CapturingHost::capture(cx.host(), &dirs.binary, |host| {
        let mut child = Interp::new(
            InterpConfig {
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root,
                cmake_command,
                cwd: dirs.binary.clone(),
                limits,
                diag: Default::default(),
                defines: seed.defines,
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        if let Some(mut trace) = trace {
            let redirect = trace.redirect.replace(PathBuf::new());
            child.set_trace_config(Some(trace));
            let _ = host.trace_output.set(redirect);
        }
        let mut child_context = child.cx();
        let model = child_context.project();
        model.generator = generator;
        model.global.props.set("IN_TRY_COMPILE".into(), "1".into());
        if !seed.enabled_languages.is_empty() {
            model
                .global
                .props
                .set("ENABLED_LANGUAGES".into(), seed.enabled_languages);
        }
        for (imported, aliases) in imports {
            let id = model
                .add_target(imported)
                .map_err(|error| error.to_string())?;
            for alias in aliases {
                model
                    .add_alias(DirId::ROOT, alias, id)
                    .map_err(|error| error.to_string())?;
            }
        }
        let configure = child.run_configure(&dirs.source, &dirs.binary, &[]);
        if configure.exit_code != 0 {
            return Ok((
                BuildOutcome {
                    exit_code: configure.exit_code,
                    ..BuildOutcome::default()
                },
                None,
            ));
        }
        build_child(&mut child.cx(), &dirs.binary, target)
    })
    .map_err(error_text)?;
    let (outcome, artifact) = match build {
        Ok(values) => values,
        Err(error) => (
            BuildOutcome {
                exit_code: 1,
                output: error.into(),
                ..BuildOutcome::default()
            },
            None,
        ),
    };
    output.extend_from_slice(&outcome.output);
    let no_cache = options.has(b"NO_CACHE");
    set_result(
        cx,
        build_var,
        if outcome.success {
            "TRUE".into()
        } else {
            "FALSE".into()
        },
        no_cache,
    );
    if let Some(var) = options
        .one(b"COMPILE_OUTPUT_VARIABLE")
        .or_else(|| options.one(b"OUTPUT_VARIABLE"))
    {
        cx.set_var(var, output.clone());
    }
    let run_result = if outcome.success
        && let Some(var) = run_var
    {
        Some(run_executable(
            cx,
            signature,
            options,
            artifact.as_deref(),
            var,
            &output,
        ))
    } else {
        None
    };
    if !options.has(b"NO_LOG") {
        log_probe(
            cx,
            dirs,
            options,
            build_var,
            outcome.exit_code,
            &output,
            run_result.as_ref(),
        )?;
    }
    if let Some(destination) = options.one(b"COPY_FILE") {
        let destination = absolute(&cx.host().initial_cwd(), destination);
        let result = if outcome.success {
            copy_artifact(cx.host(), artifact.as_deref(), &destination)
        } else {
            Ok(())
        };
        let message = result.err().map_or_else(BString::default, |error| {
            format!(
                "Cannot copy the test artifact to {}: {error}",
                destination.display()
            )
            .into()
        });
        if let Some(var) = options.one(b"COPY_FILE_ERROR") {
            cx.set_var(var, message);
        } else if !message.is_empty() {
            return Err(status(message));
        }
    }
    Ok(())
}

fn copy_artifact(
    host: &dyn Host,
    artifact: Option<&Path>,
    destination: &Path,
) -> Result<(), HostError> {
    let file = artifact.ok_or_else(|| HostError::unsupported("The build produced no artifact."))?;
    if let Some(parent) = destination.parent() {
        host.create_dir_all(parent)?;
    }
    host.copy_file(
        file,
        destination,
        CopyOptions {
            preserve_permissions: true,
            preserve_mtime: false,
        },
    )
}

fn set_result(cx: &mut Cx<'_>, name: &[u8], value: BString, no_cache: bool) {
    if no_cache {
        cx.set_var(name, value);
    } else {
        cx.cache().insert(
            name.into(),
            CacheEntry {
                value,
                ty: CacheType::Internal,
                help: "Result of the test build or run".into(),
                advanced: false,
                props: Default::default(),
            },
        );
    }
}

fn write_project(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    sources: &[Source<'_>],
    directory: &Path,
    run: bool,
) -> Result<(), CmdError> {
    let target_type = cx.var_or_empty(b"CMAKE_TRY_COMPILE_TARGET_TYPE");
    let archive = match (run, target_type.as_slice()) {
        (true, _) | (_, b"" | b"EXECUTABLE") => false,
        (_, b"STATIC_LIBRARY") => true,
        _ => {
            return Err(status(
                "CMAKE_TRY_COMPILE_TARGET_TYPE must be EXECUTABLE or STATIC_LIBRARY.",
            ));
        }
    };
    let mut files = Vec::new();
    let mut languages = Vec::new();
    for &source in sources {
        let file = match source {
            Source::File(file) => absolute(directory, file),
            Source::Content(name, value) => write_source(cx.host(), directory, name, value)?,
            Source::Variable(name, var) => {
                write_source(cx.host(), directory, name, &cx.var_or_empty(var))?
            }
            Source::Copy(name, file) => {
                let source = absolute(&cx.host().initial_cwd(), file);
                let target = directory.join(path::to_native(name));
                let contents = cx
                    .host()
                    .read(
                        &source,
                        rcmake_host::ReadRange::All {
                            limit: cx.limits().max_file_read,
                        },
                    )
                    .map_err(error_text)?;
                cx.host()
                    .write(&target, &contents, WriteMode::CreateNew)
                    .map_err(error_text)?;
                target
            }
        };
        if let Some(language) = source_language(&file)
            && !languages.contains(&language)
        {
            languages.push(language);
        }
        files.push(path::from_native(&file));
    }
    let language = options.one(b"LINKER_LANGUAGE");
    if let Some(known @ (b"C" | b"CXX")) = language
        && !languages.contains(&known)
    {
        languages.push(known);
    }
    if languages.is_empty() {
        return Err(status("The probe requires a C or C++ source file."));
    }

    let mut code = BString::from(
        b"cmake_minimum_required(VERSION 4.5)\nproject(CMAKE_TRY_COMPILE LANGUAGES".as_slice(),
    );
    for lang in languages {
        code.push(b' ');
        code.extend_from_slice(lang);
    }
    code.extend_from_slice(
        b")\ninclude_directories(${INCLUDE_DIRECTORIES})\nlink_directories(${LINK_DIRECTORIES})\n",
    );
    code.extend_from_slice(
        b"separate_arguments(_probe_compile_flags UNIX_COMMAND \"${COMPILE_DEFINITIONS}\")\n\
          add_compile_options(${_probe_compile_flags})\n",
    );
    let prefix = if archive {
        "add_library(cmTryCompile STATIC"
    } else {
        "add_executable(cmTryCompile"
    };
    cmake_command(
        &mut code,
        prefix,
        files.iter().map(|value| value.as_slice()),
    );
    let compile_flags: Vec<_> = options
        .many(b"COMPILE_DEFINITIONS")
        .iter()
        .flat_map(|arg| compile_definition_words(cx, arg))
        .collect();
    for (command, definitions) in [
        ("target_compile_definitions", true),
        ("target_compile_options", false),
    ] {
        let values: Vec<_> = compile_flags
            .iter()
            .filter_map(|arg| {
                let arg = arg.as_slice();
                let definition = arg.strip_prefix(b"-D").or_else(|| arg.strip_prefix(b"/D"));
                if definitions {
                    definition
                } else {
                    definition.is_none().then_some(arg)
                }
            })
            .collect();
        if !values.is_empty() {
            cmake_command(
                &mut code,
                &format!("{command}(cmTryCompile PRIVATE"),
                values,
            );
        }
    }
    if !archive {
        code.extend_from_slice(
            b"separate_arguments(_probe_link_flags UNIX_COMMAND \"${EXE_LINKER_FLAGS}\")\n\
              target_link_options(cmTryCompile PRIVATE ${_probe_link_flags})\n",
        );
    }
    let link_options = options.many(b"LINK_OPTIONS");
    if !link_options.is_empty() {
        if archive {
            let flags = list::join(link_options);
            cmake_command(
                &mut code,
                "set_property(TARGET cmTryCompile PROPERTY STATIC_LIBRARY_OPTIONS",
                [flags.as_slice()],
            );
        } else {
            cmake_command(
                &mut code,
                "target_link_options(cmTryCompile PRIVATE",
                link_options.iter().copied(),
            );
        }
    }
    if options.has(b"LINK_LIBRARIES") {
        cmake_command(
            &mut code,
            "target_link_libraries(cmTryCompile PRIVATE",
            options.many(b"LINK_LIBRARIES").iter().copied(),
        );
    } else {
        code.extend_from_slice(b"target_link_libraries(cmTryCompile PRIVATE ${LINK_LIBRARIES})\n");
    }
    if let Some(language) = language {
        cmake_command(
            &mut code,
            "set_property(TARGET cmTryCompile PROPERTY LINKER_LANGUAGE",
            [language],
        );
    }
    cx.host()
        .write(
            &directory.join("CMakeLists.txt"),
            &code,
            WriteMode::CreateNew,
        )
        .map_err(error_text)
}

fn write_source(
    host: &dyn Host,
    directory: &Path,
    name: &[u8],
    value: &[u8],
) -> Result<PathBuf, CmdError> {
    let file = directory.join(path::to_native(name));
    host.write(&file, value, WriteMode::CreateNew)
        .map_err(error_text)?;
    Ok(file)
}

fn cmake_command<'a>(code: &mut BString, prefix: &str, args: impl IntoIterator<Item = &'a [u8]>) {
    code.extend_from_slice(prefix.as_bytes());
    for arg in args {
        code.push(b' ');
        write_bracket_arg(code, arg);
    }
    code.extend_from_slice(b")\n");
}

fn write_bracket_arg(code: &mut BString, arg: &[u8]) {
    let mut delimiter = vec![b']', b']'];
    while arg
        .windows(delimiter.len())
        .any(|window| window == delimiter)
        || arg.ends_with(delimiter.get(..delimiter.len() - 1).unwrap_or_default())
    {
        delimiter.insert(1, b'=');
    }
    code.push(b'[');
    code.extend_from_slice(delimiter.get(1..delimiter.len() - 1).unwrap_or_default());
    code.extend_from_slice(b"[\n");
    code.extend_from_slice(arg);
    code.extend_from_slice(&delimiter);
}

fn source_language(file: &Path) -> Option<&'static [u8]> {
    match file.extension()?.as_encoded_bytes() {
        b"c" => Some(b"C"),
        b"C" | b"cc" | b"cpp" | b"cxx" | b"c++" | b"CPP" | b"ixx" | b"cppm" => Some(b"CXX"),
        _ => None,
    }
}

struct TryCompileSeed {
    defines: Vec<CacheDefine>,
    enabled_languages: BString,
}

fn toolchain_variable(name: &[u8]) -> bool {
    [
        b"CMAKE_C_".as_slice(),
        b"CMAKE_CXX_",
        b"CMAKE_OSX_",
        b"CMAKE_SYSROOT",
        b"CMAKE_SYSTEM_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name.starts_with(b"CMAKE_")
            && name
                .windows(b"_LINKER_FLAGS".len())
                .any(|part| part == b"_LINKER_FLAGS")
        || matches!(
            name,
            b"CMAKE_SYSTEM" | b"CMAKE_AR" | b"CMAKE_RANLIB" | b"CMAKE_MAKE_PROGRAM"
        )
}

impl TryCompileSeed {
    fn new(cx: &mut Cx<'_>, options: &Options<'_>) -> Self {
        let config = cx.var_or_empty(b"CMAKE_TRY_COMPILE_CONFIGURATION");
        let mut languages = cx
            .global()
            .props
            .get(b"ENABLED_LANGUAGES")
            .map(|value| list::split(value, list::Empty::Drop))
            .unwrap_or_default();
        let mut defines = Vec::new();
        for name in cx
            .variable_names()
            .iter()
            .filter(|name| toolchain_variable(name))
        {
            seed_var(cx, &mut defines, name);
        }
        for name in [
            "CMAKE_PLATFORM_INFO_DIR",
            "CMAKE_TRY_COMPILE_PLATFORM_VARIABLES",
            "CMAKE_TRY_COMPILE_CONFIGURATION",
            "CMAKE_DEBUG_TRY_COMPILE",
            "CMAKE_GENERATOR",
        ] {
            seed_var(cx, &mut defines, name.as_bytes());
        }
        for language in ["C", "CXX"] {
            let compiler = format!("CMAKE_{language}_COMPILER");
            if !cx.var_or_empty(compiler.as_bytes()).is_empty()
                && !languages.iter().any(|loaded| loaded == language)
            {
                languages.push(language.into());
            }
            defines.push(CacheDefine {
                name: format!("CMAKE_{language}_COMPILER_WORKS").into(),
                ty: Some(CacheType::Internal),
                value: "1".into(),
            });
        }
        for name in list::split(
            &cx.var_or_empty(b"CMAKE_TRY_COMPILE_PLATFORM_VARIABLES"),
            list::Empty::Drop,
        ) {
            seed_var(cx, &mut defines, &name);
        }
        if !config.is_empty() {
            defines.push(CacheDefine {
                name: "CMAKE_BUILD_TYPE".into(),
                ty: Some(CacheType::String),
                value: config,
            });
        }
        let mut flags = options.many(b"CMAKE_FLAGS").iter().copied();
        while let Some(flag) = flags.next() {
            let Some(define) = (if flag == b"-D" {
                flags.next()
            } else {
                flag.strip_prefix(b"-D")
            }) else {
                continue;
            };
            if let Some((key, value)) = split_byte(define, b'=') {
                let (name, ty) = split_byte(key, b':')
                    .map_or((key, None), |(name, ty)| (name, CacheType::parse(ty)));
                defines.push(CacheDefine {
                    name: name.into(),
                    ty,
                    value: value.into(),
                });
            }
        }
        for name in [
            "C_STANDARD",
            "C_STANDARD_REQUIRED",
            "C_EXTENSIONS",
            "CXX_STANDARD",
            "CXX_STANDARD_REQUIRED",
            "CXX_EXTENSIONS",
        ] {
            if let Some(value) = options.one(name.as_bytes()) {
                defines.push(CacheDefine {
                    name: format!("CMAKE_{name}").into(),
                    ty: Some(CacheType::String),
                    value: value.into(),
                });
            }
        }
        Self {
            defines,
            enabled_languages: list::join(languages),
        }
    }
}

fn seed_var(cx: &Cx<'_>, defines: &mut Vec<CacheDefine>, name: &[u8]) {
    if cx.var_defined(name) {
        defines.push(CacheDefine {
            name: name.into(),
            ty: None,
            value: cx.var_or_empty(name),
        });
    }
}

fn split_byte(bytes: &[u8], byte: u8) -> Option<(&[u8], &[u8])> {
    let mut parts = bytes.splitn(2, |&value| value == byte);
    Some((parts.next()?, parts.next()?))
}

fn probe_imports(cx: &mut Cx<'_>, options: &Options<'_>) -> Vec<(Target, Vec<BString>)> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let Some(libraries) = options.values.get(b"LINK_LIBRARIES".as_slice()) else {
        return Vec::new();
    };
    let mut targets: Vec<(Target, Vec<BString>)> = Vec::new();
    let mut imported = BTreeMap::new();
    let mut pending: Vec<BString> = libraries.iter().map(|&name| name.into()).collect();
    while let Some(name) = pending.pop() {
        let Some(id) = model.find_target(dir, &name) else {
            continue;
        };
        let Some(target) = model.target(id).filter(|target| target.is_imported()) else {
            continue;
        };
        let idx = *imported.entry(id.index()).or_insert_with(|| {
            let mut target = target.clone();
            target.dir = DirId::ROOT;
            for item in target.usage.interface_link_libraries.iter_mut().flatten() {
                item.lookup_dir = DirId::ROOT;
                pending.push(item.text.value.clone());
            }
            targets.push((target, Vec::new()));
            targets.len() - 1
        });
        if name != target.name
            && let Some((_, aliases)) = targets.get_mut(idx)
            && !aliases.contains(&name)
        {
            aliases.push(name);
        }
    }
    targets
}

fn build_child(
    child: &mut Cx<'_>,
    directory: &Path,
    target: Option<&[u8]>,
) -> Result<(BuildOutcome, Option<PathBuf>), String> {
    let config = ConfigId(child.var_or_empty(b"CMAKE_BUILD_TYPE"));
    let mut model = child.project().clone();
    // Lowering reads toolchain variables from the directory model or cache.
    for name in child.variable_names() {
        if toolchain_variable(&name) || name == "CMAKE_COMMAND" {
            let value = child.var_or_empty(&name);
            model.dir_mut(DirId::ROOT).props.set(name, value);
        }
    }
    let graph = match rcmake_resolve::resolve(&model, child.host(), std::slice::from_ref(&config)) {
        Ok(generation) => generation
            .graphs
            .into_iter()
            .next()
            .ok_or_else(|| "The resolver returned no build graph.".to_owned())?,
        Err(ResolveError::Unimplemented(_)) => DirectBuild::new(child, config).graph(&model)?,
        Err(error) => return Err(error.to_string()),
    };
    let artifact = target
        .and_then(|name| {
            graph
                .targets
                .iter()
                .find(|target| target.name.as_slice() == name)
        })
        .and_then(|target| {
            target
                .steps
                .iter()
                .rev()
                .find_map(|&id| match graph.steps.get(id) {
                    Some(BuildStep::Link { step, .. }) => {
                        Some(directory.join(path::to_native(&step.output)))
                    }
                    _ => None,
                })
        });
    let options = BuildOptions {
        build_dir: directory.to_path_buf(),
        targets: target.into_iter().map(BString::from).collect(),
        ..BuildOptions::default()
    };
    let outcome =
        rcmake_build::execute(&graph, child.host(), &options).map_err(|error| error.to_string())?;
    Ok((outcome, artifact))
}

// Use basic compiler commands while the resolver cannot lower a project.
struct DirectBuild {
    config: ConfigId,
    c: Vec<BString>,
    cxx: Vec<BString>,
    ar: BString,
    executable_flags: Vec<BString>,
    static_flags: Vec<BString>,
    platform_flags: Vec<BString>,
}

impl DirectBuild {
    fn new(cx: &mut Cx<'_>, config: ConfigId) -> Self {
        let c = compiler_args(cx, "C", "cc", &config.0);
        let cxx = compiler_args(cx, "CXX", "c++", &config.0);
        let ar = cx.var_or_empty(b"CMAKE_AR");
        let ar = if ar.is_empty() { "ar".into() } else { ar };
        let executable_flags = command_words(cx, &cx.var_or_empty(b"CMAKE_EXE_LINKER_FLAGS"));
        let static_flags = command_words(cx, &cx.var_or_empty(b"CMAKE_STATIC_LINKER_FLAGS"));
        let mut platform_flags = Vec::new();
        let apple = cx.var_or_empty(b"CMAKE_SYSTEM_NAME") == "Darwin";
        if apple {
            for arch in list::split(
                &cx.var_or_empty(b"CMAKE_OSX_ARCHITECTURES"),
                list::Empty::Drop,
            ) {
                platform_flags.extend(["-arch".into(), arch]);
            }
            for (name, prefix) in [
                (b"CMAKE_OSX_SYSROOT".as_slice(), b"-isysroot".as_slice()),
                (b"CMAKE_OSX_DEPLOYMENT_TARGET", b"-mmacosx-version-min="),
            ] {
                let value = cx.var_or_empty(name);
                if !value.is_empty() {
                    if prefix == b"-isysroot" {
                        platform_flags.extend([prefix.into(), value]);
                    } else {
                        platform_flags.push([prefix, value.as_slice()].concat().into());
                    }
                }
            }
        } else {
            let sysroot = cx.var_or_empty(b"CMAKE_SYSROOT");
            if !sysroot.is_empty() {
                platform_flags.push(
                    [b"--sysroot=".as_slice(), sysroot.as_slice()]
                        .concat()
                        .into(),
                );
            }
        }
        Self {
            config,
            c,
            cxx,
            ar,
            executable_flags,
            static_flags,
            platform_flags,
        }
    }

    fn graph(&self, model: &ProjectModel) -> Result<BuildGraph, String> {
        let mut graph = BuildGraph {
            config: self.config.clone(),
            ..BuildGraph::default()
        };
        let targets: Vec<_> = model
            .targets()
            .filter(|(_, target)| !target.is_imported())
            .collect();
        let indices: BTreeMap<_, _> = targets
            .iter()
            .enumerate()
            .map(|(index, (id, _))| (id.index(), index))
            .collect();
        for (id, target) in targets {
            let dir = model.dir(target.dir);
            let mut node = GraphTarget {
                name: target.name.clone(),
                in_all: !dir.exclude_from_all
                    && !target
                        .props
                        .get(b"EXCLUDE_FROM_ALL")
                        .is_some_and(|v| truth::is_on(v)),
                ..GraphTarget::default()
            };
            for dependency in &target.utility_deps {
                if let Some(index) = model
                    .find_target(target.dir, &dependency.value)
                    .and_then(|id| indices.get(&id.index()))
                {
                    node.depends.push(*index);
                }
            }
            if target.kind == TargetKind::InterfaceLibrary {
                node.in_all = false;
                graph.targets.push(node);
                continue;
            }
            let first = graph.steps.len();
            let mut objects = Vec::new();
            let mut cxx = false;
            for source in &target.sources {
                for file in list::split(&source.value, list::Empty::Drop) {
                    let source = absolute(&path::to_native(&dir.source_dir), &file);
                    let Some(lang) = source_language(&source) else {
                        continue;
                    };
                    cxx |= lang == b"CXX";
                    let object: BString =
                        format!("CMakeFiles/direct-{}.dir/{}.o", id.index(), objects.len()).into();
                    let mut argv = self.compiler(lang);
                    for (field, prefix) in [
                        (UsageField::CompileDefinitions, b"-D".as_slice()),
                        (UsageField::IncludeDirectories, b"-I"),
                        (UsageField::CompileOptions, b""),
                        (UsageField::InterfaceCompileOptions, b""),
                    ] {
                        argv.extend(usage_args(target, field, prefix));
                    }
                    let standard = [lang, b"_STANDARD"].concat();
                    let extensions = [lang, b"_EXTENSIONS"].concat();
                    if let Some(standard) = target.props.get(&standard) {
                        let gnu = !target
                            .props
                            .get(&extensions)
                            .is_some_and(|value| truth::is_off(value));
                        let prefix: &[u8] = match (lang, gnu) {
                            (b"CXX", true) => b"-std=gnu++",
                            (b"CXX", false) => b"-std=c++",
                            (_, true) => b"-std=gnu",
                            (_, false) => b"-std=c",
                        };
                        argv.push([prefix, standard.as_slice()].concat().into());
                    }
                    let source = path::from_native(&source);
                    argv.extend(["-o".into(), object.clone(), "-c".into(), source.clone()]);
                    graph.steps.push(BuildStep::Compile {
                        object: object.clone(),
                        source,
                        depfile: None,
                        invocation: Invocation {
                            argv,
                            ..Invocation::default()
                        },
                    });
                    objects.push(object);
                }
            }
            let language: &[u8] = target
                .props
                .get(b"LINKER_LANGUAGE")
                .map_or(if cxx { b"CXX".as_slice() } else { b"C" }, |value| {
                    value.as_slice()
                });
            let kind = match target.kind {
                TargetKind::Executable => LinkKind::Executable,
                TargetKind::StaticLibrary => LinkKind::StaticArchive,
                _ => {
                    return Err(format!(
                        "Direct probe builds cannot lower target {} of type {}.",
                        target.name,
                        target.kind.as_str()
                    ));
                }
            };
            let output = direct_output(model, target);
            let mut argv = if kind == LinkKind::StaticArchive {
                let mut args = vec![self.ar.clone(), "rcs".into()];
                args.extend(self.static_flags.iter().cloned());
                if let Some(flags) = target.props.get(b"STATIC_LIBRARY_OPTIONS") {
                    list::split_into(flags, list::Empty::Drop, &mut args);
                }
                args.push(output.clone());
                args
            } else {
                let mut args = self.compiler(language);
                args.extend(self.executable_flags.iter().cloned());
                args.extend(usage_args(target, UsageField::LinkOptions, b""));
                args.extend(usage_args(target, UsageField::LinkDirectories, b"-L"));
                args.extend(["-o".into(), output.clone()]);
                args
            };
            argv.extend(objects.iter().cloned());
            for item in target.usage.link_libraries.iter().flatten() {
                let library = &item.text.value;
                let resolved = model.find_target(item.lookup_dir, library);
                if let Some(index) = resolved.and_then(|id| indices.get(&id.index())) {
                    node.depends.push(*index);
                }
                if kind == LinkKind::StaticArchive {
                    continue;
                }
                let linked = resolved.and_then(|id| model.target(id));
                if let Some(linked) = linked {
                    if !linked.is_imported() {
                        argv.push(direct_output(model, linked));
                    } else if let Some(location) = linked.props.get(b"IMPORTED_LOCATION") {
                        argv.push(location.clone());
                    }
                } else if library.starts_with(b"-") || path::to_native(library).is_absolute() {
                    argv.push(library.clone());
                } else {
                    argv.push([b"-l".as_slice(), library.as_slice()].concat().into());
                }
            }
            graph.steps.push(BuildStep::Link {
                inputs: objects,
                step: LinkStep {
                    kind,
                    output,
                    language: language.into(),
                    fragments: Vec::new(),
                },
                invocations: vec![Invocation {
                    argv,
                    ..Invocation::default()
                }],
            });
            node.steps.extend(first..graph.steps.len());
            graph.targets.push(node);
        }
        Ok(graph)
    }

    fn compiler(&self, language: &[u8]) -> Vec<BString> {
        let mut args = if language == b"CXX" {
            self.cxx.clone()
        } else {
            self.c.clone()
        };
        args.extend(self.platform_flags.iter().cloned());
        args
    }
}

fn direct_output(model: &ProjectModel, target: &Target) -> BString {
    let archive = target.kind == TargetKind::StaticLibrary;
    let name = target.props.get(b"OUTPUT_NAME").unwrap_or(&target.name);
    let file = if archive {
        format!("lib{name}.a").into()
    } else {
        name.clone()
    };
    let dir = model.dir(target.dir);
    let binary = path::to_native(&dir.binary_dir);
    let output_dir = if archive {
        b"ARCHIVE_OUTPUT_DIRECTORY".as_slice()
    } else {
        b"RUNTIME_OUTPUT_DIRECTORY"
    };
    let output_dir = target
        .props
        .get(output_dir)
        .map_or(binary.clone(), |value| absolute(&binary, value));
    path::from_native(&output_dir.join(path::to_native(&file)))
}

fn usage_args(target: &Target, field: UsageField, prefix: &[u8]) -> impl Iterator<Item = BString> {
    target
        .usage
        .entries(field)
        .iter()
        .flat_map(|item| list::split(&item.value, list::Empty::Drop))
        .map(move |value| [prefix, value.as_slice()].concat().into())
}

fn compiler_args(cx: &mut Cx<'_>, language: &str, default: &str, config: &[u8]) -> Vec<BString> {
    let compiler = cx.var_or_empty(format!("CMAKE_{language}_COMPILER").as_bytes());
    let mut args = list::split(&compiler, list::Empty::Drop);
    if args.is_empty() {
        args.push(default.into());
    }
    let config = String::from_utf8_lossy(config).to_ascii_uppercase();
    for suffix in [
        "COMPILER_ARG1".to_owned(),
        "FLAGS".to_owned(),
        format!("FLAGS_{config}"),
    ] {
        let value = cx.var_or_empty(format!("CMAKE_{language}_{suffix}").as_bytes());
        args.extend(command_words(cx, &value));
    }
    args
}

fn compile_definition_words(cx: &mut Cx<'_>, value: &[u8]) -> Vec<BString> {
    let mut words = command_words(cx, value);
    if let [word] = words.as_mut_slice()
        && (value.starts_with(b"-D") || value.starts_with(b"/D"))
        && let Some((_, definition)) = split_byte(value, b'=')
        && definition.starts_with(b"\"")
        && definition.ends_with(b"\"")
    {
        // Keep the quotes in a single macro definition for the preprocessor.
        *word = value.into();
    }
    words
}

fn command_words(cx: &mut Cx<'_>, value: &[u8]) -> Vec<BString> {
    if value.is_empty() {
        return Vec::new();
    }
    let var = b"__rcmake_try_compile_words";
    let previous = cx.var_normal_only(var);
    let args = [var.as_slice(), b"UNIX_COMMAND", value]
        .into_iter()
        .map(|value| ExpandedArg {
            value: value.into(),
            kind: rcmake_syntax::ArgKind::Quoted,
            line: 0,
        })
        .collect();
    let words = if cx.call_command(b"separate_arguments", args).is_ok() {
        list::split(&cx.var_or_empty(var), list::Empty::Keep)
    } else {
        Vec::new()
    };
    if let Some(previous) = previous {
        cx.set_var(var, previous);
    } else {
        cx.unset_var(var);
    }
    words
}

struct RunResult<'a> {
    variable: &'a [u8],
    value: BString,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_executable<'a>(
    cx: &mut Cx<'_>,
    signature: &Signature<'_>,
    options: &Options<'_>,
    artifact: Option<&Path>,
    variable: &'a [u8],
    build_output: &[u8],
) -> RunResult<'a> {
    let default_cwd = match signature {
        Signature::Sources {
            base: Some(base), ..
        } => path::to_native(base),
        _ => cx.host().initial_cwd(),
    };
    let cwd = options
        .one(b"WORKING_DIRECTORY")
        .map_or_else(|| default_cwd.clone(), |dir| absolute(&default_cwd, dir));
    let combined = options.has(b"RUN_OUTPUT_VARIABLE") || options.has(b"OUTPUT_VARIABLE");
    let mut command = Vec::new();
    let cross = cx.var_is_on(b"CMAKE_CROSSCOMPILING");
    if cross {
        command = list::split(
            &cx.var_or_empty(b"CMAKE_CROSSCOMPILING_EMULATOR"),
            list::Empty::Drop,
        );
    }
    let result = if let Some(artifact) = artifact
        && (!cross || !command.is_empty())
    {
        command.push(path::from_native(artifact));
        command.extend(options.many(b"ARGS").iter().copied().map(BString::from));
        command.split_first().and_then(|(program, args)| {
            cx.host()
                .run(
                    &RunRequest {
                        pipeline: vec![CommandSpec {
                            program: program.clone(),
                            args: args.to_vec(),
                        }],
                        cwd,
                        env: cx.host().env_all(),
                        stdin: Input::Null,
                        stdout: Output::Capture,
                        stderr: if combined {
                            Output::SharedWithStdout
                        } else {
                            Output::Capture
                        },
                        timeout: Some(TIMEOUT),
                        tee: false,
                    },
                    &mut |_, _| {},
                )
                .ok()
        })
    } else {
        None
    }
    .unwrap_or_default();

    let value: BString = match result.results.first() {
        Some(ProcessResult::Exited(code)) => code.to_string().into(),
        _ => "FAILED_TO_RUN".into(),
    };
    set_result(cx, variable, value.clone(), options.has(b"NO_CACHE"));
    for (key, output) in [
        (b"RUN_OUTPUT_VARIABLE".as_slice(), result.stdout.as_slice()),
        (b"RUN_OUTPUT_STDOUT_VARIABLE", result.stdout.as_slice()),
        (b"RUN_OUTPUT_STDERR_VARIABLE", result.stderr.as_slice()),
    ] {
        if let Some(var) = options.one(key) {
            cx.set_var(var, output.into());
        }
    }
    if let Some(var) = options.one(b"OUTPUT_VARIABLE") {
        cx.set_var(
            var,
            [
                build_output,
                result.stdout.as_slice(),
                result.stderr.as_slice(),
            ]
            .concat()
            .into(),
        );
    }
    RunResult {
        variable,
        value,
        stdout: result.stdout,
        stderr: result.stderr,
    }
}

fn log_probe(
    cx: &Cx<'_>,
    dirs: &ProbeDirs,
    options: &Options<'_>,
    build_var: &[u8],
    exit_code: i32,
    output: &[u8],
    run_result: Option<&RunResult<'_>>,
) -> Result<(), CmdError> {
    use std::fmt::Write as _;

    let directory = path::to_native(&cx.var_or_empty(b"CMAKE_BINARY_DIR")).join("CMakeFiles");
    let kind = if run_result.is_some()
        || cx
            .backtrace()
            .top()
            .and_then(|frame| frame.name.as_deref())
            .is_some_and(|name| name.eq_ignore_ascii_case(b"try_run"))
    {
        "try_run-v1"
    } else {
        "try_compile-v1"
    };
    let cached = !options.has(b"NO_CACHE");
    let mut text = format!("---\nevents:\n  - kind: \"{kind}\"\n    backtrace:");
    let trace = cx.backtrace();
    if trace.is_empty() {
        text.push_str(" []\n");
    } else {
        text.push('\n');
        for frame in trace.frames() {
            let call = format!(
                "{}:{} ({})",
                String::from_utf8_lossy(&frame.file),
                frame.line,
                String::from_utf8_lossy(frame.name.as_deref().unwrap_or_default()),
            );
            let _ = writeln!(text, "      - {}", yaml_string(call.as_bytes()));
        }
    }
    if let Some(description) = options.one(b"LOG_DESCRIPTION") {
        let _ = writeln!(text, "    description: {}", yaml_string(description));
    }
    let _ = writeln!(
        text,
        "    directories:\n      source: {}\n      binary: {}\n    buildResult:\n      variable: {}\n      cached: {cached}\n      exitCode: {exit_code}",
        yaml_string(&path::from_native(&dirs.source)),
        yaml_string(&path::from_native(&dirs.binary)),
        yaml_string(build_var),
    );
    log_output(&mut text, "stdout", output);
    if let Some(run) = run_result {
        let _ = writeln!(
            text,
            "    runResult:\n      variable: {}\n      cached: {cached}\n      exitCode: {}",
            yaml_string(run.variable),
            run.value,
        );
        log_output(&mut text, "stdout", &run.stdout);
        log_output(&mut text, "stderr", &run.stderr);
    }
    text.push_str("...\n");
    cx.host().create_dir_all(&directory).map_err(error_text)?;
    cx.host()
        .write(
            &directory.join("CMakeConfigureLog.yaml"),
            text.as_bytes(),
            WriteMode::Append,
        )
        .map_err(error_text)
}

fn yaml_string(value: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut quoted = String::from("\"");
    for &byte in value {
        match byte {
            b'\\' | b'"' => {
                quoted.push('\\');
                quoted.push(char::from(byte));
            }
            b' '..=b'~' => quoted.push(char::from(byte)),
            _ => {
                let _ = write!(quoted, "\\x{byte:02x}");
            }
        }
    }
    quoted.push('"');
    quoted
}

fn log_output(text: &mut String, key: &str, output: &[u8]) {
    use std::fmt::Write as _;

    let _ = writeln!(
        text,
        "      {key}: |2{}",
        if output.ends_with(b"\n") { "+" } else { "-" }
    );
    for line in output.split_inclusive(|&byte| byte == b'\n') {
        text.push_str("        ");
        for &byte in line.strip_suffix(b"\n").unwrap_or(line) {
            match byte {
                b'\\' => text.push_str("\\\\"),
                b' '..=b'~' => text.push(char::from(byte)),
                _ => {
                    let _ = write!(text, "\\x{byte:02x}");
                }
            }
        }
        text.push('\n');
    }
}

use rcmake_host::{
    DirEntry, FileTime, Follow, HostResult, Metadata, ReadRange, RunOutput, Stream, SystemInfo,
};

type HostCall = Box<dyn FnOnce(&dyn Host) + Send>;

/// The evaluator owns its host, but the command context only provides a borrow.
/// Keep the borrow on a scoped thread and forward child calls to that thread.
struct CapturingHost {
    calls: mpsc::Sender<HostCall>,
    output: Mutex<BString>,
    trace_output: OnceLock<Option<PathBuf>>,
    system: SystemInfo,
    cwd: PathBuf,
}

impl CapturingHost {
    fn capture<T>(
        parent: &dyn Host,
        cwd: &Path,
        configure: impl FnOnce(Arc<Self>) -> T,
    ) -> HostResult<(T, BString)> {
        std::thread::scope(|scope| {
            let (calls, requests) = mpsc::channel::<HostCall>();
            std::thread::Builder::new()
                .name("rcmake-probe-host".to_owned())
                .spawn_scoped(scope, move || {
                    for call in requests {
                        call(parent);
                    }
                })
                .map_err(|error| HostError::from_io(error, None))?;
            let host = Arc::new(Self {
                calls,
                output: Mutex::default(),
                trace_output: OnceLock::new(),
                system: parent.system(),
                cwd: cwd.to_path_buf(),
            });
            let outcome = configure(host.clone());
            let output = std::mem::take(
                &mut *host
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            Ok((outcome, output))
        })
    }

    fn forward<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn Host) -> T + Send + 'static,
    ) -> HostResult<T> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Box::new(move |host| {
            let _ = reply.send(call(host));
        }))?;
        result
            .recv()
            .map_err(|_| HostError::unsupported("The test project host stopped."))
    }

    fn send(&self, call: HostCall) -> HostResult<()> {
        self.calls
            .send(call)
            .map_err(|_| HostError::unsupported("The test project host stopped."))
    }
}

// Clone borrowed arguments into the request so the child can own its host.
macro_rules! forward_host {
    ($name:ident($($arg:ident: $ty:ty),*) -> $result:ty; $($owned:ident),* $(,)?) => {
        fn $name(&self, $($arg: $ty),*) -> HostResult<$result> {
            $(let $owned = $owned.to_owned();)*
            self.forward(move |host| host.$name($($arg.as_ref()),*))?
        }
    };
}

impl Host for CapturingHost {
    forward_host!(read_dir(p: &Path) -> Vec<DirEntry>; p);
    forward_host!(create_dir_all(p: &Path) -> (); p);
    forward_host!(remove_file(p: &Path) -> (); p);
    forward_host!(remove_dir(p: &Path) -> (); p);
    forward_host!(remove_dir_all(p: &Path) -> (); p);
    forward_host!(rename(from: &Path, to: &Path) -> (); from, to);
    forward_host!(symlink(target: &Path, link: &Path) -> (); target, link);
    forward_host!(hard_link(src: &Path, dst: &Path) -> (); src, dst);
    forward_host!(read_link(p: &Path) -> PathBuf; p);
    forward_host!(canonicalize(p: &Path) -> PathBuf; p);

    fn metadata(&self, p: &Path, follow: Follow) -> HostResult<Metadata> {
        let p = p.to_owned();
        self.forward(move |host| host.metadata(&p, follow))?
    }

    fn read(&self, p: &Path, range: ReadRange) -> HostResult<Vec<u8>> {
        let p = p.to_owned();
        self.forward(move |host| host.read(&p, range))?
    }

    fn write(&self, p: &Path, data: &[u8], mode: WriteMode) -> HostResult<()> {
        let data = data.to_vec();
        // The empty path routes trace lines. The parent already initialized its trace output.
        if p.as_os_str().is_empty() {
            return match self.trace_output.get() {
                Some(Some(path)) => {
                    let path = path.clone();
                    self.forward(move |host| host.write(&path, &data, mode))?
                }
                Some(None) => self.forward(move |host| host.write_out(Stream::Stderr, &data)),
                None => Ok(()),
            };
        }
        let p = p.to_owned();
        self.forward(move |host| host.write(&p, &data, mode))?
    }

    fn copy_file(&self, from: &Path, to: &Path, opts: CopyOptions) -> HostResult<()> {
        let (from, to) = (from.to_owned(), to.to_owned());
        self.forward(move |host| host.copy_file(&from, &to, opts))?
    }

    fn set_permissions(&self, p: &Path, mode: u32) -> HostResult<()> {
        let p = p.to_owned();
        self.forward(move |host| host.set_permissions(&p, mode))?
    }

    fn set_times(
        &self,
        p: &Path,
        atime: Option<FileTime>,
        mtime: Option<FileTime>,
    ) -> HostResult<()> {
        let p = p.to_owned();
        self.forward(move |host| host.set_times(&p, atime, mtime))?
    }

    fn env_get(&self, key: &[u8]) -> Option<BString> {
        let key = key.to_vec();
        self.forward(move |host| host.env_get(&key)).ok().flatten()
    }

    fn env_set(&self, key: &[u8], value: Option<&[u8]>) {
        let key = key.to_vec();
        let value = value.map(<[u8]>::to_vec);
        let _ = self.forward(move |host| host.env_set(&key, value.as_deref()));
    }

    fn env_all(&self) -> Vec<(BString, BString)> {
        self.forward(|host| host.env_all()).unwrap_or_default()
    }

    fn run(
        &self,
        req: &RunRequest,
        on_output: &mut dyn FnMut(Stream, &[u8]),
    ) -> HostResult<RunOutput> {
        enum Event {
            Chunk(Stream, Vec<u8>),
            Done(HostResult<RunOutput>),
        }
        let mut req = req.clone();
        req.timeout = Some(req.timeout.map_or(TIMEOUT, |timeout| timeout.min(TIMEOUT)));
        let stdout = req.stdout == Output::Inherit;
        let stderr = req.stderr == Output::Inherit;
        if stdout {
            req.stdout = Output::Capture;
        }
        if stderr {
            req.stderr = Output::Capture;
        }
        if stdout || stderr {
            req.tee = true;
        }
        let (output, events) = mpsc::sync_channel(1);
        self.send(Box::new(move |host| {
            let result = host.run(&req, &mut |stream, bytes| {
                let _ = output.send(Event::Chunk(stream, bytes.to_vec()));
            });
            let _ = output.send(Event::Done(result));
        }))?;
        for event in events {
            match event {
                Event::Chunk(stream, bytes) => {
                    let inherit = if stream == Stream::Stdout {
                        stdout
                    } else {
                        stderr
                    };
                    if inherit {
                        self.write_out(stream, &bytes);
                    } else {
                        on_output(stream, &bytes);
                    }
                }
                Event::Done(result) => {
                    return result.map(|mut output| {
                        if stdout {
                            output.stdout.clear();
                        }
                        if stderr {
                            output.stderr.clear();
                        }
                        output
                    });
                }
            }
        }
        Err(HostError::unsupported("The test project host stopped."))
    }

    fn write_out(&self, _stream: Stream, bytes: &[u8]) {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
    }

    fn read_stdin(&self, limit: u64) -> HostResult<Vec<u8>> {
        self.forward(move |host| host.read_stdin(limit))?
    }

    fn is_terminal(&self, stream: Stream) -> bool {
        self.forward(move |host| host.is_terminal(stream))
            .unwrap_or(false)
    }

    fn now(&self) -> FileTime {
        self.forward(|host| host.now()).unwrap_or_default()
    }

    fn monotonic(&self) -> Duration {
        self.forward(|host| host.monotonic()).unwrap_or_default()
    }

    fn sleep(&self, duration: Duration) {
        let _ = self.forward(move |host| host.sleep(duration));
    }

    fn time_zone(&self) -> jiff::tz::TimeZone {
        self.forward(|host| host.time_zone())
            .unwrap_or(jiff::tz::TimeZone::UTC)
    }

    fn current_exe(&self) -> HostResult<PathBuf> {
        self.forward(|host| host.current_exe())?
    }

    fn initial_cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    fn system(&self) -> SystemInfo {
        self.system.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn interpreter(host: Arc<dyn Host>, cwd: &Path) -> Interp {
        let mut interp = Interp::new(
            InterpConfig {
                host,
                mode: RunMode::Configure,
                cmake_root: cwd.join("cmake"),
                cmake_command: cwd.join("cmake/bin/cmake"),
                cwd: cwd.to_path_buf(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        let mut cx = interp.cx();
        cx.set_var(b"CMAKE_SYSTEM", cx.host().system().sysname);
        for name in [
            "CMAKE_SYSTEM_LOADED",
            "CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED",
            "CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED",
            "CMAKE_C_INFORMATION_LOADED",
            "CMAKE_CXX_INFORMATION_LOADED",
        ] {
            cx.set_var(name.as_bytes(), "1".into());
        }
        interp
    }

    #[test]
    fn builds_all_four_signatures_and_captures_child_output() -> TestResult {
        let host = Arc::new(PureHost::default());
        host.seed_file(Path::new("/work/probe-cc"), b"")?;
        host.seed_file(Path::new("/work/main.c"), b"int main(void) { return 0; }")?;
        host.seed_file(
            Path::new("/work/sub/main.c"),
            b"int main(void) { return 0; }",
        )?;
        host.seed_file(
            Path::new("/work/sub/missing.c"),
            b"int main(void) { return 0; }",
        )?;
        host.seed_file(
            Path::new("/work/sub/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Child C)
get_property(nested GLOBAL PROPERTY IN_TRY_COMPILE)
message(STATUS "child nested=${nested} flag=${PASSED}")
add_executable(app main.c)
add_executable(excluded missing.c)
"#,
        )?;
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
set(CMAKE_C_COMPILER /work/probe-cc)
set(CMAKE_TRY_COMPILE_PLATFORM_VARIABLES PASSED)
set(PASSED parent)
try_compile(NEW SOURCES /work/main.c NO_CACHE NO_LOG)
try_compile(LEGACY /work/legacy /work/main.c NO_LOG)
try_compile(PROJECT_NEW PROJECT Child SOURCE_DIR /work/sub BINARY_DIR /work/build/project-new TARGET app
    CMAKE_FLAGS -DPASSED:STRING=override OUTPUT_VARIABLE CHILD_OUT LOG_DESCRIPTION "Child build")
try_compile(PROJECT_OLD /work/project-old /work/sub Child app OUTPUT_VARIABLE CHILD_OLD NO_LOG)
"#,
        )?;
        host.set_command_handler(|_, request, _| {
            let command = &request.pipeline[0];
            assert_eq!(command.program, "/work/probe-cc");
            assert!(!command.args.iter().any(|arg| arg.ends_with(b"missing.c")));
            Ok(RunOutput {
                stdout: b"compiler output\n".to_vec(),
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let mut interp = interpreter(host.clone(), Path::new("/work"));
        let result = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
        assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let mut cx = interp.cx();
        assert_eq!(cx.var_normal_only(b"NEW"), Some("TRUE".into()));
        assert!(cx.cache().get(b"NEW").is_none());
        for name in [b"LEGACY".as_slice(), b"PROJECT_NEW", b"PROJECT_OLD"] {
            let entry = cx.cache().get(name).ok_or("Missing cache result.")?;
            assert_eq!(entry.value, "TRUE");
            assert_eq!(entry.ty, CacheType::Internal);
        }
        let captured = cx.var_or_empty(b"CHILD_OUT");
        assert!(
            captured.starts_with(b"-- child nested=1 flag=override\n"),
            "{captured}"
        );
        assert!(
            captured.ends_with(b"compiler output\ncompiler output\n"),
            "{captured}"
        );
        let inherited = cx.var_or_empty(b"CHILD_OLD");
        assert!(
            inherited.starts_with(b"-- child nested=1 flag=parent\n"),
            "{inherited}"
        );
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
        assert!(
            host.read_dir(Path::new("/work/build/CMakeFiles/CMakeScratch"))?
                .is_empty()
        );
        assert!(
            host.metadata(Path::new("/work/legacy/CMakeFiles/CMakeTmp"), Follow::No)
                .is_err()
        );
        assert_eq!(
            host.metadata(Path::new("/work/build/project-new"), Follow::No)?
                .kind,
            rcmake_host::FileKind::Dir
        );
        let log = host.read(
            Path::new("/work/build/CMakeFiles/CMakeConfigureLog.yaml"),
            ReadRange::All { limit: 16_384 },
        )?;
        let log = std::str::from_utf8(&log)?;
        assert_eq!(log.matches("try_compile-v1").count(), 1);
        assert!(log.contains("description: \"Child build\""));
        Ok(())
    }

    #[test]
    fn stops_nested_probes_at_two_and_captures_the_error() -> TestResult {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
try_compile(TOP PROJECT Child SOURCE_DIR "${CMAKE_SOURCE_DIR}/child" OUTPUT_VARIABLE TOP_LOG NO_LOG)
"#,
        )?;
        host.seed_file(
            Path::new("/work/child/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Child NONE)
try_compile(CHILD PROJECT Child SOURCE_DIR ${CMAKE_SOURCE_DIR} OUTPUT_VARIABLE CHILD_LOG NO_LOG)
if(NOT CHILD)
    message(FATAL_ERROR "${CHILD_LOG}")
endif()
"#,
        )?;
        let mut interp = interpreter(host.clone(), Path::new("/work"));
        let result = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
        assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(interp.cx().var_or_empty(b"TOP"), "FALSE");
        let output = interp.cx().var_or_empty(b"TOP_LOG");
        assert!(
            output.to_string().contains("nesting limit is two"),
            "{output}"
        );
        assert!(host.stream_bytes(Stream::Stderr).is_empty());
        Ok(())
    }

    #[test]
    fn sends_child_trace_to_the_parent_destination() -> TestResult {
        use rcmake_eval::{TraceConfig, TraceFormat};

        for format in [TraceFormat::Human, TraceFormat::JsonV1] {
            let host = Arc::new(PureHost::default());
            host.seed_file(
                Path::new("/work/CMakeLists.txt"),
                br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
try_compile(CHILD PROJECT Child SOURCE_DIR /work/child OUTPUT_VARIABLE child_output NO_LOG)
message(STATUS "after probe")
"#,
            )?;
            host.seed_file(
                Path::new("/work/child/CMakeLists.txt"),
                br#"
cmake_minimum_required(VERSION 4.5)
project(Child NONE)
message(STATUS "inside probe")
"#,
            )?;
            let redirect = (format == TraceFormat::JsonV1).then(|| PathBuf::from("/work/trace"));
            let mut interp = interpreter(host.clone(), Path::new("/work"));
            interp.set_trace_config(Some(TraceConfig {
                format,
                redirect: redirect.clone(),
                ..TraceConfig::default()
            }));
            let result = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
            assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
            assert_eq!(
                interp.cx().var_or_empty(b"child_output"),
                "-- inside probe\n"
            );

            let trace = if let Some(path) = redirect {
                assert!(host.stream_bytes(Stream::Stderr).is_empty());
                host.read(&path, ReadRange::All { limit: 16_384 })?
            } else {
                host.stream_bytes(Stream::Stderr).to_vec()
            };
            let trace = std::str::from_utf8(&trace)?;
            let before = trace.find("try_compile").ok_or("Missing parent trace.")?;
            let inside = trace
                .find("/work/child/CMakeLists.txt")
                .ok_or("Missing child trace.")?;
            let after = trace.find("after probe").ok_or("Missing final trace.")?;
            assert!(before < inside && inside < after, "{trace}");
            if format == TraceFormat::JsonV1 {
                assert_eq!(trace.matches(r#""version":"#).count(), 1, "{trace}");
            }
        }
        Ok(())
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn builds_real_macho_archive_and_runs_a_probe() -> TestResult {
        use rcmake_host::NativeHost;

        let host = Arc::new(NativeHost::new()?);
        let temp = host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "mktemp".into(),
                    args: vec!["-d".into(), "/tmp/rcmake-probe.XXXXXXXX".into()],
                }],
                cwd: "/tmp".into(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(15)),
                tee: false,
            },
            &mut |_, _| {},
        )?;
        assert_eq!(temp.results, [ProcessResult::Exited(0)]);
        let root = PathBuf::from(std::str::from_utf8(&temp.stdout)?.trim());
        struct Temp<'a>(&'a dyn Host, &'a Path);
        impl Drop for Temp<'_> {
            fn drop(&mut self) {
                let _ = self.0.remove_dir_all(self.1);
            }
        }
        let _temp = Temp(host.as_ref(), &root);
        host.write(
            &root.join("copied.c"),
            b"int f(void) { return 5; }",
            WriteMode::CreateNew,
        )?;
        host.write(
            &root.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Probe NONE)
set(CMAKE_C_COMPILER /usr/bin/cc)
set(CMAKE_CXX_COMPILER /usr/bin/c++)
set(CMAKE_OSX_ARCHITECTURES arm64)
set(probe_source [=[
#if FOO != 1 || BAR != 2
#error Missing compiler definitions
#endif
_Static_assert(sizeof(TEXT) == 4, "Expected a string with a space");
int main(void) { return 0; }
]=])
try_compile(RES SOURCE_FROM_VAR main.c probe_source
  COMPILE_DEFINITIONS "-v -Wl,-v" "-DFOO=1 -DBAR=2" "-DTEXT=\"a b\""
  CMAKE_FLAGS "-DEXE_LINKER_FLAGS=-v -Wl,-v"
  COPY_FILE "${CMAKE_BINARY_DIR}/out.bin" OUTPUT_VARIABLE out)
try_compile(CACHE_FLAGS SOURCE_FROM_VAR main.c probe_source
  CMAKE_FLAGS [=[-DCOMPILE_DEFINITIONS:STRING=-DFOO=1 -DBAR=2 -DTEXT='"a b"']=]
  OUTPUT_VARIABLE cache_out NO_LOG)
file(READ_MACHO "${CMAKE_BINARY_DIR}/out.bin" ARCHITECTURES arch)
try_compile(BAD SOURCE_FROM_CONTENT bad.c "int main(void){ return missing_symbol; }"
  OUTPUT_VARIABLE error NO_CACHE)
file(WRITE "${CMAKE_BINARY_DIR}/blocked" "file")
try_compile(COPY_RES SOURCE_FROM_CONTENT main.c "int main(void){return 0;}"
  COPY_FILE "${CMAKE_BINARY_DIR}/blocked/out.bin" COPY_FILE_ERROR copy_error NO_LOG)
try_run(RUN_RES COMPILE_RES SOURCE_FROM_CONTENT run.c [=[
#include <stdio.h>
int main(int argc, char **argv) {
  if (argc != 2) return 1;
  printf("arg=%s\n", argv[1]);
  fprintf(stderr, "error stream\n");
  return 7;
}
]=] NO_CACHE ARGS "hello world"
  RUN_OUTPUT_STDOUT_VARIABLE stdout RUN_OUTPUT_STDERR_VARIABLE stderr COMPILE_OUTPUT_VARIABLE build)
try_run(COMBINED_RUN COMBINED_BUILD SOURCE_FROM_CONTENT run.c [=[
#include <stdio.h>
int main(void) {
  fputs("first\n", stdout); fflush(stdout);
  fputs("second\n", stderr);
  return 0;
}
]=] NO_CACHE RUN_OUTPUT_VARIABLE combined NO_LOG)
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
set(code [=[
#if CHECK_VALUE != 3
#error Bad compiler definition
#endif
int g(void) {return CHECK_VALUE;}
]=])
try_compile(ARCHIVE SOURCE_FROM_FILE copied.c "${CMAKE_SOURCE_DIR}/copied.c" SOURCE_FROM_VAR more.c code
  COMPILE_DEFINITIONS -DCHECK_VALUE=3 C_STANDARD 11 C_EXTENSIONS OFF
  COPY_FILE "${CMAKE_BINARY_DIR}/library.a" OUTPUT_VARIABLE archive_out)
set(CMAKE_TRY_COMPILE_TARGET_TYPE EXECUTABLE)
try_compile(CPP SOURCE_FROM_CONTENT main.cpp [=[
template<class T> concept Valid = sizeof(T) > 0;
int main() { static_assert(Valid<int>); }
]=] CXX_STANDARD 20 CXX_EXTENSIONS OFF OUTPUT_VARIABLE cpp_out)
"#,
            WriteMode::CreateNew,
        )?;
        let mut interp = interpreter(host.clone(), &root);
        let binary = root.join("build");
        let result = interp.run_configure(&root, &binary, &[]);
        let cx = interp.cx();
        assert_eq!(result.exit_code, 0, "{}", cx.var_or_empty(b"out"));
        assert_eq!(
            cx.var_or_empty(b"RES"),
            "TRUE",
            "{}",
            cx.var_or_empty(b"out")
        );
        assert!(
            cx.var_or_empty(b"out")
                .to_string()
                .contains("clang version")
        );
        assert!(
            cx.var_or_empty(b"out").to_string().contains("PROGRAM:ld"),
            "{}",
            cx.var_or_empty(b"out")
        );
        assert_eq!(
            cx.var_or_empty(b"CACHE_FLAGS"),
            "TRUE",
            "{}",
            cx.var_or_empty(b"cache_out")
        );
        assert_eq!(cx.var_or_empty(b"arch"), "arm64");
        assert_eq!(cx.var_or_empty(b"BAD"), "FALSE");
        let error = cx.var_or_empty(b"error").to_string();
        assert!(
            error.contains("error:") && error.contains("missing_symbol"),
            "{error}"
        );
        assert_eq!(cx.var_or_empty(b"COPY_RES"), "TRUE");
        assert!(!cx.var_or_empty(b"copy_error").is_empty());
        assert_eq!(
            cx.var_or_empty(b"COMPILE_RES"),
            "TRUE",
            "{}",
            cx.var_or_empty(b"build")
        );
        assert_eq!(cx.var_or_empty(b"RUN_RES"), "7");
        assert_eq!(cx.var_or_empty(b"stdout"), "arg=hello world\n");
        assert_eq!(cx.var_or_empty(b"stderr"), "error stream\n");
        assert_eq!(cx.var_or_empty(b"COMBINED_RUN"), "0");
        assert_eq!(cx.var_or_empty(b"combined"), "first\nsecond\n");
        assert_eq!(
            cx.var_or_empty(b"ARCHIVE"),
            "TRUE",
            "{}",
            cx.var_or_empty(b"archive_out")
        );
        assert_eq!(
            host.read(
                &binary.join("library.a"),
                ReadRange::Slice {
                    offset: 0,
                    len: Some(8)
                }
            )?,
            b"!<arch>\n"
        );
        assert_eq!(
            cx.var_or_empty(b"CPP"),
            "TRUE",
            "{}",
            cx.var_or_empty(b"cpp_out")
        );
        let log = host.read(
            &binary.join("CMakeFiles/CMakeConfigureLog.yaml"),
            ReadRange::All { limit: 256 * 1024 },
        )?;
        let log = std::str::from_utf8(&log)?;
        assert_eq!(log.matches("try_compile-v1").count(), 4);
        assert_eq!(log.matches("try_run-v1").count(), 1);
        assert!(log.contains("variable: \"RUN_RES\""));
        Ok(())
    }
}
