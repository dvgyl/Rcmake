use std::num::NonZeroUsize;

use rcmake_eval::{FetchLockMode, HermeticMode, ProbeCacheMode, SpeedOptions};
use rcmake_host::Host;

use crate::options::{CacheEdit, Options};
use crate::persona::Persona;

pub(crate) fn settings(host: &dyn Host, opts: &Options, profiled: bool) -> (SpeedOptions, bool) {
    if opts.persona != Persona::Native || opts.view {
        return (SpeedOptions::default(), false);
    }

    let env = |key: &[u8]| host.env_get(key);
    let enabled = |key: &[u8]| env(key).is_some_and(|value| value == b"1");
    let no_probe_cache = env_mode(
        host,
        "RCMAKE_NO_PROBE_CACHE",
        false,
        true,
        |value| match value {
            b"0" => Some(false),
            b"1" => Some(true),
            _ => None,
        },
    );
    let probe_cache = env_mode(
        host,
        "RCMAKE_PROBE_CACHE",
        ProbeCacheMode::ReadWrite,
        ProbeCacheMode::Off,
        |value| match value {
            b"off" | b"0" => Some(ProbeCacheMode::Off),
            b"on" | b"1" | b"readwrite" => Some(ProbeCacheMode::ReadWrite),
            b"readonly" => Some(ProbeCacheMode::ReadOnly),
            b"verify" => Some(ProbeCacheMode::Verify),
            _ => None,
        },
    );
    let probe_cache = if opts.extensions.no_probe_cache || no_probe_cache {
        ProbeCacheMode::Off
    } else {
        probe_cache
    };
    let fetch_lock = env_mode(
        host,
        "RCMAKE_FETCH_LOCK",
        FetchLockMode::Auto,
        FetchLockMode::Off,
        |value| match value {
            b"off" => Some(FetchLockMode::Off),
            b"auto" => Some(FetchLockMode::Auto),
            b"update" => Some(FetchLockMode::Update),
            b"frozen" => Some(FetchLockMode::Frozen),
            _ => None,
        },
    );
    let fetch_lock = opts.extensions.fetch_lock.unwrap_or(fetch_lock);
    let hermetic = env_mode(
        host,
        "RCMAKE_HERMETIC",
        HermeticMode::Off,
        HermeticMode::Off,
        |value| match value {
            b"off" | b"0" => Some(HermeticMode::Off),
            b"1" | b"warn" => Some(HermeticMode::Warn),
            b"strict" => Some(HermeticMode::Strict),
            _ => None,
        },
    );
    let hermetic = opts.extensions.hermetic.unwrap_or(hermetic);

    // The runtime reads path overrides, offline mode and allowlists from this host.
    let fetch_prefetch = fetch_lock != FetchLockMode::Off
        && hermetic == HermeticMode::Off
        && !enabled(b"RCMAKE_FETCH_OFFLINE")
        && !env(b"RCMAKE_FETCH_PREFETCH").is_some_and(|value| value == b"0");
    let setup_gates = setup_replay_disabled(host);
    let incremental = !profiled
        && !opts.fresh
        && opts.trace.is_none()
        && (!setup_gates || opts.preset.is_none())
        && !opts.check_system_vars
        && !opts.log_context
        && !opts.debug_output
        && !opts.debug_find
        && opts.debug_find_variables.is_empty()
        && opts.debug_find_packages.is_empty()
        && !(setup_gates
            && opts.cache_edits.iter().any(|edit| match edit {
                CacheEdit::Preload(_) => true,
                CacheEdit::Define(define) => define.name == b"CMAKE_TOOLCHAIN_FILE",
                CacheEdit::Remove(_) => false,
            }))
        && !enabled(b"RCMAKE_NO_INCREMENTAL");

    (
        SpeedOptions {
            probe_cache,
            probe_jobs: jobs(host, "RCMAKE_PROBE_JOBS"),
            incremental,
            fetch_lock,
            fetch_prefetch,
            fetch_jobs: jobs(host, "RCMAKE_FETCH_JOBS"),
            hermetic,
        },
        true,
    )
}

/// Return `true` if `RCMAKE_REPLAY_SETUP` is exactly `0`. Then configure replay is off
/// for toolchain files, presets, `-C` scripts, and `PreLoad.cmake` files.
pub(crate) fn setup_replay_disabled(host: &dyn Host) -> bool {
    host.env_get(b"RCMAKE_REPLAY_SETUP")
        .is_some_and(|value| value == b"0")
}

