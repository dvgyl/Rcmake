use rcmake_host::{PureHost, Stream};
use rcmake_model::CacheType;

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Tree {
    host: Arc<PureHost>,
    bin: PathBuf,
    src: PathBuf,
    cc: PathBuf,
}

impl Tree {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let root = PathBuf::from(if cfg!(windows) { "C:/work" } else { "/work" });
        let host = Arc::new(PureHost::new(root.clone(), root.join("rcmake")));
        let tree = Self {
            bin: root.join("build"),
            src: root.join("src"),
            cc: root.join("toolchain/bin/cc"),
            host,
        };
        tree.host
            .seed_file(&tree.src.join("CMakeLists.txt"), b"project(P C)\n")?;
        tree.host.seed_file(&tree.cc, b"compiler one")?;
        tree.write_compiler_file("1.0")?;
        Ok(tree)
    }

    fn dyn_host(&self) -> Arc<dyn Host> {
        Arc::clone(&self.host) as Arc<dyn Host>
    }

    fn write_compiler_file(&self, version: &str) -> rcmake_host::HostResult<()> {
        let text = format!(
            "set(CMAKE_C_COMPILER \"{}\")\nset(CMAKE_C_COMPILER_ID \"Fake\")\nset(CMAKE_C_COMPILER_VERSION \"{version}\")\n",
            path::from_native_cmake(&self.cc)
        );
        let file = platform_dir(&self.bin).join("CMakeCCompiler.cmake");
        self.host.create_dir_all(&platform_dir(&self.bin))?;
        self.host.write(&file, text.as_bytes(), WriteMode::Truncate)
    }

    fn cache(&self, prefix: Option<&str>) -> Cache {
        let mut cache = Cache::default();
        let mut set = |name: &[u8], value: BString| {
            crate::cache::insert(&mut cache, name, value, Some(CacheType::String));
        };
        set(b"CMAKE_HOME_DIRECTORY", path::from_native_cmake(&self.src));
        set(b"CMAKE_C_COMPILER", path::from_native_cmake(&self.cc));
        if let Some(prefix) = prefix {
            set(b"CMAKE_PREFIX_PATH", prefix.into());
        }
        cache
    }

    fn stderr(&self) -> String {
        self.host.stream_bytes(Stream::Stderr).to_string()
    }

    fn receipt(&self) -> Option<Vec<u8>> {
        self.host
            .read(
                &receipt_path(&self.bin),
                ReadRange::All {
                    limit: RECEIPT_LIMIT,
                },
            )
            .ok()
    }

    /// Run one configure through the tracker and return the new stderr text.
    fn configure(
        &self,
        cache: &Cache,
        fresh: bool,
        native: bool,
        view: bool,
        success: bool,
    ) -> String {
        let before = self.stderr().len();
        let tracker = Tracker::begin(&self.dyn_host(), native, view, &self.bin, &self.src);
        if fresh {
            // A fresh configure removes the cache files and identifies the compiler again.
            let _ = self.host.remove_dir_all(&self.bin.join("CMakeFiles"));
            let _ = self.write_compiler_file("2.0");
        }
        tracker.check(cache, fresh);
        tracker.check(cache, fresh);
        if success {
            tracker.generated(cache);
        }
        tracker.finish(success);
        self.stderr().get(before..).unwrap_or_default().to_owned()
    }
}

#[test]
fn drift_receipt_round_trips_and_keeps_absent_and_empty_lists_apart() -> TestResult {
    let tree = Tree::new()?;
    let memo = DigestMemo::default();
    let mut facts = collect(tree.host.as_ref(), &memo, &tree.bin, &tree.cache(Some("")));
    facts.languages.get_mut("C").ok_or("C")?.identified = true;
    assert_eq!(facts.prefix_cache, Some(Vec::new()));
    assert_eq!(facts.prefix_env, None);
    let receipt = Receipt {
        binary: path::from_native(&tree.bin),
        source: path::from_native(&tree.src),
        facts,
    };
    let text = write_styled(&receipt.to_json(), "  ");
    let parsed = Receipt::from_json(&parse(&text, &READ_OPTIONS)?)?;
    assert_eq!(parsed, receipt);
    Ok(())
}

#[test]
fn drift_from_an_in_place_compiler_replacement_stays_until_a_fresh_configure() -> TestResult {
    let tree = Tree::new()?;
    let cache = tree.cache(None);
    assert_eq!(tree.configure(&cache, true, true, false, true), "");
    let baseline = tree.receipt().ok_or("receipt")?;

    tree.host
        .write(&tree.cc, b"compiler two", WriteMode::Truncate)?;
    let warning = tree.configure(&cache, false, true, false, true);
    assert_eq!(warning.matches("CMake Warning:").count(), 1, "{warning}");
    assert!(
        warning.contains("CMAKE_C_COMPILER (cache) content changed after compiler detection"),
        "{warning}"
    );
    assert!(warning.contains("still describe Fake 2.0"), "{warning}");
    assert!(warning.contains("rcmake --fresh -B"), "{warning}");
    assert_eq!(
        tree.receipt().as_ref(),
        Some(&baseline),
        "the warm run kept the baseline"
    );

    let again = tree.configure(&cache, false, true, false, true);
    assert!(
        again.contains("content changed after compiler detection"),
        "{again}"
    );

    assert_eq!(tree.configure(&cache, true, true, false, true), "");
    assert_ne!(tree.receipt().as_ref(), Some(&baseline));
    assert_eq!(tree.configure(&cache, false, true, false, true), "");
    Ok(())
}

