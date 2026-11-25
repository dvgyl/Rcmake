use std::error::Error;

use rcmake_host::{HostError, PureHost, ReadRange};

use super::*;

fn invoke(host: Arc<PureHost>, args: &[&str]) -> i32 {
    let argv = std::iter::once("ctest")
        .chain(args.iter().copied())
        .map(BString::from)
        .collect::<Vec<_>>();
    crate::main(&argv, host)
}

#[test]
fn checks_test_arguments_in_input_order() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/dashboard.cmake"),
        br#"
        ctest_test(CAPTURE_CMAKE_ERROR captured INCLUDE)
        ctest_test(CAPTURE_CMAKE_ERROR captured QUIET QUIET)
        ctest_test(CAPTURE_CMAKE_ERROR captured INCLUDE_LABEL EXCLUDE_LABEL)
        ctest_test(CAPTURE_CMAKE_ERROR captured UNKNOWN INCLUDE)
        if(NOT captured EQUAL -1)
          message(FATAL_ERROR "Wrong captured error code")
        endif()
        "#,
    )?;
    assert_eq!(invoke(host.clone(), &["-S", "dashboard.cmake"]), 0);
    assert_eq!(
        host.stream_bytes(Stream::Stderr),
        "requires a value for INCLUDE\n\
         called with more than one value for QUIET\n\
         Error after keyword \"INCLUDE_LABEL\":\n  missing required value\n\
         ctest_test called with unknown argument \"UNKNOWN\".\n",
    );
    Ok(())
}

#[test]
fn reports_first_start_group_without_a_value() -> Result<(), Box<dyn Error>> {
    for keyword in ["TRACK", "GROUP"] {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/dashboard.cmake"),
            format!("ctest_start(Experimental {keyword} QUIET)").as_bytes(),
        )?;
        assert_eq!(invoke(host.clone(), &["-S", "dashboard.cmake"]), 255);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .to_string()
                .contains(&format!("{keyword} argument missing group name"))
        );
    }
    Ok(())
}

#[test]
fn configures_from_a_ctest_link_outside_the_install_directory() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::new("/work".into(), "/work/ctest".into()));
    let binary = Path::new("/install/bin/rcmake");
    let cmake = Path::new(if cfg!(windows) {
        "/install/bin/cmake.exe"
    } else {
        "/install/bin/cmake"
    });
    host.seed_file(binary, b"")?;
    host.symlink(binary, Path::new("/work/ctest"))?;
    host.symlink(binary, cmake)?;
    host.seed_file(
        Path::new("/work/src/CMakeLists.txt"),
        b"project(sample NONE)",
    )?;
    host.seed_file(
        Path::new("/work/dashboard.cmake"),
        br#"
        set(CTEST_SOURCE_DIRECTORY "/work/src")
        set(CTEST_BINARY_DIRECTORY "/work/build")
        set(CTEST_CMAKE_GENERATOR Ninja)
        set(CTEST_CONFIGURATION_TYPE Debug)
        ctest_start(Experimental)
        ctest_configure()
    "#,
    )?;
    host.set_command_handler(move |_, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing configure command"))?;
        assert_eq!(path::to_native(&command.program), cmake);
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg == b"-DCMAKE_BUILD_TYPE:STRING=Debug")
        );
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(invoke(host, &["-S", "dashboard.cmake"]), 0);
    Ok(())
}

