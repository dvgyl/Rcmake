//! Exercise CMake command compatibility through the CLI.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, Follow, Host, Input, NativeHost, Output, ProcessResult, ReadRange, RunOutput,
    RunRequest, WriteMode,
};

struct Fixture {
    host: NativeHost,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let host = NativeHost::new()?;
        let output = run(
            &host,
            Path::new("/tmp"),
            "mktemp",
            &["-d", "/tmp/rcmake-commands.XXXXXXXX"],
        )?;
        success(&output);
        let root = std::str::from_utf8(&output.stdout)?.trim().into();
        Ok(Self { host, root })
    }

    fn write(&self, file: &str, body: &[u8]) -> rcmake_host::HostResult<()> {
        self.host
            .write(&self.root.join(file), body, WriteMode::Truncate)
    }

    fn cmake(&self, args: &[&str]) -> rcmake_host::HostResult<RunOutput> {
        run(&self.host, &self.root, env!("CARGO_BIN_EXE_rcmake"), args)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.root);
    }
}

fn run(
    host: &NativeHost,
    cwd: &Path,
    program: &str,
    args: &[&str],
) -> rcmake_host::HostResult<RunOutput> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/cmake-4.5");
    let mut environment = host.env_all();
    environment.retain(|(name, _)| name != b"RCMAKE_VENDORED_DATA_ROOT");
    environment.push((
        "RCMAKE_VENDORED_DATA_ROOT".into(),
        rcmake_host::path::from_native(&source),
    ));
    host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: program.into(),
                args: args.iter().copied().map(BString::from).collect(),
            }],
            cwd: cwd.to_owned(),
            env: environment,
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(120)),
            tee: false,
        },
        &mut |_, _| {},
    )
}

