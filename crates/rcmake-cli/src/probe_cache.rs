use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_archive::{PortableLimits, PortableName, read_portable, write_portable};
use rcmake_base::BString;
use rcmake_eval::ProbeCacheMode;
use rcmake_host::{
    FileKind, Follow, Host, HostError, HostResult, ReadRange, Stream, WriteMode,
    discover_cmake_root, path,
};
use rcmake_store::{
    ArchiveLimits, Digest, ImportError, ImportReport, ImportTrust, ProbeBundle, Store, StoreConfig,
    StoreMode,
};

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

const CACHE_USAGE: &[u8] =
    b"Usage: rcmake cache [status|dir|clear|export <file>|import <file> [--sha256 <hex>]]\n";

pub(crate) fn cache(args: &[BString], host: &dyn Host) -> i32 {
    if let [flag] | [_, flag] = args
        && is_help(flag)
    {
        let help: &[u8] = match args {
            [_] => {
                b"Usage: rcmake cache [status|dir|clear|export <file>|import <file>]\n\
                status  Show cache size and entry counts (default).\n\
                dir     Print the cache directory.\n\
                clear   Remove cache entries and blobs.\n\
                export  Write the probe entries to a portable archive.\n\
                import  Read probe entries from a portable archive.\n"
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
            [action, _] if action == b"export" => {
                b"Usage: rcmake cache export <file>\n\
                Write the probe entries and their blobs to a gzip tar archive.\n\
                The archive contains compiler paths, arguments, and captured compiler output.\n"
            }
            [action, _] if action == b"import" => {
                b"Usage: rcmake cache import <file> [--sha256 <hex>]\n\
                Read probe entries from an archive made by rcmake cache export.\n\
                With a matching --sha256 digest, the entries are used directly.\n\
                Without it, the entries are kept as candidates and are not used for cache hits.\n"
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
    let Some(action) = Action::parse(args) else {
        host.write_out(Stream::Stderr, CACHE_USAGE);
        return 2;
    };
    let result = open(host).and_then(|store| {
        let root = store.root();
        if root.as_os_str().is_empty() {
            return Ok(Err("No cache directory is configured.".to_owned()));
        }
        let shown = path::from_native_cmake(root);
        Ok(match action {
            Action::Dir => Ok(format!("{shown}\n")),
            Action::Clear => {
                let report = store.clear()?;
                Ok(format!(
                    "Removed {} entries and {} blobs. Reclaimed {} bytes.\n",
                    report.entries_removed, report.blobs_removed, report.bytes_removed
                ))
            }
            Action::Status => Ok(format!("Cache directory: {shown}\n{}", inventory(&store)?)),
            Action::Export(file) => export(host, &store, file),
            Action::Import { file, sha256 } => import(host, &store, file, sha256),
        })
    });
    match result {
        Ok(Ok(text)) => {
            host.write_out(Stream::Stdout, text.as_bytes());
            0
        }
        Ok(Err(message)) => {
            host.write_out(
                Stream::Stderr,
                format!("rcmake cache: {message}\n").as_bytes(),
            );
            1
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

enum Action<'a> {
    Status,
    Dir,
    Clear,
    Export(&'a [u8]),
    Import {
        file: &'a [u8],
        sha256: Option<&'a [u8]>,
    },
}

impl<'a> Action<'a> {
    fn parse(args: &'a [BString]) -> Option<Self> {
        let operand = |value: &'a BString| (!value.is_empty()).then_some(value.as_slice());
        match args {
            [] => Some(Self::Status),
            [action] if action == b"status" => Some(Self::Status),
            [action] if action == b"dir" => Some(Self::Dir),
            [action] if action == b"clear" => Some(Self::Clear),
            [action, file] if action == b"export" => operand(file).map(Self::Export),
            [action, rest @ ..] if action == b"import" => {
                let mut file = None;
                let mut sha256 = None;
                let mut rest = rest.iter();
                while let Some(arg) = rest.next() {
                    if arg == b"--sha256" && sha256.is_none() {
                        sha256 = Some(operand(rest.next()?)?);
                    } else if let Some(value) = arg.strip_prefix(b"--sha256=")
                        && sha256.is_none()
                    {
                        sha256 = Some(value).filter(|value| !value.is_empty());
                        sha256?;
                    } else if file.is_none() && !arg.starts_with(b"-") {
                        file = operand(arg);
                    } else {
                        return None;
                    }
                }
                Some(Self::Import {
                    file: file?,
                    sha256,
                })
            }
            _ => None,
        }
    }
}

/// The archive limits of `rcmake cache export` and `rcmake cache import`.
const LIMITS: ArchiveLimits = ArchiveLimits::DEFAULT;

/// The archive reader and writer form of [`LIMITS`].
fn portable_limits(limits: &ArchiveLimits) -> PortableLimits {
    PortableLimits {
        archive_bytes: limits.archive_bytes,
        tar_bytes: limits.tar_bytes,
        member_bytes: limits.member_bytes,
        manifest_bytes: limits.manifest_bytes,
        total_bytes: limits.total_bytes,
        members: limits.members,
    }
}

/// Read an archive and collect its members into a bundle.
fn read_bundle(archive: &[u8], limits: &ArchiveLimits) -> Result<ProbeBundle, String> {
    let members = read_portable(archive, &portable_limits(limits))
        .map_err(|error| format!("Invalid archive: {error} Nothing was imported."))?;
    let mut bundle = ProbeBundle::default();
    for member in members {
        match member.name {
            PortableName::Manifest => bundle.manifest = member.data,
            PortableName::Entry(key) => {
                bundle.entries.insert(Digest::from_bytes(key), member.data);
            }
            PortableName::Blob(digest) => {
                bundle.blobs.insert(Digest::from_bytes(digest), member.data);
            }
        }
    }
    Ok(bundle)
}

/// Write the verified probe entries of `store` to a portable archive at `file`.
///
/// The archive must pass the same checks as `rcmake cache import` before it
/// replaces the destination. It is written to a temporary file in the
/// destination directory and renamed after it is complete.
fn export(host: &dyn Host, store: &Store<'_>, file: &[u8]) -> Result<String, String> {
    let destination = host.initial_cwd().join(path::to_native(file));
    let (Some(parent), Some(name)) = (destination.parent(), destination.file_name()) else {
        return Err("The export destination must name a file.".into());
    };
    let parent = host
        .canonicalize(parent)
        .map_err(|_| format!("The directory {} does not exist.", parent.display()))?;
    if parent.starts_with(store.root()) {
        return Err("Choose an export destination outside the cache directory.".into());
    }
    let destination = parent.join(name);
    if host
        .metadata(&destination, Follow::No)
        .is_ok_and(|meta| meta.kind != FileKind::File)
    {
        return Err(format!(
            "{} exists and is not a regular file.",
            destination.display()
        ));
    }
    let export = store
        .export_probes(&LIMITS)
        .map_err(|error| error.to_string())?;
    let manifest = export
        .manifest
        .to_json()
        .map_err(|error| error.to_string())?;
    let mut members = vec![(PortableName::Manifest, manifest.as_slice())];
    members.extend(
        export
            .entries
            .iter()
            .map(|(key, frame)| (PortableName::Entry(*key.as_bytes()), frame.as_slice())),
    );
    members.extend(
        export
            .blobs
            .iter()
            .map(|(digest, frame)| (PortableName::Blob(*digest.as_bytes()), frame.as_slice())),
    );
    let archive = write_portable(&members, &portable_limits(&LIMITS))
        .map_err(|error| format!("{error} Nothing was exported."))?;
    read_bundle(&archive, &LIMITS)
        .and_then(|bundle| bundle.check(&LIMITS).map_err(|error| error.to_string()))
        .map_err(|error| {
            format!("The archive does not pass the import checks: {error} Nothing was exported.")
        })?;
    replace_file(host, &destination, &archive).map_err(|error| error.to_string())?;
    let mut text = format!(
        "Exported {} probe entries and {} blobs to {}.\nSHA-256: {}\n",
        export.entries.len(),
        export.blobs.len(),
        path::from_native_cmake(&destination),
        Digest::hash(&archive)
    );
    let skipped = export.skipped;
    if skipped.total() > 0 {
        text.push_str(&format!(
            "Skipped {} entries: {} stale, {} invalid, {} with missing blobs, {} over the size limit.\n",
            skipped.total(),
            skipped.stale,
            skipped.invalid,
            skipped.missing_blob,
            skipped.over_budget
        ));
    }
    Ok(text)
}

/// Check a portable archive and import its probe entries into `store`.
fn import(
    host: &dyn Host,
    store: &Store<'_>,
    file: &[u8],
    sha256: Option<&[u8]>,
) -> Result<String, String> {
    let expected = match sha256 {
        None => None,
        Some(hex) => Some(
            std::str::from_utf8(hex)
                .ok()
                .and_then(Digest::from_hex)
                .ok_or("--sha256 needs 64 hexadecimal digits.")?,
        ),
    };
    let source = host.initial_cwd().join(path::to_native(file));
    let limits = portable_limits(&LIMITS);
    let meta = host
        .metadata(&source, Follow::Yes)
        .map_err(|error| error.to_string())?;
    if meta.kind != FileKind::File {
        return Err(format!("{} is not a regular file.", source.display()));
    }
    if meta.len > limits.archive_bytes {
        return Err(format!(
            "The archive exceeds {} bytes.",
            limits.archive_bytes
        ));
    }
    // Check and parse one private copy, so a concurrent change cannot alter it.
    let archive = host
        .read(
            &source,
            ReadRange::All {
                limit: limits.archive_bytes,
            },
        )
        .map_err(|error| error.to_string())?;
    let actual = Digest::hash(&archive);
    let trust = match expected {
        Some(expected) if expected != actual => {
            return Err(format!(
                "The archive SHA-256 is {actual}, but --sha256 is {expected}. Nothing was imported."
            ));
        }
        Some(_) => ImportTrust::Trusted,
        None => ImportTrust::Untrusted,
    };
    let bundle = read_bundle(&archive, &LIMITS)?;
    let shown = path::from_native_cmake(&source);
    let report = store
        .import_probes(&bundle, trust, &LIMITS)
        .map_err(|error| match error {
            ImportError::Invalid(reason) => {
                format!("Invalid archive: {reason} Nothing was imported.")
            }
            ImportError::Store { error, completed } => format!(
                "{}The import stopped: {error}\nRun the same import again to finish it.",
                import_counts(&completed, &shown)
            ),
        })?;
    let mut text = import_counts(&report, &shown);
    if trust == ImportTrust::Untrusted && report.staged > 0 {
        text.push_str(
            "No --sha256 digest was given. Staged entries are not used for cache hits. \
             They can restore missing blobs of matching local entries.\n",
        );
    }
    Ok(text)
}

/// Replace `file` with `bytes` through a new temporary file in the same directory.
///
/// The temporary file gets a unique name and is created exclusively, so an existing
/// path or link is never opened. On failure, only that temporary file is removed.
pub(crate) fn replace_file(host: &dyn Host, file: &Path, bytes: &[u8]) -> HostResult<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        return Err(HostError::unsupported("The destination must name a file."));
    };
    let pid = host.system().pid;
    let mut last = None;
    for _ in 0..16 {
        let now = host.now();
        let temp = dir.join(format!(
            ".{}.{pid}.{}{:09}.{}.tmp",
            name.to_string_lossy(),
            now.secs,
            now.nanos,
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match host.write(&temp, bytes, WriteMode::CreateNew) {
            Ok(()) => {}
            Err(error) if error.kind == std::io::ErrorKind::AlreadyExists => {
                last = Some(error);
                continue;
            }
            Err(error) => {
                let _ = host.remove_file(&temp);
                return Err(error);
            }
        }
        return host.rename(&temp, file).inspect_err(|_| {
            let _ = host.remove_file(&temp);
        });
    }
    Err(last.unwrap_or_else(|| HostError::unsupported("No temporary file name is free.")))
}

fn import_counts(report: &ImportReport, source: &BString) -> String {
    format!(
        "Imported {} probe entries and {} blobs from {source}.\nUnchanged: {}. Recovered: {}. Staged: {}. Conflicts: {}.\n",
        report.entries_published,
        report.blobs_published,
        report.unchanged,
        report.recovered,
        report.staged,
        report.conflicts
    )
}

fn is_help(flag: &[u8]) -> bool {
    matches!(flag, b"-h" | b"--help")
}

fn inventory(store: &Store<'_>) -> HostResult<String> {
    let stats = store.stats()?;
    let mut text = format!(
        "Size: {} bytes\nEntries: {}\nBlobs: {}\n",
        stats.bytes, stats.entries, stats.blobs
    );
    if stats.candidates > 0 {
        text.push_str(&format!("Import candidates: {}\n", stats.candidates));
    }
    Ok(text)
}

pub(crate) fn doctor(argv0: Option<&BString>, args: &[BString], host: &dyn Host) -> i32 {
    use std::fmt::Write as _;

    if let [flag] = args
        && is_help(flag)
    {
        host.write_out(
            Stream::Stdout,
            b"Usage: rcmake doctor [-B <build>]\nShow the CMake root, available tools, and probe cache status.\n\
            With -B, also compare the build directory with its last successful configure.\n",
        );
        return 0;
    }
    let build = match args {
        [] => None,
        [flag, dir] if flag == b"-B" && !dir.is_empty() => Some(dir.as_slice()),
        [arg] => match arg.strip_prefix(b"-B") {
            Some(dir) if !dir.is_empty() => Some(dir),
            _ => None,
        },
        _ => None,
    };
    if !args.is_empty() && build.is_none() {
        host.write_out(Stream::Stderr, b"Usage: rcmake doctor [-B <build>]\n");
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
    let code = build.map_or(0, |dir| {
        text.push('\n');
        let dir = host.initial_cwd().join(path::to_native(dir));
        crate::drift::doctor(host, &dir, &mut text)
    });
    host.write_out(Stream::Stdout, text.as_bytes());
    code
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

    fn run(host: &PureHost, args: &[&str]) -> (i32, String, String) {
        let before_out = host.stream_bytes(Stream::Stdout).len();
        let before_err = host.stream_bytes(Stream::Stderr).len();
        let args: Vec<BString> = args.iter().copied().map(BString::from).collect();
        let code = cache(&args, host);
        let tail = |stream, before: usize| {
            host.stream_bytes(stream)
                .get(before..)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default()
        };
        (
            code,
            tail(Stream::Stdout, before_out),
            tail(Stream::Stderr, before_err),
        )
    }

    /// A compiled probe payload whose artifact is `blob`.
    fn probe_payload(blob: Digest) -> rcmake_json::Value {
        use rcmake_json::{ToJson, Value};
        let artifact = Value::object_from([
            ("blob", blob.to_json()),
            ("relative_path", "a.out".to_json()),
            ("mode", 0o755u32.to_json()),
        ]);
        let outcome = Value::object_from([
            ("max_start_depth", 0u32.to_json()),
            ("compiled", true.to_json()),
            ("exit_code", 0i32.to_json()),
            ("output", "probe".to_json()),
            ("artifact", artifact),
            ("diagnostics", Value::Array(Vec::new())),
        ]);
        Value::object_from([("outcome", outcome), ("checks", Value::object())])
    }

    #[test]
    fn replace_file_never_opens_an_existing_path_and_cleans_only_its_own_file() -> HostResult<()> {
        let host = PureHost::default();
        let dir = Path::new("/work/out");
        let destination = dir.join("probes.tar.gz");
        let pid = host.system().pid;
        let old_name = dir.join(format!(".probes.tar.gz.{pid}.tmp"));
        let target = Path::new("/work/target");
        host.seed_file(target, b"keep")?;
        host.create_dir_all(dir)?;
        host.symlink(target, &old_name)?;

        replace_file(&host, &destination, b"first")?;
        replace_file(&host, &destination, b"second")?;
        let read = |file: &Path| host.read(file, ReadRange::All { limit: 64 });
        assert_eq!(read(&destination)?, b"second");
        assert_eq!(read(target)?, b"keep");
        let names: Vec<_> = host
            .read_dir(dir)?
            .into_iter()
            .map(|entry| entry.name.to_string())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");

        // A failed rename removes the new temporary file and nothing else.
        let blocked = dir.join("blocked");
        host.create_dir_all(&blocked.join("child"))?;
        assert!(replace_file(&host, &blocked, b"data").is_err());
        assert_eq!(host.read_dir(dir)?.len(), 3);
        assert_eq!(read(target)?, b"keep");
        Ok(())
    }

    fn seeded_store(host: &PureHost, dir: &str) -> HostResult<rcmake_store::Digest> {
        use rcmake_store::{Domain, Entry};
        let root = format!("/work/{dir}");
        host.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(root.as_bytes()));
        let store = open(host)?;
        let blob = store.put_blob(b"artifact")?;
        let key = Domain::ProbeV1.key(b"probe");
        let entry = Entry {
            trace: Vec::new(),
            blobs: vec![blob],
            payload: probe_payload(blob),
        };
        store.put_entry(Domain::ProbeV1, key, &entry)?;
        Ok(key)
    }

    #[test]
    fn cache_export_and_trusted_import_round_trip_through_an_empty_store() -> HostResult<()> {
        let host = PureHost::default();
        let key = seeded_store(&host, "producer")?;
        let (code, out, err) = run(&host, &["export", "probes.tar.gz"]);
        assert_eq!(code, 0, "{err}");
        assert!(
            out.starts_with("Exported 1 probe entries and 1 blobs to "),
            "{out}"
        );
        let digest = out
            .lines()
            .find_map(|line| line.strip_prefix("SHA-256: "))
            .unwrap_or_default()
            .to_owned();
        let archive = host.read(
            &host.initial_cwd().join("probes.tar.gz"),
            ReadRange::All { limit: 1 << 20 },
        )?;
        assert_eq!(rcmake_store::Digest::hash(&archive).to_hex(), digest);

        host.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/consumer"));
        let wrong = "0".repeat(64);
        let (code, _, err) = run(&host, &["import", "probes.tar.gz", "--sha256", &wrong]);
        assert_eq!(code, 1);
        assert!(err.contains("Nothing was imported."), "{err}");
        assert!(
            host.metadata(Path::new("/work/consumer"), Follow::No)
                .is_err()
        );

        let flag = format!("--sha256={digest}");
        let (code, out, err) = run(&host, &["import", &flag, "probes.tar.gz"]);
        assert_eq!(code, 0, "{err}");
        assert!(
            out.starts_with("Imported 1 probe entries and 1 blobs"),
            "{out}"
        );
        let store = open(&host)?;
        assert!(
            store
                .get_entry::<rcmake_json::Value>(rcmake_store::Domain::ProbeV1, key)?
                .is_some()
        );
        let (_, out, _) = run(&host, &["import", "probes.tar.gz", "--sha256", &digest]);
        assert!(
            out.contains("Imported 0 probe entries and 0 blobs"),
            "{out}"
        );
        assert!(out.contains("Unchanged: 1."), "{out}");
        Ok(())
    }

    #[test]
    fn cache_import_without_a_digest_stages_candidates_only() -> HostResult<()> {
        let host = PureHost::default();
        let key = seeded_store(&host, "producer")?;
        assert_eq!(run(&host, &["export", "probes.tar.gz"]).0, 0);
        host.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/consumer"));
        let (code, out, _) = run(&host, &["import", "probes.tar.gz"]);
        assert_eq!(code, 0);
        assert!(out.contains("Staged: 1."), "{out}");
        assert!(out.contains("not used for cache hits"), "{out}");
        let store = open(&host)?;
        assert!(
            store
                .get_entry::<rcmake_json::Value>(rcmake_store::Domain::ProbeV1, key)?
                .is_none()
        );
        let (_, status, _) = run(&host, &[]);
        assert!(status.contains("Entries: 0\n"), "{status}");
        assert!(status.contains("Import candidates: 2\n"), "{status}");
        Ok(())
    }

    #[test]
    fn cache_export_and_import_reject_bad_operands() -> HostResult<()> {
        let host = PureHost::default();
        seeded_store(&host, "producer")?;
        // A full operand keeps POSIX test paths free of the Windows separator from `Path::join`.
        let (code, _, err) = run(&host, &["export", "/work/producer/probes.tar.gz"]);
        assert_eq!(code, 1);
        assert!(err.contains("outside the cache directory"), "{err}");
        for args in [
            &["export"][..],
            &["export", "a", "b"],
            &["import"],
            &["import", "a", "--sha256"],
            &["import", "a", "--sha256", "b", "--sha256", "c"],
            &["import", "a", "--unknown"],
        ] {
            assert_eq!(run(&host, args).0, 2, "{args:?}");
        }
        let (code, _, err) = run(&host, &["import", "missing.tar.gz", "--sha256", "xyz"]);
        assert_eq!(code, 1);
        assert!(err.contains("64 hexadecimal digits"), "{err}");
        host.seed_file(Path::new("/work/bad.tar.gz"), b"not an archive")?;
        let (code, _, err) = run(&host, &["import", "bad.tar.gz"]);
        assert_eq!(code, 1);
        assert!(err.contains("Invalid archive"), "{err}");
        Ok(())
    }

    #[test]
    fn doctor_accepts_both_build_directory_forms() {
        for args in [&["-B", "missing"][..], &["-Bmissing"]] {
            let host = PureHost::default();
            let args: Vec<BString> = args.iter().copied().map(BString::from).collect();
            assert_eq!(doctor(None, &args, &host), 1);
            let out = host.stream_bytes(Stream::Stdout).to_string();
            assert!(out.contains("Build directory: /work/missing"), "{out}");
            assert!(out.contains("no CMakeCache.txt found"), "{out}");
        }
        for args in [&["-B"][..], &["-B", ""], &["--build", "x"]] {
            let host = PureHost::default();
            let args: Vec<BString> = args.iter().copied().map(BString::from).collect();
            assert_eq!(doctor(None, &args, &host), 2);
        }
    }
}
