//! Unified multi-call CLI driver for `rcmake` (`cmake`, `ctest`, `cpack` dispatch via `argv[0]`).

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use rcmake_base::{BString, CMAKE_VERSION_STR};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, RenderOptions, render};
use rcmake_host::{Host, Stream};

/// Active tool persona selected from the `argv[0]` basename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `cmake` (or default `rcmake`).
    Cmake,
    /// `ctest`.
    Ctest,
    /// `cpack`.
    Cpack,
}

impl Tool {
    /// Canonical lowercase executable name printed in `--version`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Ctest => "ctest",
            Self::Cpack => "cpack",
        }
    }
}

/// Determine the [`Tool`] persona from the `argv[0]` basename (not the resolved symlink target).
#[must_use]
pub fn tool_from_argv0(argv0: &OsStr) -> Tool {
    let base = Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cmake");
    if base.eq_ignore_ascii_case("ctest") {
        Tool::Ctest
    } else if base.eq_ignore_ascii_case("cpack") {
        Tool::Cpack
    } else {
        Tool::Cmake
    }
}

/// Run the Rcmake CLI with `argv` (including `argv[0]`) against `host`, returning the process exit code.
#[must_use]
pub fn run(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    let tool = argv
        .first()
        .map(|a| {
            let p = rcmake_host::path::to_native(a.as_slice());
            tool_from_argv0(p.as_os_str())
        })
        .unwrap_or(Tool::Cmake);

    let rest = argv.get(1..).unwrap_or(&[]);
    if let Some(first) = rest.first()
        && (first.as_slice() == b"--version" || first.as_slice() == b"-version")
    {
        let banner = format!(
            "{} version {CMAKE_VERSION_STR}\n\nRcmake native Rust drop-in replacement for CMake {CMAKE_VERSION_STR}.\n",
            tool.name()
        );
        host.write_out(Stream::Stdout, banner.as_bytes());
        return 0;
    }

    let diag = Diagnostic {
        kind: MessageKind::Error,
        effect: Effect::Fatal,
        category: None,
        policy: None,
        location: Location::None,
        body: BString::from(
            "Requested CLI mode is not yet implemented (scheduled for Milestone 1)",
        ),
    };
    let rendered = render(&diag, &RenderOptions::default());
    host.write_out(Stream::Stderr, rendered.as_slice());
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn argv0_dispatch_and_version_banner() {
        assert_eq!(tool_from_argv0(OsStr::new("/tmp/bin/cmake")), Tool::Cmake);
        assert_eq!(tool_from_argv0(OsStr::new("ctest")), Tool::Ctest);
        assert_eq!(tool_from_argv0(OsStr::new("/opt/cpack")), Tool::Cpack);
        assert_eq!(tool_from_argv0(OsStr::new("rcmake")), Tool::Cmake);

        let host = Arc::new(PureHost::default());
        let code = run(
            &[BString::from("cmake"), BString::from("--version")],
            host.clone(),
        );
        assert_eq!(code, 0);
        let out = host.stream_bytes(Stream::Stdout);
        assert!(out.starts_with(b"cmake version 4.5.1\n\n"));
    }
}
