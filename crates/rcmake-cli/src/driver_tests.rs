use std::path::Path;
use std::sync::{Arc, Mutex};

use rcmake_base::BString;
use rcmake_host::{Host, ProcessResult, PureHost, ReadRange, RunOutput, Stream};

use crate::run;

fn argv(args: &[&str]) -> Vec<BString> {
    args.iter().copied().map(BString::from).collect()
}

fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
    run(&argv(args), host.clone())
}

#[test]
fn configure_stages_install_export_separately_from_build_export() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"cmake_minimum_required(VERSION 3.10)
project(example NONE)
add_library(headers INTERFACE)
install(TARGETS headers EXPORT example)
install(EXPORT example DESTINATION lib/cmake/example FILE targets.cmake)
export(EXPORT example FILE targets.cmake)
file(GENERATE OUTPUT "config-$<CONFIG>.txt" CONTENT "$<TARGET_PROPERTY:TYPE>" TARGET headers)
"#,
    )
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "cmake",
                "-S",
                ".",
                "-B",
                "out",
                "-G",
                "Ninja",
                "-DCMAKE_INSTALL_PREFIX=/packages/example",
                "-DCMAKE_BUILD_TYPE=Release",
            ]
        ),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let read = |file| {
        let file = Path::new("/work/out").join(file);
        host.read(&file, ReadRange::All { limit: 16384 }).unwrap()
    };
    let script = BString::from(read("cmake_install.cmake")).to_string();
    let staged_path = "CMakeFiles/Export/0/targets.cmake";
    assert!(script.contains("\"/work/out/CMakeFiles/Export/0/targets.cmake\""));
    assert!(script.contains("set(CMAKE_INSTALL_PREFIX \"/packages/example\")"));
    assert!(script.contains("set(CMAKE_INSTALL_CONFIG_NAME \"Release\")"));
    let installed = BString::from(read(staged_path)).to_string();
    assert!(installed.contains("get_filename_component(_IMPORT_PREFIX"));
    let build = BString::from(read("targets.cmake")).to_string();
    assert!(!build.contains("get_filename_component(_IMPORT_PREFIX"));
    assert_eq!(read("config-Release.txt"), b"INTERFACE_LIBRARY");
}

#[test]
fn configure_enables_debug_trycompile_for_flag_and_preset() {
    for args in [
        vec![
            "cmake",
            "-S",
            ".",
            "-B",
            "out",
            "-G",
            "Ninja",
            "--debug-trycompile",
        ],
        vec!["cmake", "--preset=debug"],
    ] {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            br#"cmake_minimum_required(VERSION 3.10)
project(example NONE)
if(NOT CMAKE_DEBUG_TRY_COMPILE)
  message(FATAL_ERROR "Expected CMAKE_DEBUG_TRY_COMPILE=ON.")
endif()
"#,
        )
        .unwrap();
        host.seed_file(
            Path::new("/work/CMakePresets.json"),
            br#"{"version": 6, "configurePresets": [
              {"name": "debug", "generator": "Ninja", "binaryDir": "out", "debug": {"tryCompile": true}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            invoke(&host, &args),
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
    }
}

#[test]
fn build_ninja_multi_config_streams_output_and_cleans_first() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/out/CMakeCache.txt"),
        b"CMAKE_GENERATOR:INTERNAL=Ninja Multi-Config\nCMAKE_MAKE_PROGRAM:FILEPATH=/tools/ninja\n",
    )
    .unwrap();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let requests = commands.clone();
    host.set_command_handler(move |_, request, write| {
        assert_eq!(request.cwd, Path::new("/work/out"));
        assert!(request.tee);
        requests.lock().unwrap().extend(request.pipeline.clone());
        write(Stream::Stdout, b"build output\n");
        write(Stream::Stderr, b"build diagnostic\n");
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "cmake",
                "--build",
                "out",
                "-j4",
                "--target",
                "one",
                "two",
                "--config=Release",
                "--clean-first",
                "-v",
                "--resolve-package-references=OFF",
                "--",
                "-k",
                "0",
            ]
        ),
        0
    );
    let requests = commands.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let mut stages = requests.iter();
    let clean = stages.next().unwrap();
    let build = stages.next().unwrap();
    let common = [
        "-C",
        "/work/out",
        "-f",
        "build-Release.ninja",
        "-j",
        "4",
        "-v",
    ];
    assert_eq!(clean.program, "/tools/ninja");
    assert_eq!(build.program, "/tools/ninja");
    assert_eq!(
        clean.args,
        argv(&[common.as_slice(), &["clean", "-k", "0"]].concat())
    );
    assert_eq!(
        build.args,
        argv(&[common.as_slice(), &["one", "two", "-k", "0"]].concat())
    );
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        "build output\nbuild output\n"
    );
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "build diagnostic\nbuild diagnostic\n"
    );
}

