//! Select the command-line surface and diagnostic style.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::Path;

use rcmake_host::{Host, Stream};

/// Identifies the command-line surface for this invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Persona {
    /// Use the CMake command-line surface.
    #[default]
    Compat,
    /// Accept Rcmake extension flags and subcommands.
    Native,
}

/// Selects the format for human-readable diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DiagStyle {
    /// Use CMake diagnostic formatting.
    #[default]
    Cmake,
    /// Show source locations and annotations where available.
    Rich,
}

impl DiagStyle {
    /// Parse the value of `--diagnostics` or `RCMAKE_DIAGNOSTICS`.
    pub(crate) fn parse(value: &[u8]) -> Option<Self> {
        match value {
            b"cmake" => Some(Self::Cmake),
            b"rich" => Some(Self::Rich),
            _ => None,
        }
    }

    /// Return the value to pass to CLI-owned child processes.
    pub(crate) const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Cmake => b"cmake",
            Self::Rich => b"rich",
        }
    }
}

/// Select Native only if the executable name has the exact stem `rcmake`.
pub(crate) fn persona_from_argv0(argv0: &OsStr) -> Persona {
    if Path::new(argv0).file_stem() == Some(OsStr::new("rcmake")) {
        Persona::Native
    } else {
        Persona::Compat
    }
}

/// Select the style from oracle mode, a Native flag, the environment, or the terminal.
pub(crate) fn resolve_diag_style(
    persona: Persona,
    cli_override: Option<DiagStyle>,
    host: &dyn Host,
) -> DiagStyle {
    if host
        .env_get(b"RCMAKE_ORACLE_MODE")
        .is_some_and(|value| value.as_slice() == b"1")
    {
        return DiagStyle::Cmake;
    }
    if persona == Persona::Native
        && let Some(style) = cli_override
    {
        return style;
    }
    if let Some(style) = host
        .env_get(b"RCMAKE_DIAGNOSTICS")
        .and_then(|value| DiagStyle::parse(value.as_slice()))
    {
        return style;
    }
    terminal_style(persona, || host.is_terminal(Stream::Stderr))
}

fn terminal_style(persona: Persona, is_terminal: impl FnOnce() -> bool) -> DiagStyle {
    if persona == Persona::Native && is_terminal() {
        DiagStyle::Rich
    } else {
        DiagStyle::Cmake
    }
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn only_exact_rcmake_stem_uses_native() {
        for name in ["rcmake", "/shim/rcmake", "rcmake.exe"] {
            assert_eq!(persona_from_argv0(OsStr::new(name)), Persona::Native);
        }
        for name in ["", "cmake", "cpack", "ctest", "Rcmake.exe", "rcmake-debug"] {
            assert_eq!(persona_from_argv0(OsStr::new(name)), Persona::Compat);
        }
    }

    #[test]
    fn style_obeys_oracle_flag_environment_and_terminal_precedence() {
        let host = PureHost::default();
        let resolve = |persona, cli| resolve_diag_style(persona, cli, &host);
        assert_eq!(resolve(Persona::Native, None), DiagStyle::Cmake);
        assert_eq!(terminal_style(Persona::Native, || true), DiagStyle::Rich);
        assert_eq!(terminal_style(Persona::Compat, || true), DiagStyle::Cmake);

        host.env_set(b"RCMAKE_DIAGNOSTICS", Some(b"rich"));
        assert_eq!(
            resolve(Persona::Compat, Some(DiagStyle::Cmake)),
            DiagStyle::Rich
        );
        assert_eq!(
            resolve(Persona::Native, Some(DiagStyle::Cmake)),
            DiagStyle::Cmake
        );
        host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
        assert_eq!(
            resolve(Persona::Native, Some(DiagStyle::Rich)),
            DiagStyle::Cmake
        );

        host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"0"));
        host.env_set(b"RCMAKE_DIAGNOSTICS", Some(b"RICH"));
        assert_eq!(resolve(Persona::Native, None), DiagStyle::Cmake);
        for style in [DiagStyle::Cmake, DiagStyle::Rich] {
            assert_eq!(DiagStyle::parse(style.as_bytes()), Some(style));
        }
    }
}
