use std::sync::Mutex;
use std::time::Duration;

use rcmake_host::Host;
use rcmake_store::remote::{RemoteAccess, RemoteCacheConfig, RemoteStoreClient, local_probe_pin};
use rcmake_store::{Store, StoreConfig, VerifyOutcome, verify_trace};

use super::super::cache::{CachedProbe, ObservedOutcome, decode_outcome, verify_remote_payload};
use super::super::plan::{ProbeRecipe, compute_probe_key};

pub(super) struct RemoteProbe {
    config: RemoteCacheConfig,
    time_left: Mutex<Duration>,
}

impl RemoteProbe {
    pub(super) fn new(host: &dyn Host) -> Option<Self> {
        if !host.supports_process_execution()
            || [b"RCMAKE_FETCH_OFFLINE".as_slice(), b"RCMAKE_OFFLINE"]
                .iter()
                .any(|key| host.env_get(key).is_some_and(|value| value == "1"))
        {
            return None;
        }
        let config = RemoteCacheConfig::from_host(host).ok()??;
        let time_left = Mutex::new(config.session_timeout);
        Some(Self { config, time_left })
    }

    pub(super) fn get(
        &self,
        host: &dyn Host,
        config: &StoreConfig,
        recipe: &ProbeRecipe,
    ) -> Option<ObservedOutcome> {
        let store = Store::new(host, config.clone()).ok()?;
        let key = compute_probe_key(recipe);
        let (pin, local) = local_probe_pin::<CachedProbe>(&store, key).ok()??;
        if !verify_remote_payload(host, recipe, &local) {
            return None;
        }
        self.with_client(host, |client| {
            let entry = client
                .get_probe::<CachedProbe>(&store, key, Some(&pin), &local.trace, |entry| {
                    verify_remote_payload(host, recipe, entry)
                })
                .ok()??;
            if verify_trace(host, &entry.trace) != VerifyOutcome::Hit {
                return None;
            }
            decode_outcome(host, &store, recipe, entry)
        })
    }

    pub(super) fn put(&self, host: &dyn Host, config: &StoreConfig, recipe: &ProbeRecipe) {
        if self.config.access != RemoteAccess::ReadWrite || !self.config.export_captured_output {
            return;
        }
        let Some(store) = Store::new(host, config.clone()).ok() else {
            return;
        };
        let key = compute_probe_key(recipe);
        // The foreground probe cache already read back the local frame and artifact.
        self.with_client(host, |client| client.put_probe(&store, key).ok());
    }

    fn with_client<T>(
        &self,
        host: &dyn Host,
        operation: impl FnOnce(&mut RemoteStoreClient<'_>) -> Option<T>,
    ) -> Option<T> {
        let Ok(mut left) = self.time_left.lock() else {
            return None;
        };
        if left.is_zero() {
            return None;
        }
        let mut config = self.config.clone();
        config.session_timeout = *left;
        config.export_timeout = config.export_timeout.min(*left);
        let mut client = RemoteStoreClient::new(host, config).ok()?;
        let start = host.monotonic();
        let result = operation(&mut client);
        *left = left.saturating_sub(host.monotonic().saturating_sub(start));
        if client.take_diagnostic().is_some() {
            *left = Duration::ZERO;
        }
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rcmake_host::{HostError, HostResult, PureHost};
    use rcmake_store::StoreMode;

    use super::*;
    use crate::configure::try_compile::{
        cache::{ProbeCache, tests::recipe},
        journal::{JournalHost, JournalMode},
    };

    #[test]
    fn requires_a_local_pin_and_both_upload_settings() -> HostResult<()> {
        let host = PureHost::default();
        let curl = Path::new("/usr/bin/curl");
        host.seed_file(curl, b"curl fixture")?;
        host.set_permissions(curl, 0o755)?;
        let runs = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&runs);
        host.set_command_handler(move |_, _, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            Err(HostError::unsupported("Remote service unavailable."))
        })?;
        host.env_set(b"RCMAKE_CACHE_REMOTE", Some(b"http://localhost"));
        assert!(RemoteProbe::new(&host).is_none());
        host.env_set(b"RCMAKE_CACHE_REMOTE", Some(b"https://cache.example"));
        host.env_set(b"RCMAKE_CACHE_REMOTE_WRITE", Some(b"1"));
        let write_only = RemoteProbe::new(&host)
            .ok_or_else(|| HostError::unsupported("Expected cache settings."))?;
        host.env_set(b"RCMAKE_CACHE_REMOTE_WRITE", None);
        host.env_set(b"RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT", Some(b"1"));
        let export_only = RemoteProbe::new(&host)
            .ok_or_else(|| HostError::unsupported("Expected cache settings."))?;
        host.env_set(b"RCMAKE_CACHE_REMOTE_WRITE", Some(b"1"));
        let enabled = RemoteProbe::new(&host)
            .ok_or_else(|| HostError::unsupported("Expected cache settings."))?;
        host.env_set(b"RCMAKE_FETCH_OFFLINE", Some(b"1"));
        assert!(RemoteProbe::new(&host).is_none());
        host.env_set(b"RCMAKE_FETCH_OFFLINE", None);

        let root = Path::new("/work");
        let scratch = root.join("build/scratch");
        host.create_dir_all(&scratch)?;
        let config = StoreConfig::new("/cache".into());
        let recipe = recipe(&host, root)?;
        let cache = ProbeCache::new(StoreMode::ReadWrite);
        assert!(enabled.get(&host, &config, &recipe).is_none());
        let journal = JournalHost::new(&host, &scratch, JournalMode::Observe, None)?.finish();
        let outcome = ObservedOutcome {
            compiled: true,
            exit_code: 0,
            output: b"probe output".to_vec(),
            artifact: None,
            diagnostics: Vec::new(),
        };
        assert!(
            cache
                .publish(&host, &config, &recipe, &outcome, &journal)
                .available()
        );
        write_only.put(&host, &config, &recipe);
        export_only.put(&host, &config, &recipe);
        assert_eq!(runs.load(Ordering::Relaxed), 0);
        enabled.put(&host, &config, &recipe);
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert_eq!(cache.lookup(&host, &config, &recipe), Some(outcome));
        Ok(())
    }
}
