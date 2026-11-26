use std::path::Path;
use std::sync::{Arc, Mutex};

use rcmake_base::BString;
use rcmake_host::{Host, ProcessResult, PureHost, ReadRange, RunOutput, Stream};

use crate::run;

#[path = "../tests/support/project_modules.rs"]
mod project_modules;

pub(crate) fn project_host() -> Arc<PureHost> {
    let host = Arc::new(PureHost::default());
    project_modules::seed(&host).unwrap();
    host
}

#[test]
fn configure_reports_a_missing_cmake_root() {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"project(example NONE)\n",
    )
    .unwrap();
    assert_eq!(invoke(&host, &["cmake", "-S", ".", "-B", "out"]), 1);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        b"CMake Error: Could not find CMAKE_ROOT !!!\nCMake has most likely not been installed correctly.\nModules directory not found in\n/usr/local/share/cmake-4.5\nCMake Error: Error executing cmake::LoadCache(). Aborting.\n\n",
    );
}

#[test]
fn skip_install_rules_suppresses_the_install_script() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 3.10)\nproject(example NONE)\ninstall(FILES CMakeLists.txt DESTINATION doc)\n",
    ).unwrap();
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
                "-DCMAKE_SKIP_INSTALL_RULES=ON"
            ]
        ),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr),
    );
    assert!(
        host.metadata(
            Path::new("/work/out/cmake_install.cmake"),
            rcmake_host::Follow::Yes
        )
        .is_err()
    );
    assert!(host.stream_bytes(Stream::Stderr).starts_with(
        b"CMake Warning in CMakeLists.txt:\n  CMAKE_SKIP_INSTALL_RULES was enabled even though installation rules have\n  been specified\n"
    ));
}

fn argv(args: &[&str]) -> Vec<BString> {
    args.iter().copied().map(BString::from).collect()
}

fn invoke(host: &Arc<PureHost>, args: &[&str]) -> i32 {
    run(&argv(args), host.clone())
}

#[test]
fn configure_stages_install_export_separately_from_build_export() {
    let host = project_host();
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
    let digest = rcmake_base::hash::HashAlgo::Md5.hex_digest(b"lib/cmake/example");
    let staged_path = format!("CMakeFiles/Export/{digest}/targets.cmake");
    assert!(script.contains(&format!("\"/work/out/{staged_path}\"")));
    assert!(script.contains("set(CMAKE_INSTALL_PREFIX \"/packages/example\")"));
    assert!(script.contains("set(CMAKE_INSTALL_CONFIG_NAME \"Release\")"));
    let installed = BString::from(read(&staged_path)).to_string();
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
        let host = project_host();
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
    let host = project_host();
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
    let common = ["-f", "build-Release.ninja", "-j", "4", "-v"];
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
    let host = project_host();
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
        assert_eq!(request.cwd, Path::new("/work/out"));
        assert_eq!(command.program, "/tools/make");
        assert_eq!(
            command.args,
            argv(&["-f", "Makefile", "-j", "3", "VERBOSE=1", "cli-target", "-n"])
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
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeCache.txt"),
        b"CMAKE_GENERATOR:INTERNAL=Ninja\n",
    )
    .unwrap();
    host.env_set(b"CMAKE_BUILD_PARALLEL_LEVEL", Some(b"5"));
    host.set_command_handler(|_, request, _| {
        let command = request.pipeline.first().unwrap();
        assert_eq!(command.program, "ninja");
        assert_eq!(command.args, argv(&["-j", "5", "clean"]));
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
        let host = project_host();
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
    let host = project_host();
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
        let host = project_host();
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
    let host = project_host();
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
    assert_eq!(code, 0, "{}", host.stream_bytes(Stream::Stderr));
}

#[test]
fn regeneration_flag_uses_configure_and_view_skips_generation() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 3.10)\nproject(P NONE)\nmessage(STATUS \"evaluated\")\n",
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
    let host = project_host();
    assert_eq!(invoke(&host, &["cmake", "-E", "capabilities"]), 0);
    let json = host.stream_bytes(Stream::Stdout);
    assert!(String::from_utf8_lossy(&json).contains("\"tls\":true"));
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

#[test]
fn generator_host_gate_and_help_list_follow_generator_kind() {
    let host = project_host();
    host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"")
        .expect("seed");
    let code = invoke(
        &host,
        &[
            "cmake",
            "-G",
            "Visual Studio 17 2022",
            "-S",
            "/work/src",
            "-B",
            "/work/vs",
        ],
    );
    let vs_stderr = host.stream_bytes(Stream::Stderr).to_string();
    if cfg!(any(windows, feature = "vs-cross")) {
        assert!(!vs_stderr.contains("Could not create named generator"));
    } else {
        assert_eq!(code, 1);
        assert!(
            vs_stderr
                .contains("CMake Error: Could not create named generator Visual Studio 17 2022")
        );
    }
    let host = project_host();
    assert_eq!(invoke(&host, &["cmake", "-G", "invalid-generator"]), 1);
    let stderr = host.stream_bytes(Stream::Stderr);
    let contains = |needle: &str| {
        stderr
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    assert_eq!(
        contains("Visual Studio 18 2026"),
        cfg!(any(windows, feature = "vs-cross"))
    );
    // The generator list after the error has the oracle order.
    let order = ["Unix Makefiles", "Ninja", "Ninja Multi-Config"];
    let positions: Vec<usize> = order
        .iter()
        .filter_map(|name| {
            let needle = format!(" {name} ");
            stderr
                .windows(needle.len())
                .position(|window| window == needle.as_bytes())
        })
        .collect();
    assert_eq!(positions.len(), order.len());
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(contains(" Xcode "), cfg!(target_os = "macos"));
}

#[test]
fn warns_about_uninitialized_variables_in_the_build_cmakefiles_directory() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(sample NONE)
file(WRITE "${CMAKE_BINARY_DIR}/CMakeFiles/check.cmake" [=[message(STATUS "${UNDEFINED_IN_BUILD_CMAKEFILES}")]=])
include("${CMAKE_BINARY_DIR}/CMakeFiles/check.cmake")
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
                "--warn-uninitialized"
            ]
        ),
        0
    );
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains("uninitialized variable 'UNDEFINED_IN_BUILD_CMAKEFILES'"),
        "{stderr}"
    );
}

