use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow, Interp, InterpConfig, RunMode};
use rcmake_expr::ExpandedArg;
use rcmake_host::{CopyOptions, Host, HostError, path};
use rcmake_model::{CacheEntry, CacheType, DirId};

use args::{Options, Signature, parse};
use capture::CapturingHost;
use log::log_probe;
use project::write_project;
use run::{build_child, run_executable};
use seed::{TryCompileSeed, probe_imports};

mod args;
mod capture;
mod log;
mod project;
mod run;
mod seed;
#[cfg(test)]
mod tests;

thread_local! {
    static PROBE_DEPTH: Cell<u8> = const { Cell::new(0) };
}

const TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn try_compile(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    probe(cx, args, false)
}

pub(super) fn try_run(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    probe(cx, args, true)
}

fn error_text(error: impl std::fmt::Display) -> CmdError {
    CmdError::status(error.to_string())
}

fn probe_error(cx: &mut Cx<'_>, message: impl Into<BString>) -> CmdError {
    cx.issue(MessageKind::Error, Effect::SetError, None, message.into());
    CmdError::Reported
}

struct ProbeDirs {
    source: PathBuf,
    binary: PathBuf,
    scratch: Option<PathBuf>,
}

impl ProbeDirs {
    fn create(cx: &mut Cx<'_>, signature: &Signature<'_>) -> Result<Self, CmdError> {
        match signature {
            Signature::Project {
                source,
                binary: Some(binary),
                ..
            } => {
                let binary = explicit_binary_dir(cx, binary)?;
                let source = absolute(&binary, source);
                if binary == path::to_native(&cx.var_or_empty(b"CMAKE_BINARY_DIR")) {
                    return Err(CmdError::status(
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
                let dir = explicit_binary_dir(cx, base)?.join("CMakeFiles/CMakeTmp");
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
                Err(CmdError::status(
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
        return Err(CmdError::status("The scratch path must be a directory."));
    }
    Ok(())
}

fn explicit_binary_dir(cx: &mut Cx<'_>, value: &[u8]) -> Result<PathBuf, CmdError> {
    if value.is_empty() {
        return Err(probe_error(cx, "No <bindir> specified."));
    }
    let dir = path::to_native(value);
    if !dir.is_absolute() {
        return Err(probe_error(
            cx,
            [b"<bindir> is not an absolute path:\n\n '", value, b"'"].concat(),
        ));
    }
    Ok(absolute(Path::new("/"), value))
}

fn absolute(base: &Path, value: &[u8]) -> PathBuf {
    let joined = path::from_native(&base.join(path::to_native(value)));
    path::to_native(&rcmake_base::path_lex::Lexical::new(&joined).normalize())
}

fn probe(cx: &mut Cx<'_>, args: &[ExpandedArg], run: bool) -> CmdResult {
    let (run_var, build_var, first, rest) = match (run, args) {
        (true, [run_var, build_var, first, next, rest @ ..]) => {
            (Some(run_var), build_var, first, (next, rest))
        }
        (false, [build_var, first, next, rest @ ..]) => (None, build_var, first, (next, rest)),
        (true, _) => {
            return Err(probe_error(
                cx,
                "The try_run() command requires at least 4 arguments.",
            ));
        }
        (false, _) => {
            return Err(probe_error(
                cx,
                "The try_compile() command requires at least 3 arguments.",
            ));
        }
    };
    let args = std::iter::once(rest.0)
        .chain(rest.1)
        .map(|arg| arg.value.as_slice());
    let build_var = build_var.value.as_slice();
    let run_var = run_var.map(|arg| arg.value.as_slice());
    let (signature, options) =
        parse(&first.value, args, run).map_err(|message| probe_error(cx, message))?;
    if !options.unknown.is_empty() {
        let mut message = BString::from("Unknown arguments:\n");
        for arg in &options.unknown {
            message.extend_from_slice(b"\n  \"");
            message.extend_from_slice(arg);
            message.push(b'"');
        }
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            message,
        );
    }
    let depth = PROBE_DEPTH.get();
    if depth >= 2 {
        return Err(CmdError::status("The try_compile nesting limit is two."));
    }
    let dirs = ProbeDirs::create(cx, &signature)?;
    if cx.var_is_on(b"CMAKE_DEBUG_TRY_COMPILE") {
        let command = if run { "try_run" } else { "try_compile" };
        cx.issue(
            MessageKind::Log,
            Effect::None,
            None,
            format!(
                "Executing {command} ({}) in:\n\n  {}",
                BString::from(build_var),
                dirs.binary.display()
            )
            .into(),
        );
    }
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
    let source_signature = matches!(signature, Signature::Sources { .. });
    let imports = if source_signature {
        probe_imports(cx, options)?
    } else {
        Vec::new()
    };
    let probe_target;
    let target = match signature {
        Signature::Sources { sources, .. } => {
            static NEXT_TARGET: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT_TARGET.fetch_add(1, Ordering::Relaxed);
            probe_target = format!("cmTC_{suffix:05x}");
            write_project(
                cx,
                options,
                sources,
                &dirs.source,
                &probe_target,
                run_var.is_some(),
            )?;
            Some(probe_target.as_bytes())
        }
        Signature::Project { target, .. } => *target,
    };
    let seed = TryCompileSeed::new(cx, options, source_signature);
    let cmake_root = path::to_native(&cx.var_or_empty(b"CMAKE_ROOT"));
    let cmake_command = path::to_native(&cx.var_or_empty(b"CMAKE_COMMAND"));
    let limits = cx.limits().clone();
    let trace = cx.trace_config().cloned();
    let generator = cx.project().generator;
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
        build_child(&mut child.cx(), &dirs.binary, target, source_signature)
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
            return Err(CmdError::status(message));
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

fn split_byte(bytes: &[u8], byte: u8) -> Option<(&[u8], &[u8])> {
    let mut parts = bytes.splitn(2, |&value| value == byte);
    Some((parts.next()?, parts.next()?))
}