#[test]
fn build_make_preset_uses_cli_overrides_and_inherits_environment() {
    let host = Arc::new(PureHost::default());
    host.env_set(b"REMOVE", Some(b"parent"));
    host.seed_file(
        Path::new("/work/CMakePresets.json"),
        br#"{
          "version": 6,
          "configurePresets": [{
            "name": "config", "generator": "Unix Makefiles", "binaryDir": "${sourceDir}/out",
            "environment": {"INHERITED": "from-config"}
          }],
          "buildPresets": [{
            "name": "build", "configurePreset": "config", "jobs": 2,
            "targets": ["preset-target"], "verbose": true, "nativeToolOptions": ["-k"],
            "environment": {"REMOVE": null}
          }]
        }"#,
    )
    .unwrap();
    host.seed_file(
        Path::new("/work/out/CMakeCache.txt"),
        b"CMAKE_GENERATOR:INTERNAL=Unix Makefiles\nCMAKE_MAKE_PROGRAM:FILEPATH=/tools/make\n",
    )
    .unwrap();
    host.set_command_handler(|_, request, _| {
        let command = request.pipeline.first().unwrap();
        assert_eq!(command.program, "/tools/make");
        assert_eq!(
            command.args,
            argv(&[
                "-C",
                "/work/out",
                "-j",
                "3",
                "VERBOSE=1",
                "cli-target",
                "-k",
                "-n"
            ])
        );
        assert!(
            request
                .env
                .iter()
                .any(|(key, value)| key == "INHERITED" && value == "from-config")
        );
        assert!(!request.env.iter().any(|(key, _)| key == "REMOVE"));
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(2)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "cmake",
                "--build",
                "--preset=build",
                "--parallel=3",
                "-t",
                "cli-target",
                "--",
                "-n"
            ]
        ),
        2
    );
    assert_eq!(host.env_get(b"REMOVE"), Some("parent".into()));
}

#[test]
fn build_stops_after_a_failed_clean() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/CMakeCache.txt"),
        b"CMAKE_GENERATOR:INTERNAL=Ninja\n",
    )
    .unwrap();
    host.env_set(b"CMAKE_BUILD_PARALLEL_LEVEL", Some(b"5"));
    host.set_command_handler(|_, request, _| {
        let command = request.pipeline.first().unwrap();
        assert_eq!(command.program, "ninja");
        assert_eq!(command.args, argv(&["-C", "/work", "-j", "5", "clean"]));
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(9)],
            ..RunOutput::default()
        })
    })
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &["cmake", "--build", ".", "--clean-first", "-t", "all"]
        ),
        9
    );
}

#[test]
fn build_reports_cache_and_argument_errors() {
    for (args, cache, error) in [
        (&["--build"][..], None, "Usage: cmake --build <dir>"),
        (
            &["--build", "--target=one"],
            None,
            "Usage: cmake --build <dir>",
        ),
        (
            &["--build", "."],
            None,
            "Error: not a CMake build directory (missing CMakeCache.txt)",
        ),
        (
            &["--build", "absent"],
            None,
            "Error: /work/absent is not a directory",
        ),
        (
            &["--build", "."],
            Some("UNRELATED:STRING=ignored\n"),
            "Error: could not find CMAKE_GENERATOR in Cache",
        ),
        (
            &["--build", "."],
            Some("CMAKE_GENERATOR:INTERNAL=Bad Generator\n"),
            "Error: could not create CMAKE_GENERATOR \"Bad Generator\"",
        ),
        (
            &["--build", ".", "--targe"],
            None,
            "Unknown argument --targe. Did you mean: --target?",
        ),
        (
            &["--build", ".", "--parallel12ab"],
            None,
            "'--parallel' invalid number '12ab' given.",
        ),
        (
            &["--build", ".", "-j", "0"],
            None,
            "The <jobs> value requires a positive integer argument.",
        ),
        (
            &["--build", ".", "--target", "all", "clean"],
            None,
            "Error: Building 'clean' and other targets together is not supported.",
        ),
    ] {
        let host = Arc::new(PureHost::default());
        if let Some(cache) = cache {
            host.seed_file(Path::new("/work/CMakeCache.txt"), cache.as_bytes())
                .unwrap();
        }
        let command = [&["cmake"][..], args].concat();
        assert_eq!(invoke(&host, &command), 1, "{command:?}");
        assert!(
            host.stream_bytes(Stream::Stderr)
                .starts_with(error.as_bytes()),
            "{command:?}"
        );
    }
}