#[test]
fn help_shows_extensions_only_for_native_persona() {
    for (name, native) in [("cmake", false), ("rcmake", true)] {
        let host = Arc::new(PureHost::default());
        assert_eq!(invoke(&host, &[name, "--help"]), 0);
        let output = host.stream_bytes(Stream::Stdout);
        let heading = b"rcmake extensions";
        assert_eq!(
            output.windows(heading.len()).any(|line| line == heading),
            native
        );
        let usage = if native {
            b"  rcmake [options]".as_slice()
        } else {
            b"  cmake [options]".as_slice()
        };
        assert!(output.windows(usage.len()).any(|line| line == usage));
    }
}

#[test]
fn usage_errors_and_preset_typos_follow_persona() -> rcmake_host::HostResult<()> {
    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["rcmake", "--bogus"]), 1);
    let native_err = host.stream_bytes(Stream::Stderr).to_string();
    assert!(native_err.contains("'rcmake --help'"), "{native_err}");
    assert!(!native_err.contains("'cmake --help'"), "{native_err}");

    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["cmake", "--bogus"]), 1);
    let compat_err = host.stream_bytes(Stream::Stderr).to_string();
    assert!(compat_err.contains("'cmake --help'"), "{compat_err}");
    assert!(!compat_err.contains("'rcmake --help'"), "{compat_err}");

    let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let presets = br#"{
  "version": 3,
  "configurePresets": [
    {"name": "dev", "generator": "Ninja", "binaryDir": "build/dev"}
  ]
}"#;
    for (argv0, expect_suggestion) in [("rcmake", true), ("cmake", false)] {
        let host = Arc::new(PureHost::new(root.into(), root.join(argv0)));
        let data = root.join("cmake-data");
        host.seed_file(&data.join("Modules/CMakeDetermineSystem.cmake"), b"")?;
        host.env_set(
            b"RCMAKE_VENDORED_DATA_ROOT",
            Some(data.to_string_lossy().as_bytes()),
        );
        host.seed_file(&root.join("CMakePresets.json"), presets)?;
        assert_eq!(invoke(&host, &[argv0, "--preset", "devv"]), 1);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert_eq!(
            stderr.contains("Did you mean \"dev\"?"),
            expect_suggestion,
            "{argv0}: {stderr}"
        );
    }
    Ok(())
}

