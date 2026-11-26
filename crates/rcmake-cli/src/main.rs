//! Entry point for the CMake command-line tools and language server.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::exit)]

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use rcmake_host::{Host, NativeHost, Stream};

/// Generous worker stack (64 MiB) so deep CMake script recursion (1,000+ frames even in
/// debug builds) never overflows macOS's 8 MiB main thread stack.
const WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::env::var_os("RUST_BACKTRACE").is_some_and(|v| !v.is_empty() && v != "0") {
            default_hook(info);
        } else {
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unexpected internal error");
            let line = match info.location() {
                Some(loc) => format!(
                    "rcmake: internal error: {message} ({}:{})\n",
                    loc.file(),
                    loc.line()
                ),
                None => format!("rcmake: internal error: {message}\n"),
            };
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        }
        #[cfg(panic = "abort")]
        {
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"CMake Internal Error (please report a bug)\n",
            );
            #[expect(
                clippy::exit,
                reason = "Preserve exit code 1 when a panic cannot unwind"
            )]
            std::process::exit(1);
        }
    }));
}

fn stem_is(path: &OsStr, expected: &str) -> bool {
    Path::new(path).file_stem().is_some_and(|stem| {
        if cfg!(windows) {
            stem.eq_ignore_ascii_case(expected)
        } else {
            stem == expected
        }
    })
}

fn main() -> ExitCode {
    install_panic_hook();
    let mut args = std::env::args_os();
    let program = args.next();
    let first = args.next();
    if program.as_deref().is_some_and(|path| {
        stem_is(path, "rcmake-lsp")
            || (first.as_deref() == Some(OsStr::new("lsp"))
                && stem_is(path, "rcmake")
                && std::env::var_os("RCMAKE_ORACLE_MODE").as_deref() != Some(OsStr::new("1")))
    }) {
        return rcmake_lsp::run_stdio();
    }
    let argv: Vec<_> = program
        .into_iter()
        .chain(first)
        .chain(args)
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

    let host_for_panic = Arc::clone(&host);
    let spawned = std::thread::Builder::new()
        .name("rcmake-main".to_owned())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || rcmake_cli::run(&argv, host));

    let result = match spawned {
        Ok(handle) => handle.join(),
        Err(_) => {
            let fallback = Arc::clone(&host_for_panic);
            let argv_fallback: Vec<_> = std::env::args_os()
                .map(|a| rcmake_host::path::from_native(Path::new(&a)))
                .collect();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                rcmake_cli::run(&argv_fallback, fallback)
            }))
        }
    };

    match result {
        #[cfg(windows)]
        Ok(code) => std::process::exit(code),
        #[cfg(not(windows))]
        Ok(code) => ExitCode::from((code & 0xff) as u8),
        Err(_) => {
            host_for_panic.write_out(
                Stream::Stderr,
                b"CMake Internal Error (please report a bug)\n",
            );
            ExitCode::from(1)
        }
    }
}
