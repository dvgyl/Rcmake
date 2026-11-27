//! Command-line argument splitting for `ccmake`, matching CMake 4.5.1 quirks.

use rcmake_base::BString;

/// Result of splitting raw `ccmake` arguments (excluding `argv[0]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedArgs {
    /// Whether `-debug` appeared on the command line (accepted and ignored).
    pub debug: bool,
    /// Cache directory extracted from the last argument with prefix `-B` using
    /// CMake 4.5.1's `arg.substr(2)` rule.
    ///
    /// Note the upstream quirk: a separate `-B build` produces `Some(b"")`
    /// (empty byte string) here, while `cmake_args` retains the full `["-B", "build"]`
    /// pair for the standard CMake argument parser.
    pub raw_b_cache_dir: Option<BString>,
    /// Remaining arguments forwarded to the CMake options parser (with `-debug` removed).
    pub cmake_args: Vec<BString>,
}

/// Split raw `ccmake` command-line arguments (excluding `argv[0]`).
///
/// Matches CMake 4.5.1 `Source/CursesDialog/ccmake.cxx`:
/// - Every exact `-debug` argument is stripped and recorded in [`ParsedArgs::debug`].
/// - The last argument with prefix `-B` records `arg[2..]` in [`ParsedArgs::raw_b_cache_dir`].
/// - All non-`-debug` arguments remain in [`ParsedArgs::cmake_args`] in original order.
#[must_use]
pub fn split_args(args: &[BString]) -> ParsedArgs {
    let mut debug = false;
    let mut cmake_args = Vec::with_capacity(args.len());
    for arg in args {
        if arg.as_slice() == b"-debug" {
            debug = true;
        } else {
            cmake_args.push(arg.clone());
        }
    }

    let mut raw_b_cache_dir = None;
    for arg in &cmake_args {
        if let Some(rest) = arg.strip_prefix(b"-B") {
            raw_b_cache_dir = Some(BString::from(rest));
        }
    }

    ParsedArgs {
        debug,
        raw_b_cache_dir,
        cmake_args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<BString> {
        items.iter().copied().map(BString::from).collect()
    }

    #[test]
    fn strips_debug_flag_and_preserves_remaining_args() {
        let parsed = split_args(&args(&[
            "-debug", "-S", "src", "-Bbuild", "-debug", "-DFOO=1",
        ]));
        assert!(parsed.debug);
        assert_eq!(parsed.raw_b_cache_dir, Some(BString::from("build")));
        assert_eq!(
            parsed.cmake_args,
            args(&["-S", "src", "-Bbuild", "-DFOO=1"])
        );
    }

    #[test]
    fn matches_upstream_separate_b_prefix_quirk() {
        let separate = split_args(&args(&["-S", "src", "-B", "build"]));
        assert!(!separate.debug);
        assert_eq!(separate.raw_b_cache_dir, Some(BString::from("")));
        assert_eq!(separate.cmake_args, args(&["-S", "src", "-B", "build"]));

        let attached_then_separate = split_args(&args(&["-Bfirst", "-B", "second"]));
        assert_eq!(
            attached_then_separate.raw_b_cache_dir,
            Some(BString::from(""))
        );

        let separate_then_attached = split_args(&args(&["-B", "first", "-Bsecond"]));
        assert_eq!(
            separate_then_attached.raw_b_cache_dir,
            Some(BString::from("second"))
        );
    }

    #[test]
    fn no_b_prefix_yields_none() {
        let parsed = split_args(&args(&["."]));
        assert!(!parsed.debug);
        assert_eq!(parsed.raw_b_cache_dir, None);
        assert_eq!(parsed.cmake_args, args(&["."]));
    }
}
