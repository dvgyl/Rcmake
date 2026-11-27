//! CLI debugger lifecycle tests: option checks, start errors, and one real socket
//! session.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{PureHost, Stream};

use super::*;

fn host() -> Arc<PureHost> {
    Arc::new(PureHost::new(
        PathBuf::from("/w"),
        PathBuf::from("/bin/cmake"),
    ))
}

fn stderr(host: &PureHost) -> String {
    String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned()
}

#[test]
fn capabilities_follow_the_transport() {
    assert_eq!(capabilities_supported(), cfg!(unix));
}

#[test]
fn no_debugger_option_starts_nothing() {
    let host = host();
    let opts = Options::default();
    assert!(matches!(start_if_enabled(host.clone(), &opts), Ok(None)));
    assert_eq!(stderr(&host), "");
}

#[test]
fn debugger_without_pipe_is_an_error() {
    let host = host();
    let opts = Options {
        debugger: true,
        ..Options::default()
    };
    assert!(matches!(start_if_enabled(host.clone(), &opts), Err(1)));
    assert_eq!(
        stderr(&host),
        "Error: --debugger-pipe must be set when debugging is enabled.\n"
    );
    // In `-P` mode the failure is also an argument error.
    let host = self::host();
    let opts = Options {
        debugger: true,
        script_mode: true,
        ..Options::default()
    };
    assert!(matches!(start_if_enabled(host.clone(), &opts), Err(1)));
    assert!(stderr(&host).ends_with("for all supported options.\n"));
}

#[cfg(unix)]
#[test]
fn bind_failure_reports_the_adapter_error() {
    let host = host();
    let opts = Options {
        debugger: true,
        debugger_pipe: Some(PathBuf::from("/nonexistent-rcmake-dir/sock")),
        ..Options::default()
    };
    assert!(matches!(start_if_enabled(host.clone(), &opts), Err(1)));
    assert_eq!(
        stderr(&host),
        "Error: Failed to create debugger adapter.\nFailed to bind name \
         '/nonexistent-rcmake-dir/sock' to socket: No such file or directory\n"
    );
}

#[cfg(unix)]
fn frame(stream: &mut UnixStream, body: &str) {
    let message = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(message.as_bytes()).unwrap();
}

/// Read messages until one contains `needle`.
#[cfg(unix)]
fn read_until(reader: &mut BufReader<UnixStream>, needle: &str) -> String {
    loop {
        let mut header = String::new();
        let mut length = 0;
        loop {
            header.clear();
            assert!(
                reader.read_line(&mut header).unwrap() > 0,
                "end before {needle}"
            );
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.strip_prefix("Content-Length: ") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body = String::from_utf8(body).unwrap();
        if body.contains(needle) {
            return body;
        }
    }
}

#[cfg(unix)]
fn connect(path: &Path) -> UnixStream {
    for _ in 0..3000 {
        if let Ok(stream) = UnixStream::connect(path) {
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            return stream;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no socket at {}", path.display());
}

#[cfg(unix)]
#[test]
// The test owns a real socket file next to the `PureHost` interpreter.
#[allow(clippy::disallowed_methods)]
fn socket_session_stops_and_finishes() {
    let socket = std::env::temp_dir().join(format!("rcmake-dbg-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let host = host();
    host.seed_file(
        Path::new("/w/main.cmake"),
        b"message(STATUS one)\nmessage(STATUS two)\n",
    )
    .unwrap();
    let opts = Options {
        debugger: true,
        debugger_pipe: Some(socket.clone()),
        ..Options::default()
    };
    let client = std::thread::spawn({
        let socket = socket.clone();
        move || {
            let mut stream = connect(&socket);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            frame(
                &mut stream,
                r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"t"}}"#,
            );
            read_until(&mut reader, r#""event":"initialized""#);
            frame(
                &mut stream,
                r#"{"seq":2,"type":"request","command":"setBreakpoints","arguments":{"source":{"path":"/w/main.cmake"},"breakpoints":[{"line":2}]}}"#,
            );
            frame(
                &mut stream,
                r#"{"seq":3,"type":"request","command":"configurationDone"}"#,
            );
            let stopped = read_until(&mut reader, r#""event":"stopped""#);
            frame(
                &mut stream,
                r#"{"seq":4,"type":"request","command":"continue","arguments":{"threadId":1}}"#,
            );
            let exited = read_until(&mut reader, r#""event":"exited""#);
            read_until(&mut reader, r#""event":"terminated""#);
            frame(
                &mut stream,
                r#"{"seq":5,"type":"request","command":"disconnect","arguments":{}}"#,
            );
            read_until(&mut reader, r#""command":"disconnect""#);
            (stopped, exited)
        }
    });
    let session = start_if_enabled(host.clone(), &opts).unwrap();
    let config = InterpConfig::new(host.clone(), RunMode::Script, "/share", "/bin/cmake", "/w");
    let mut interp = Interp::new(config, |_| {});
    attach(session.as_ref(), &mut interp);
    assert!(interp.has_debugger());
    let code = interp.run_script(Path::new("main.cmake")).exit_code;
    finish(session, code);
    let (stopped, exited) = client.join().unwrap();
    assert!(stopped.contains(r#""hitBreakpointIds":[0]"#), "{stopped}");
    assert!(exited.contains(r#""exitCode":0"#), "{exited}");
    assert_eq!(
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stdout)),
        "Waiting for debugger client to connect...\nDebugger client connected.\n-- one\n-- two\n"
    );
    let _ = std::fs::remove_file(&socket);
}
