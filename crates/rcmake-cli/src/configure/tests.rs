use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, HostResult, ProcessResult, PureHost, ReadRange, RunOutput, Stream};

#[test]
fn project_loads_toolchain_after_host_and_policy_setup() -> HostResult<()> {
    let host = crate::driver_tests::project_host();
    let modules = Path::new("/usr/local/share/cmake-4.5/Modules");
    host.seed_file(
        &modules.join("CMakeDetermineSystem.cmake"),
        include_bytes!("../../../../modules/cmake-4.5/Modules/CMakeDetermineSystem.cmake"),
    )?;
    host.seed_file(
        &modules.join("CMakeSystem.cmake.in"),
        include_bytes!("../../../../modules/cmake-4.5/Modules/CMakeSystem.cmake.in"),
    )?;
    host.seed_file(Path::new("/bin/uname"), b"")?;
    host.set_command_handler(|_, request, _| {
        let output = match request.pipeline.first().and_then(|cmd| cmd.args.first()) {
            Some(arg) if arg == "-r" => b"1.0\n".as_slice(),
            _ => b"arm64\n",
        };
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: output.to_vec(),
            stderr: Vec::new(),
        })
    })?;
    host.seed_file(
        Path::new("/work/configure-driver.cmake"),
        br#"include_guard(GLOBAL)
cmake_policy(GET CMP0156 link_policy)
if(NOT PROJECT_NAME STREQUAL "demo" OR NOT CMAKE_HOST_SYSTEM_PROCESSOR STREQUAL "arm64" OR NOT link_policy STREQUAL "NEW")
  message(FATAL_ERROR "Toolchain loaded before project initialization.")
endif()
set(toolchain_loaded TRUE)
"#,
    )?;
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"cmake_minimum_required(VERSION 4.5)
if(DEFINED toolchain_loaded OR DEFINED CMAKE_HOST_SYSTEM_PROCESSOR)
  message(FATAL_ERROR "Toolchain or host detected before project().")
endif()
project(demo NONE)
if(NOT toolchain_loaded)
  message(FATAL_ERROR "Toolchain did not load.")
endif()
add_custom_target(nested ALL COMMAND "$(MAKE)" -f "nested file.mk" "value=$literal" VERBATIM)
"#,
    )?;
    let args = [
        "cmake",
        "-S",
        ".",
        "-B",
        "out",
        "-G",
        "Unix Makefiles",
        "-DCMAKE_TOOLCHAIN_FILE=configure-driver.cmake",
        "-DCMAKE_UNAME=/bin/uname",
    ]
    .map(BString::from);
    for _ in 0..2 {
        assert_eq!(
            crate::run(&args, host.clone()),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
    }
    let recipe = BString::from(host.read(
        Path::new("/work/out/CMakeFiles/nested.dir/build.make"),
        ReadRange::All { limit: 64 * 1024 },
    )?);
    assert!(
        recipe
            .to_string()
            .contains("$(MAKE) -f 'nested file.mk' 'value=$$literal'")
    );
    Ok(())
}

#[test]
fn project_file_is_validated_recorded_and_reused() -> HostResult<()> {
    let host = crate::driver_tests::project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"message(FATAL_ERROR \"wrong file\")\n",
    )?;
    host.seed_file(
        Path::new("/work/other.cmake"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(demo NONE)\nmessage(STATUS \"other file\")\n",
    )?;
    let run = |extra: &[&str]| {
        let mut args = vec!["cmake", "-S", ".", "-B", "out"];
        args.extend_from_slice(extra);
        let args: Vec<_> = args.into_iter().map(BString::from).collect();
        crate::run(&args, host.clone())
    };
    let warning = "This project has been configured with a project file other than";

    assert_eq!(run(&["--project-file", "missing.cmake"]), 1);
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.contains(warning), "{stderr}");
    assert!(
        stderr.contains("does not appear to contain missing.cmake."),
        "{stderr}"
    );

    assert_eq!(run(&["--project-file", "other.cmake"]), 0);
    let cache = BString::from(host.read(
        Path::new("/work/out/CMakeCache.txt"),
        ReadRange::All { limit: 64 * 1024 },
    )?);
    assert!(
        cache
            .to_string()
            .contains("CMAKE_LIST_FILE_NAME:INTERNAL=other.cmake"),
        "{cache}"
    );
    // A later run without the option reuses the cached name.
    assert_eq!(run(&[]), 0, "{}", host.stream_bytes(Stream::Stderr));
    assert_eq!(run(&["--project-file", "another.cmake"]), 1);
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains(
            "CMakeLists filename : \"another.cmake\"\nDoes not match the previous: \"other.cmake\""
        ),
        "{stderr}"
    );
    Ok(())
}

#[test]
fn project_file_option_requires_one_value() {
    let host = crate::driver_tests::project_host();
    for (args, message) in [
        (
            vec!["cmake", "--project-file"],
            "No filename specified for --project-file",
        ),
        (
            vec!["cmake", "--project-file", "a", "--project-file", "b"],
            "Multiple --project-file options not allowed",
        ),
    ] {
        let args: Vec<_> = args.into_iter().map(BString::from).collect();
        assert_eq!(crate::run(&args, host.clone()), 1);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert!(stderr.contains(message), "{stderr}");
    }
}