fn success(output: &RunOutput) {
    assert_eq!(
        output.results,
        [ProcessResult::Exited(0)],
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn configure_file_template_edit_regenerates_build() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.write(
        "CMakeLists.txt",
        br#"
cmake_minimum_required(VERSION 4.5)
project(Template C)
configure_file(./value.h.in value.h COPYONLY)
include(CMakePackageConfigHelpers)
configure_package_config_file(package.cmake.in "${CMAKE_BINARY_DIR}/package.cmake"
  INSTALL_DESTINATION lib/cmake/Template)
add_executable(read_value main.c)
target_include_directories(read_value PRIVATE "${CMAKE_BINARY_DIR}")
"#,
    )?;
    fixture.write("value.h.in", b"#define VALUE 3\n")?;
    fixture.write("package.cmake.in", b"@PACKAGE_INIT@\nset(VALUE 3)\n")?;
    fixture.write(
        "main.c",
        b"#include \"value.h\"\nint main(void) { return VALUE; }\n",
    )?;
    success(&fixture.cmake(&["-S", ".", "-B", "build", "-G", "Ninja"])?);
    success(&fixture.cmake(&["--build", "build"])?);
    let execute = || run(&fixture.host, &fixture.root, "./build/read_value", &[]);
    assert_eq!(execute()?.results, [ProcessResult::Exited(3)]);
    fixture.write("value.h.in", b"#define VALUE 9\n")?;
    fixture.write("package.cmake.in", b"@PACKAGE_INIT@\nset(VALUE 9)\n")?;
    success(&fixture.cmake(&["--build", "build"])?);
    assert_eq!(execute()?.results, [ProcessResult::Exited(9)]);
    let result = fixture.host.read(
        &fixture.root.join("build/package.cmake"),
        ReadRange::All { limit: 8192 },
    )?;
    assert!(std::str::from_utf8(&result)?.contains("set(VALUE 9)"));
    Ok(())
}

#[test]
fn probe_imports_genex_dependencies_and_preserves_cross_run_results()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.write(
        "CMakeLists.txt",
        br##"
cmake_minimum_required(VERSION 4.5)
project(Probe C)
add_library(dep INTERFACE IMPORTED)
set_property(TARGET dep PROPERTY INTERFACE_COMPILE_DEFINITIONS REQUIRED_FLAG=1)
add_library(root INTERFACE IMPORTED)
set_property(TARGET root PROPERTY INTERFACE_LINK_LIBRARIES
  "$<$<CONFIG:Debug>:$<TARGET_NAME_IF_EXISTS:dep>>;$<$<CONFIG:Release>:missing::name>")
set(probe_source [=[
#ifndef REQUIRED_FLAG
#error Missing compile definition
#endif
int main(void) { return 0; }
]=])
try_compile(flags_config SOURCE_FROM_CONTENT probe.c "${probe_source}"
  CMAKE_FLAGS -DCMAKE_BUILD_TYPE:STRING=Debug LINK_LIBRARIES root)
if(flags_config)
  message(FATAL_ERROR "CMAKE_FLAGS must not select the import configuration")
endif()
try_compile(direct_genex SOURCE_FROM_CONTENT probe.c "${probe_source}"
  LINK_LIBRARIES "$<TARGET_NAME_IF_EXISTS:dep>")
if(direct_genex)
  message(FATAL_ERROR "A direct LINK_LIBRARIES genex must not import a target")
endif()
set(CMAKE_TRY_COMPILE_CONFIGURATION Debug)
try_compile(compiled SOURCE_FROM_CONTENT probe.c "${probe_source}"
  LINK_LIBRARIES root OUTPUT_VARIABLE log)
if(NOT compiled)
  message(FATAL_ERROR "Probe failed\n${log}")
endif()
unset(CMAKE_TRY_COMPILE_CONFIGURATION)
set(CMAKE_CROSSCOMPILING TRUE)
set(CROSS_RUN 17 CACHE STRING "Original help")
set(CROSS_RUN__TRYRUN_OUTPUT "" CACHE STRING "Original output help")
if(MISSING)
  set(result MISSING_RUN)
else()
  set(result CROSS_RUN)
endif()
try_run(${result} built SOURCE_FROM_CONTENT probe.c "int main(void) { return 2; }"
  RUN_OUTPUT_VARIABLE output NO_CACHE)
if(NOT built)
  message(FATAL_ERROR "Cross build failed")
endif()
if(NOT MISSING AND (NOT CROSS_RUN STREQUAL "17" OR NOT output STREQUAL ""))
  message(FATAL_ERROR "Cross result changed")
endif()
"##,
    )?;
    success(&fixture.cmake(&["-S", ".", "-B", "build", "-G", "Ninja"])?);
    assert!(
        fixture
            .host
            .metadata(&fixture.root.join("build/TryRunResults.cmake"), Follow::No)
            .is_err()
    );
    let missing = fixture.cmake(&["-S", ".", "-B", "missing", "-G", "Ninja", "-DMISSING=ON"])?;
    assert_eq!(missing.results, [ProcessResult::Exited(1)]);
    let stderr = std::str::from_utf8(&missing.stderr)?;
    assert!(
        stderr.contains("   MISSING_RUN (advanced)\n   MISSING_RUN__TRYRUN_OUTPUT (advanced)\n"),
        "{stderr}"
    );
    let partial = fixture.cmake(&[
        "-S",
        ".",
        "-B",
        "partial",
        "-G",
        "Ninja",
        "-DMISSING=ON",
        "-DMISSING_RUN=0",
    ])?;
    assert_eq!(partial.results, [ProcessResult::Exited(1)]);
    let stderr = std::str::from_utf8(&partial.stderr)?;
    assert!(
        stderr.contains("   MISSING_RUN (advanced)\n   MISSING_RUN__TRYRUN_OUTPUT (advanced)\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("_STDOUT"), "{stderr}");
    let results = fixture.host.read(
        &fixture.root.join("missing/TryRunResults.cmake"),
        ReadRange::All { limit: 8192 },
    )?;
    assert!(std::str::from_utf8(&results)?.contains("PLEASE_FILL_OUT-FAILED_TO_RUN"));
    assert!(
        fixture
            .host
            .read_dir(&fixture.root.join("missing/CMakeFiles"))?
            .iter()
            .any(|entry| entry.name.ends_with(b"-MISSING_RUN"))
    );
    Ok(())
}

#[test]
fn scope_capture_keeps_function_bindings_local() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.write(
        "CMakeLists.txt",
        br#"
cmake_minimum_required(VERSION 4.5)
project(Scope NONE)
set(E rootE)
set(U rootU)
function(f)
  set(E fval)
  unset(U)
  enable_language(C)
endfunction()
f()
if(NOT E STREQUAL "rootE" OR NOT U STREQUAL "rootU")
  message(FATAL_ERROR "Unexpected root bindings. E=[${E}] U=[${U}]")
endif()
"#,
    )?;
    success(&fixture.cmake(&["-S", ".", "-B", "build", "-G", "Ninja"])?);
    Ok(())
}

#[test]
fn file_transfers_encode_spaces_in_local_urls() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.write("input file", b"Transfer payload")?;
    fixture.write("run.cmake", br#"
file(DOWNLOAD "file://${CMAKE_CURRENT_LIST_DIR}/input file"
     "${CMAKE_CURRENT_LIST_DIR}/downloaded file" STATUS downloaded)
file(UPLOAD "${CMAKE_CURRENT_LIST_DIR}/downloaded file"
     "file://${CMAKE_CURRENT_LIST_DIR}/uploaded file" STATUS uploaded)
if(NOT downloaded STREQUAL "0;\"No error\"" OR NOT uploaded STREQUAL "0;\"No error\"")
  message(FATAL_ERROR "Transfer failed. Download result = ${downloaded}. Upload result = ${uploaded}.")
endif()
"#)?;
    success(&fixture.cmake(&["-P", "run.cmake"])?);
    let payload = fixture.host.read(
        &fixture.root.join("uploaded file"),
        ReadRange::All { limit: 1024 },
    )?;
    assert_eq!(payload, b"Transfer payload");
    Ok(())
}
