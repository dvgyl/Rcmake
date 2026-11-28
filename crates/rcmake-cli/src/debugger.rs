//! `--debugger` Debug Adapter Protocol session lifecycle for top-level CLI invocations.
//!
//! Each top-level configure or `-P` run owns at most one [`Session`]. The session starts
//! at the CMake startup point (`cmake::StartDebuggerIfEnabled`), every interpreter of
//! the run gets a hook through [`attach`], and [`finish`] reports the exit code
//! (`cmake::StopDebuggerIfNeeded`). A hook that an interpreter drops does not end the
//! session. `try_compile` projects make their own interpreters and get no hook.

use std::sync::Arc;

use rcmake_dap::DebugServer;
use rcmake_eval::Interp;
use rcmake_host::{Host, Stream, WriteMode};

use crate::options::Options;

#[cfg(test)]
mod tests;

/// Per-invocation active DAP session handle (owned by one top-level configure or `-P` run).
pub(crate) struct Session {
    server: DebugServer,
}

/// Report whether `-E capabilities` advertises DAP debugger support on this platform.
pub(crate) const fn capabilities_supported() -> bool {
    rcmake_dap::TRANSPORT_SUPPORTED
}

/// Start the debugger adapter if `--debugger` was requested on this invocation.
///
/// Called at CMake's startup point: after initial `-C` and automatic `PreLoad.cmake`
/// scripts succeed for configure mode, or before script execution for `-P` mode.
pub(crate) fn start_if_enabled(
    host: Arc<dyn Host>,
    opts: &Options,
) -> Result<Option<Session>, i32> {
    if !opts.debugger {
        return Ok(None);
    }
    let Some(pipe) = opts.debugger_pipe.as_deref() else {
        host.write_out(
            Stream::Stderr,
            b"Error: --debugger-pipe must be set when debugging is enabled.\n",
        );
        return Err(script_mode_failure(host.as_ref(), opts));
    };
    let config = rcmake_dap::Config {
        host: Arc::clone(&host),
        stdout: stdout_sink(&host),
        log: opts
            .debugger_dap_log
            .as_ref()
            .and_then(|path| log_sink(&host, path)),
        handshake_limit: None,
        exit_limit: None,
    };
    match listen(pipe, &host, opts.persona) {
        Ok(listener) => Ok(Some(Session {
            server: DebugServer::start(listener, config),
        })),
        Err(reason) => {
            let text = format!("Error: Failed to create debugger adapter.\n{reason}\n");
            host.write_out(Stream::Stderr, text.as_bytes());
            Err(script_mode_failure(host.as_ref(), opts))
        }
    }
}

/// In `-P` mode the debugger starts while CMake parses the arguments, so a start
/// failure is also an argument error (`Run 'cmake --help' for all supported options.`).
fn script_mode_failure(host: &dyn Host, opts: &Options) -> i32 {
    if opts.script_mode {
        crate::options::cli_usage_error_only(host, opts.persona);
    }
    1
}

#[cfg(unix)]
fn listen(
    pipe: &std::path::Path,
    host: &Arc<dyn Host>,
    _persona: crate::persona::Persona,
) -> Result<Box<dyn rcmake_dap::transport::Listener>, String> {
    rcmake_dap::transport::unix::UnixEndpoint::bind(pipe, Arc::clone(host))
        .map(|endpoint| Box::new(endpoint) as Box<dyn rcmake_dap::transport::Listener>)
}

/// Make the Windows named pipe (ADR-050). The compat persona adds the `FormatMessage`
/// line break (`\r\n`) that the Rust error text does not have. The text is the same
/// as in CMake on English systems. On other locales, it can be different, because
/// CMake uses `FormatMessageA` and Rust uses `FormatMessageW`.
#[cfg(windows)]
fn listen(
    pipe: &std::path::Path,
    _host: &Arc<dyn Host>,
    persona: crate::persona::Persona,
) -> Result<Box<dyn rcmake_dap::transport::Listener>, String> {
    match rcmake_dap::transport::windows::WindowsEndpoint::bind(pipe) {
        Ok(endpoint) => Ok(Box::new(endpoint)),
        Err(reason) => Err(match persona {
            crate::persona::Persona::Compat => format!("{reason}\r\n"),
            crate::persona::Persona::Native => reason,
        }),
    }
}

/// Report the missing debugger transport. The compat persona keeps the exact CMake
/// text.
#[cfg(not(any(unix, windows)))]
fn listen(
    _pipe: &std::path::Path,
    _host: &Arc<dyn Host>,
    persona: crate::persona::Persona,
) -> Result<Box<dyn rcmake_dap::transport::Listener>, String> {
    Err(unsupported_transport_reason(persona).into())
}

/// Reason text when the platform has no debugger pipe transport.
#[cfg_attr(any(unix, windows), allow(dead_code))]
fn unsupported_transport_reason(persona: crate::persona::Persona) -> &'static str {
    match persona {
        crate::persona::Persona::Compat => "CMake was not built with support for --debugger",
        crate::persona::Persona::Native => {
            "Debugger transport is not supported on this platform in rcmake"
        }
    }
}

fn stdout_sink(host: &Arc<dyn Host>) -> rcmake_dap::OutputSink {
    let host = Arc::clone(host);
    Arc::new(move |bytes: &[u8]| host.write_out(Stream::Stdout, bytes))
}

/// Truncate the DAP log and return a sink that appends to it. A log that can not be
/// opened is not written, as in CMake.
fn log_sink(host: &Arc<dyn Host>, path: &std::path::Path) -> Option<rcmake_dap::OutputSink> {
    host.write(path, b"", WriteMode::Truncate).ok()?;
    let host = Arc::clone(host);
    let path = path.to_path_buf();
    Some(Arc::new(move |bytes: &[u8]| {
        let _ = host.write(&path, bytes, WriteMode::Append);
    }))
}

/// Attach the active debugger session (if any) to `interp`.
pub(crate) fn attach(session: Option<&Session>, interp: &mut Interp) {
    if let Some(session) = session {
        interp.set_debug_hook(Some(session.server.hook()));
    }
}

/// Finish the active debugger session with `exit_code`.
pub(crate) fn finish(session: Option<Session>, exit_code: i32) {
    if let Some(session) = session {
        session.server.finish(exit_code);
    }
}