#[test]
fn oracle_mode_rejects_native_diagnostics_flag() {
    let host = Arc::new(PureHost::default());
    host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
    assert_eq!(
        invoke(
            &host,
            &["rcmake", "--diagnostics=rich", "-S", ".", "-B", "out"]
        ),
        1
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Unknown argument --diagnostics=rich")
    );
}

#[test]
fn native_profile_flag_obeys_script_argument_boundary() -> rcmake_host::HostResult<()> {
    let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let host = Arc::new(PureHost::new(root.into(), root.join("rcmake")));
    host.seed_file(
        &root.join("args.cmake"),
        b"message(\"Script argument: ${CMAKE_ARGV3}\")\n",
    )?;
    assert_eq!(
        invoke(&host, &["rcmake", "--profile", "-P", "args.cmake"]),
        1
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("--profile allowed only when configuring a project")
    );
    assert_eq!(
        invoke(&host, &["rcmake", "-P", "args.cmake", "--profile"]),
        0
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Script argument: --profile")
    );
    Ok(())
}

#[test]
fn strict_hermetic_configure_exits_on_undeclared_read() -> rcmake_host::HostResult<()> {
    let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let host = Arc::new(PureHost::new(root.into(), root.join("rcmake")));
    let data = root.join("cmake-data");
    let ninja = root.join("tools/ninja");
    host.seed_file(&ninja, b"")?;
    host.set_permissions(&ninja, 0o755)?;
    let ninja_cmake = rcmake_host::path::from_native_cmake(&ninja);
    let find_ninja = format!("set(CMAKE_MAKE_PROGRAM \"{ninja_cmake}\" CACHE FILEPATH \"\")\n");
    for (file, text) in [
        ("CMakeDetermineSystem.cmake", ""),
        ("CMakeSystemSpecificInitialize.cmake", ""),
        ("CMakeSystemSpecificInformation.cmake", ""),
        ("CMakeNinjaFindMake.cmake", find_ninja.as_str()),
    ] {
        host.seed_file(&data.join("Modules").join(file), text.as_bytes())?;
    }
    host.env_set(
        b"RCMAKE_HERMETIC_ALLOW_PROGRAMS",
        Some(&rcmake_host::path::from_native(&ninja)),
    );
    host.env_set(
        b"RCMAKE_VENDORED_DATA_ROOT",
        Some(data.to_string_lossy().as_bytes()),
    );
    host.set_command_handler(|_, request, _| {
        assert_eq!(
            request.pipeline[0].args.as_slice(),
            [BString::from("--version")]
        );
        Ok(rcmake_host::RunOutput {
            results: vec![rcmake_host::ProcessResult::Exited(0)],
            stdout: b"1.12.1\n".to_vec(),
            stderr: Vec::new(),
        })
    })?;
    host.seed_file(&root.join("CMakeLists.txt"), b"cmake_minimum_required(VERSION 4.5)\nproject(Audit NONE)\nif(DEFINED ENV{RCMAKE_TEST_UNDECLARED})\nendif()\n")?;
    host.env_set(b"RCMAKE_TEST_UNDECLARED", Some(b"input"));
    assert_eq!(
        invoke(
            &host,
            &[
                "rcmake",
                "--hermetic=strict",
                "-S",
                ".",
                "-B",
                "out",
                "-G",
                "Ninja"
            ]
        ),
        1
    );
    let output = host.stream_bytes(Stream::Stderr).to_string();
    assert!(output.contains("RCMAKE_HERMETIC_ENV"), "{output}");
    Ok(())
}

