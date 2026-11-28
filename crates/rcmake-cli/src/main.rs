//! Entry point for the CMake command-line tools and language server.

#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::exit)]

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rcmake_base::BString;
use rcmake_host::{Host, NativeHost, Stream};

const SHIM_TOOL_FLAG_PREFIX: &str = "--rcmake-internal-shim-tool=";
const SHIM_ARGV0_FLAG: &str = "--rcmake-internal-shim-argv0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShimTool {
    Cmake,
    Ctest,
    Cpack,
    Ccmake,
    RcmakeLsp,
}

impl ShimTool {
    fn parse_flag(arg: &OsStr) -> Option<Self> {
        let rest = arg.to_str()?.strip_prefix(SHIM_TOOL_FLAG_PREFIX)?;
        match rest {
            "cmake" => Some(Self::Cmake),
            "ctest" => Some(Self::Ctest),
            "cpack" => Some(Self::Cpack),
            "ccmake" => Some(Self::Ccmake),
            "rcmake-lsp" => Some(Self::RcmakeLsp),
            _ => None,
        }
    }

    fn as_cli_tool(self) -> Option<rcmake_cli::Tool> {
        match self {
            Self::Cmake => Some(rcmake_cli::Tool::Cmake),
            Self::Ctest => Some(rcmake_cli::Tool::Ctest),
            Self::Cpack => Some(rcmake_cli::Tool::Cpack),
            Self::Ccmake => Some(rcmake_cli::Tool::Ccmake),
            Self::RcmakeLsp => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedInvocation {
    program: Option<OsString>,
    args: Vec<OsString>,
    shim_tool: Option<ShimTool>,
}

/// Strip the private `rcmake-shim` launcher prefix ONLY when invoked via `rcmake` with the full
/// validated 4-argument prefix:
/// `--rcmake-internal-shim-tool=<allowed> --rcmake-internal-shim-argv0 <orig_argv0> -- <user_args...>`
fn parse_invocation(raw_args: Vec<OsString>) -> ParsedInvocation {
    let valid_shim = raw_args
        .first()
        .is_some_and(|program| stem_is(program, "rcmake"))
        && raw_args.get(2).map(OsString::as_os_str) == Some(OsStr::new(SHIM_ARGV0_FLAG))
        && raw_args.get(4).map(OsString::as_os_str) == Some(OsStr::new("--"))
        && raw_args
            .get(1)
            .and_then(|flag| ShimTool::parse_flag(flag))
            .is_some();

    let mut iter = raw_args.into_iter();
    if valid_shim {
        let _engine = iter.next();
        let shim_tool = iter.next().and_then(|flag| ShimTool::parse_flag(&flag));
        let _argv0_flag = iter.next();
        let orig_argv0 = iter.next();
        let _separator = iter.next();
        return ParsedInvocation {
            program: orig_argv0,
            args: iter.collect(),
            shim_tool,
        };
    }

    let program = iter.next();
    ParsedInvocation {
        program,
        args: iter.collect(),
        shim_tool: None,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DispatchDecision {
    is_native: bool,
    lsp_program: Option<&'static str>,
}

fn resolve_dispatch(
    parsed: &ParsedInvocation,
    oracle_mode: bool,
    lsp_dir_exists: bool,
) -> DispatchDecision {
    if let Some(shim_tool) = parsed.shim_tool {
        return DispatchDecision {
            is_native: false,
            lsp_program: (shim_tool == ShimTool::RcmakeLsp).then_some("rcmake-lsp"),
        };
    }

    let is_rcmake = parsed
        .program
        .as_deref()
        .is_some_and(|path| stem_is(path, "rcmake"));
    let is_native = is_rcmake && !oracle_mode;
    let first = parsed.args.first().map(OsString::as_os_str);
    let lsp_program = parsed.program.as_deref().and_then(|path| {
        if stem_is(path, "rcmake-lsp") {
            Some("rcmake-lsp")
        } else if first == Some(OsStr::new("lsp")) && is_native && !lsp_dir_exists {
            Some("rcmake lsp")
        } else {
            None
        }
    });
    DispatchDecision {
        is_native,
        lsp_program,
    }
}

/// Generous worker stack (64 MiB) so deep CMake script recursion (1,000+ frames even in
/// debug builds) never overflows macOS's 8 MiB main thread stack.
const WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;
/// Stack kept free for native recursion that the command guard does not see (genex,
/// `math`, JSON, and syntax parsing).
const STACK_RESERVE_BYTES: usize = 8 * 1024 * 1024;

static PANIC_BANNER_PRINTED: AtomicBool = AtomicBool::new(false);

fn install_panic_hook(is_lsp: bool, is_native: bool) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace =
            std::env::var_os("RUST_BACKTRACE").is_some_and(|v| !v.is_empty() && v != "0");
        if backtrace {
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
            PANIC_BANNER_PRINTED.store(true, Ordering::Relaxed);
        }
        #[cfg(panic = "abort")]
        {
            if !is_lsp && backtrace {
                let banner: &[u8] = if is_native {
                    b"rcmake: internal error (please report a bug)\n"
                } else {
                    b"CMake Internal Error (please report a bug)\n"
                };
                let _ = std::io::Write::write_all(&mut std::io::stderr(), banner);
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
    let parsed = parse_invocation(std::env::args_os().collect());
    let oracle_mode = std::env::var_os("RCMAKE_ORACLE_MODE").as_deref() == Some(OsStr::new("1"));
    let lsp_dir_exists = parsed.shim_tool.is_none()
        && parsed.args.first().map(OsString::as_os_str) == Some(OsStr::new("lsp"))
        && Path::new("lsp").is_dir();
    let dispatch = resolve_dispatch(&parsed, oracle_mode, lsp_dir_exists);
    let is_native = dispatch.is_native;
    install_panic_hook(dispatch.lsp_program.is_some(), is_native);
    if let Some(lsp_program) = dispatch.lsp_program {
        let skip = usize::from(lsp_program == "rcmake lsp");
        let lsp_args: Vec<_> = parsed
            .args
            .into_iter()
            .skip(skip)
            .map(|a| rcmake_host::path::from_native(Path::new(&a)))
            .collect();
        return run_lsp(lsp_program, lsp_args);
    }
    let shimmed_cli_tool = parsed.shim_tool.and_then(ShimTool::as_cli_tool);
    let argv: Vec<_> = parsed
        .program
        .into_iter()
        .chain(parsed.args)
        .map(|a| rcmake_host::path::from_native(Path::new(&a)))
        .collect();

    let host = match NativeHost::new() {
        Ok(h) => Arc::new(h),
        Err(err) => {
            let prefix = if is_native {
                "rcmake: internal error"
            } else {
                "CMake Internal Error"
            };
            let msg = format!("{prefix}: failed to initialize host: {err}\n");
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
                match shimmed_cli_tool {
                    Some(tool) => rcmake_cli::run_compat(tool, &argv, host),
                    None => rcmake_cli::run(&argv, host),
                }
            })
        });

    // The main thread has an unknown stack size, so it never runs the evaluator.
    let result = match spawned {
        Ok(handle) => {
            // Keep Thread 0 from taking process-directed SIGINT while the worker
            // thread runs an instrumented command under an interrupt guard.
            rcmake_instrument::interrupt::shield_current_thread();
            handle.join()
        }
        Err(error) => {
            let prefix = if is_native {
                "rcmake: internal error"
            } else {
                "CMake Internal Error"
            };
            let message = format!("{prefix}: failed to start the worker thread: {error}\n");
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
            if !PANIC_BANNER_PRINTED.load(Ordering::Relaxed) {
                let banner: &[u8] = if is_native {
                    b"rcmake: internal error (please report a bug)\n"
                } else {
                    b"CMake Internal Error (please report a bug)\n"
                };
                host_for_panic.write_out(Stream::Stderr, banner);
            }
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_vec(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn shim_prefix_strips_only_complete_validated_four_arg_header() {
        for (tool_name, expected) in [
            ("cmake", ShimTool::Cmake),
            ("ctest", ShimTool::Ctest),
            ("cpack", ShimTool::Cpack),
            ("ccmake", ShimTool::Ccmake),
            ("rcmake-lsp", ShimTool::RcmakeLsp),
        ] {
            let tool_flag = format!("--rcmake-internal-shim-tool={tool_name}");
            let orig = format!("C:\\Program Files\\rcmake\\bin\\{tool_name}.exe");
            let parsed = parse_invocation(os_vec(&[
                "/bin/rcmake",
                &tool_flag,
                "--rcmake-internal-shim-argv0",
                &orig,
                "--",
                "--version",
                "--rcmake-internal-shim-tool=ctest",
            ]));
            assert_eq!(parsed.shim_tool, Some(expected));
            assert_eq!(parsed.program, Some(OsString::from(orig)));
            assert_eq!(
                parsed.args,
                os_vec(&["--version", "--rcmake-internal-shim-tool=ctest"])
            );
        }

        // Zero user args after `--` is valid.
        let empty_user_args = parse_invocation(os_vec(&[
            "rcmake.exe",
            "--rcmake-internal-shim-tool=rcmake-lsp",
            "--rcmake-internal-shim-argv0",
            "rcmake-lsp.exe",
            "--",
        ]));
        assert_eq!(empty_user_args.shim_tool, Some(ShimTool::RcmakeLsp));
        assert_eq!(
            empty_user_args.program,
            Some(OsString::from("rcmake-lsp.exe"))
        );
        assert!(empty_user_args.args.is_empty());
    }

    #[test]
    fn partial_or_invalid_shim_prefixes_are_not_stripped() {
        let cases: [&[&str]; 6] = [
            &["rcmake", "--rcmake-internal-shim-tool=cmake"],
            &[
                "rcmake",
                "--rcmake-internal-shim-tool=cmake",
                "--rcmake-internal-shim-argv0",
                "cmake",
            ],
            &[
                "rcmake",
                "--rcmake-internal-shim-tool=cmake",
                "--rcmake-internal-shim-argv0",
                "cmake",
                "--not-sep",
            ],
            &[
                "rcmake",
                "--rcmake-internal-shim-tool=rcmake",
                "--rcmake-internal-shim-argv0",
                "rcmake",
                "--",
            ],
            &[
                "rcmake",
                "--rcmake-internal-shim-tool=cmake",
                "--wrong-flag",
                "cmake",
                "--",
            ],
            &[
                "cmake",
                "--rcmake-internal-shim-tool=cmake",
                "--rcmake-internal-shim-argv0",
                "cmake",
                "--",
            ],
        ];
        for raw in cases {
            let parsed = parse_invocation(os_vec(raw));
            assert_eq!(parsed.shim_tool, None, "raw={raw:?}");
            assert_eq!(parsed.program, Some(OsString::from(raw[0])));
            assert_eq!(parsed.args, os_vec(&raw[1..]));
        }
    }

    #[test]
    fn dispatch_respects_shim_and_oracle_mode_rules() {
        let shimmed_lsp = parse_invocation(os_vec(&[
            "rcmake.exe",
            "--rcmake-internal-shim-tool=rcmake-lsp",
            "--rcmake-internal-shim-argv0",
            "rcmake-lsp.exe",
            "--",
            "--version",
        ]));
        for oracle_mode in [false, true] {
            assert_eq!(
                resolve_dispatch(&shimmed_lsp, oracle_mode, false),
                DispatchDecision {
                    is_native: false,
                    lsp_program: Some("rcmake-lsp"),
                }
            );
        }

        let shimmed_cmake = parse_invocation(os_vec(&[
            "rcmake.exe",
            "--rcmake-internal-shim-tool=cmake",
            "--rcmake-internal-shim-argv0",
            "cmake.exe",
            "--",
            "lsp",
        ]));
        assert_eq!(
            resolve_dispatch(&shimmed_cmake, false, false),
            DispatchDecision {
                is_native: false,
                lsp_program: None,
            }
        );

        let direct_rcmake_lsp = parse_invocation(os_vec(&["rcmake", "lsp"]));
        assert_eq!(
            resolve_dispatch(&direct_rcmake_lsp, false, false),
            DispatchDecision {
                is_native: true,
                lsp_program: Some("rcmake lsp"),
            }
        );
        assert_eq!(
            resolve_dispatch(&direct_rcmake_lsp, true, false),
            DispatchDecision {
                is_native: false,
                lsp_program: None,
            }
        );
        assert_eq!(
            resolve_dispatch(&direct_rcmake_lsp, false, true),
            DispatchDecision {
                is_native: true,
                lsp_program: None,
            }
        );

        let direct_lsp = parse_invocation(os_vec(&["rcmake-lsp", "--version"]));
        assert_eq!(
            resolve_dispatch(&direct_lsp, true, false),
            DispatchDecision {
                is_native: false,
                lsp_program: Some("rcmake-lsp"),
            }
        );
    }
}
