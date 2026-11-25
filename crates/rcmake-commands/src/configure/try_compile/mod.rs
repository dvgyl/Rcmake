use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_build::BuildOutcome;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{CopyOptions, Host, HostError, Input, Output, RunRequest, path};
use rcmake_model::{CacheEntry, CacheType};

use args::{Options, Signature, parse};
use child::{ChildBuild, ChildConfig};
use log::log_probe;
use project::write_project;
use run::run_executable;
use seed::{TryCompileSeed, probe_imports};

#[cfg(all(feature = "native", any(unix, windows)))]
pub(in crate::configure) mod active;
mod args;
#[cfg(all(feature = "native", any(unix, windows)))]
pub(super) mod cache;
mod capture;
mod child;
#[cfg(all(feature = "native", any(unix, windows)))]
pub(super) mod journal;
mod log;
#[cfg(all(feature = "native", any(unix, windows)))]
pub(super) mod plan;
mod project;
mod run;
mod seed;
#[cfg(all(feature = "native", any(unix, windows)))]
mod template;
#[cfg(test)]
mod tests;

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
    let joined = path::from_native_cmake(&base.join(path::to_native(value)));
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
    if let Some(denial) = process_denial(cx.host()) {
        set_build_result(cx, build_var, &options, false, &denial);
        if let Some(var) = run_var {
            run_executable(cx, &options, None, var, &denial);
        }
        if let Some(var) = options.one(b"COPY_FILE_ERROR") {
            cx.set_var(var, BString::default());
        }
        let command = if run { "try_run" } else { "try_compile" };
        cx.issue(
            MessageKind::Warning,
            Effect::None,
            None,
            format!("{command} skipped the test build. {denial}").into(),
        );
        return Ok(Flow::Normal);
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
    let result = build_probe(cx, &signature, &options, &dirs, build_var, run_var);
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

fn process_denial(host: &dyn Host) -> Option<BString> {
    if host.supports_process_execution() {
        return None;
    }
    // An empty pipeline cannot start a process. Denying hosts reject the request
    // before pipeline validation.
    let request = RunRequest {
        pipeline: Vec::new(),
        cwd: host.initial_cwd(),
        env: Vec::new(),
        stdin: Input::Null,
        stdout: Output::Null,
        stderr: Output::Null,
        timeout: None,
        tee: false,
    };
    if let Err(error) = host.run_bounded(&request, 0)
        && error.kind == std::io::ErrorKind::Unsupported
    {
        return Some(error.os_message.into());
    }
    Some("The host does not support the test build.".into())
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
    let mut seed = TryCompileSeed::new(cx, options, source_signature);
    let limit = b"CMAKE_MAXIMUM_RECURSION_DEPTH";
    if let Some(value) = cx.var(limit)
        && !seed
            .defines
            .iter()
            .any(|define| define.name.as_slice() == limit)
    {
        seed.defines.push(rcmake_eval::CacheDefine {
            name: limit.as_slice().into(),
            ty: Some(rcmake_model::CacheType::String),
            value,
        });
    }
    let save_cache = dirs.scratch.is_none() || cx.var_is_on(b"CMAKE_DEBUG_TRY_COMPILE");
    #[cfg(all(feature = "native", any(unix, windows)))]
    let mut cached_probe = if run_var.is_none() {
        active::PreparedProbe::prepare(cx, signature, options, dirs, target, &seed, &imports)
    } else {
        None
    };
    let child = ChildConfig::new(cx, dirs, seed);
    #[cfg(all(feature = "native", any(unix, windows)))]
    if let Some(probe) = &cached_probe
        && imports.is_empty()
    {
        probe.queue(cx, signature, target, &child, &dirs.source);
    }
    #[cfg(all(feature = "native", any(unix, windows)))]
    if let Some(probe) = &mut cached_probe
        && let Some(hit) = probe.hit(cx, dirs, options)
    {
        return apply_probe(cx, options, dirs, build_var, run_var, hit);
    }

    let parent = cx.host_arc();
    #[cfg(all(feature = "native", any(unix, windows)))]
    let journal = cached_probe.as_ref().and_then(|probe| probe.journal(dirs));
    #[cfg(all(feature = "native", any(unix, windows)))]
    let parent = journal.as_ref().map_or(parent, |host| {
        std::sync::Arc::clone(host) as std::sync::Arc<dyn Host>
    });

    let (result, recursion_exceeded) = child.run(
        parent,
        dirs,
        ChildBuild {
            target,
            imports,
            save_cache,
            source_signature,
        },
        |child| crate::hermetic::inherit(cx, child),
    );
    if recursion_exceeded {
        cx.host()
            .write_out(rcmake_host::Stream::Stderr, &result.output);
        cx.fail_nested_recursion();
        return Err(CmdError::Reported);
    }
    #[cfg(all(feature = "native", any(unix, windows)))]
    if let Some(probe) = cached_probe
        && let Some(journal) = journal.and_then(|host| std::sync::Arc::try_unwrap(host).ok())
    {
        probe.record(cx, dirs, build_var, &result, journal.finish());
    }
    apply_probe(cx, options, dirs, build_var, run_var, result)
}

struct ProbeResult {
    outcome: BuildOutcome,
    output: BString,
    artifact: Option<PathBuf>,
}

fn apply_probe(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    dirs: &ProbeDirs,
    build_var: &[u8],
    run_var: Option<&[u8]>,
    result: ProbeResult,
) -> Result<(), CmdError> {
    let ProbeResult {
        outcome,
        output,
        artifact,
    } = result;
    set_build_result(cx, build_var, options, outcome.success, &output);
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

fn probe_cache(child: &mut Cx<'_>) -> BString {
    let mut data = BString::default();
    let entries: std::collections::BTreeMap<_, _> = child.cache().iter().collect();
    for (name, entry) in entries {
        if name.iter().any(|byte| matches!(byte, b'\n' | b'\r' | b'"')) {
            continue;
        }
        let quote_name = name.contains(&b':') || name.starts_with(b"#") || name.starts_with(b"//");
        if quote_name {
            data.push(b'"');
        }
        data.extend_from_slice(name);
        if quote_name {
            data.push(b'"');
        }
        data.push(b':');
        data.extend_from_slice(entry.ty.as_str().as_bytes());
        data.push(b'=');
        let value = entry
            .value
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or_default();
        let quote_value = value.starts_with(b"'") || value != value.trim_ascii_end();
        if quote_value {
            data.push(b'\'');
        }
        data.extend_from_slice(value);
        if quote_value {
            data.push(b'\'');
        }
        data.push(b'\n');
    }
    data
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

fn set_build_result(
    cx: &mut Cx<'_>,
    name: &[u8],
    options: &Options<'_>,
    success: bool,
    output: &[u8],
) {
    let value = if success { "TRUE" } else { "FALSE" };
    set_result(cx, name, value.into(), options.has(b"NO_CACHE"));
    if let Some(var) = options
        .one(b"COMPILE_OUTPUT_VARIABLE")
        .or_else(|| options.one(b"OUTPUT_VARIABLE"))
    {
        cx.set_var(var, output.into());
    }
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