#[test]
fn runs_dashboard_handlers_and_sets_script_variables() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/src/CMakeLists.txt"), b"project(test NONE)")?;
    host.seed_file(
        Path::new("/work/build/CTestTestfile.cmake"),
        br#"
        add_test(wanted runner)
        add_test(excluded unexpected)
        set_tests_properties(wanted PROPERTIES LABELS fast)
    "#,
    )?;
    host.seed_file(Path::new("/work/nested.cmake"), b"cmake_language(EXIT 7)")?;
    host.seed_file(Path::new("/work/dashboard.cmake"), br#"
        if(NOT CTEST_SCRIPT_NAME STREQUAL "dashboard.cmake" OR NOT CTEST_SCRIPT_ARG STREQUAL "a,b")
          message(FATAL_ERROR "Wrong script variables")
        endif()
        set(CTEST_SITE "test-site")
        set(CTEST_BUILD_NAME "test-build")
        set(CTEST_CMAKE_GENERATOR "Ninja")
        set(CTEST_SOURCE_DIRECTORY "/work/src")
        set(CTEST_BINARY_DIRECTORY "/work/build")
        set(CMAKE_COMMAND fake-cmake)
        ctest_start(Experimental TRACK "Test track")
        ctest_configure(OPTIONS "-DONE=1;-DTWO=2" RETURN_VALUE configure CAPTURE_CMAKE_ERROR captured)
        if(NOT configure EQUAL 0 OR NOT captured EQUAL 0)
          message(FATAL_ERROR "Configure failed")
        endif()
        ctest_build(TARGET sample CONFIGURATION Debug FLAGS [=[-v "two words"]=] NUMBER_WARNINGS warnings NUMBER_ERRORS errors RETURN_VALUE build)
        if(NOT build EQUAL 0 OR NOT warnings EQUAL 1 OR NOT errors EQUAL 0)
          message(FATAL_ERROR "Wrong build result")
        endif()
        ctest_test(INCLUDE_LABEL "^fast$" PARALLEL_LEVEL 2 STOP_ON_FAILURE OUTPUT_JUNIT junit.xml RETURN_VALUE test CAPTURE_CMAKE_ERROR captured)
        if(NOT test EQUAL -1 OR NOT captured EQUAL 0)
          message(FATAL_ERROR "Wrong test result")
        endif()
        ctest_run_script(nested.cmake RETURN_VALUE child)
        if(NOT child EQUAL 7)
          message(FATAL_ERROR "Wrong script exit code")
        endif()
        ctest_configure(SOURCE /work/missing CAPTURE_CMAKE_ERROR captured)
        if(NOT captured EQUAL -1)
          message(FATAL_ERROR "Missing configure error")
        endif()
    "#)?;
    host.set_command_handler(|_, request, _| {
        assert_eq!(request.cwd, Path::new("/work/build"));
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing dashboard command"))?;
        let (code, text): (_, &[u8]) = if command.program == "runner" {
            (12, b"test output")
        } else {
            assert_eq!(command.program, "fake-cmake");
            if command.args.first().is_some_and(|arg| arg == "--build") {
                assert_eq!(
                    command.args,
                    [
                        "--build",
                        ".",
                        "--config",
                        "Debug",
                        "--target",
                        "sample",
                        "--",
                        "-v",
                        "two words"
                    ]
                );
                (0, b"compiler: warning: sample\n")
            } else {
                assert_eq!(
                    command.args,
                    [
                        "-S/work/src",
                        "-B/work/build",
                        "-GNinja",
                        "-DONE=1",
                        "-DTWO=2"
                    ]
                );
                (0, b"")
            }
        };
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(code)],
            stdout: text.to_vec(),
            stderr: Vec::new(),
        })
    })?;
    assert_eq!(
        invoke(host.clone(), &["-S", "dashboard.cmake,a,b"]),
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let dir = Path::new("/work/build");
    let tag = Tag::read(host.as_ref(), dir)?;
    assert_eq!(tag.track, "Test track");
    let xml = host.read(
        &dir.join("Testing").join(tag.stamp()).join("Test.xml"),
        ReadRange::All { limit: 16_384 },
    )?;
    let xml = String::from_utf8(xml)?;
    assert!(xml.contains("<Site BuildName=\"test-build\""));
    assert!(xml.contains("Name=\"test-site\""));
    assert!(xml.contains("<Test Status=\"failed\">"));
    assert!(xml.contains("<Value>12</Value>"));
    assert!(!xml.contains("excluded"));
    Ok(())
}

#[test]
fn starts_a_child_ctest_for_sp() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.set_command_handler(|_, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing child CTest command"))?;
        assert_eq!(command.args, ["-S", "/work/dashboard.cmake,argument", "-V"]);
        assert!(
            request
                .env
                .iter()
                .any(|(key, value)| key.as_slice() == DEPTH_KEY && value == "1")
        );
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(3)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(invoke(host, &["-V", "-SP", "dashboard.cmake,argument"]), 3);
    Ok(())
}

