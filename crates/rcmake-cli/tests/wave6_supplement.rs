//! Wave 6 supplement regressions for cache edits, presets, `--build`,
//! `--install`, and `--fresh`. Each test drives the CLI on an in-memory host.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use rcmake_base::BString;
use rcmake_host::{Follow, Host, ProcessResult, PureHost, ReadRange, RunOutput, Stream, WriteMode};

#[path = "support/project_modules.rs"]
mod project_modules;

const PROJECT: &str = "cmake_minimum_required(VERSION 4.5)\nproject(supplement NONE)\nmessage(STATUS \"SEED=${SEED}\")\n";

fn new_host() -> Arc<PureHost> {
    let host = Arc::new(PureHost::default());
    project_modules::seed(&host).expect("seed modules");
    host
}

fn seed(host: &PureHost, path: &str, contents: &str) {
    host.seed_file(Path::new(path), contents.as_bytes())
        .expect("seed file");
}

fn exists(host: &PureHost, path: &str) -> bool {
    host.metadata(Path::new(path), Follow::No).is_ok()
}

fn read(host: &PureHost, path: &str) -> String {
    let bytes = host
        .read(Path::new(path), ReadRange::All { limit: 1 << 24 })
        .expect("read file");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The result of one CLI run, with only the output of that run.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn cmake(host: &Arc<PureHost>, args: &[&str]) -> Run {
    let before_out = host.stream_bytes(Stream::Stdout).len();
    let before_err = host.stream_bytes(Stream::Stderr).len();
    let argv: Vec<BString> = std::iter::once("cmake")
        .chain(args.iter().copied())
        .map(BString::from)
        .collect();
    let code = rcmake_cli::run(&argv, host.clone());
    let tail = |stream, before: usize| {
        let bytes = host.stream_bytes(stream);
        String::from_utf8_lossy(bytes.get(before..).unwrap_or_default()).into_owned()
    };
    Run {
        code,
        stdout: tail(Stream::Stdout, before_out),
        stderr: tail(Stream::Stderr, before_err),
    }
}

fn success(host: &Arc<PureHost>, args: &[&str]) -> Run {
    let run = cmake(host, args);
    assert_eq!(run.code, 0, "{args:?}\n{}\n{}", run.stdout, run.stderr);
    run
}

/// Return the value of a cache entry in `<dir>/CMakeCache.txt`.
fn cache_value(host: &PureHost, dir: &str, name: &str) -> Option<String> {
    let cache = read(host, &format!("{dir}/CMakeCache.txt"));
    cache.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(':')?;
        Some(rest.split_once('=')?.1.to_owned())
    })
}

/// One native command: its working directory and its argv.
type Recorded = (String, Vec<String>);

/// The shared log of native commands.
type CommandLog = Arc<Mutex<Vec<Recorded>>>;