#[test]
fn find_package_mode_runs_the_module_with_the_find_package_role() {
    let host = project_host();
    host.seed_file(
        Path::new("/usr/local/share/cmake-4.5/Modules/CMakeFindPackageMode.cmake"),
        b"get_property(role GLOBAL PROPERTY CMAKE_ROLE)\nif(role STREQUAL \"FIND_PACKAGE\" AND NAME STREQUAL \"Found\")\n  set(PACKAGE_FOUND TRUE)\nendif()\n",
    )
    .unwrap();
    let args = ["cmake", "--find-package", "-DNAME=Found", "-DMODE=EXIST"];
    assert_eq!(invoke(&host, &args), 0);
    assert_eq!(host.stream_bytes(Stream::Stdout), b"Found found.\n");
    let host = project_host();
    host.seed_file(
        Path::new("/usr/local/share/cmake-4.5/Modules/CMakeFindPackageMode.cmake"),
        b"set(PACKAGE_FOUND FALSE)\n",
    )
    .unwrap();
    let args = ["cmake", "--find-package", "-DNAME=Missing", "-DMODE=EXIST"];
    assert_eq!(invoke(&host, &args), 1);
    assert_eq!(host.stream_bytes(Stream::Stdout), b"Missing not found.\n");
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        b"CMake Error: Run 'cmake --help' for all supported options.\n"
    );
}

#[test]
fn compile_no_warning_as_error_records_the_ignore_entry() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 3.10)\nproject(example NONE)\n",
    )
    .unwrap();
    let args = [
        "cmake",
        "-S",
        ".",
        "-B",
        "out",
        "--compile-no-warning-as-error",
    ];
    assert_eq!(invoke(&host, &args), 0);
    assert!(host.stream_bytes(Stream::Stdout).starts_with(
        b"Ignoring COMPILE_WARNING_AS_ERROR target property and CMAKE_COMPILE_WARNING_AS_ERROR variable.\n"
    ));
    let cache = host
        .read(
            Path::new("/work/out/CMakeCache.txt"),
            rcmake_host::ReadRange::All { limit: 1 << 20 },
        )
        .unwrap();
    assert!(
        cache
            .windows(b"_RCMAKE_NO_WARNING_AS_ERROR:INTERNAL=ON".len())
            .any(|window| window == b"_RCMAKE_NO_WARNING_AS_ERROR:INTERNAL=ON")
    );
}

#[test]
fn wave8a_cache_records_generator_metadata_and_omits_placeholders() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(Example NONE)
cmake_policy(SET CMP0102 OLD)
mark_as_advanced(FORCE ABSENT PRESENT)
set(PRESENT "value" CACHE STRING "")
add_subdirectory(sub)
"#,
    )
    .unwrap();
    host.seed_file(Path::new("/work/sub/CMakeLists.txt"), b"")
        .unwrap();
    assert_eq!(
        invoke(&host, &["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
        0
    );
    let cache = host
        .read(
            Path::new("/work/out/CMakeCache.txt"),
            ReadRange::All { limit: 16384 },
        )
        .unwrap();
    let cache = String::from_utf8(cache).unwrap();
    assert!(!cache.contains("ABSENT"), "{cache}");
    let diagnostic_init = cache
        .lines()
        .find_map(|line| line.strip_prefix("CMAKE_DIAGNOSTIC_INIT:INTERNAL="))
        .unwrap();
    let categories: Vec<_> = diagnostic_init
        .split(';')
        .filter_map(|entry| entry.split_once('=').map(|(name, _)| name))
        .collect();
    assert_eq!(
        categories,
        [
            "CMD_AUTHOR",
            "CMD_DEPRECATED",
            "CMD_EXPERIMENTAL",
            "CMD_STRICT",
            "CMD_INSTALL_ABSOLUTE_DESTINATION",
            "CMD_NON_TARGET_DIRECTIVE",
            "CMD_POLICY",
            "CMD_UNINITIALIZED",
            "CMD_UNUSED_CLI",
        ]
    );
    for entry in [
        "CMAKE_EXTRA_GENERATOR:INTERNAL=",
        "CMAKE_GENERATOR_INSTANCE:INTERNAL=",
        "CMAKE_GENERATOR_PLATFORM:INTERNAL=",
        "CMAKE_GENERATOR_TOOLSET:INTERNAL=",
        "CMAKE_NUMBER_OF_MAKEFILES:INTERNAL=2",
        "PRESENT:STRING=value",
        "PRESENT-ADVANCED:INTERNAL=1",
    ] {
        assert!(
            cache.lines().any(|line| line == entry),
            "Missing {entry} in {cache}"
        );
    }
}

#[test]
fn wave8a_failed_configure_saves_cache() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(Example NONE)
set(RESULT pending CACHE STRING "")
message(FATAL_ERROR "Stop")
"#,
    )
    .unwrap();
    assert_eq!(
        invoke(&host, &["cmake", "-S", ".", "-B", "out", "-G", "Ninja"]),
        1
    );
    let cache = host
        .read(
            Path::new("/work/out/CMakeCache.txt"),
            ReadRange::All { limit: 16384 },
        )
        .unwrap();
    assert!(
        String::from_utf8(cache)
            .unwrap()
            .lines()
            .any(|line| line == "RESULT:STRING=pending")
    );
}