#[test]
fn empties_only_build_directories_with_cmake_cache() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/build/CMakeCache.txt"), b"")?;
    host.seed_file(Path::new("/work/source/CMakeLists.txt"), b"")?;
    host.seed_file(
        Path::new("/work/empty.cmake"),
        b"ctest_empty_binary_directory(/work/build)",
    )?;
    host.seed_file(
        Path::new("/work/invalid.cmake"),
        b"ctest_empty_binary_directory(/work/source)",
    )?;
    assert_eq!(invoke(host.clone(), &["-S", "empty.cmake"]), 0);
    assert!(
        host.metadata(Path::new("/work/build"), Follow::Yes)
            .is_err()
    );
    assert_eq!(invoke(host.clone(), &["-S", "invalid.cmake"]), 255);
    assert!(
        host.metadata(Path::new("/work/source/CMakeLists.txt"), Follow::Yes)
            .is_ok()
    );
    Ok(())
}

#[test]
fn empties_a_build_directory_under_a_symlinked_ancestor() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/private/tmp/build/CMakeCache.txt"), b"")?;
    host.seed_file(Path::new("/private/tmp/other/CMakeCache.txt"), b"")?;
    host.symlink(Path::new("/private/tmp"), Path::new("/tmp"))?;
    host.symlink(Path::new("other"), Path::new("/private/tmp/link"))?;

    for (argument, expected) in [
        ("/tmp/build/", 0),
        ("/tmp/link", 255),
        ("/tmp/link/", 255),
        ("/tmp/link/.", 255),
        ("/", 255),
    ] {
        host.seed_file(
            Path::new("/work/empty.cmake"),
            format!("ctest_empty_binary_directory({argument})").as_bytes(),
        )?;
        assert_eq!(invoke(host.clone(), &["-S", "empty.cmake"]), expected);
    }
    assert!(
        host.metadata(Path::new("/private/tmp/build"), Follow::No)
            .is_err()
    );
    assert_eq!(
        host.metadata(Path::new("/tmp/link"), Follow::No)?.kind,
        FileKind::Symlink
    );
    assert_eq!(
        host.metadata(Path::new("/private/tmp/other/CMakeCache.txt"), Follow::No)?
            .kind,
        FileKind::File
    );
    Ok(())
}

#[test]
fn update_honors_version_options_and_counts_distinct_changed_files() -> Result<(), Box<dyn Error>> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/src/.git/HEAD"), b"")?;
    host.seed_file(
        Path::new("/work/dashboard.cmake"),
        br#"
        set(CTEST_SOURCE_DIRECTORY "/work/src")
        set(CTEST_BINARY_DIRECTORY "/work/build")
        set(CTEST_GIT_COMMAND fake-git)
        ctest_start(Experimental)
        set(CTEST_UPDATE_VERSION_OVERRIDE from-variable)
        ctest_update(VERSION_OVERRIDE from-argument RETURN_VALUE count)
        if(NOT count EQUAL 0)
          message(FATAL_ERROR "The version override changed files")
        endif()
        ctest_update(RETURN_VALUE count)
        if(NOT count EQUAL 0)
          message(FATAL_ERROR "The version variable changed files")
        endif()
        unset(CTEST_UPDATE_VERSION_OVERRIDE)
        ctest_update(VERSION_ONLY RETURN_VALUE count)
        if(NOT count EQUAL 0)
          message(FATAL_ERROR "The version query changed files")
        endif()
        ctest_update(RETURN_VALUE count)
        if(NOT count EQUAL 2)
          message(FATAL_ERROR "Wrong update count")
        endif()
        "#,
    )?;
    let calls = Arc::new(AtomicUsize::new(0));
    let commands = Arc::clone(&calls);
    host.set_command_handler(move |_, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing update command"))?;
        assert_eq!(request.cwd, Path::new("/work/src"));
        assert_eq!(command.program, "fake-git");
        let (operation, output): (_, &[u8]) = match commands.fetch_add(1, Ordering::Relaxed) {
            0 | 1 => ("rev-parse", b"1111111111111111111111111111111111111111\n"),
            2 => ("pull", b""),
            3 => ("rev-parse", b"2222222222222222222222222222222222222222\n"),
            4 => ("log", b"src/first.c\0src/second.c\0src/first.c\0"),
            _ => return Err(HostError::unsupported("Unexpected update command")),
        };
        assert_eq!(command.args.first(), Some(&BString::from(operation)));
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: output.to_vec(),
            ..RunOutput::default()
        })
    })?;
    assert_eq!(invoke(host.clone(), &["-S", "dashboard.cmake"]), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 5);
    let build = Path::new("/work/build");
    let tag = Tag::read(host.as_ref(), build)?;
    let report = build.join("Testing").join(tag.stamp()).join("Update.xml");
    let xml = host.read(&report, ReadRange::All { limit: 4096 })?;
    let xml = std::str::from_utf8(&xml)?;
    assert_eq!(xml.matches("<Updated>").count(), 2);
    assert!(xml.contains("<FullName>src/second.c</FullName>"));
    Ok(())
}

