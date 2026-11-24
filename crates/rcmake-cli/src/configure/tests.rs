use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, HostResult, ProcessResult, ReadRange, RunOutput, Stream};

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
