//! Parse options for the Native Rcmake command line.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::{FetchLockMode, HermeticMode};
use rcmake_host::{Host, path::to_native};

use crate::persona::DiagStyle;

use super::split_flag;

/// Selects the output directory for a native profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProfileRequest {
    /// Write to `rcmake-profile` within the build directory.
    BuildDefault,
    /// Write to the specified directory, relative to the startup directory if necessary.
    Directory(PathBuf),
}

impl ProfileRequest {
    /// Read the optional profile directory from `RCMAKE_PROFILE`.
    pub(crate) fn from_env(host: &dyn Host) -> Option<Self> {
        let value = host.env_get(b"RCMAKE_PROFILE")?;
        (!value.is_empty()).then(|| Self::Directory(to_native(value.as_slice())))
    }

    /// Return the absolute path for the profile files.
    pub(crate) fn output_directory(&self, build_dir: &Path, host: &dyn Host) -> PathBuf {
        let path = match self {
            Self::BuildDefault => build_dir.join("rcmake-profile"),
            Self::Directory(path) => path.clone(),
        };
        if path.is_absolute() {
            path
        } else {
            host.initial_cwd().join(path)
        }
    }
}

/// Identifies a subcommand and its arguments in the Native persona.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeSubcommand {
    /// Explain where a value came from.
    Explain(Vec<BString>),
    /// Start the language server.
    Lsp(Vec<BString>),
}

impl NativeSubcommand {
    /// Parse the first positional argument, if it names a Native subcommand.
    pub(crate) fn parse(args: &[BString]) -> Option<Self> {
        let (name, rest) = args.split_first()?;
        match name.as_slice() {
            b"explain" => Some(Self::Explain(rest.to_vec())),
            b"lsp" => Some(Self::Lsp(rest.to_vec())),
            _ => None,
        }
    }
}

/// Identifies an explicit probe cache request from the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeCacheRequest {
    /// Bypass the probe cache.
    Off,
    /// Compare cached probe results with fresh results.
    Verify,
    /// Request probe cache maintenance.
    Clear,
}

/// Collects extension flags for the Native persona.
#[derive(Default)]
pub(crate) struct RcmakeExt {
    /// Diagnostic style requested on the command line.
    pub(crate) diagnostics: Option<DiagStyle>,
    /// Output directory requested on the command line.
    pub(crate) profile: Option<ProfileRequest>,
    /// Subcommand requested on the command line.
    pub(crate) subcommand: Option<NativeSubcommand>,
    /// Probe cache mode requested on the command line.
    pub(crate) probe_cache: Option<ProbeCacheRequest>,
    /// Fetch lock mode requested on the command line.
    pub(crate) fetch_lock: Option<FetchLockMode>,
    /// Hermetic mode requested on the command line.
    pub(crate) hermetic: Option<HermeticMode>,
}

/// Help text for `rcmake --help` in the Native persona.
pub(crate) const NATIVE_EXTENSION_HELP: &str = "\
\nRcmake extensions\n\
  --diagnostics=cmake|rich           Select the diagnostic format.\n\
  --profile[=<dir>]                 Write a configure profile.\n\
  --no-probe-cache                  Bypass the probe cache.\n\
  --probe-cache=verify|clear        Verify or clear the probe cache.\n\
  --fetch-lock=off|auto|update|frozen  Select the fetch lock mode.\n\
  --hermetic[=warn|strict]           Check for undeclared inputs. Omit the value to warn.\n\
  explain, lsp                     Run a Native subcommand.\n\
  Use -S explain or -S lsp to select a source directory with that name.\n";