#[test]
fn drift_check_is_silent_and_writes_nothing_for_compat_oracle_and_view() -> TestResult {
    let tree = Tree::new()?;
    let cache = tree.cache(None);
    tree.configure(&cache, true, true, false, true);
    let baseline = tree.receipt();
    tree.host
        .write(&tree.cc, b"compiler two", WriteMode::Truncate)?;
    assert_eq!(tree.configure(&cache, false, false, false, true), "");
    assert_eq!(tree.configure(&cache, false, true, true, true), "");
    tree.host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
    assert_eq!(tree.configure(&cache, false, true, false, true), "");
    assert_eq!(tree.receipt(), baseline);

    let fresh = Tree::new()?;
    fresh.host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
    fresh.configure(&cache, true, true, false, true);
    assert_eq!(fresh.receipt(), None);
    Ok(())
}

#[test]
fn drift_failed_configure_keeps_and_restores_the_baseline() -> TestResult {
    let tree = Tree::new()?;
    let cache = tree.cache(Some("/a;/b"));
    tree.configure(&cache, true, true, false, true);
    let baseline = tree.receipt().ok_or("receipt")?;
    // A failure after a restart or --fresh cleanup removed CMakeFiles.
    tree.configure(&tree.cache(Some("/c")), true, true, false, false);
    assert_eq!(tree.receipt().as_ref(), Some(&baseline));
    // A failure that leaves the receipt in place does not change it.
    let output = tree.configure(&tree.cache(Some("/b;/a")), false, true, false, false);
    assert!(
        output.contains("CMAKE_PREFIX_PATH (cache) order changed"),
        "{output}"
    );
    assert_eq!(tree.receipt().as_ref(), Some(&baseline));
    Ok(())
}

#[test]
fn drift_reports_prefix_and_toolchain_changes_with_their_origin() -> TestResult {
    let tree = Tree::new()?;
    let toolchain_one = tree.src.join("one.cmake");
    let toolchain_two = tree.src.join("two.cmake");
    let link = tree.src.join("toolchain.cmake");
    tree.host.seed_file(&toolchain_one, b"set(A 1)\n")?;
    tree.host.seed_file(&toolchain_two, b"set(A 2)\n")?;
    tree.host.symlink(&toolchain_one, &link)?;
    let mut cache = tree.cache(Some("/a;/b"));
    crate::cache::insert(
        &mut cache,
        b"CMAKE_TOOLCHAIN_FILE",
        path::from_native_cmake(&link),
        Some(CacheType::Filepath),
    );
    tree.configure(&cache, true, true, false, true);

    tree.host.remove_file(&link)?;
    tree.host.symlink(&toolchain_two, &link)?;
    tree.host.env_set(b"CMAKE_PREFIX_PATH", Some(b""));
    let output = tree.configure(&cache, false, true, false, true);
    assert!(
        output.contains("CMAKE_TOOLCHAIN_FILE (cache) now resolves to another file"),
        "{output}"
    );
    assert!(
        output.contains(
            "CMAKE_PREFIX_PATH (environment) changed.\n  Previous: not set\n  Current: empty"
        ),
        "{output}"
    );

    tree.host.remove_file(&link)?;
    tree.host.symlink(&toolchain_one, &link)?;
    tree.host.env_set(b"CMAKE_PREFIX_PATH", None);
    tree.host
        .write(&toolchain_one, b"set(A 3)\n", WriteMode::Truncate)?;
    let output = tree.configure(&cache, false, true, false, true);
    assert!(
        output.contains("CMAKE_TOOLCHAIN_FILE (cache) content changed"),
        "{output}"
    );
    assert!(!output.contains("environment"), "{output}");
    Ok(())
}

#[test]
fn doctor_distinguishes_missing_legacy_bad_missing_tool_and_drift() -> TestResult {
    let tree = Tree::new()?;
    let report = |tree: &Tree| {
        let mut text = String::new();
        let code = doctor(tree.host.as_ref(), &tree.bin, &mut text);
        (code, text)
    };
    let (code, text) = report(&tree);
    assert_eq!(code, 1);
    assert!(text.contains("no CMakeCache.txt found"), "{text}");

    let cache = tree.cache(None);
    crate::cache::save(tree.host.as_ref(), &tree.bin, &cache)?;
    let (code, text) = report(&tree);
    assert_eq!(code, 0);
    assert!(text.contains("Drift receipt: unavailable"), "{text}");
    assert!(text.contains("Baseline: unavailable"), "{text}");

    tree.configure(&cache, true, true, false, true);
    let (code, text) = report(&tree);
    assert_eq!(
        (code, text.contains("Drift: none found.")),
        (0, true),
        "{text}"
    );

    tree.host
        .write(&tree.cc, b"compiler two", WriteMode::Truncate)?;
    let (code, text) = report(&tree);
    assert_eq!(code, 1);
    assert!(
        text.contains("Drift: CMAKE_C_COMPILER (cache) content changed"),
        "{text}"
    );

    tree.host.remove_file(&tree.cc)?;
    let (code, text) = report(&tree);
    assert_eq!(code, 1);
    assert!(text.contains("file is missing"), "{text}");

    tree.host
        .write(&receipt_path(&tree.bin), b"{", WriteMode::Truncate)?;
    let (code, text) = report(&tree);
    assert_eq!(code, 1);
    assert!(text.contains("Drift receipt: invalid"), "{text}");
    Ok(())
}

#[test]
fn drift_receipt_from_another_build_directory_is_not_used() -> TestResult {
    let tree = Tree::new()?;
    let cache = tree.cache(None);
    tree.configure(&cache, true, true, false, true);
    let copy = tree.host.initial_cwd().join("copy");
    let bytes = tree.receipt().ok_or("receipt")?;
    tree.host.seed_file(&receipt_path(&copy), &bytes)?;
    assert!(matches!(
        load(tree.host.as_ref(), &copy, &tree.src).0,
        Baseline::Bad(_)
    ));
    Ok(())
}
