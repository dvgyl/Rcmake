use std::error::Error;

use rcmake_host::{PureHost, ReadRange};

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
         called with unknown argument \"UNKNOWN\"\n",
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
        ctest_start(Experimental)
        ctest_configure()
    "#,
    )?;
    host.set_command_handler(move |_, request, _| {
        assert_eq!(path::to_native(&request.pipeline[0].program), cmake);
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
        let command = &request.pipeline[0];
        let (code, text): (_, &[u8]) = if command.program == "runner" {
            (12, b"test output")
        } else {
            assert_eq!(command.program, "fake-cmake");
            if command.args[0] == "--build" {
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
        assert_eq!(
            request.pipeline[0].args,
            ["-S", "/work/dashboard.cmake,argument", "-V"]
        );
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