#[test]
fn wave8a_variables_property_includes_normal_and_cache_bindings() {
    let host = project_host();
    host.seed_file(
        Path::new("/work/check.cmake"),
        br#"
cmake_minimum_required(VERSION 4.5)
set(WAVE8A_CACHE 1 CACHE STRING "")
set(WAVE8A_CACHE 2)
get_property(names DIRECTORY PROPERTY VARIABLES)
list(FILTER names INCLUDE REGEX "^WAVE8A_CACHE$")
if(NOT names STREQUAL "WAVE8A_CACHE;WAVE8A_CACHE")
  message(FATAL_ERROR "Incorrect variable names: ${names}")
endif()
"#,
    )
    .unwrap();
    assert_eq!(
        invoke(&host, &["cmake", "-P", "/work/check.cmake"]),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
}

#[test]
fn native_subcommands_dispatch_ctest_and_cpack_and_respect_oracle_mode() {
    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["rcmake", "ctest", "--version"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        concat!(
            "ctest version 4.5.1\nrcmake ",
            env!("CARGO_PKG_VERSION"),
            "\n"
        )
        .as_bytes()
    );

    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["rcmake", "cpack", "--version"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        concat!(
            "cpack version 4.5.1\nrcmake ",
            env!("CARGO_PKG_VERSION"),
            "\n"
        )
        .as_bytes()
    );

    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["rcmake", "cpack", "-V", "--version"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout),
        concat!(
            "cpack version 4.5.1\nrcmake ",
            env!("CARGO_PKG_VERSION"),
            "\n"
        )
        .as_bytes()
    );

    // In oracle mode `ctest` is not a subcommand, so a CTest option is unknown.
    let host = Arc::new(PureHost::default());
    host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
    assert_eq!(invoke(&host, &["rcmake", "ctest", "--show-only"]), 1);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Unknown argument --show-only")
    );

    // As with `cmake ctest --version`, the `cmake` version banner prints.
    let host = Arc::new(PureHost::default());
    host.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
    assert_eq!(invoke(&host, &["rcmake", "ctest", "--version"]), 0);
    assert!(
        host.stream_bytes(Stream::Stdout)
            .starts_with(b"cmake version 4.5.1\n")
    );
}

#[test]
fn native_help_uses_consistent_rcmake_branding_and_documents_subcommands() {
    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["rcmake", "--help"]), 0);
    let native_help = host.stream_bytes(Stream::Stdout).to_string();
    assert!(!native_help.contains("Rcmake"), "{native_help}");
    for expected in [
        "  rcmake [options]",
        "\"rcmake -E\"",
        "\"rcmake --build\"",
        "\"rcmake --install\"",
        "rcmake ctest [options]",
        "rcmake cpack [options]",
        "rcmake lsp [options]",
        "RCMAKE_ORACLE_MODE=1",
    ] {
        assert!(
            native_help.contains(expected),
            "missing {expected} in:\n{native_help}"
        );
    }

    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["cmake", "--help"]), 0);
    let compat_help = host.stream_bytes(Stream::Stdout).to_string();
    assert!(compat_help.contains("\"cmake -E\""), "{compat_help}");
    assert!(!compat_help.contains("rcmake extensions"), "{compat_help}");
}