fn env_mode<T>(
    host: &dyn Host,
    key: &str,
    default: T,
    invalid: T,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> T {
    let Some(value) = host.env_get(key.as_bytes()) else {
        return default;
    };
    parse(&value).unwrap_or_else(|| {
        let message = format!("rcmake warning: Unrecognized value for {key}.\n");
        host.write_out(rcmake_host::Stream::Stderr, message.as_bytes());
        invalid
    })
}

fn jobs(host: &dyn Host, key: &str) -> Option<NonZeroUsize> {
    env_mode(host, key, None, None, |value| {
        std::str::from_utf8(value).ok()?.parse().ok().map(Some)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::{PureHost, Stream};

    #[test]
    fn resolves_native_speed_modes_and_keeps_compat_stateless() {
        let host = PureHost::default();
        let mut opts = Options {
            persona: Persona::Native,
            ..Options::default()
        };
        let (speed, persistent) = settings(&host, &opts, false);
        assert!(persistent && speed.incremental && speed.fetch_prefetch);
        assert_eq!(speed.probe_cache, ProbeCacheMode::ReadWrite);
        assert_eq!(speed.fetch_lock, FetchLockMode::Auto);
        assert_eq!(speed.hermetic, HermeticMode::Off);

        host.env_set(b"RCMAKE_PROBE_CACHE", Some(b"readonly"));
        host.env_set(b"RCMAKE_PROBE_JOBS", Some(b"1"));
        host.env_set(b"RCMAKE_FETCH_LOCK", Some(b"frozen"));
        host.env_set(b"RCMAKE_FETCH_JOBS", Some(b"4"));
        host.env_set(b"RCMAKE_FETCH_PREFETCH", Some(b"0"));
        host.env_set(b"RCMAKE_HERMETIC", Some(b"warn"));
        opts.extensions.hermetic = Some(HermeticMode::Strict);
        let (speed, _) = settings(&host, &opts, true);
        assert_eq!(speed.probe_cache, ProbeCacheMode::ReadOnly);
        assert_eq!(speed.probe_jobs, NonZeroUsize::new(1));
        assert_eq!(speed.fetch_lock, FetchLockMode::Frozen);
        assert_eq!(speed.fetch_jobs, NonZeroUsize::new(4));
        assert_eq!(speed.hermetic, HermeticMode::Strict);
        assert!(!speed.incremental && !speed.fetch_prefetch);

        opts.extensions.no_probe_cache = true;
        opts.extensions.fetch_lock = Some(FetchLockMode::Off);
        let (speed, _) = settings(&host, &opts, false);
        assert_eq!(speed.probe_cache, ProbeCacheMode::Off);
        assert_eq!(speed.fetch_lock, FetchLockMode::Off);
        opts.persona = Persona::Compat;
        let (speed, persistent) = settings(&host, &opts, false);
        assert!(!persistent && !speed.incremental && !speed.fetch_prefetch);
        assert!(speed.probe_jobs.is_none() && speed.fetch_jobs.is_none());
        assert_eq!(speed.probe_cache, ProbeCacheMode::Off);
        assert_eq!(speed.fetch_lock, FetchLockMode::Off);
        assert_eq!(speed.hermetic, HermeticMode::Off);
    }

    #[test]
    fn invalid_settings_warn_and_disable_probe_cache() {
        let host = PureHost::default();
        let opts = Options {
            persona: Persona::Native,
            ..Options::default()
        };
        let invalid: [(&[u8], &[u8]); 5] = [
            (b"RCMAKE_PROBE_CACHE", b"false"),
            (b"RCMAKE_NO_PROBE_CACHE", b"true"),
            (b"RCMAKE_HERMETIC", b"on"),
            (b"RCMAKE_FETCH_LOCK", b"bogus"),
            (b"RCMAKE_PROBE_JOBS", b" 4"),
        ];
        for (key, value) in invalid {
            host.env_set(key, Some(value));
        }
        let (speed, _) = settings(&host, &opts, false);
        assert_eq!(speed.probe_cache, ProbeCacheMode::Off);
        assert_eq!(speed.fetch_lock, FetchLockMode::Off);
        assert_eq!(speed.hermetic, HermeticMode::Off);
        assert!(speed.probe_jobs.is_none());
        let stderr = host.stream_bytes(Stream::Stderr);
        for (key, _) in invalid {
            assert_eq!(
                stderr
                    .windows(key.len())
                    .filter(|window| *window == key)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn setup_inputs_keep_replay_unless_the_setup_opt_out_is_set() {
        let host = PureHost::default();
        let mut opts = Options {
            persona: Persona::Native,
            preset: Some("dev".into()),
            ..Options::default()
        };
        opts.cache_edits = vec![
            CacheEdit::Preload("init.cmake".into()),
            CacheEdit::Define(rcmake_eval::CacheDefine {
                name: "CMAKE_TOOLCHAIN_FILE".into(),
                ty: None,
                value: "tc.cmake".into(),
            }),
        ];
        assert!(settings(&host, &opts, false).0.incremental);
        // Only the exact value `0` keeps the earlier gates.
        host.env_set(b"RCMAKE_REPLAY_SETUP", Some(b"1"));
        assert!(settings(&host, &opts, false).0.incremental);
        host.env_set(b"RCMAKE_REPLAY_SETUP", Some(b"0"));
        assert!(!settings(&host, &opts, false).0.incremental);
        opts.preset = None;
        opts.cache_edits.clear();
        assert!(settings(&host, &opts, false).0.incremental);
        host.env_set(b"RCMAKE_NO_INCREMENTAL", Some(b"1"));
        assert!(!settings(&host, &opts, false).0.incremental);
    }

    #[test]
    fn diagnostic_format_keeps_incremental_evaluation_enabled() {
        let host = PureHost::default();
        for style in [
            crate::persona::DiagStyle::Cmake,
            crate::persona::DiagStyle::Rich,
        ] {
            let mut opts = Options {
                persona: Persona::Native,
                ..Options::default()
            };
            opts.extensions.diagnostics = Some(style);
            assert!(settings(&host, &opts, false).0.incremental);
        }
    }
}
