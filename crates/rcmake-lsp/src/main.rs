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
    #[cfg(feature = "eval")]
    use stdio_eval::serve;

    #[cfg(panic = "abort")]
    rcmake_lsp::install_stdio_panic_hook();
    serve().unwrap_or(ExitCode::FAILURE)
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