#[test]
fn native_workflow_preserves_native_persona_during_configure() {
    let presets = br#"{
  "version": 6,
  "configurePresets": [
    {"name": "cfg", "generator": "Ninja", "binaryDir": "out"}
  ],
  "workflowPresets": [
    {"name": "wf", "steps": [{"type": "configure", "name": "cfg"}]}
  ]
}"#;
    let lists =
        b"cmake_minimum_required(VERSION 4.5)\nproject(Flow NONE)\nmessage(FATAL_ERROR \"boom\")\n";

    let host = project_host();
    host.env_set(b"RCMAKE_DIAGNOSTICS", Some(b"rich"));
    host.seed_file(Path::new("/work/CMakeLists.txt"), lists)
        .unwrap();
    host.seed_file(Path::new("/work/CMakePresets.json"), presets)
        .unwrap();
    assert_eq!(
        invoke(&host, &["rcmake", "--workflow", "--preset", "wf"]),
        1
    );
    let native_err = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        native_err.contains("error[CMake.FatalError]"),
        "expected rich diagnostic in native workflow configure:\n{native_err}"
    );

    let host = project_host();
    host.env_set(b"RCMAKE_DIAGNOSTICS", Some(b"rich"));
    host.seed_file(Path::new("/work/CMakeLists.txt"), lists)
        .unwrap();
    host.seed_file(Path::new("/work/CMakePresets.json"), presets)
        .unwrap();
    assert_eq!(invoke(&host, &["cmake", "--workflow", "--preset", "wf"]), 1);
    let compat_err = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        !compat_err.contains("error[CMake.FatalError]"),
        "compat workflow configure must stay CMake-formatted:\n{compat_err}"
    );
    assert!(
        compat_err.contains("CMake Error at CMakeLists.txt:3 (message):"),
        "{compat_err}"
    );
}

#[test]
fn bare_profile_followed_by_non_project_path_suggests_equals_syntax() {
    let host = project_host();
    assert_eq!(invoke(&host, &["rcmake", "--profile", "missing_dir"]), 1);
    let err = host.stream_bytes(Stream::Stderr).to_string();
    assert!(err.contains("does not exist"), "{err}");
    assert!(err.contains("--profile=<dir>"), "{err}");

    let host = project_host();
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Prof NONE)\n",
    )
    .unwrap();
    assert_eq!(
        invoke(
            &host,
            &[
                "rcmake",
                "--profile",
                "out",
                "-S",
                ".",
                "-B",
                "build",
                "-G",
                "Ninja"
            ]
        ),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let err = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        err.contains("Ignoring extra path from command line"),
        "{err}"
    );
    assert!(err.contains("--profile=<dir>"), "{err}");
}

mod generation;

#[test]
fn native_doctor_reports_paths_and_cache_mode() -> rcmake_host::HostResult<()> {
    let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let host = Arc::new(PureHost::new(root.into(), root.join("rcmake")));
    let modules = root.join("cmake-data");
    host.seed_file(&modules.join("Modules/CMakeDetermineSystem.cmake"), b"")?;
    host.env_set(
        b"RCMAKE_VENDORED_DATA_ROOT",
        Some(&rcmake_host::path::from_native(&modules)),
    );
    host.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"probe-cache"));
    host.env_set(b"RCMAKE_PROBE_CACHE", Some(b"readonly"));
    host.env_set(b"RCMAKE_CACHE_REMOTE_TOKEN", Some(b"secret-value"));
    let tool = root
        .join("bin")
        .join(if cfg!(windows) { "ninja.exe" } else { "ninja" });
    host.seed_file(&tool, b"")?;
    host.set_permissions(&tool, 0o755)?;
    host.env_set(
        b"PATH",
        Some(&rcmake_host::path::from_native(&root.join("bin"))),
    );

    assert_eq!(invoke(&host, &["rcmake", "doctor"]), 0);
    let output = host.stream_bytes(Stream::Stdout).to_string();
    for line in [
        "Persona: native".to_owned(),
        format!(
            "CMAKE_ROOT: {}",
            rcmake_host::path::from_native_cmake(&modules)
        ),
        format!("ninja: {}", rcmake_host::path::from_native_cmake(&tool)),
        "Probe cache mode: read-only".to_owned(),
        "RCMAKE_CACHE_REMOTE_TOKEN".to_owned(),
    ] {
        assert!(output.contains(&line), "{output}");
    }
    assert!(!output.contains("secret-value"));
    Ok(())
}