/// Capture the native build commands that `--build` runs.
fn record_commands(host: &PureHost) -> CommandLog {
    let commands = Arc::new(Mutex::new(Vec::new()));
    let recorded = commands.clone();
    host.set_command_handler(move |_, request, _| {
        for spec in &request.pipeline {
            let mut argv = vec![spec.program.to_string()];
            argv.extend(spec.args.iter().map(ToString::to_string));
            recorded
                .lock()
                .expect("command log")
                .push((request.cwd.display().to_string(), argv));
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .expect("command handler");
    commands
}

fn take(commands: &Mutex<Vec<Recorded>>) -> Vec<Recorded> {
    std::mem::take(&mut *commands.lock().expect("command log"))
}

const PRESETS: &str = r#"{
  "version": 6,
  "configurePresets": [
    {"name": "p", "generator": "Ninja", "binaryDir": "${sourceDir}/build",
     "cacheVariables": {"SEED": "preset", "KEEP": "k", "OTHER": "o"}}
  ]
}"#;

fn preset_host() -> Arc<PureHost> {
    let host = new_host();
    seed(&host, "/work/CMakeLists.txt", PROJECT);
    seed(&host, "/work/CMakePresets.json", PRESETS);
    seed(
        &host,
        "/work/initial.cmake",
        "message(STATUS \"SEEN=${SEED}\")\n",
    );
    host
}

fn report(entries: &[&str]) -> String {
    let mut text = String::from("Preset CMake variables:\n\n");
    for entry in entries {
        text.push_str(&format!("  {entry}\n"));
    }
    text.push('\n');
    text
}

#[test]
fn preset_cache_is_visible_to_preload_before_cli_override() {
    let host = preset_host();
    let run = success(
        &host,
        &[
            "--preset",
            "p",
            "-C",
            "initial.cmake",
            "-DSEED=cli",
            "--log-level=VERBOSE",
        ],
    );
    assert!(run.stdout.contains("-- SEEN=preset\n"), "{}", run.stdout);
    assert!(run.stdout.contains("-- SEED=cli\n"), "{}", run.stdout);
    assert!(
        run.stdout.contains(&report(&["KEEP=\"k\"", "OTHER=\"o\""])),
        "{}",
        run.stdout
    );
    assert_eq!(
        cache_value(&host, "/work/build", "SEED").as_deref(),
        Some("cli")
    );

    let host = preset_host();
    let run = success(
        &host,
        &["--preset", "p", "-DSEED=cli", "-C", "initial.cmake"],
    );
    assert!(run.stdout.contains("-- SEEN=cli\n"), "{}", run.stdout);
    assert!(
        !run.stdout.contains("Preset CMake variables"),
        "{}",
        run.stdout
    );

    // `-U` before and after `-C` erases only the entries that it removes.
    for args in [
        ["--preset", "p", "-UOTH*", "-C", "initial.cmake"],
        ["--preset", "p", "-C", "initial.cmake", "-UOTH*"],
    ] {
        let host = preset_host();
        let mut args = args.to_vec();
        args.push("--log-level=VERBOSE");
        let run = success(&host, &args);
        assert!(run.stdout.contains("-- SEEN=preset\n"), "{}", run.stdout);
        assert!(
            run.stdout
                .contains(&report(&["KEEP=\"k\"", "SEED=\"preset\""])),
            "{}",
            run.stdout
        );
        assert_eq!(cache_value(&host, "/work/build", "OTHER"), None);
    }

    // A `-D` with the preset value still erases the report entry. A `-U`
    // pattern that matches nothing keeps every entry.
    let host = preset_host();
    let run = success(
        &host,
        &[
            "--preset",
            "p",
            "-DSEED=preset",
            "-UNOPE*",
            "--log-level=VERBOSE",
        ],
    );
    assert!(
        run.stdout.contains(&report(&["KEEP=\"k\"", "OTHER=\"o\""])),
        "{}",
        run.stdout
    );
}

#[test]
fn unset_glob_removes_matching_cache_and_report_entries() {
    let presets = r#"{
  "version": 6,
  "configurePresets": [
    {"name": "p", "generator": "Ninja", "binaryDir": "${sourceDir}/build-p",
     "toolchainFile": "${sourceDir}/tc.cmake",
     "cacheVariables": {"PRE_A": "1", "PREFIX_B": "2", "PRX": "3", "KEEP": "k"}},
    {"name": "q", "generator": "Ninja", "binaryDir": "${sourceDir}/build-q",
     "cacheVariables": {"PRE_A": "1", "PREFIX_B": "2", "PRX": "3", "KEEP": "k"}}
  ]
}"#;
    let host = new_host();
    seed(&host, "/work/CMakeLists.txt", PROJECT);
    seed(&host, "/work/CMakePresets.json", presets);
    seed(&host, "/work/tc.cmake", "# toolchain\n");

    // Prefix collisions and preserved STATIC entries.
    success(&host, &["--preset", "q"]);
    host.write(
        Path::new("/work/build-q/CMakeCache.txt"),
        b"PRE_STATIC:STATIC=s\n",
        WriteMode::Append,
    )
    .expect("append static entry");
    let run = success(&host, &["--preset", "q", "-UPRE*", "--log-level=VERBOSE"]);
    assert!(
        run.stdout.contains(&report(&["KEEP=\"k\"", "PRX=\"3\""])),
        "{}",
        run.stdout
    );
    for (name, value) in [
        ("PRE_A", None),
        ("PREFIX_B", None),
        ("PRX", Some("3")),
        ("KEEP", Some("k")),
        ("PRE_STATIC", Some("s")),
    ] {
        assert_eq!(
            cache_value(&host, "/work/build-q", name).as_deref(),
            value,
            "{name}"
        );
    }

    // A bracket expression matches the toolchain file. The removal is not a
    // toolchain change: unrelated cache entries and `CMakeFiles` stay.
    success(&host, &["--preset", "p"]);
    assert!(cache_value(&host, "/work/build-p", "CMAKE_TOOLCHAIN_FILE").is_some());
    host.write(
        Path::new("/work/build-p/CMakeCache.txt"),
        b"SAVED:STRING=keep\n",
        WriteMode::Append,
    )
    .expect("append unrelated entry");
    seed(&host, "/work/build-p/CMakeFiles/marker.txt", "keep\n");
    let run = success(
        &host,
        &[
            "--preset",
            "p",
            "-UCMAKE_TOOLCHAIN_FIL[DEF]",
            "--log-level=VERBOSE",
        ],
    );
    assert!(
        !run.stderr.contains("require your cache to be deleted"),
        "{}",
        run.stderr
    );
    assert_eq!(
        cache_value(&host, "/work/build-p", "SAVED").as_deref(),
        Some("keep")
    );
    assert!(exists(&host, "/work/build-p/CMakeFiles/marker.txt"));
    assert!(
        run.stdout.contains(&report(&[
            "KEEP=\"k\"",
            "PREFIX_B=\"2\"",
            "PRE_A=\"1\"",
            "PRX=\"3\"",
        ])),
        "{}",
        run.stdout
    );
    assert_eq!(
        cache_value(&host, "/work/build-p", "CMAKE_TOOLCHAIN_FILE"),
        None
    );
}

#[test]
fn quoted_definitions_keep_key_and_value_bytes() {
    let host = new_host();
    seed(
        &host,
        "/work/show.cmake",
        "foreach(v A B E W U \"a:b=c\" \"q=r\")\n  get_property(value CACHE \"${v}\" PROPERTY VALUE)\n  message(\"[${v}]=[${value}]\")\nendforeach()\n",
    );
    let run = success(
        &host,
        &[
            "-D\"a:b=c\":STRING=v1",
            "-D\"q=r\"=v2",
            "-DA=  x \t ",
            "-DB='  y  '",
            "-DE=",
            "-DW=   ",
            "-DU='z",
            "-P",
            "show.cmake",
        ],
    );
    for line in [
        "[A]=[  x]",
        "[B]=[  y  ]",
        "[E]=[]",
        "[W]=[   ]",
        "[U]=['z]",
        "[a:b=c]=[v1]",
        "[q=r]=[v2]",
    ] {
        assert!(
            run.stderr.contains(&format!("{line}\n")),
            "{line}\n{}",
            run.stderr
        );
    }

    // Configure, then reload the cache that it wrote.
    seed(
        &host,
        "/work/CMakeLists.txt",
        "cmake_minimum_required(VERSION 4.5)\nproject(quoted NONE)\nget_property(value CACHE \"K:EY=1\" PROPERTY VALUE)\nmessage(STATUS \"[${value}]\")\n",
    );
    let run = success(
        &host,
        &[
            "-S",
            ".",
            "-B",
            "build",
            "-G",
            "Ninja",
            "-D\"K:EY=1\":STRING='  sp  '",
        ],
    );
    assert!(run.stdout.contains("-- [  sp  ]\n"), "{}", run.stdout);
    assert!(read(&host, "/work/build/CMakeCache.txt").contains("\"K:EY=1\":STRING='  sp  '\n"));
    let run = success(&host, &["-S", ".", "-B", "build"]);
    assert!(run.stdout.contains("-- [  sp  ]\n"), "{}", run.stdout);
}

#[test]
fn fresh_clears_build_metadata_only_when_cache_exists() {
    let host = new_host();
    seed(&host, "/work/CMakeLists.txt", PROJECT);
    seed(&host, "/work/build/CMakeFiles/marker.txt", "keep\n");
    seed(&host, "/work/build/CMakeFiles/stale.cmake", "# stale\n");
    seed(
        &host,
        "/work/build/CMakeFiles/sub/nested.cmake",
        "# nested\n",
    );
    let args = ["-S", ".", "-B", "build", "-G", "Ninja", "--fresh"];
    // Without a cache, only the `CMakeFiles/*.cmake` scripts go.
    success(&host, &args);
    assert!(exists(&host, "/work/build/CMakeFiles/marker.txt"));
    assert!(!exists(&host, "/work/build/CMakeFiles/stale.cmake"));
    assert!(exists(&host, "/work/build/CMakeFiles/sub/nested.cmake"));
    assert!(exists(&host, "/work/build/CMakeCache.txt"));

    success(&host, &args);
    assert!(!exists(&host, "/work/build/CMakeFiles/marker.txt"));
    assert!(exists(&host, "/work/build/CMakeCache.txt"));

    // A `CMakeFiles` symlink is removed without following it.
    host.remove_dir_all(Path::new("/work/build/CMakeFiles"))
        .expect("remove metadata");
    seed(&host, "/work/elsewhere/marker.txt", "keep\n");
    host.symlink(
        Path::new("/work/elsewhere"),
        Path::new("/work/build/CMakeFiles"),
    )
    .expect("link metadata");
    success(&host, &args);
    assert!(exists(&host, "/work/elsewhere/marker.txt"));
    assert!(exists(&host, "/work/build/CMakeCache.txt"));
}

#[test]
fn path_definitions_use_the_preset_home() {
    let host = new_host();
    host.env_set(b"HOME", Some(b"/process-home"));
    seed(&host, "/work/CMakeLists.txt", PROJECT);
    seed(
        &host,
        "/work/CMakePresets.json",
        r#"{
  "version": 6,
  "configurePresets": [
    {"name": "p", "generator": "Ninja", "binaryDir": "${sourceDir}/build",
     "environment": {"HOME": "/preset-home"},
     "cacheVariables": {"PV": {"type": "PATH", "value": "~/pv/"}, "SV": "~/sv"}}
  ]
}"#,
    );
    success(
        &host,
        &[
            "--preset",
            "p",
            "-DA=~/q",
            "-DB:PATH=~/q",
            "-DC:FILEPATH=~\\c",
            "-DL:PATH=~/a;;~/b",
            "-DK:PATH=[x;~/b]",
        ],
    );
    for (name, value) in [
        ("A", "~/q"),
        ("B", "/preset-home/q"),
        ("C", "/preset-home/c"),
        ("L", "/preset-home/a;/preset-home/b"),
        ("K", "[x;~/b]"),
        ("PV", "/preset-home/pv"),
        ("SV", "~/sv"),
    ] {
        assert_eq!(
            cache_value(&host, "/work/build", name).as_deref(),
            Some(value),
            "{name}"
        );
    }
}

