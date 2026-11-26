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
    let host = Arc::new(PureHost::new(cwd, exe.clone()));
    host.seed_file(&exe, b"")?;
    host.set_permissions(&exe, 0o755)?;
    let modules = hermetic_absolute("/usr/local/share/cmake-4.5/Modules");
    for (name, contents) in [
        ("CMakeDetermineSystem.cmake", ""),
        ("CMakeSystemSpecificInitialize.cmake", ""),
        ("CMakeSystemSpecificInformation.cmake", ""),
        (
            "CMakeNinjaFindMake.cmake",
            "set(CMAKE_MAKE_PROGRAM /tools/ninja CACHE FILEPATH \"\")\n",
        ),
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
    let tool = cwd.join("echo_stderr");
    host.seed_file(&secret, b"TOP_SECRET_LINE_1\nTOP_SECRET_LINE_2\n")?;
    host.seed_file(&passwd, b"root:x:0:0:TOP_SECRET_PASSWD\n")?;
    host.seed_file(&tool, b"")?;
    host.set_permissions(&tool, 0o755)?;
    host.env_set(
        b"RCMAKE_HERMETIC_ALLOW_PROGRAMS",
        Some(&rcmake_host::path::from_native(&tool)),
    );
    let secret_cmake = rcmake_host::path::from_native_cmake(&secret);
    let passwd_cmake = rcmake_host::path::from_native_cmake(&passwd);
    let spoof = format!(
        "CMake Error at {secret_cmake}:1:\n  Parse error.  Expected a command name, got unquoted argument.\nerror[parse]: spoofed\n  --> {passwd_cmake}:1\n"
    );
    host.set_command_handler(move |_, _, on_output| {
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