#[test]
fn native_cache_commands_report_and_remove_cache_data() -> rcmake_host::HostResult<()> {
    let root = Path::new(if cfg!(windows) { "C:/work" } else { "/work" });
    let host = Arc::new(PureHost::new(root.into(), root.join("rcmake")));
    host.env_set(b"RCMAKE_CACHE_DIR", Some(b"probe-cache"));
    let store = rcmake_store::Store::new(
        host.as_ref(),
        rcmake_store::StoreConfig::from_host(host.as_ref()),
    )?;
    let blob = store.put_blob(b"sample data")?;
    let entry = rcmake_store::Entry {
        trace: Vec::new(),
        blobs: vec![blob],
        payload: String::from("sample entry"),
    };
    let domain = rcmake_store::Domain::ProbeV1;
    assert_eq!(
        store.put_entry(domain, domain.key(b"key"), &entry)?,
        rcmake_store::PublishOutcome::Published
    );
    let marker = store.root().join("keep.txt");
    host.seed_file(&marker, b"Keep this file.")?;
    assert_eq!(invoke(&host, &["rcmake", "cache", "dir"]), 0);
    let directory = rcmake_host::path::from_native_cmake(store.root());
    assert_eq!(
        host.stream_bytes(Stream::Stdout).to_string(),
        format!("{directory}\n")
    );

    assert_eq!(invoke(&host, &["rcmake", "cache"]), 0);
    assert_eq!(invoke(&host, &["rcmake", "cache", "status"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stdout)
            .to_string()
            .matches("Entries: 1\nBlobs: 1\n")
            .count(),
        2
    );
    let expected_bytes = store.stats()?.bytes;
    assert_eq!(invoke(&host, &["rcmake", "cache", "clear"]), 0);
    assert!(
        host.stream_bytes(Stream::Stdout)
            .to_string()
            .contains(&format!(
                "Removed 1 entries and 1 blobs. Reclaimed {expected_bytes} bytes."
            ))
    );
    assert_eq!(store.stats()?, rcmake_store::StoreStats::default());
    assert_eq!(
        host.read(&marker, ReadRange::All { limit: 64 })?,
        b"Keep this file."
    );
    Ok(())
}

#[test]
fn doctor_and_cache_keep_source_directory_dispatch() -> rcmake_host::HostResult<()> {
    for command in ["doctor", "cache"] {
        let host = project_host();
        assert_eq!(invoke(&host, &["cmake", command]), 1);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains("does not exist")
        );

        for persona in ["cmake", "rcmake"] {
            let host = project_host();
            let script = host.initial_cwd().join(command).join("CMakeLists.txt");
            host.seed_file(
                &script,
                b"cmake_minimum_required(VERSION 4.5)\nproject(Example NONE)\nmessage(STATUS \"Source directory selected\")\n",
            )?;
            assert_eq!(
                invoke(&host, &[persona, command, "-G", "Ninja"]),
                0,
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
            assert!(
                host.stream_bytes(Stream::Stdout)
                    .to_string()
                    .contains("Source directory selected")
            );
        }
    }
    Ok(())
}

#[test]
fn native_build_usage_names_rcmake_on_both_lines() {
    let host = Arc::new(PureHost::default());
    assert_eq!(invoke(&host, &["rcmake", "--build"]), 1);
    assert!(host.stream_bytes(Stream::Stderr).to_string().contains(
        "Usage: rcmake --build <dir>             [options] [-- [native-options]]\n       rcmake --build --preset"
    ));
}

#[test]
fn explain_suggests_build_directory_when_default_cache_is_missing() -> rcmake_host::HostResult<()> {
    let host = Arc::new(PureHost::default());
    host.seed_file(&host.initial_cwd().join("build/CMakeCache.txt"), b"")?;
    assert_eq!(invoke(&host, &["rcmake", "explain", "app"]), 2);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Hint: found ./build/CMakeCache.txt. Try `rcmake explain ... -B build`.")
    );
    Ok(())
}
