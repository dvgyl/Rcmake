use std::num::NonZeroUsize;

use rcmake_eval::{FetchLockMode, HermeticMode, ProbeCacheMode, SpeedOptions};
use rcmake_host::Host;

use crate::options::{CacheEdit, Options};
use crate::persona::{Persona, oracle_mode};

pub(super) fn settings(host: &dyn Host, opts: &Options, profiled: bool) -> (SpeedOptions, bool) {
    if opts.persona != Persona::Native || oracle_mode(host) || opts.view {
        return (SpeedOptions::default(), false);
    }

    let env = |key: &[u8]| host.env_get(key);
    let enabled = |key: &[u8]| env(key).is_some_and(|value| value == b"1");
    let jobs =
        |key: &[u8]| -> Option<NonZeroUsize> { std::str::from_utf8(&env(key)?).ok()?.parse().ok() };
    let probe_cache = if opts.extensions.no_probe_cache || enabled(b"RCMAKE_NO_PROBE_CACHE") {
        ProbeCacheMode::Off
    } else {
        match env(b"RCMAKE_PROBE_CACHE")
            .as_ref()
            .map(|value| value.as_slice())
        {
            Some(b"off" | b"0") => ProbeCacheMode::Off,
            Some(b"readonly") => ProbeCacheMode::ReadOnly,
            Some(b"verify") => ProbeCacheMode::Verify,
            _ => ProbeCacheMode::ReadWrite,
        }
    };
    let fetch_lock = opts.extensions.fetch_lock.unwrap_or_else(|| {
        match env(b"RCMAKE_FETCH_LOCK")
            .as_ref()
            .map(|value| value.as_slice())
        {
            Some(b"off") => FetchLockMode::Off,
            Some(b"update") => FetchLockMode::Update,
            Some(b"frozen") => FetchLockMode::Frozen,
            _ => FetchLockMode::Auto,
        }
    });
    let hermetic = opts.extensions.hermetic.unwrap_or_else(|| {
        match env(b"RCMAKE_HERMETIC")
            .as_ref()
            .map(|value| value.as_slice())
        {
            Some(b"1" | b"warn") => HermeticMode::Warn,
            Some(b"strict") => HermeticMode::Strict,
            _ => HermeticMode::Off,
        }
    });

    // The runtime reads path overrides, offline mode and allowlists from this host.
    let fetch_prefetch = fetch_lock != FetchLockMode::Off
        && hermetic == HermeticMode::Off
        && !enabled(b"RCMAKE_FETCH_OFFLINE")
        && !env(b"RCMAKE_FETCH_PREFETCH").is_some_and(|value| value == b"0");
    let incremental = !profiled
        && !opts.fresh
        && opts.trace.is_none()
        && opts.preset.is_none()
        && opts.extensions.diagnostics.is_none()
        && !opts.cache_edits.iter().any(|edit| match edit {
            CacheEdit::Preload(_) => true,
            CacheEdit::Define(define) => define.name == b"CMAKE_TOOLCHAIN_FILE",
            CacheEdit::Remove(_) => false,
        })
        && !enabled(b"RCMAKE_NO_INCREMENTAL");

    (
        SpeedOptions {
            probe_cache,
            probe_jobs: jobs(b"RCMAKE_PROBE_JOBS"),
            incremental,
            fetch_lock,
            fetch_prefetch,
            fetch_jobs: jobs(b"RCMAKE_FETCH_JOBS"),
            hermetic,
        },
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

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
        for persona in [Persona::Compat, Persona::Native] {
            if persona == Persona::Native {
                host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
            }
            opts.persona = persona;
            let (speed, persistent) = settings(&host, &opts, false);
            assert!(!persistent && !speed.incremental && !speed.fetch_prefetch);
            assert!(speed.probe_jobs.is_none() && speed.fetch_jobs.is_none());
            assert_eq!(speed.probe_cache, ProbeCacheMode::Off);
            assert_eq!(speed.fetch_lock, FetchLockMode::Off);
            assert_eq!(speed.hermetic, HermeticMode::Off);
        }
    }
}
