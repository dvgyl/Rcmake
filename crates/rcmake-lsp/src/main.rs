//! Run the CMake language server over standard input and output.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::process::ExitCode;

#[cfg(any(unix, windows))]
use std::io::{self, Write};

#[cfg(all(any(unix, windows), not(feature = "eval")))]
use rcmake_host::native::NativeHost;
#[cfg(any(unix, windows))]
use rcmake_lsp::{
    RpcMessage,
    rpc::{MAX_MESSAGE_BYTES, write_frame},
};

#[cfg(all(any(unix, windows), feature = "eval"))]
mod stdio_eval;

#[cfg(any(unix, windows))]
fn main() -> ExitCode {
    use std::io::IsTerminal as _;
    #[cfg(feature = "eval")]
    use stdio_eval::serve;

    #[cfg(panic = "abort")]
    rcmake_lsp::install_stdio_panic_hook();

    let args: Vec<Vec<u8>> = std::env::args_os().skip(1).map(os_arg_bytes).collect();
    match rcmake_lsp::cli::startup("rcmake-lsp", &args) {
        rcmake_lsp::cli::Startup::Exit { stdout, code } => {
            let mut out = io::stdout().lock();
            let _ = out.write_all(stdout.as_bytes());
            let _ = out.flush();
            ExitCode::from(code)
        }
        rcmake_lsp::cli::Startup::Serve => {
            if io::stdin().is_terminal() {
                let mut err = io::stderr().lock();
                let _ = writeln!(
                    err,
                    "rcmake-lsp: waiting for LSP messages on standard input. Run rcmake-lsp --help for usage."
                );
                let _ = err.flush();
            }
            serve().unwrap_or(ExitCode::FAILURE)
        }
    }
}

#[cfg(unix)]
fn os_arg_bytes(arg: std::ffi::OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    arg.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn os_arg_bytes(arg: std::ffi::OsString) -> Vec<u8> {
    arg.to_string_lossy().into_owned().into_bytes()
}

#[cfg(all(any(unix, windows), not(feature = "eval")))]
fn serve() -> io::Result<ExitCode> {
    use rcmake_lsp::{RequestId, Server, rpc::read_frame};
    let host = NativeHost::new().map_err(io::Error::other)?;
    let mut server = Server::new(&host);
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let body = match read_frame(&mut input) {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(ExitCode::SUCCESS),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                ) =>
            {
                send(
                    &mut output,
                    RpcMessage::error(RequestId::Null, -32700, "Invalid Content-Length frame."),
                )?;
                return Ok(ExitCode::FAILURE);
            }
            Err(error) => return Err(error),
        };
        let outgoing = match RpcMessage::decode(&body) {
            Ok(message) => server.handle(message),
            Err(error) => vec![error],
        };
        for message in outgoing {
            send(&mut output, message)?;
        }
        if let Some(code) = server.exit_code() {
            return Ok(if code == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
    }
}

#[cfg(any(unix, windows))]
fn send(output: &mut impl Write, message: RpcMessage) -> io::Result<()> {
    let body = message.encode();
    if body.len() <= MAX_MESSAGE_BYTES {
        write_frame(output, &body)?;
    } else if let RpcMessage::Result { id, .. } = message {
        let error = RpcMessage::error(id, -32603, "Response exceeds the message size limit.");
        write_frame(output, &error.encode())?;
    }
    output.flush()
}

#[cfg(not(any(unix, windows)))]
fn main() -> ExitCode {
    ExitCode::FAILURE
}