#[test]
fn untyped_definition_resets_the_cache_type() {
    let host = new_host();
    seed(&host, "/work/CMakeLists.txt", PROJECT);
    let args = ["-S", ".", "-B", "build", "-G", "Ninja"];
    success(&host, &[&args[..], &["-DFIRST:BOOL=ON"]].concat());
    assert!(read(&host, "/work/build/CMakeCache.txt").contains("FIRST:BOOL=ON\n"));
    success(&host, &[&args[..], &["-DFIRST=second"]].concat());
    let cache = read(&host, "/work/build/CMakeCache.txt");
    assert!(cache.contains("FIRST:UNINITIALIZED=second\n"), "{cache}");
}

#[test]
fn install_component_lists_stage_every_named_component() {
    let host = new_host();
    seed(
        &host,
        "/work/out/cmake_install.cmake",
        r#"foreach(component Runtime Development Docs Extra)
  if(NOT CMAKE_INSTALL_COMPONENT OR CMAKE_INSTALL_COMPONENT STREQUAL component)
    file(WRITE "${CMAKE_INSTALL_PREFIX}/${component}.txt" "${component}\n")
  endif()
endforeach()
"#,
    );
    success(
        &host,
        &[
            "--install",
            "out",
            "--prefix",
            "/work/stage",
            "--component",
            "Runtime;Development",
            "Docs",
        ],
    );
    for (component, present) in [
        ("Runtime", true),
        ("Development", true),
        ("Docs", true),
        ("Extra", false),
    ] {
        assert_eq!(
            exists(&host, &format!("/work/stage/{component}.txt")),
            present,
            "{component}"
        );
    }

    // Repeated flags and list escapes reach the script as separate names.
    seed(
        &host,
        "/work/esc/cmake_install.cmake",
        "file(APPEND \"${CMAKE_INSTALL_PREFIX}/order.txt\" \"<${CMAKE_INSTALL_COMPONENT}>\")\n",
    );
    success(
        &host,
        &[
            "--install",
            "esc",
            "--prefix",
            "/work/esc-stage",
            "--component=a;b",
            "--component",
            "c\\;d",
        ],
    );
    assert_eq!(read(&host, "/work/esc-stage/order.txt"), "<a><b><c;d>");
}