#[test]
fn collects_gcov_data_and_filters_coverage_by_multiple_labels() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    for name in ["first", "second", "excluded"] {
        host.seed_file(
            &Path::new("/work/src").join(format!("{name}.c")),
            b"int main(void) { return 0; }\n",
        )?;
    }
    host.seed_file(
        Path::new("/work/build/CMakeFiles/TargetDirectories.txt"),
        b"/work/build/CMakeFiles/sample.dir\n",
    )?;
    host.seed_file(
        Path::new("/work/build/CMakeFiles/sample.dir/Labels.txt"),
        b"/work/src/first.c\n fast\n/work/src/second.c\n slow\n/work/src/excluded.c\n ignored\n",
    )?;
    host.seed_file(
        Path::new("/work/build/CMakeFiles/sample.dir/sample.gcda"),
        b"",
    )?;
    host.seed_file(
        Path::new("/work/dashboard.cmake"),
        br#"
        set(CTEST_SOURCE_DIRECTORY "/work/src")
        set(CTEST_BINARY_DIRECTORY "/work/build")
        set(CTEST_COVERAGE_COMMAND fake-gcov)
        ctest_start(Experimental)
        ctest_coverage(LABELS fast slow APPEND QUIET RETURN_VALUE result)
        if(NOT result EQUAL 0)
          message(FATAL_ERROR "Coverage failed")
        endif()
    "#,
    )?;
    host.set_command_handler(|host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing coverage command"))?;
        assert_eq!(command.program, "fake-gcov");
        for (name, hits) in [("first", "1"), ("second", "#####"), ("excluded", "1")] {
            let file = request.cwd.join(format!("{name}.c.gcov"));
            host.seed_file(
                &file,
                format!(
                    " -: 0:Source:/work/src/{name}.c\n {hits}: 1:int main(void) {{ return 0; }}\n"
                )
                .as_bytes(),
            )?;
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(invoke(host.clone(), &["-S", "dashboard.cmake"]), 0);
    let build = Path::new("/work/build");
    let tag = Tag::read(host.as_ref(), build)?;
    let report = build.join("Testing").join(tag.stamp()).join("Coverage.xml");
    let xml = host.read(&report, ReadRange::All { limit: 4096 })?;
    let xml = std::str::from_utf8(&xml)?;
    assert!(xml.contains("Append=\"true\""));
    assert!(xml.contains("Name=\"first.c\""));
    assert!(xml.contains("Name=\"second.c\""));
    assert!(!xml.contains("excluded.c"));
    assert!(xml.contains("<LOC>2</LOC>"));
    assert!(xml.contains("<PercentCoverage>50.00</PercentCoverage>"));
    Ok(())
}

#[test]
fn runs_dashboard_steps_in_order_and_combines_error_bits() -> Result<(), Box<dyn Error>> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/src/CMakeLists.txt"),
        b"project(sample NONE)",
    )?;
    host.seed_file(
        Path::new("/work/build/DartConfiguration.tcl"),
        b"SourceDirectory: /work/src\nMakeCommand: builder \"two words\"\n",
    )?;
    let calls = Arc::new(AtomicUsize::new(0));
    let commands = Arc::clone(&calls);
    host.set_command_handler(move |_, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing dashboard command"))?;
        let result = match commands.fetch_add(1, Ordering::Relaxed) {
            0 => {
                assert_eq!(request.cwd, Path::new("/work/src"));
                assert_eq!(command.args.first(), Some(&BString::from("-S")));
                ProcessResult::Exited(1)
            }
            1 => {
                assert_eq!(request.cwd, Path::new("/work/build"));
                assert_eq!(command.program, "builder");
                assert_eq!(command.args, [BString::from("two words")]);
                return Err(HostError::unsupported("Build process could not start"));
            }
            _ => return Err(HostError::unsupported("Unexpected dashboard command")),
        };
        Ok(RunOutput {
            results: vec![result],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(
        invoke(
            host.clone(),
            &[
                "--build-dir",
                "/work/build",
                "-T",
                "Submit",
                "-T",
                "Build",
                "-T",
                "Configure"
            ]
        ),
        4 | 64
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    Ok(())
}

#[test]
fn runs_dashboard_build_presets_from_the_build_directory() -> Result<(), Box<dyn Error>> {
    let (work, build_text) = if cfg!(windows) {
        ("C:/work", "C:/work/build")
    } else {
        ("/work", "/work/build")
    };
    let root = Path::new(work);
    let build = Path::new(build_text);
    let host = Arc::new(PureHost::new(root.into(), root.join("bin/ctest")));
    host.seed_file(
        &root.join("src/CMakePresets.json"),
        br#"{
            "version": 3,
            "configurePresets": [{"name": "setup", "generator": "Ninja", "binaryDir": "${sourceDir}/../build"}],
            "buildPresets": [{"name": "compile", "configurePreset": "setup"}]
        }"#,
    )?;
    host.seed_file(
        &build.join("DartConfiguration.tcl"),
        format!("SourceDirectory: {work}/src\nMakeCommand: unexpected\n").as_bytes(),
    )?;
    host.set_command_handler(move |_, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing build command"))?;
        assert_eq!(request.cwd, build);
        assert_eq!(
            command.args,
            ["--build", ".", "--preset", "compile"].map(BString::from)
        );
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    assert_eq!(
        invoke(
            host.clone(),
            &[
                "--build-dir",
                build_text,
                "-T",
                "Build",
                "-D",
                "CTEST_BUILD_PRESET=compile",
                "-D",
                "CTEST_BUILD_COMMAND=ignored"
            ]
        ),
        0
    );
    let tag = Tag::read(host.as_ref(), build)?;
    assert!(
        host.metadata(
            &build.join("Testing").join(tag.stamp()).join("Build.xml"),
            Follow::Yes
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn dashboard_start_runs_checkout_before_source_validation() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/dashboard.cmake"),
        br#"
        set(CTEST_SOURCE_DIRECTORY "/work/checkout/src")
        set(CTEST_BINARY_DIRECTORY "/work/build")
        set(CTEST_CHECKOUT_COMMAND [=[checkout "new tree"]=])
        set(CTEST_CVS_CHECKOUT ignored)
        ctest_start(Experimental QUIET)
    "#,
    )?;
    host.set_command_handler(|host, request, _| {
        assert_eq!(request.cwd, Path::new("/work/checkout"));
        let checkout = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Missing checkout command."))?;
        assert_eq!(checkout.program, "checkout");
        assert_eq!(checkout.args, ["new tree"]);
        host.create_dir_all(&request.cwd.join("src"))?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: b"finished\n".to_vec(),
            stderr: b"notice\n".to_vec(),
        })
    })?;
    assert_eq!(invoke(host.clone(), &["-S", "dashboard.cmake"]), 0);
    let contents = host.read(
        Path::new("/work/build/Testing/Temporary/LastStart.log"),
        ReadRange::All { limit: 4096 },
    )?;
    let contents = std::str::from_utf8(&contents)?;
    assert!(contents.contains("--- Begin Initial Checkout ---"));
    assert!(contents.contains("co-out> finished\nco-err> notice\n"));
    assert!(contents.contains("--- End Initial Checkout ---"));
    assert!(
        host.metadata(Path::new("/work/build/Testing/TAG"), Follow::Yes)
            .is_ok()
    );
    Ok(())
}

#[test]
fn dashboard_start_reports_failed_checkout_on_stderr() -> Result<(), Box<dyn Error>> {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/dashboard.cmake"),
        br#"
        set(CTEST_SOURCE_DIRECTORY "/work/checkout/src")
        set(CTEST_BINARY_DIRECTORY "/work/build")
        set(CTEST_CHECKOUT_COMMAND checkout)
        ctest_start(Experimental QUIET)
        "#,
    )?;
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(1)],
            stdout: Vec::new(),
            stderr: b"checkout output\n".to_vec(),
        })
    })?;
    assert_ne!(invoke(host.clone(), &["-S", "dashboard.cmake"]), 0);
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(stderr.starts_with("Initial checkout failed!\n"), "{stderr}");
    assert!(stderr.contains("ctest_start unknown error."), "{stderr}");
    let log = host.read(
        Path::new("/work/build/Testing/Temporary/LastStart.log"),
        ReadRange::All { limit: 4096 },
    )?;
    assert_eq!(
        std::str::from_utf8(&log)?,
        "--- Begin Initial Checkout ---\ncheckout\nco-err> checkout output\n--- End Initial Checkout ---\n"
    );
    Ok(())
}