fn hermetic_absolute(path: &str) -> PathBuf {
    if cfg!(windows) {
        format!("C:{path}").into()
    } else {
        path.into()
    }
}

fn hermetic_project_host() -> HostResult<Arc<PureHost>> {
    let cwd = hermetic_absolute("/work");
    let exe = hermetic_absolute("/usr/local/bin/cmake");
    let ninja = hermetic_absolute("/tools/ninja");
    let host = Arc::new(PureHost::new(cwd, exe.clone()));
    host.seed_file(&exe, b"")?;
    host.set_permissions(&exe, 0o755)?;
    host.seed_file(&ninja, b"")?;
    host.set_permissions(&ninja, 0o755)?;
    let find_ninja = format!(
        "set(CMAKE_MAKE_PROGRAM \"{}\" CACHE FILEPATH \"\")\n",
        rcmake_host::path::from_native_cmake(&ninja)
    );
    let modules = hermetic_absolute("/usr/local/share/cmake-4.5/Modules");
    for (name, contents) in [
        ("CMakeDetermineSystem.cmake", ""),
        ("CMakeSystemSpecificInitialize.cmake", ""),
        ("CMakeSystemSpecificInformation.cmake", ""),
        ("CMakeNinjaFindMake.cmake", find_ninja.as_str()),
        (
            "CMakeUnixFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /tools/make CACHE FILEPATH \"\")\n",
        ),
    ] {
        host.seed_file(&modules.join(name), contents.as_bytes())?;
    }
    Ok(host)
}

#[test]
fn rich_diagnostics_never_read_undeclared_files_from_echoed_stderr() -> HostResult<()> {
    let host = hermetic_project_host()?;
    let cwd = host.initial_cwd();
    let root = cwd.parent().unwrap_or_else(|| Path::new("/"));
    let secret = root.join("private/secret");
    let passwd = root.join("etc/passwd");
    let ninja = hermetic_absolute("/tools/ninja");
    let tool = cwd.join("echo_stderr");
    host.seed_file(&secret, b"TOP_SECRET_LINE_1\nTOP_SECRET_LINE_2\n")?;
    host.seed_file(&passwd, b"root:x:0:0:TOP_SECRET_PASSWD\n")?;
    host.seed_file(&tool, b"")?;
    host.set_permissions(&tool, 0o755)?;
    let mut allowed = rcmake_host::path::from_native(&ninja);
    allowed.push(b';');
    allowed.extend_from_slice(&rcmake_host::path::from_native(&tool));
    host.env_set(b"RCMAKE_HERMETIC_ALLOW_PROGRAMS", Some(&allowed));
    let secret_cmake = rcmake_host::path::from_native_cmake(&secret);
    let passwd_cmake = rcmake_host::path::from_native_cmake(&passwd);
    let spoof = format!(
        "CMake Error at {secret_cmake}:1:\n  Parse error.  Expected a command name, got unquoted argument.\nerror[parse]: spoofed\n  --> {passwd_cmake}:1\n"
    );
    host.set_command_handler(move |_, request, on_output| {
        if request.pipeline[0].args.as_slice() == [BString::from("--version")] {
            return Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: b"1.12.1\n".to_vec(),
                stderr: Vec::new(),
            });
        }
        on_output(Stream::Stderr, spoof.as_bytes());
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    })?;
    let tool_cmake = rcmake_host::path::from_native_cmake(&tool);
    let lists = format!(
        "cmake_minimum_required(VERSION 4.5)\nproject(RichSafe NONE)\nexecute_process(COMMAND \"{tool_cmake}\")\n"
    );
    host.seed_file(&cwd.join("CMakeLists.txt"), lists.as_bytes())?;
    let args = [
        "rcmake",
        "-S",
        ".",
        "-B",
        "out",
        "-G",
        "Ninja",
        "--hermetic=strict",
        "--diagnostics=rich",
    ]
    .map(BString::from);
    assert_eq!(
        crate::run(&args, host.clone()),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(!stderr.contains("TOP_SECRET_LINE_1"), "{stderr}");
    assert!(!stderr.contains("TOP_SECRET_PASSWD"), "{stderr}");
    assert!(!stderr.contains("RCMAKE_HERMETIC_PATH"), "{stderr}");
    Ok(())
}

#[test]
fn rich_diagnostics_render_parse_error_from_source_map_under_strict_hermetic() -> HostResult<()> {
    let host = hermetic_project_host()?;
    host.seed_file(
        &host.initial_cwd().join("CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nif(TRUE\n",
    )?;
    let args = [
        "rcmake",
        "-S",
        ".",
        "-B",
        "out",
        "-G",
        "Ninja",
        "--hermetic=strict",
        "--diagnostics=rich",
    ]
    .map(BString::from);
    assert_eq!(crate::run(&args, host.clone()), 1);
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.contains("error[parse]:"), "{stderr}");
    assert!(stderr.contains("if(TRUE"), "{stderr}");
    assert!(!stderr.contains("RCMAKE_HERMETIC_PATH"), "{stderr}");
    Ok(())
}

