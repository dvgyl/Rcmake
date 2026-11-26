//! Entry point for the CMake command-line tools and language server.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::exit)]

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, NativeHost, Stream};

/// Generous worker stack (64 MiB) so deep CMake script recursion (1,000+ frames even in
/// debug builds) never overflows macOS's 8 MiB main thread stack.
const WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;
/// Stack kept free for native recursion that the command guard does not see (genex,
/// `math`, JSON, and syntax parsing).
const STACK_RESERVE_BYTES: usize = 8 * 1024 * 1024;

fn install_panic_hook(is_lsp: bool, is_native: bool) {
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
            let line = if is_lsp {
                format!("rcmake-lsp: internal error: {message}\n")
            } else if is_native {
                format!(
                    "rcmake: internal error: {message}\nPlease report this bug with RUST_BACKTRACE=1.\n"
                )
            } else {
                format!("CMake Internal Error: {message}\n")
            };
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        }
        #[cfg(panic = "abort")]
        {
            if !is_lsp {
                let _ = std::io::Write::write_all(
                    &mut std::io::stderr(),
                    b"CMake Internal Error (please report a bug)\n",
                );
            }
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

fn run_lsp(program: &'static str, lsp_args: Vec<BString>) -> ExitCode {
    let spawned = std::thread::Builder::new()
        .name("rcmake-lsp".to_owned())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            rcmake_eval::stack_guard::with_budget(WORKER_STACK_BYTES - STACK_RESERVE_BYTES, || {
                rcmake_lsp::run_stdio_with_args(program, &lsp_args)
            })
        });

    let result = match spawned {
        Ok(handle) => handle.join(),
        Err(error) => {
            let message = format!("{program}: failed to start worker thread: {error}\n");
            let _ = std::io::Write::write_all(&mut std::io::stderr(), message.as_bytes());
            return ExitCode::from(1);
        }
    };

    match result {
        Ok(code) => code,
        Err(_) => {
            let message = format!("{program}: internal error\n");
            let _ = std::io::Write::write_all(&mut std::io::stderr(), message.as_bytes());
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let program = args.next();
    let first = args.next();
    let oracle_mode = std::env::var_os("RCMAKE_ORACLE_MODE").as_deref() == Some(OsStr::new("1"));
    let is_rcmake = program
        .as_deref()
        .is_some_and(|path| stem_is(path, "rcmake"));
    let lsp_program = program.as_deref().and_then(|path| {
        if stem_is(path, "rcmake-lsp") {
            Some("rcmake-lsp")
        } else if first.as_deref() == Some(OsStr::new("lsp")) && is_rcmake && !oracle_mode {
            Some("rcmake lsp")
        } else {
            None
        }
    });
    install_panic_hook(lsp_program.is_some(), is_rcmake && !oracle_mode);
    if let Some(lsp_program) = lsp_program {
        let lsp_args: Vec<_> = if lsp_program == "rcmake-lsp" {
            first
                .into_iter()
                .chain(args)
                .map(|a| rcmake_host::path::from_native(Path::new(&a)))
                .collect()
        } else {
            args.map(|a| rcmake_host::path::from_native(Path::new(&a)))
                .collect()
        };
        return run_lsp(lsp_program, lsp_args);
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
        .spawn(move || {
            rcmake_eval::stack_guard::with_budget(WORKER_STACK_BYTES - STACK_RESERVE_BYTES, || {
                rcmake_cli::run(&argv, host)
            })
        });

    // The main thread has an unknown stack size, so it never runs the evaluator.
    let result = match spawned {
        Ok(handle) => handle.join(),
        Err(error) => {
            let message =
                format!("CMake Internal Error: failed to start the worker thread: {error}\n");
            host_for_panic.write_out(Stream::Stderr, message.as_bytes());
            return ExitCode::from(1);
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
