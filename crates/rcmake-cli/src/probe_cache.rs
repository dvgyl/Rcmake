use rcmake_base::BString;
use rcmake_eval::ProbeCacheMode;
use rcmake_host::{Host, HostResult, Stream, discover_cmake_root, path};
use rcmake_store::{Store, StoreConfig, StoreMode};

use crate::{configure::speed, options::Options, persona::Persona};

fn open(host: &dyn Host) -> HostResult<Store<'_>> {
    let mut config = StoreConfig::from_host(host);
    if let Some(root) = host
        .env_get(b"RCMAKE_PROBE_CACHE_DIR")
        .filter(|value| !value.is_empty())
    {
        config.root = host.initial_cwd().join(path::to_native(&root));
        if config.root.is_absolute() {
            config.mode = StoreMode::ReadWrite;
        }
    }
    Store::new(host, config)
}

pub(crate) fn cache(args: &[BString], host: &dyn Host) -> i32 {
    if let [flag] | [_, flag] = args
        && is_help(flag)
    {
        let help: &[u8] = match args {
            [_] => {
                b"Usage: rcmake cache [status|dir|clear]\n\
                status  Show cache size and entry counts (default).\n\
                dir     Print the cache directory.\n\
                clear   Remove cache entries and blobs.\n"
            }
            [action, _] if action == b"status" => {
                b"Usage: rcmake cache status\nShow cache size and entry counts.\n"
            }
            [action, _] if action == b"dir" => {
                b"Usage: rcmake cache dir\nPrint the cache directory.\n"
            }
            [action, _] if action == b"clear" => {
                b"Usage: rcmake cache clear\nRemove cache entries and blobs.\n"
            }
            _ => b"",
        };
        if !help.is_empty() {
            host.write_out(Stream::Stdout, help);
            host.write_out(
                Stream::Stdout,
                b"Set RCMAKE_CACHE_DIR to select the cache directory.\n",
            );
            return 0;
        }
    }
    let action = match args {
        [] => b"status".as_slice(),
        [action] if matches!(action.as_slice(), b"dir" | b"status" | b"clear") => action,
        _ => {
            host.write_out(Stream::Stderr, b"Usage: rcmake cache [status|dir|clear]\n");
            return 2;
        }
    };
    let result = open(host).and_then(|store| {
        let root = store.root();
        if root.as_os_str().is_empty() {
            return Ok("No cache directory is configured.\n".to_owned());
        }
        let root = path::from_native_cmake(root);
        match action {
            b"dir" => Ok(format!("{root}\n")),
            b"clear" => {
                let report = store.clear()?;
                Ok(format!(
                    "Removed {} entries and {} blobs. Reclaimed {} bytes.\n",
                    report.entries_removed, report.blobs_removed, report.bytes_removed
                ))
            }
            _ => Ok(format!("Cache directory: {root}\n{}", inventory(&store)?)),
        }
    });
    match result {
        Ok(text) => {
            host.write_out(Stream::Stdout, text.as_bytes());
            0
        }
        Err(error) => {
            host.write_out(
                Stream::Stderr,
                format!("rcmake cache: {error}\n").as_bytes(),
            );
            1
        }
    }
}

fn is_help(flag: &[u8]) -> bool {
    matches!(flag, b"-h" | b"--help")
}

fn inventory(store: &Store<'_>) -> HostResult<String> {
    let stats = store.stats()?;
    Ok(format!(
        "Size: {} bytes\nEntries: {}\nBlobs: {}\n",
        stats.bytes, stats.entries, stats.blobs
    ))
}