#[test]
fn install_executes_script_for_each_component_with_destdir_and_options() {
    let host = Arc::new(PureHost::default());
    host.env_set(b"DESTDIR", Some(b"/stage"));
    host.seed_file(
        Path::new("/work/out/cmake_install.cmake"),
        br#"file(WRITE "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/${CMAKE_INSTALL_COMPONENT}.txt"
          "${CMAKE_INSTALL_CONFIG_NAME}|${CMAKE_INSTALL_DO_STRIP}|${CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS}")"#,
    )
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "cmake",
                "--install",
                "out",
                "--prefix",
                "/pkg",
                "--strip",
                "--config=Release",
                "--component",
                "runtime",
                "dev",
                "--default-directory-permissions=u=rwx,g=rx,o=rx",
                "--parallel",
                "2",
                "-v",
            ]
        ),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    for path in ["/stage/pkg/runtime.txt", "/stage/pkg/dev.txt"] {
        let data = host
            .read(Path::new(path), ReadRange::All { limit: 128 })
            .unwrap();
        assert_eq!(data, b"Release|1|OWNER_READ;OWNER_WRITE;OWNER_EXECUTE;GROUP_READ;GROUP_EXECUTE;WORLD_READ;WORLD_EXECUTE");
    }
}

#[test]
fn install_validates_directory_and_flags() {
    for (args, error) in [
        (&["--install"][..], "Usage: cmake --install <dir>"),
        (
            &["-B", "out", "--install-prefix"],
            "CMake Error: No install directory specified for --install-prefix",
        ),
        (
            &["--install", "absent"],
            "CMake Error: Not a file: /work/absent/cmake_install.cmake",
        ),
        (
            &["--install", ".", "--component"],
            "CMake Error: Invalid value used with --component",
        ),
        (
            &["--install", ".", "--compnent"],
            "Unknown argument --compnent. Did you mean: --component?",
        ),
        (
            &[
                "--install",
                ".",
                "--default-directory-permissions",
                "u=rwx,",
            ],
            "--default-directory-permissions is in incorrect format",
        ),
    ] {
        let host = Arc::new(PureHost::default());
        assert_eq!(invoke(&host, &[&["cmake"][..], args].concat()), 1);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .starts_with(error.as_bytes()),
            "{args:?}: {}",
            host.stream_bytes(Stream::Stderr)
        );
    }
}

#[test]
fn ctest_persona_calls_runner_with_supported_options() {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/out/CTestTestfile.cmake"), b"")
        .unwrap();
    let code = invoke(
        &host,
        &[
            "ctest",
            "--test-dir",
            "out",
            "-C",
            "Release",
            "-R",
            "include",
            "-E",
            "exclude",
            "-j3",
            "--output-on-failure",
        ],
    );
    // The frozen runner reports Unimplemented until its wave lands.
    let stderr = host.stream_bytes(Stream::Stderr);
    if stderr
        .windows(b"not implemented yet".len())
        .any(|text| text == b"not implemented yet")
    {
        assert_eq!(code, 1);
        assert!(stderr.starts_with(b"CMake Error: the ctest runner"));
    } else {
        assert_eq!(code, 0, "{stderr}");
    }
}

#[test]
fn regeneration_flag_uses_configure_and_view_skips_generation() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"message(STATUS \"evaluated\")\n",
    )
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "cmake",
                "--regenerate-during-build",
                "-S",
                ".",
                "-B",
                "out",
                "-N"
            ]
        ),
        0
    );
    assert!(host.stream_bytes(Stream::Stdout).is_empty());
    assert_eq!(
        invoke(
            &host,
            &["cmake", "--regenerate-during-build", "-S", ".", "-B", "out"]
        ),
        0
    );
    assert!(
        host.stream_bytes(Stream::Stdout)
            .starts_with(b"-- evaluated\n-- Configuring done")
    );
}

#[test]
fn capabilities_advertises_supported_generators_and_file_api() {
    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["cmake", "-E", "capabilities"]), 0);
    let json = host.stream_bytes(Stream::Stdout);
    for field in [
        "Ninja",
        "Ninja Multi-Config",
        "Unix Makefiles",
        "requests",
        "codemodel",
        "configureLog",
        "cache",
        "cmakeFiles",
        "toolchains",
    ] {
        let field = format!("\"{field}\"");
        assert!(
            json.windows(field.len())
                .any(|window| window == field.as_bytes())
        );
    }
}
