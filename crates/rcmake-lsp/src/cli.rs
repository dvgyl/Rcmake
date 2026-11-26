//! Command-line argument handling for the `rcmake-lsp` executable.

#![forbid(unsafe_code)]

/// Result of inspecting command-line arguments before starting the language server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    /// Start the LSP server on standard input and output.
    Serve,
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
/// `program` is the command name shown in the usage line (for example `"rcmake-lsp"`
/// or `"rcmake lsp"`). Unknown arguments (such as `--stdio` or `--clientProcessId=<pid>`
/// passed by editor clients) are ignored and return [`Startup::Serve`].
#[must_use]
pub fn startup(program: &str, args: &[impl AsRef<[u8]>]) -> Startup {
    for arg in args {
        match arg.as_ref() {
            b"-h" | b"--help" => {
                return Startup::Exit {
                    stdout: help_text(program),
                    code: 0,
                };
            }
            b"-V" | b"--version" => {
                return Startup::Exit {
                    stdout: version_text(),
                    code: 0,
                };
            }
            _ => {}
        }
    }
    Startup::Serve
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