/// Parse one Native extension flag without consuming another argument.
///
/// Return `Ok(false)` if the flag belongs to another parser. Return an error if
/// the flag is recognized and its value is invalid.
pub(crate) fn parse_native_extension_flag(
    arg: &[u8],
    ext: &mut RcmakeExt,
) -> Result<bool, BString> {
    let (flag, value) = split_flag(arg, b'=');
    match flag {
        b"--diagnostics" => {
            ext.diagnostics =
                Some(value.and_then(DiagStyle::parse).ok_or_else(|| {
                    BString::from("Use --diagnostics=cmake or --diagnostics=rich.")
                })?);
        }
        b"--profile" => {
            ext.profile = Some(match value {
                None => ProfileRequest::BuildDefault,
                Some(b"") => return Err("Specify a directory after --profile=.".into()),
                Some(dir) => ProfileRequest::Directory(to_native(dir)),
            });
        }
        b"--no-probe-cache" if value.is_none() => ext.probe_cache = Some(ProbeCacheRequest::Off),
        b"--no-probe-cache" => return Err("Use --no-probe-cache without a value.".into()),
        b"--probe-cache" => {
            ext.probe_cache = Some(match value {
                Some(b"verify") => ProbeCacheRequest::Verify,
                Some(b"clear") => ProbeCacheRequest::Clear,
                _ => return Err("Use --probe-cache=verify or --probe-cache=clear.".into()),
            });
        }
        b"--fetch-lock" => {
            ext.fetch_lock = Some(match value {
                Some(b"off") => FetchLockMode::Off,
                Some(b"auto") => FetchLockMode::Auto,
                Some(b"update") => FetchLockMode::Update,
                Some(b"frozen") => FetchLockMode::Frozen,
                _ => return Err("Set --fetch-lock to off, auto, update, or frozen.".into()),
            });
        }
        b"--hermetic" => {
            ext.hermetic = Some(match value {
                None | Some(b"warn") => HermeticMode::Warn,
                Some(b"strict") => HermeticMode::Strict,
                _ => return Err("Use --hermetic=warn or --hermetic=strict.".into()),
            });
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn parses_extension_flags_and_keeps_other_flags_available() -> Result<(), BString> {
        let mut ext = RcmakeExt::default();
        for flag in [
            "--diagnostics=rich",
            "--profile",
            "--no-probe-cache",
            "--fetch-lock=frozen",
            "--hermetic",
        ] {
            assert!(parse_native_extension_flag(flag.as_bytes(), &mut ext)?);
        }
        assert!(!parse_native_extension_flag(
            b"--profiling-output=file",
            &mut ext
        )?);
        assert_eq!(ext.diagnostics, Some(DiagStyle::Rich));
        assert_eq!(ext.profile, Some(ProfileRequest::BuildDefault));
        assert_eq!(ext.probe_cache, Some(ProbeCacheRequest::Off));
        assert_eq!(ext.fetch_lock, Some(FetchLockMode::Frozen));
        assert_eq!(ext.hermetic, Some(HermeticMode::Warn));

        assert!(parse_native_extension_flag(b"--profile=data=a", &mut ext)?);
        assert_eq!(
            ext.profile,
            Some(ProfileRequest::Directory("data=a".into()))
        );
        assert!(parse_native_extension_flag(
            b"--probe-cache=verify",
            &mut ext
        )?);
        assert_eq!(ext.probe_cache, Some(ProbeCacheRequest::Verify));
        assert!(parse_native_extension_flag(
            b"--probe-cache=clear",
            &mut ext
        )?);
        assert_eq!(ext.probe_cache, Some(ProbeCacheRequest::Clear));
        Ok(())
    }

    #[test]
    fn rejects_invalid_values() {
        let mut ext = RcmakeExt::default();
        for flag in [
            "--diagnostics",
            "--diagnostics=json",
            "--profile=",
            "--no-probe-cache=1",
            "--probe-cache=off",
            "--fetch-lock",
            "--fetch-lock=typo",
            "--hermetic=",
        ] {
            assert!(parse_native_extension_flag(flag.as_bytes(), &mut ext).is_err());
        }
        assert_eq!(ext.profile, None);
        assert_eq!(ext.probe_cache, None);
    }

    #[test]
    fn resolves_profile_paths_and_matches_only_first_argument_for_subcommands() {
        let host = PureHost::default();
        assert_eq!(ProfileRequest::from_env(&host), None);
        host.env_set(b"RCMAKE_PROFILE", Some(b"reports"));
        assert_eq!(
            ProfileRequest::from_env(&host)
                .map(|request| request.output_directory(Path::new("build"), &host)),
            Some(host.initial_cwd().join("reports")),
        );
        assert_eq!(
            ProfileRequest::BuildDefault.output_directory(Path::new("build"), &host),
            host.initial_cwd().join("build/rcmake-profile"),
        );
        let args = ["explain".into(), "target".into(), "demo".into()];
        assert_eq!(
            NativeSubcommand::parse(&args),
            Some(NativeSubcommand::Explain(vec![
                "target".into(),
                "demo".into()
            ]))
        );
        assert_eq!(
            NativeSubcommand::parse(&["lsp".into()]),
            Some(NativeSubcommand::Lsp(Vec::new()))
        );
        assert_eq!(
            NativeSubcommand::parse(&["-S".into(), "explain".into()]),
            None
        );
    }
}
