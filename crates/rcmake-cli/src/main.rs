//! Binary entry point for `rcmake` (multi-call `cmake`/`ctest`/`cpack`).

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::exit)]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use rcmake_host::{Host, NativeHost, Stream};

fn main() -> ExitCode {
    let argv: Vec<_> = std::env::args_os()
        .map(|a| rcmake_host::path::from_native(Path::new(&a)))
        .collect();

    let host = match NativeHost::new() {
        Ok(h) => Arc::new(h),
        Err(err) => {
            let msg = format!("CMake Internal Error: failed to initialize host: {err}\n");
            let _ = std::io::Write::write_all(&mut std::io::stderr(), msg.as_bytes());
            return ExitCode::from(1);
        }
    };

    let host_for_panic = host.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        rcmake_cli::run(&argv, host)
    }));

    match result {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(_) => {
            host_for_panic.write_out(
                Stream::Stderr,
                b"CMake Internal Error (please report a bug)\n",
            );
            ExitCode::from(1)
        }
    }
}
