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

fn inventory(store: &Store<'_>) -> HostResult<String> {
    let stats = store.stats()?;
    Ok(format!(
        "Size: {} bytes\nEntries: {}\nBlobs: {}\n",
        stats.bytes, stats.entries, stats.blobs
    ))
}

pub(crate) fn doctor(argv0: Option<&BString>, args: &[BString], host: &dyn Host) -> i32 {
    use std::fmt::Write as _;

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
