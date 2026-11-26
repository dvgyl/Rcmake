//! Command-line argument handling for the `rcmake-lsp` executable.

#![forbid(unsafe_code)]

/// Result of inspecting command-line arguments before starting the language server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    /// Start the LSP server on standard input and output.
    Serve,
    /// Print an argument error on standard error and exit with code 2.
    InvalidArgument(String),
    /// Print this text on standard output and exit with this status code.
    Exit {
        /// Text to write to standard output.
        stdout: String,
        /// Process exit code.
        code: u8,
    },
}

/// Inspect server command-line arguments without reading standard input.
///
/// `program` is the command name shown in diagnostics and help.
#[must_use]
pub fn startup(program: &str, args: &[impl AsRef<[u8]>]) -> Startup {
    let mut startup = Startup::Serve;
    for arg in args {
        let arg = arg.as_ref();
        let text: fn(&str) -> String = match arg {
            b"-h" | b"--help" => help_text,
            b"-V" | b"--version" => |_| version_text(),
            b"--stdio" => continue,
            _ if arg
                .strip_prefix(b"--clientProcessId=")
                .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit)) =>
            {
                continue;
            }
            _ => {
                return Startup::InvalidArgument(format!(
                    "{program}: Unknown argument {}. Run {program} --help.\n",
                    String::from_utf8_lossy(arg)
                ));
            }
        };
        if startup == Startup::Serve {
            startup = Startup::Exit {
                stdout: text(program),
                code: 0,
            };
        }
    }
    startup
}

fn help_text(program: &str) -> String {
    format!(
        "Usage: {program} [--help | --version]\n\
\n\
Run the CMake language server. An editor starts this program.\n\
The server reads LSP messages on standard input and writes replies on standard output.\n\
\n\
  -h, --help     Print this help and exit.\n\
  -V, --version  Print the version and exit.\n\
\n\
Editor setup: https://github.com/dvgyl/rcmake/blob/main/crates/rcmake-lsp/README.md\n"
    )
}

fn version_text() -> String {
    let features = if cfg!(feature = "eval") {
        "eval"
    } else {
        "lexical"
    };
    format!(
        "rcmake-lsp {} (CMake {} language)\nFeatures: {features}\n",
        env!("CARGO_PKG_VERSION"),
        rcmake_base::CMAKE_VERSION_STR
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_flags_exit_without_blocking() {
        for flag in ["--help", "-h"] {
            let Startup::Exit { stdout, code } = startup("rcmake-lsp", &[flag]) else {
                panic!("expected Exit for {flag}");
            };
            assert_eq!(code, 0);
            assert!(stdout.starts_with("Usage: rcmake-lsp [--help | --version]\n"));
            assert!(stdout.contains("--help"));
            assert!(stdout.contains("--version"));
        }
    }

    #[test]
    fn version_flags_report_package_version_and_feature_mode() {
        for flag in ["--version", "-V"] {
            let Startup::Exit { stdout, code } = startup("rcmake-lsp", &[flag]) else {
                panic!("expected Exit for {flag}");
            };
            assert_eq!(code, 0);
            assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
            assert!(stdout.contains(rcmake_base::CMAKE_VERSION_STR));
            if cfg!(feature = "eval") {
                assert!(stdout.contains("Features: eval\n"));
            } else {
                assert!(stdout.contains("Features: lexical\n"));
            }
        }
    }

    #[test]
    fn editor_transport_flags_and_empty_args_serve() {
        assert_eq!(startup("rcmake-lsp", &[] as &[&str]), Startup::Serve);
        assert_eq!(
            startup("rcmake-lsp", &["--stdio", "--clientProcessId=12"]),
            Startup::Serve
        );
    }
}