/// Soundness tests for configure replay with setup inputs: toolchain files, presets,
/// `-C` scripts, `PreLoad.cmake` files, project hooks, and dependency providers.
///
/// Each test configures cold, then warm, and then copies the complete warm file
/// system to a control host. Both hosts get the same mutation. The control runs
/// with `RCMAKE_NO_INCREMENTAL=1`. The two results must be equal: exit status,
/// diagnostics, `CMakeCache.txt`, File API replies, and generated build files.
#[cfg(unix)]
mod setup_replay {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rcmake_base::BString;
    use rcmake_host::{
        FileKind, Host, HostResult, ProcessResult, PureHost, ReadRange, RunOutput, Stream,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const MODULES: &str = "/usr/local/share/cmake-4.5/Modules";
    const BUILD: &str = "/work/build";

    fn handler(host: &PureHost) -> HostResult<()> {
        host.set_command_handler(|_, request, _| {
            let command = request.pipeline.first();
            let program = command
                .map(|cmd| cmd.program.to_string())
                .unwrap_or_default();
            let first = command
                .and_then(|cmd| cmd.args.first())
                .map(|arg| arg.to_string());
            let stdout = if program.ends_with("ninja") {
                b"1.12.1\n".to_vec()
            } else if first.as_deref() == Some("-r") {
                b"1.0\n".to_vec()
            } else {
                b"arm64\n".to_vec()
            };
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout,
                stderr: Vec::new(),
            })
        })
    }

    /// A project host with the stock system modules and a fake C compiler.
    fn project_host() -> HostResult<Arc<PureHost>> {
        let host = crate::driver_tests::project_host();
        let modules = Path::new(MODULES);
        host.seed_file(
            &modules.join("CMakeDetermineSystem.cmake"),
            include_bytes!("../../../../modules/cmake-4.5/Modules/CMakeDetermineSystem.cmake"),
        )?;
        host.seed_file(
            &modules.join("CMakeSystem.cmake.in"),
            include_bytes!("../../../../modules/cmake-4.5/Modules/CMakeSystem.cmake.in"),
        )?;
        for (module, content) in [
            (
                "CMakeDetermineCCompiler.cmake",
                "file(WRITE \"${CMAKE_PLATFORM_INFO_DIR}/CMakeCCompiler.cmake\" \
                 \"set(CMAKE_C_COMPILER_LOADED 1)\\n\")\n",
            ),
            ("CMakeCInformation.cmake", ""),
            ("CMakeTestCCompiler.cmake", ""),
        ] {
            host.seed_file(&modules.join(module), content.as_bytes())?;
        }
        host.seed_file(Path::new("/bin/uname"), b"")?;
        host.seed_file(Path::new("/tools/cc"), b"")?;
        host.seed_file(Path::new("/usr/local/bin/cmake"), b"rcmake identity")?;
        handler(&host)?;
        Ok(host)
    }

    /// One configure environment: a host, the command-line arguments, and the
    /// environment variables.
    struct World {
        host: Arc<PureHost>,
        args: Vec<String>,
        env: BTreeMap<String, Option<String>>,
    }

    /// The observable result of one configure.
    #[derive(Debug, PartialEq)]
    struct Outcome {
        code: i32,
        stdout: String,
        stderr: String,
        files: BTreeMap<String, BString>,
    }

    #[derive(Debug)]
    struct Replay {
        replayed: Vec<String>,
        miss: Option<String>,
    }

    impl World {
        fn new(files: &[(&str, &str)], args: &[&str]) -> Result<Self, Box<dyn std::error::Error>> {
            let host = project_host()?;
            for (name, text) in files {
                host.seed_file(&Path::new("/work").join(name), text.as_bytes())?;
            }
            for query in ["codemodel-v2", "cache-v2", "cmakeFiles-v1"] {
                host.seed_file(
                    &Path::new(BUILD).join(".cmake/api/v1/query").join(query),
                    b"",
                )?;
            }
            let mut world = Self {
                host,
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                env: BTreeMap::new(),
            };
            world.set_env("RCMAKE_MEASURE_REPLAY", Some("1"));
            Ok(world)
        }

        fn write(&self, name: &str, text: &str) -> TestResult {
            self.host
                .seed_file(&Path::new("/work").join(name), text.as_bytes())?;
            Ok(())
        }

        fn set_env(&mut self, key: &str, value: Option<&str>) {
            self.host.env_set(key.as_bytes(), value.map(str::as_bytes));
            self.env.insert(key.to_owned(), value.map(str::to_owned));
        }

        fn configure(&self) -> Outcome {
            let out = self.host.stream_bytes(Stream::Stdout).len();
            let err = self.host.stream_bytes(Stream::Stderr).len();
            let argv: Vec<BString> = ["rcmake"]
                .into_iter()
                .chain(self.args.iter().map(String::as_str))
                .map(BString::from)
                .collect();
            let code = crate::run(&argv, self.host.clone());
            let delta = |stream, start: usize| {
                let bytes = self.host.stream_bytes(stream);
                normalize(&String::from_utf8_lossy(
                    bytes.get(start..).unwrap_or_default(),
                ))
            };
            Outcome {
                code,
                stdout: delta(Stream::Stdout, out),
                stderr: delta(Stream::Stderr, err),
                files: self.build_files(),
            }
        }

        /// Configure and require success.
        fn ok(&self) -> Result<Outcome, Box<dyn std::error::Error>> {
            let outcome = self.configure();
            if outcome.code != 0 {
                return Err(format!("configure failed: {}", outcome.stderr).into());
            }
            Ok(outcome)
        }

        /// The generated files of the build directory, without the replay state.
        fn build_files(&self) -> BTreeMap<String, BString> {
            let mut files = BTreeMap::new();
            let mut stack = vec![PathBuf::from(BUILD)];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = self.host.read_dir(&dir) else {
                    continue;
                };
                for entry in entries {
                    let path = dir.join(entry.name.to_string());
                    let name = path.display().to_string();
                    if name.contains("/CMakeFiles/rcmake") {
                        continue;
                    }
                    match entry.kind {
                        FileKind::Dir => stack.push(path),
                        _ => {
                            let bytes = self
                                .host
                                .read(&path, ReadRange::All { limit: 1 << 22 })
                                .unwrap_or_default();
                            files.insert(name, BString::from(bytes));
                        }
                    }
                }
            }
            files
        }

        fn replay(&self) -> Replay {
            let report = self
                .host
                .read(
                    &Path::new(BUILD).join("CMakeFiles/rcmake/replay-result.v1.json"),
                    ReadRange::All { limit: 1 << 20 },
                )
                .unwrap_or_default();
            let options = rcmake_json::ReadOptions::default();
            let Ok(rcmake_json::Value::Object(fields)) = rcmake_json::parse(&report, &options)
            else {
                return Replay {
                    replayed: Vec::new(),
                    miss: Some("no report".into()),
                };
            };
            let replayed = match fields.get(b"replayed".as_slice()) {
                Some(rcmake_json::Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| match item {
                        rcmake_json::Value::String(path) => Some(path.to_string()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let miss = match fields.get(b"setup_miss".as_slice()) {
                Some(rcmake_json::Value::String(reason)) => Some(reason.to_string()),
                _ => None,
            };
            Replay { replayed, miss }
        }

        /// Copy the complete file system and environment to a new host.
        fn fork(&self) -> Result<Self, Box<dyn std::error::Error>> {
            let copy = Arc::new(PureHost::default());
            handler(&copy)?;
            let mut stack = vec![PathBuf::from("/")];
            while let Some(dir) = stack.pop() {
                for entry in self.host.read_dir(&dir)? {
                    let path = dir.join(entry.name.to_string());
                    match entry.kind {
                        FileKind::Dir => {
                            copy.create_dir_all(&path)?;
                            stack.push(path);
                        }
                        FileKind::Symlink => {
                            if let Some(parent) = path.parent() {
                                copy.create_dir_all(parent)?;
                            }
                            copy.symlink(&self.host.read_link(&path)?, &path)?;
                        }
                        _ => {
                            let bytes = self.host.read(&path, ReadRange::All { limit: 1 << 24 })?;
                            copy.seed_file(&path, &bytes)?;
                        }
                    }
                }
            }
            let mut world = Self {
                host: copy,
                args: self.args.clone(),
                env: BTreeMap::new(),
            };
            for (key, value) in &self.env {
                world.set_env(key, value.as_deref());
            }
            Ok(world)
        }
    }

    /// Remove the configure durations from the output.
    fn normalize(text: &str) -> String {
        text.lines()
            .map(|line| match line.find(" done (") {
                Some(at) if line.ends_with("s)") => line.get(..at + 5).unwrap_or(line),
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const ROOT: &str = "cmake_minimum_required(VERSION 4.5)\nproject(demo LANGUAGES C)\n\
                        add_subdirectory(leaf)\nadd_subdirectory(other)\n";
    const LEAF: &str = "add_library(leaf STATIC leaf.c)\n";
    const OTHER: &str = "add_executable(app main.c)\n";
    const TOOLCHAIN: &str = "# Toolchain.\nset(CMAKE_C_COMPILER /tools/cc)\n\
                             set(CMAKE_C_COMPILER_LAUNCHER ccache)\n";

    fn sources(root: &str) -> Vec<(&str, String)> {
        vec![
            ("src/CMakeLists.txt", root.to_owned()),
            ("src/leaf/CMakeLists.txt", LEAF.to_owned()),
            (
                "src/leaf/leaf.c",
                "int leaf(void) { return 0; }\n".to_owned(),
            ),
            ("src/other/CMakeLists.txt", OTHER.to_owned()),
            (
                "src/other/main.c",
                "int main(void) { return 0; }\n".to_owned(),
            ),
        ]
    }

    fn base_args() -> Vec<&'static str> {
        vec![
            "-S",
            "src",
            "-B",
            "build",
            "-G",
            "Ninja",
            "-DCMAKE_MAKE_PROGRAM=ninja",
            "-DCMAKE_UNAME=/bin/uname",
        ]
    }

    /// What the mutated replay configure must show.
    #[derive(Debug)]
    enum Expect {
        /// These leaves replay and setup allows replay.
        Hits(&'static [&'static str]),
        /// No listfile replays. The setup miss starts with this text. An empty text
        /// accepts any reason, for example a restart that records again.
        Miss(&'static str),
        /// These leaves replay, and the setup miss starts with this text.
        Partial(&'static [&'static str], &'static str),
        /// Both configures fail.
        Failure,
    }

    /// Run cold, warm, and then one mutation against a replay-disabled copy of the
    /// warm state. Return the replay world after the mutation.
    fn check(
        files: &[(&str, String)],
        args: &[&str],
        env: &[(&str, &str)],
        warm_hits: &[&str],
        mutate: impl Fn(&mut World) -> TestResult,
        expect: Expect,
    ) -> Result<World, Box<dyn std::error::Error>> {
        let files: Vec<(&str, &str)> = files.iter().map(|(n, t)| (*n, t.as_str())).collect();
        let mut world = World::new(&files, args)?;
        for (key, value) in env {
            world.set_env(key, Some(value));
        }
        world.ok()?;
        world.ok()?;
        let warm = world.replay();
        assert_eq!(warm.miss, None, "warm configure: {warm:?}");
        assert_eq!(leaves(&warm), warm_hits, "warm configure: {warm:?}");

        let mut control = world.fork()?;
        control.set_env("RCMAKE_NO_INCREMENTAL", Some("1"));
        mutate(&mut world)?;
        mutate(&mut control)?;
        let replayed = world.configure();
        let live = control.configure();
        assert_eq!(replayed.code, live.code, "{}", replayed.stderr);
        assert_eq!(replayed.stdout, live.stdout);
        assert_eq!(replayed.stderr, live.stderr);
        let names = |outcome: &Outcome| outcome.files.keys().cloned().collect::<Vec<_>>();
        assert_eq!(names(&replayed), names(&live));
        for (name, bytes) in &replayed.files {
            assert_eq!(Some(bytes), live.files.get(name), "{name} differs");
        }
        let report = world.replay();
        match expect {
            Expect::Hits(hits) => {
                assert_eq!(report.miss, None, "{report:?}");
                assert_eq!(leaves(&report), hits, "{report:?}");
            }
            Expect::Miss(reason) => {
                assert!(leaves(&report).is_empty(), "{report:?}");
                assert!(
                    reason.is_empty()
                        || report
                            .miss
                            .as_deref()
                            .is_some_and(|miss| miss.starts_with(reason)),
                    "{report:?}"
                );
            }
            Expect::Failure => assert_ne!(replayed.code, 0),
            Expect::Partial(hits, reason) => {
                assert_eq!(leaves(&report), hits, "{report:?}");
                assert!(
                    report
                        .miss
                        .as_deref()
                        .is_some_and(|miss| miss.starts_with(reason)),
                    "{report:?}"
                );
            }
        }
        Ok(world)
    }

    /// The names of the replayed leaf directories, in order.
    fn leaves(report: &Replay) -> Vec<&str> {
        report
            .replayed
            .iter()
            .filter_map(|path| path.strip_prefix("/work/src/"))
            .filter_map(|path| path.strip_suffix("/CMakeLists.txt"))
            .collect()
    }

    fn toolchain_files(toolchain: &str) -> Vec<(&'static str, String)> {
        let mut files = sources(ROOT);
        files.push(("src/tc.cmake", toolchain.to_owned()));
        files
    }

    fn toolchain_args() -> Vec<&'static str> {
        let mut args = base_args();
        args.push("-DCMAKE_TOOLCHAIN_FILE=tc.cmake");
        args
    }

    fn plain_args() -> Vec<&'static str> {
        let mut args = base_args();
        args.push("-DCMAKE_C_COMPILER=/tools/cc");
        args
    }

    const BOTH: &[&str] = &["leaf", "other"];

    #[test]
    fn m1_toolchain_value_change_misses() -> TestResult {
        check(
            &toolchain_files(TOOLCHAIN),
            &toolchain_args(),
            &[],
            BOTH,
            |w| w.write("src/tc.cmake", &TOOLCHAIN.replace("ccache", "sccache")),
            Expect::Miss("checkpoint 0"),
        )?;
        Ok(())
    }

    #[test]
    fn m2_comment_only_toolchain_edit_hits() -> TestResult {
        check(
            &toolchain_files(TOOLCHAIN),
            &toolchain_args(),
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/tc.cmake",
                    &TOOLCHAIN.replace("Toolchain.", "Edited comment."),
                )
            },
            Expect::Hits(BOTH),
        )?;
        Ok(())
    }

    #[test]
    fn m3_nested_toolchain_include_change_misses() -> TestResult {
        let mut files = toolchain_files(&format!(
            "{TOOLCHAIN}include(${{CMAKE_CURRENT_LIST_DIR}}/tc2.cmake)\n"
        ));
        files.push((
            "src/tc2.cmake",
            "set(CMAKE_POSITION_INDEPENDENT_CODE ON)\n".to_owned(),
        ));
        check(
            &files,
            &toolchain_args(),
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/tc2.cmake",
                    "set(CMAKE_POSITION_INDEPENDENT_CODE OFF)\n",
                )
            },
            Expect::Miss("checkpoint 0"),
        )?;
        Ok(())
    }

    #[test]
    fn m4_toolchain_environment_read_change_misses() -> TestResult {
        check(
            &toolchain_files(&format!(
                "{TOOLCHAIN}set(CMAKE_C_FLAGS_INIT \"$ENV{{TC_FLAGS}}\")\n"
            )),
            &toolchain_args(),
            &[("TC_FLAGS", "-O1")],
            BOTH,
            |w| {
                w.set_env("TC_FLAGS", Some("-O2"));
                Ok(())
            },
            Expect::Miss("environment TC_FLAGS"),
        )?;
        Ok(())
    }

    // A cold configure runs the toolchain twice. Like vcpkg, the override needs an
    // include guard, or the second definition makes `_add_library` call itself.
    const OVERRIDE: &str =
        "include_guard(GLOBAL)\nfunction(add_library)\n  _add_library(${ARGV})\nendfunction()\n";

    #[test]
    fn m5_toolchain_command_override_added_stays_live() -> TestResult {
        check(
            &toolchain_files(TOOLCHAIN),
            &toolchain_args(),
            &[],
            BOTH,
            |w| w.write("src/tc.cmake", &format!("{TOOLCHAIN}{OVERRIDE}")),
            Expect::Miss("checkpoint 0"),
        )?;
        Ok(())
    }

    #[test]
    fn m5_toolchain_command_override_keeps_other_leaves_replayable() -> TestResult {
        // The override makes `add_library()` scripted, so only the leaf without it
        // replays. Removing the override is a setup change.
        check(
            &toolchain_files(&format!("{TOOLCHAIN}{OVERRIDE}")),
            &toolchain_args(),
            &[],
            &["other"],
            |w| w.write("src/tc.cmake", TOOLCHAIN),
            Expect::Miss("checkpoint 0"),
        )?;
        Ok(())
    }

    #[test]
    fn m6_toolchain_global_property_change_misses() -> TestResult {
        let property = "set_property(GLOBAL PROPERTY TARGET_SUPPORTS_SHARED_LIBS FALSE)\n";
        check(
            &toolchain_files(&format!("{TOOLCHAIN}{property}")),
            &toolchain_args(),
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/tc.cmake",
                    &format!("{TOOLCHAIN}{}", property.replace("FALSE", "TRUE")),
                )
            },
            Expect::Miss("checkpoint 0 global"),
        )?;
        Ok(())
    }

    fn preset(cache: &str, environment: &str, toolchain: &str) -> String {
        format!(
            r#"{{"version": 6, "configurePresets": [{{"name": "dev", "generator": "Ninja",
  "binaryDir": "${{sourceDir}}/../build", "toolchainFile": "{toolchain}",
  "cacheVariables": {{"CMAKE_MAKE_PROGRAM": "ninja", "CMAKE_UNAME": "/bin/uname", "OPT": "{cache}"}},
  "environment": {{"FOO": "{environment}"}}}}]}}"#
        )
    }

    fn preset_files() -> Vec<(&'static str, String)> {
        let mut files = toolchain_files(TOOLCHAIN);
        files.push((
            "src/CMakePresets.json",
            preset("ON", "1", "/work/src/tc.cmake"),
        ));
        files.push(("src/tc_copy.cmake", TOOLCHAIN.to_owned()));
        files
    }

    const PRESET_ARGS: &[&str] = &["-S", "src", "--preset", "dev"];

    #[test]
    fn m7_preset_cache_variable_change_misses() -> TestResult {
        check(
            &preset_files(),
            PRESET_ARGS,
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/CMakePresets.json",
                    &preset("OFF", "1", "/work/src/tc.cmake"),
                )
            },
            Expect::Miss("cache OPT"),
        )?;
        Ok(())
    }

    #[test]
    fn m8_preset_environment_change_misses() -> TestResult {
        check(
            &preset_files(),
            PRESET_ARGS,
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/CMakePresets.json",
                    &preset("ON", "2", "/work/src/tc.cmake"),
                )
            },
            Expect::Miss("input preset_environment"),
        )?;
        Ok(())
    }

    #[test]
    fn m9_preset_toolchain_file_change_misses() -> TestResult {
        check(
            &preset_files(),
            PRESET_ARGS,
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/CMakePresets.json",
                    &preset("ON", "1", "/work/src/tc_copy.cmake"),
                )
            },
            Expect::Miss(""),
        )?;
        Ok(())
    }

    #[test]
    fn preset_generator_change_fails_the_same_way() -> TestResult {
        check(
            &preset_files(),
            PRESET_ARGS,
            &[],
            BOTH,
            |w| {
                let text =
                    preset("ON", "1", "/work/src/tc.cmake").replace("Ninja", "Unix Makefiles");
                w.write("src/CMakePresets.json", &text)
            },
            Expect::Failure,
        )?;
        Ok(())
    }

    fn preload_args() -> Vec<&'static str> {
        let mut args = plain_args();
        args.push("-C");
        args.push("init.cmake");
        args
    }

    fn preload_files(init: &str) -> Vec<(&'static str, String)> {
        let mut files = sources(ROOT);
        files.push(("init.cmake", init.to_owned()));
        files
    }

    const INIT: &str = "set(X 1 CACHE STRING \"\" FORCE)\n";

    #[test]
    fn m10_preload_cache_change_misses() -> TestResult {
        check(
            &preload_files(INIT),
            &preload_args(),
            &[],
            BOTH,
            |w| w.write("init.cmake", &INIT.replace('1', "2")),
            Expect::Miss("cache X"),
        )?;
        Ok(())
    }

    #[test]
    fn m11_preload_status_only_edit_hits() -> TestResult {
        check(
            &preload_files(INIT),
            &preload_args(),
            &[],
            BOTH,
            |w| {
                w.write(
                    "init.cmake",
                    &format!("# Comment.\nmessage(STATUS hi)\n{INIT}"),
                )
            },
            Expect::Hits(BOTH),
        )?;
        Ok(())
    }

    #[test]
    fn m12_source_preload_created_misses() -> TestResult {
        check(
            &sources(ROOT),
            &plain_args(),
            &[],
            BOTH,
            |w| w.write("src/PreLoad.cmake", "set(Y 1 CACHE STRING \"\")\n"),
            Expect::Miss("input"),
        )?;
        Ok(())
    }

    #[test]
    fn m13_build_preload_deleted_misses() -> TestResult {
        let mut files = sources(ROOT);
        files.push((
            "build/PreLoad.cmake",
            "set(Y 1 CACHE STRING \"\")\n".to_owned(),
        ));
        check(
            &files,
            &plain_args(),
            &[],
            BOTH,
            |w| {
                w.host.remove_file(Path::new("/work/build/PreLoad.cmake"))?;
                Ok(())
            },
            Expect::Miss("input"),
        )?;
        Ok(())
    }

    #[test]
    fn m14_empty_environment_toolchain_keeps_replay() -> TestResult {
        check(
            &sources(ROOT),
            &plain_args(),
            &[("CMAKE_TOOLCHAIN_FILE", "")],
            BOTH,
            |_| Ok(()),
            Expect::Hits(BOTH),
        )?;
        Ok(())
    }

    #[test]
    fn m15_corrupt_manifest_records_again() -> TestResult {
        let world = check(
            &toolchain_files(TOOLCHAIN),
            &toolchain_args(),
            &[],
            BOTH,
            |w| {
                let manifest = Path::new(BUILD).join("CMakeFiles/rcmake/configure.v1.bin");
                let bytes = w.host.read(&manifest, ReadRange::All { limit: 1 << 24 })?;
                let half = bytes.get(..bytes.len() / 2).unwrap_or_default().to_vec();
                if w.env.contains_key("RCMAKE_NO_INCREMENTAL") {
                    return Ok(());
                }
                w.host.seed_file(&manifest, &half)?;
                Ok(())
            },
            Expect::Hits(&[]),
        )?;
        world.ok()?;
        assert_eq!(leaves(&world.replay()), BOTH);
        Ok(())
    }

    #[test]
    fn m16_listfiles_before_project_never_replay() -> TestResult {
        let root = "cmake_minimum_required(VERSION 4.5)\nadd_subdirectory(early)\n\
                    project(demo LANGUAGES C)\nadd_subdirectory(leaf)\nadd_subdirectory(other)\n";
        let mut files = toolchain_files(TOOLCHAIN);
        files[0] = ("src/CMakeLists.txt", root.to_owned());
        files.push(("src/early/CMakeLists.txt", "set(EARLY 1)\n".to_owned()));
        check(
            &files,
            &toolchain_args(),
            &[],
            BOTH,
            |w| w.write("src/tc.cmake", &TOOLCHAIN.replace("ccache", "sccache")),
            Expect::Miss("checkpoint 0"),
        )?;
        Ok(())
    }

    #[test]
    fn m17_toolchain_initializer_read_by_target_constructor_misses() -> TestResult {
        let mut files = toolchain_files(&format!(
            "{TOOLCHAIN}set(CMAKE_C_VISIBILITY_PRESET hidden)\n"
        ));
        files.push((
            "src/leaf/CMakeLists.txt",
            "add_executable(tool leaf.c)\n".to_owned(),
        ));
        check(
            &files,
            &toolchain_args(),
            &[],
            BOTH,
            |w| {
                w.write(
                    "src/tc.cmake",
                    &format!("{TOOLCHAIN}set(CMAKE_C_VISIBILITY_PRESET default)\n"),
                )
            },
            Expect::Miss("checkpoint 0 variables"),
        )?;
        Ok(())
    }

    #[test]
    fn top_level_include_change_after_the_system_file_misses() -> TestResult {
        let mut files = sources(ROOT);
        let top = "set(CMAKE_C_VISIBILITY_PRESET hidden)\n";
        files.push(("top.cmake", top.to_owned()));
        let mut args = plain_args();
        args.push("-DCMAKE_PROJECT_TOP_LEVEL_INCLUDES=/work/top.cmake");
        check(
            &files,
            &args,
            &[],
            BOTH,
            |w| w.write("top.cmake", &top.replace("hidden", "default")),
            Expect::Miss("checkpoint 0 variables"),
        )?;
        Ok(())
    }

    #[test]
    fn project_include_hook_change_misses() -> TestResult {
        let mut files = sources(ROOT);
        let hook = "set(CMAKE_C_VISIBILITY_PRESET hidden)\n";
        files.push(("hook.cmake", hook.to_owned()));
        let mut args = plain_args();
        args.push("-DCMAKE_PROJECT_demo_INCLUDE=/work/hook.cmake");
        check(
            &files,
            &args,
            &[],
            BOTH,
            |w| w.write("hook.cmake", &hook.replace("hidden", "default")),
            Expect::Miss("checkpoint 0 variables"),
        )?;
        Ok(())
    }

    #[test]
    fn toolchain_symlink_retarget_misses() -> TestResult {
        let files = toolchain_files(TOOLCHAIN);
        let mut files: Vec<_> = files
            .into_iter()
            .filter(|(name, _)| *name != "src/tc.cmake")
            .collect();
        files.push(("src/tc_a.cmake", TOOLCHAIN.to_owned()));
        files.push(("src/tc_b.cmake", TOOLCHAIN.to_owned()));
        let setup = |world: &World| -> TestResult {
            world.host.symlink(
                Path::new("/work/src/tc_a.cmake"),
                Path::new("/work/src/tc.cmake"),
            )?;
            Ok(())
        };
        let files_ref: Vec<(&str, &str)> = files.iter().map(|(n, t)| (*n, t.as_str())).collect();
        // Build the scenario by hand, because the link is not a regular file.
        let world = World::new(&files_ref, &toolchain_args())?;
        setup(&world)?;
        world.ok()?;
        world.ok()?;
        assert_eq!(leaves(&world.replay()), BOTH, "{:?}", world.replay());
        let mut control = world.fork()?;
        control.set_env("RCMAKE_NO_INCREMENTAL", Some("1"));
        for w in [&world, &control] {
            w.host.remove_file(Path::new("/work/src/tc.cmake"))?;
            w.host.symlink(
                Path::new("/work/src/tc_b.cmake"),
                Path::new("/work/src/tc.cmake"),
            )?;
        }
        let (replayed, live) = (world.configure(), control.configure());
        assert_eq!(replayed, live);
        let report = world.replay();
        assert!(leaves(&report).is_empty(), "{report:?}");
        assert!(
            report
                .miss
                .as_deref()
                .is_some_and(|miss| miss.starts_with("input file:toolchain")),
            "{report:?}"
        );
        Ok(())
    }

    #[test]
    fn same_named_toolchain_in_the_build_directory_misses() -> TestResult {
        // A relative selection resolves in the binary directory first. A new file of
        // the same name there changes the resolved setup input.
        check(
            &toolchain_files(TOOLCHAIN),
            &toolchain_args(),
            &[],
            BOTH,
            |w| w.write("build/tc.cmake", TOOLCHAIN),
            Expect::Miss(""),
        )?;
        Ok(())
    }

    #[test]
    fn dependency_provider_change_misses_after_the_call() -> TestResult {
        let root = "cmake_minimum_required(VERSION 4.5)\nproject(demo LANGUAGES C)\n\
                    add_subdirectory(leaf)\nfind_package(Dep REQUIRED)\nadd_subdirectory(other)\n";
        let mut files = sources(root);
        // The provider reads a file in each call, so a change of the file changes
        // only the state after the call.
        let provider = "macro(provide method name)\n  set(${name}_FOUND TRUE)\n\
                        file(READ /work/dep.txt CMAKE_DEP_VALUE)\nendmacro()\n\
                        cmake_language(SET_DEPENDENCY_PROVIDER provide SUPPORTED_METHODS FIND_PACKAGE)\n";
        files.push(("provider.cmake", provider.to_owned()));
        files.push(("dep.txt", "1".to_owned()));
        let mut args = plain_args();
        args.push("-DCMAKE_PROJECT_TOP_LEVEL_INCLUDES=/work/provider.cmake");
        check(
            &files,
            &args,
            &[],
            BOTH,
            |w| w.write("dep.txt", "2"),
            Expect::Partial(&["leaf"], "checkpoint 1 variables"),
        )?;
        Ok(())
    }

    #[test]
    fn replay_setup_opt_out_keeps_the_toolchain_gate() -> TestResult {
        let world = World::new(
            &[
                ("src/CMakeLists.txt", ROOT),
                ("src/leaf/CMakeLists.txt", LEAF),
                ("src/leaf/leaf.c", ""),
                ("src/other/CMakeLists.txt", OTHER),
                ("src/other/main.c", ""),
                ("src/tc.cmake", TOOLCHAIN),
            ],
            &toolchain_args(),
        )?;
        let mut world = world;
        world.set_env("RCMAKE_REPLAY_SETUP", Some("0"));
        world.ok()?;
        world.ok()?;
        assert!(leaves(&world.replay()).is_empty());
        world.set_env("RCMAKE_REPLAY_SETUP", None);
        world.ok()?;
        world.ok()?;
        assert_eq!(leaves(&world.replay()), BOTH);
        Ok(())
    }

    #[test]
    fn replayed_ambiguous_sources_match_a_live_configure() -> TestResult {
        // `CMP0115` `OLD` resolves `a` to `a.c` through the source registration order.
        let root = "cmake_minimum_required(VERSION 3.10)\nproject(demo LANGUAGES C)\n\
                    add_subdirectory(leaf)\nadd_subdirectory(other)\n";
        let mut files = sources(root);
        files[1] = (
            "src/leaf/CMakeLists.txt",
            "add_library(leaf STATIC a /work/src/leaf/b.c a.c)\n".to_owned(),
        );
        files.push(("src/leaf/a.c", "int a(void) { return 0; }\n".to_owned()));
        files.push(("src/leaf/b.c", "int b(void) { return 0; }\n".to_owned()));
        check(
            &files,
            &plain_args(),
            &[],
            BOTH,
            |_| Ok(()),
            Expect::Hits(BOTH),
        )?;
        Ok(())
    }
}
