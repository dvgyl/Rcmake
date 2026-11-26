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

/// Collects extension flags for the Native persona.
#[derive(Default)]
pub(crate) struct RcmakeExt {
    /// Diagnostic style requested on the command line.
    pub(crate) diagnostics: Option<DiagStyle>,
    /// Output directory requested on the command line.
    pub(crate) profile: Option<ProfileRequest>,
    /// Positional path immediately following a bare `--profile`, for typo hints.
    pub(crate) bare_profile_arg: Option<PathBuf>,
    /// Bypass the probe cache for this invocation.
    pub(crate) no_probe_cache: bool,
    /// Fetch lock mode requested on the command line.
    pub(crate) fetch_lock: Option<FetchLockMode>,
    /// Hermetic mode requested on the command line.
    pub(crate) hermetic: Option<HermeticMode>,
}

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
        b"--diagnostic" => {
            let hint = match value.and_then(DiagStyle::parse) {
                Some(DiagStyle::Rich) => "Use --diagnostics=rich.",
                Some(DiagStyle::Cmake) => "Use --diagnostics=cmake.",
                None => "Use --diagnostics=cmake or --diagnostics=rich.",
            };
            let raw = String::from_utf8_lossy(arg);
            return Err(format!("Unknown argument {raw}. {hint}").into());
        }
        b"--diagnostics" => {
            ext.diagnostics =
                Some(value.and_then(DiagStyle::parse).ok_or_else(|| {
                    BString::from("Use --diagnostics=cmake or --diagnostics=rich.")
                })?);
        }
        b"--profile" => {
            ext.profile = Some(match value {
                None => ProfileRequest::BuildDefault,
                Some(b"") => {
                    return Err(
                        "Specify a profile directory. For example, use --profile=dir.".into(),
                    );
                }
                Some(dir) => ProfileRequest::Directory(to_native(dir)),
            });
        }
        b"--no-probe-cache" if value.is_none() => ext.no_probe_cache = true,
        b"--no-probe-cache" => return Err("Use --no-probe-cache without a value.".into()),
        b"--fetch-lock" => {
            ext.fetch_lock = Some(match value {
                Some(b"off") => FetchLockMode::Off,
                Some(b"auto") => FetchLockMode::Auto,
                Some(b"update") => FetchLockMode::Update,
                Some(b"frozen") => FetchLockMode::Frozen,
                Some(value) => {
                    let value = String::from_utf8_lossy(value);
                    return Err(format!(
                        "Invalid fetch lock mode {value:?}. Valid modes are off, auto, update, and frozen. For example, use --fetch-lock=frozen."
                    ).into());
                }
                None => {
                    return Err("Specify off, auto, update, or frozen after --fetch-lock=. For example, use --fetch-lock=frozen.".into());
                }
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
        assert!(ext.no_probe_cache);
        assert_eq!(ext.fetch_lock, Some(FetchLockMode::Frozen));
        assert_eq!(ext.hermetic, Some(HermeticMode::Warn));

        assert!(parse_native_extension_flag(b"--profile=data=a", &mut ext)?);
        assert_eq!(
            ext.profile,
            Some(ProfileRequest::Directory("data=a".into()))
        );
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
            "--fetch-lock",
            "--fetch-lock=typo",
            "--hermetic=",
        ] {
            assert!(parse_native_extension_flag(flag.as_bytes(), &mut ext).is_err());
        }
        assert_eq!(ext.profile, None);
        assert!(!ext.no_probe_cache);
    }

    #[test]
    fn reports_native_option_corrections() {
        let mut ext = RcmakeExt::default();
        for (flag, hint) in [
            ("--diagnostic=rich", "--diagnostics=rich"),
            ("--profile=", "--profile=dir"),
            ("--fetch-lock=bad", "--fetch-lock=frozen"),
        ] {
            let result = parse_native_extension_flag(flag.as_bytes(), &mut ext);
            assert!(matches!(result, Err(message) if message.to_string().contains(hint)));
        }
        let result = parse_native_extension_flag(b"--diagnostic=rich", &mut ext);
        assert!(matches!(
            result,
            Err(message) if message.to_string().contains("Unknown argument --diagnostic=rich.")
        ));
        let result = parse_native_extension_flag(b"--fetch-lock=bad", &mut ext);
        assert!(matches!(result, Err(message) if message.to_string().contains("\"bad\"")));
    }

    #[test]
    fn leaves_reserved_extensions_inactive() -> Result<(), BString> {
        let mut ext = RcmakeExt::default();
        for name in [
            "--probe-cache=verify",
            "--probe-cache=clear",
            "explain",
            "lsp",
        ] {
            assert!(!parse_native_extension_flag(name.as_bytes(), &mut ext)?);
        }
        assert!(!ext.no_probe_cache);
        Ok(())
    }

    #[test]
    fn resolves_profile_paths() {
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
    }
}