const BUILD_PRESETS: &str = r#"{
  "version": 6,
  "configurePresets": [{"name": "c", "generator": "Unix Makefiles", "binaryDir": "${sourceDir}/out"}],
  "buildPresets": [
    {"name": "native", "configurePreset": "c", "nativeToolOptions": ["--preset-flag"]},
    {"name": "jobs", "configurePreset": "c", "jobs": 2}
  ]
}"#;

fn build_host() -> Arc<PureHost> {
    let host = new_host();
    seed(&host, "/work/CMakePresets.json", BUILD_PRESETS);
    seed(
        &host,
        "/work/out/CMakeCache.txt",
        "CMAKE_HOME_DIRECTORY:INTERNAL=/work\nCMAKE_GENERATOR:INTERNAL=Unix Makefiles\nCMAKE_MAKE_PROGRAM:FILEPATH=/tools/make\n",
    );
    host
}

fn build_args(host: &Arc<PureHost>, args: &[&str]) -> Vec<Vec<String>> {
    let commands = record_commands(host);
    let mut argv = vec!["--build"];
    argv.extend_from_slice(args);
    success(host, &argv);
    take(&commands).into_iter().map(|(_, argv)| argv).collect()
}

#[test]
fn build_native_arguments_replace_preset_options() {
    let host = build_host();
    let commands = build_args(&host, &["--preset", "native", "--", "--dry-run"]);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].last().map(String::as_str), Some("--dry-run"));
    assert!(!commands[0].iter().any(|arg| arg == "--preset-flag"));

    for args in [&["--preset", "native"][..], &["--preset", "native", "--"]] {
        let commands = build_args(&host, args);
        assert_eq!(
            commands[0].last().map(String::as_str),
            Some("--preset-flag")
        );
    }
}

