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
    /// Accept rcmake extension flags and subcommands.
    Native,
}

impl Persona {
    /// `usage` for this persona: the Native persona names `rcmake` in the
    /// `Usage: cmake ...` line.
    pub(crate) fn usage(self, usage: &str) -> std::borrow::Cow<'_, str> {
        match (self, usage.strip_prefix("Usage: cmake ")) {
            (Self::Native, Some(rest)) => format!("Usage: rcmake {rest}").into(),
            _ => usage.into(),
        }
    }
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

/// Select Native for the stem `rcmake`. Ignore ASCII case on Windows.
pub(crate) fn persona_from_argv0(argv0: &OsStr) -> Persona {
    if Path::new(argv0).file_stem().is_some_and(|stem| {
        if cfg!(windows) {
            stem.eq_ignore_ascii_case("rcmake")
        } else {
            stem == "rcmake"
        }
    }) {
        Persona::Native
    } else {
        Persona::Compat
    }
}

/// Return whether this invocation uses oracle mode.
pub(crate) fn oracle_mode(host: &dyn Host) -> bool {
    host.env_get(b"RCMAKE_ORACLE_MODE")
        .is_some_and(|value| value.as_slice() == b"1")
}

/// Select the style for the effective persona.
pub(crate) fn resolve_diag_style(
    persona: Persona,
    cli_override: Option<DiagStyle>,
    host: &dyn Host,
) -> DiagStyle {
    if persona == Persona::Compat {
        return DiagStyle::Cmake;
    }
    if let Some(style) = cli_override {
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

/// Select ANSI color for rich diagnostics on standard error.
/// `CLICOLOR_FORCE=1` takes precedence over `NO_COLOR`.
pub(crate) fn resolve_rich_color(host: &dyn Host, stderr_is_tty: bool, style: DiagStyle) -> bool {
    style == DiagStyle::Rich
        && (host
            .env_get(b"CLICOLOR_FORCE")
            .is_some_and(|value| value.as_slice() == b"1")
            || (stderr_is_tty
                && host
                    .env_get(b"NO_COLOR")
                    .is_none_or(|value| value.is_empty())))
}

/// Resolve the terminal column width for rich diagnostics.
pub(crate) fn rich_width(host: &dyn Host) -> usize {
    if let Some(cols) = host
        .env_get(b"COLUMNS")
        .and_then(|value| std::str::from_utf8(&value).ok().map(str::to_owned))
        .and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|&cols| (20..=4096).contains(&cols))
    {
        return cols;
    }
    host.terminal_width(Stream::Stderr)
        .filter(|&cols| (20..=4096).contains(&cols))
        .unwrap_or(100)
}

/// Set the resolved diagnostic style in the host environment for CLI-owned children.
/// Call this before the host captures the child environment.
pub(crate) fn stick_diag_style(host: &dyn Host, style: DiagStyle) {
    host.env_set(b"RCMAKE_DIAGNOSTICS", Some(style.as_bytes()));
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
    fn native_usage_names_rcmake() {
        let usage = "Usage: cmake --build <dir> [options]\nOptions:\n";
        assert_eq!(
            Persona::Native.usage(usage),
            "Usage: rcmake --build <dir> [options]\nOptions:\n"
        );
        assert_eq!(Persona::Compat.usage(usage), usage);
    }

    #[test]
    fn native_stem_obeys_platform_case_rules() {
        for name in ["rcmake", "/shim/rcmake", "rcmake.exe"] {
            assert_eq!(persona_from_argv0(OsStr::new(name)), Persona::Native);
        }
        for name in ["", "cmake", "cpack", "ctest", "rcmake-debug"] {
            assert_eq!(persona_from_argv0(OsStr::new(name)), Persona::Compat);
        }
        let mixed_case = if cfg!(windows) {
            Persona::Native
        } else {
            Persona::Compat
        };
        assert_eq!(persona_from_argv0(OsStr::new("Rcmake.exe")), mixed_case);
    }

    #[test]
    fn style_obeys_persona_flag_environment_and_terminal_precedence() {
        let host = PureHost::default();
        let resolve = |persona, cli| resolve_diag_style(persona, cli, &host);
        assert_eq!(resolve(Persona::Native, None), DiagStyle::Cmake);
        assert_eq!(terminal_style(Persona::Native, || true), DiagStyle::Rich);
        assert_eq!(terminal_style(Persona::Compat, || true), DiagStyle::Cmake);

        host.env_set(b"RCMAKE_DIAGNOSTICS", Some(b"rich"));
        assert_eq!(
            resolve(Persona::Compat, Some(DiagStyle::Cmake)),
            DiagStyle::Cmake
        );
        assert_eq!(
            resolve(Persona::Native, Some(DiagStyle::Cmake)),
            DiagStyle::Cmake
        );
        host.env_set(b"RCMAKE_DIAGNOSTICS", Some(b"RICH"));
        assert_eq!(resolve(Persona::Native, None), DiagStyle::Cmake);
        for style in [DiagStyle::Cmake, DiagStyle::Rich] {
            assert_eq!(DiagStyle::parse(style.as_bytes()), Some(style));
        }
    }

    #[test]
    fn rich_color_obeys_force_and_terminal_precedence() {
        let host = PureHost::default();
        let color = |tty, style| resolve_rich_color(&host, tty, style);
        assert!(!color(false, DiagStyle::Rich));
        assert!(color(true, DiagStyle::Rich));

        host.env_set(b"NO_COLOR", Some(b""));
        assert!(color(true, DiagStyle::Rich));
        host.env_set(b"NO_COLOR", Some(b"1"));
        assert!(!color(true, DiagStyle::Rich));
        host.env_set(b"CLICOLOR_FORCE", Some(b"0"));
        assert!(!color(true, DiagStyle::Rich));
        host.env_set(b"CLICOLOR_FORCE", Some(b"1"));
        assert!(color(false, DiagStyle::Rich));
        assert!(!color(true, DiagStyle::Cmake));
    }

    #[test]
    fn child_style_uses_the_value_written_to_the_host() {
        let host = PureHost::default();
        stick_diag_style(&host, DiagStyle::Rich);
        assert_eq!(
            resolve_diag_style(Persona::Native, None, &host),
            DiagStyle::Rich
        );
    }

    #[test]
    fn rich_width_prefers_valid_columns_and_falls_back_to_default() {
        let host = PureHost::default();
        assert_eq!(rich_width(&host), 100);
        host.env_set(b"COLUMNS", Some(b"120"));
        assert_eq!(rich_width(&host), 120);
        host.env_set(b"COLUMNS", Some(b"10"));
        assert_eq!(rich_width(&host), 100);
        host.env_set(b"COLUMNS", Some(b"not-a-number"));
        assert_eq!(rich_width(&host), 100);
        host.env_set(b"COLUMNS", Some(b"200"));
        assert_eq!(rich_width(&host), 200);
    }
}
