//! Run the CMake language server over standard input and output.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::process::ExitCode;

#[cfg(any(unix, windows))]
use std::io::{self, Write};

#[cfg(any(unix, windows))]
use rcmake_host::native::NativeHost;
#[cfg(any(unix, windows))]
use rcmake_lsp::{
    RequestId, RpcMessage, Server,
    rpc::{MAX_MESSAGE_BYTES, read_frame, write_frame},
};

#[cfg(any(unix, windows))]
fn main() -> ExitCode {
    serve().unwrap_or(ExitCode::FAILURE)
}

#[cfg(any(unix, windows))]
fn serve() -> io::Result<ExitCode> {
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