#[test]
fn build_target_arguments_expand_cmake_lists() {
    let host = build_host();
    let commands = build_args(&host, &["out", "--target", "a;b", "c", "-t", "d;;e"]);
    let targets: Vec<_> = commands
        .iter()
        .flatten()
        .filter(|arg| ["a", "b", "c", "d", "e"].contains(&arg.as_str()))
        .cloned()
        .collect();
    assert_eq!(targets, ["a", "b", "c", "d", "e"]);

    // Separate values join into one list, so a bracket can span them.
    let commands = build_args(&host, &["out", "--target", "[x", "y]"]);
    assert!(commands.iter().flatten().any(|arg| arg == "[x;y]"));

    // An attached value consumes only its own argument.
    let commands = record_commands(&host);
    let run = cmake(&host, &["--build", "out", "--target=clean", "other"]);
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(take(&commands).is_empty());

    let run = cmake(&host, &["--build", "out", "--target", "clean;a"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("Error: Building 'clean' and other targets together is not supported."),
        "{}",
        run.stderr
    );
    assert!(take(&commands).is_empty());
}

#[test]
fn build_from_subdir_loads_nearest_cache() {
    let host = build_host();
    seed(&host, "/work/out/sub/CMakeFiles/progress.marks", "0\n");
    let commands = record_commands(&host);
    success(&host, &["--build", "out/sub"]);
    let recorded = take(&commands);
    assert_eq!(recorded.len(), 1);
    assert_eq!(Path::new(&recorded[0].0), Path::new("/work/out/sub"));
    assert_eq!(recorded[0].1[0], "/tools/make");

    // A local cache has priority over the parent cache.
    seed(
        &host,
        "/work/out/sub/CMakeCache.txt",
        "CMAKE_GENERATOR:INTERNAL=Unix Makefiles\nCMAKE_MAKE_PROGRAM:FILEPATH=/tools/local-make\n",
    );
    success(&host, &["--build", "out/sub"]);
    assert_eq!(take(&commands)[0].1[0], "/tools/local-make");

    // The nearest ancestor cache has priority over a cache further up.
    seed(
        &host,
        "/work/out/a/CMakeCache.txt",
        "CMAKE_GENERATOR:INTERNAL=Unix Makefiles\nCMAKE_MAKE_PROGRAM:FILEPATH=/tools/near-make\n",
    );
    seed(&host, "/work/out/a/b/CMakeFiles/progress.marks", "0\n");
    success(&host, &["--build", "out/a/b"]);
    let recorded = take(&commands);
    assert_eq!(recorded[0].1[0], "/tools/near-make");
    assert_eq!(Path::new(&recorded[0].0), Path::new("/work/out/a/b"));

    // Without `CMakeFiles`, the parent cache is not used.
    seed(&host, "/work/out/plain/keep.txt", "\n");
    let run = cmake(&host, &["--build", "out/plain"]);
    assert_eq!(run.code, 1);
    assert!(
        run.stderr
            .contains("Error: not a CMake build directory (missing CMakeCache.txt)"),
        "{}",
        run.stderr
    );
    assert!(take(&commands).is_empty());
}

#[test]
fn build_preset_jobs_fill_default_states() {
    let jobs = |command: &[String]| {
        let index = command.iter().position(|arg| arg == "-j")?;
        command
            .get(index + 1)
            .filter(|value| value.parse::<u32>().is_ok())
            .cloned()
            .or_else(|| Some(String::new()))
    };
    for (args, env, expected) in [
        (&["--preset", "jobs"][..], None, "2"),
        (&["--preset", "jobs", "--parallel"], None, "2"),
        (&["--preset", "jobs", "-j"], None, "2"),
        (&["--preset", "jobs"], Some(""), "2"),
        (&["--preset", "jobs", "-j3"], None, "3"),
        (&["--preset", "jobs"], Some("5"), "5"),
        (&["--preset", "jobs", "-j", "4"], Some("5"), "4"),
    ] {
        let host = build_host();
        if let Some(value) = env {
            host.env_set(b"CMAKE_BUILD_PARALLEL_LEVEL", Some(value.as_bytes()));
        }
        let commands = build_args(&host, args);
        assert_eq!(
            jobs(&commands[0]).as_deref(),
            Some(expected),
            "{args:?} {env:?}"
        );
    }
}

#[cfg(any(windows, feature = "vs-cross"))]
#[test]
fn vs_package_restore_uses_preset_and_cli_modes() {
    let host = new_host();
    let project = "cmake_minimum_required(VERSION 4.5)\nproject(restore NONE)\nadd_custom_target(lib)\nset_property(TARGET lib PROPERTY VS_PACKAGE_REFERENCES \"Pkg.A&B_1.0;NoVersion\")\nadd_custom_target(app ALL)\nadd_dependencies(app lib)\nadd_custom_target(plain)\n";
    seed(&host, "/work/CMakeLists.txt", project);
    seed(
        &host,
        "/work/CMakePresets.json",
        r#"{
  "version": 6,
  "configurePresets": [{"name": "c", "generator": "Visual Studio 17 2022", "binaryDir": "${sourceDir}/vs"}],
  "buildPresets": [
    {"name": "only", "configurePreset": "c", "targets": ["app"], "resolvePackageReferences": "only"},
    {"name": "off", "configurePreset": "c", "targets": ["app"], "resolvePackageReferences": "off"},
    {"name": "on", "configurePreset": "c", "targets": ["app"], "resolvePackageReferences": "on"}
  ]
}"#,
    );
    success(&host, &["--preset", "c"]);
    let library = read(&host, "/work/vs/lib.vcxproj");
    assert!(
        library.contains(r#"<PackageReference Include="Pkg.A&amp;B" Version="1.0" />"#),
        "{library}"
    );
    assert!(!library.contains("NoVersion"));
    for (name, value) in [
        ("lib_REQUIRES_VS_PACKAGE_RESTORE", Some("ON")),
        ("app_REQUIRES_VS_PACKAGE_RESTORE", Some("ON")),
        ("ALL_BUILD_REQUIRES_VS_PACKAGE_RESTORE", Some("ON")),
        ("plain_REQUIRES_VS_PACKAGE_RESTORE", None),
    ] {
        assert_eq!(
            cache_value(&host, "/work/vs", name).as_deref(),
            value,
            "{name}"
        );
    }

    let restore = |args: &[&str]| {
        let commands = build_args(&host, args);
        commands[0]
            .iter()
            .find(|arg| arg.as_str() == "/restore" || arg.as_str() == "/t:Restore")
            .cloned()
    };
    assert_eq!(
        restore(&["--preset", "only"]).as_deref(),
        Some("/t:Restore")
    );
    assert_eq!(restore(&["--preset", "off"]), None);
    assert_eq!(restore(&["--preset", "on"]).as_deref(), Some("/restore"));
    assert_eq!(
        restore(&["--preset", "off", "--resolve-package-references=on"]).as_deref(),
        Some("/restore")
    );
    assert_eq!(
        restore(&["--preset", "only", "--resolve-package-references", "OFF"]),
        None
    );
    assert_eq!(restore(&["vs", "--target", "plain"]), None);
    assert_eq!(
        restore(&["vs", "--target", "app"]).as_deref(),
        Some("/restore")
    );

    // CMake keeps a computed restore entry after the references go away.
    seed(
        &host,
        "/work/CMakeLists.txt",
        &project.replace("Pkg.A&B_1.0;NoVersion", ""),
    );
    success(&host, &["--preset", "c"]);
    assert!(!read(&host, "/work/vs/lib.vcxproj").contains("PackageReference"));
    assert_eq!(
        cache_value(&host, "/work/vs", "lib_REQUIRES_VS_PACKAGE_RESTORE").as_deref(),
        Some("ON")
    );
}