pub(crate) fn doctor(argv0: Option<&BString>, args: &[BString], host: &dyn Host) -> i32 {
    use std::fmt::Write as _;

    if let [flag] = args
        && is_help(flag)
    {
        host.write_out(
            Stream::Stdout,
            b"Usage: rcmake doctor\nShow the CMake root, available tools, and probe cache status.\n",
        );
        return 0;
    }
    if !args.is_empty() {
        host.write_out(Stream::Stderr, b"Usage: rcmake doctor\n");
        return 2;
    }

    let (_, root) = discover_cmake_root(host, argv0);
    let root = root
        .as_deref()
        .map_or_else(|| "Unavailable".into(), path::from_native_cmake);
    let mut text = format!(
        "rcmake {}\nPersona: native\nCMAKE_ROOT: {root}\n",
        env!("CARGO_PKG_VERSION"),
    );

    let opts = Options {
        persona: Persona::Native,
        ..Options::default()
    };
    let mode = match speed::settings(host, &opts, false).0.probe_cache {
        ProbeCacheMode::Off => "off",
        ProbeCacheMode::ReadWrite => "read-write",
        ProbeCacheMode::ReadOnly => "read-only",
        ProbeCacheMode::Verify => "verify",
    };
    match open(host) {
        Ok(store) if !store.root().as_os_str().is_empty() => {
            let directory = path::from_native_cmake(store.root());
            let _ = writeln!(
                text,
                "Probe cache directory: {directory}\nProbe cache mode: {mode}"
            );
            match inventory(&store) {
                Ok(stats) => text.push_str(&stats),
                Err(error) => {
                    let _ = writeln!(text, "Probe cache status: {error}");
                }
            }
            #[cfg(any(unix, windows))]
            if mode != "off" {
                for warning in [
                    rcmake_commands::sysctl_path_warning(host),
                    rcmake_commands::compiler_reuse_warning(host, false),
                    rcmake_commands::compiler_reuse_warning(host, true),
                ]
                .into_iter()
                .flatten()
                {
                    let _ = writeln!(text, "Warning: {warning}");
                }
            }
        }
        Ok(_) => text.push_str("Probe cache directory: Unavailable\nProbe cache mode: off\n"),
        Err(error) => {
            let _ = writeln!(text, "Probe cache status: {error}");
        }
    }

    for tool in ["cc", "c++", "clang", "gcc", "ninja", "make"] {
        let found = path::executable_on_absolute_path(host, tool);
        let location = found
            .as_deref()
            .map_or_else(|| "Unavailable".into(), path::from_native_cmake);
        let _ = writeln!(text, "{tool}: {location}");
    }

    let mut overrides: Vec<_> = host
        .env_all()
        .into_iter()
        .filter_map(|(name, _)| name.starts_with(b"RCMAKE_").then_some(name))
        .collect();
    overrides.sort_unstable();
    let _ = writeln!(
        text,
        "RCMAKE_* overrides: {}",
        if overrides.is_empty() {
            "None".to_owned()
        } else {
            overrides
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    host.write_out(Stream::Stdout, text.as_bytes());
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::{Follow, PureHost};

    #[test]
    fn native_subcommand_help_uses_stdout_without_accessing_the_store() {
        for flag in ["-h", "--help"] {
            for action in [None, Some("dir"), Some("status"), Some("clear")] {
                let host = PureHost::default();
                let store = host.initial_cwd().join("missing-store");
                host.env_set(b"RCMAKE_CACHE_DIR", Some(&path::from_native(&store)));
                let args = action
                    .into_iter()
                    .chain([flag])
                    .map(BString::from)
                    .collect::<Vec<_>>();
                assert_eq!(cache(&args, &host), 0);
                assert!(
                    host.stream_bytes(Stream::Stdout)
                        .starts_with(b"Usage: rcmake cache")
                );
                assert!(host.stream_bytes(Stream::Stderr).is_empty());
                assert!(host.metadata(&store, Follow::Yes).is_err());
            }
            let host = PureHost::default();
            assert_eq!(doctor(None, &[flag.into()], &host), 0);
            assert!(
                host.stream_bytes(Stream::Stdout)
                    .starts_with(b"Usage: rcmake doctor")
            );
            assert!(host.stream_bytes(Stream::Stderr).is_empty());
        }
    }

    #[test]
    fn unknown_cache_action_with_help_is_a_usage_error() {
        let host = PureHost::default();
        assert_eq!(cache(&["unknown".into(), "--help".into()], &host), 2);
        assert!(host.stream_bytes(Stream::Stdout).is_empty());
        assert!(
            host.stream_bytes(Stream::Stderr)
                .starts_with(b"Usage: rcmake cache")
        );
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn doctor_skips_reuse_warnings_when_probe_cache_is_off() -> HostResult<()> {
        for (has_store, cache_off, expects_warning) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let host = PureHost::default();
            let mut system = host.system();
            system.sysname = "Darwin".into();
            host.set_system(system)?;
            if has_store {
                let directory = host.initial_cwd().join("cache");
                host.env_set(b"RCMAKE_CACHE_DIR", Some(&path::from_native(&directory)));
            }
            if cache_off {
                host.env_set(b"RCMAKE_PROBE_CACHE", Some(b"off"));
            }
            assert_eq!(doctor(None, &[], &host), 0);
            let output = host.stream_bytes(Stream::Stdout).to_string();
            assert_eq!(
                output.contains("PATH does not include /usr/sbin"),
                expects_warning
            );
        }
        Ok(())
    }
}
