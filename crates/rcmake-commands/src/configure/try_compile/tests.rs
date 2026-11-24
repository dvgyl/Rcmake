use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Follow, Host, ProcessResult, PureHost, ReadRange, RunOutput, Stream, WriteMode};
use rcmake_model::CacheType;

const DRIVE: &str = if cfg!(windows) { "C:" } else { "" };

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn interpreter(host: Arc<dyn Host>, cwd: &Path) -> Result<Interp, Box<dyn std::error::Error>> {
    let modules = cwd.join("cmake/Modules");
    host.create_dir_all(&modules)?;
    for (name, body) in [
        (
            "CMakeSystemSpecificInitialize.cmake",
            "set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)",
        ),
        (
            "CMakeSystemSpecificInformation.cmake",
            "set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)\nfunction(probe_initialized)\nendfunction()",
        ),
        (
            "CMakeCInformation.cmake",
            "probe_initialized()\nset(CMAKE_C_INFORMATION_LOADED 1)",
        ),
        (
            "CMakeCXXInformation.cmake",
            "probe_initialized()\nset(CMAKE_CXX_INFORMATION_LOADED 1)",
        ),
    ] {
        host.write(&modules.join(name), body.as_bytes(), WriteMode::CreateNew)?;
    }
    let mut interp = Interp::new(
        InterpConfig {
            host,
            mode: RunMode::Configure,
            cmake_root: cwd.join("cmake"),
            cmake_command: cwd.join("cmake/bin/cmake"),
            cwd: cwd.to_path_buf(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    let mut cx = interp.cx();
    cx.set_var(b"CMAKE_SYSTEM", cx.host().system().sysname);
    cx.set_var(b"CMAKE_MAKE_PROGRAM", "make".into());
    for name in [
        "CMAKE_SYSTEM_LOADED",
        "CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED",
        "CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED",
        "CMAKE_C_INFORMATION_LOADED",
        "CMAKE_CXX_INFORMATION_LOADED",
    ] {
        cx.set_var(name.as_bytes(), "1".into());
    }
    Ok(interp)
}

#[test]
fn builds_all_four_signatures_and_captures_child_output() -> TestResult {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
    host.seed_file(
        Path::new(&format!("{DRIVE}/work/main.c")),
        b"int main(void) { return 0; }",
    )?;
    host.seed_file(
        Path::new(&format!("{DRIVE}/work/sub/main.c")),
        b"int main(void) { return 0; }",
    )?;
    host.seed_file(
        Path::new(&format!("{DRIVE}/work/sub/missing.c")),
        b"int main(void) { return 0; }",
    )?;
    host.seed_file(
        Path::new(&format!("{DRIVE}/work/sub/CMakeLists.txt")),
        &std::str::from_utf8(
            br#"
cmake_minimum_required(VERSION 4.5)
project(Child C)
get_property(nested GLOBAL PROPERTY IN_TRY_COMPILE)
message(STATUS "child nested=${nested} flag=${PASSED}")
add_executable(app main.c)
add_executable(excluded missing.c)
"#,
        )?
        .replace("/work", &format!("{DRIVE}/work"))
        .into_bytes(),
    )?;
    host.seed_file(
            Path::new(&format!("{DRIVE}/work/CMakeLists.txt")),
            &std::str::from_utf8(br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
set(CMAKE_C_COMPILER /work/probe-cc)
set(CMAKE_TRY_COMPILE_PLATFORM_VARIABLES PASSED)
set(PASSED parent)
try_compile(NEW SOURCES /work/main.c CMAKE_FLAGS "-DCOMPILE_DEFINITIONS=--probe-driver-flag" NO_CACHE NO_LOG)
try_compile(LEGACY /work/legacy /work/main.c NO_LOG)
try_compile(PROJECT_NEW PROJECT Child SOURCE_DIR /work/sub BINARY_DIR /work/build/project-new TARGET app
    CMAKE_FLAGS -DPASSED:STRING=override OUTPUT_VARIABLE CHILD_OUT LOG_DESCRIPTION "Child build")
try_compile(PROJECT_OLD /work/project-old /work/sub Child app OUTPUT_VARIABLE CHILD_OLD NO_LOG)
"#)?.replace("/work", &format!("{DRIVE}/work")).into_bytes(),
        )?;
    let flagged = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&flagged);
    host.set_command_handler(move |_, request, _| {
        let command = &request.pipeline[0];
        assert_eq!(
            rcmake_host::path::to_native(&command.program),
            Path::new(&format!("{DRIVE}/work/probe-cc"))
        );
        assert!(!command.args.iter().any(|arg| arg.ends_with(b"missing.c")));
        if command.args.iter().any(|arg| arg == "--probe-driver-flag") {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(RunOutput {
            stdout: b"compiler output\n".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let mut interp = interpreter(host.clone(), Path::new(&format!("{DRIVE}/work")))?;
    let result = interp.run_configure(
        Path::new(&format!("{DRIVE}/work")),
        Path::new(&format!("{DRIVE}/work/build")),
        &[],
    );
    assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
    let mut cx = interp.cx();
    assert_eq!(cx.var_normal_only(b"NEW"), Some("TRUE".into()));
    assert_eq!(flagged.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(cx.cache().get(b"NEW").is_none());
    for name in [b"LEGACY".as_slice(), b"PROJECT_NEW", b"PROJECT_OLD"] {
        let entry = cx.cache().get(name).ok_or("Missing cache result.")?;
        assert_eq!(entry.value, "TRUE");
        assert_eq!(entry.ty, CacheType::Internal);
    }
    let captured = cx.var_or_empty(b"CHILD_OUT");
    assert!(
        captured.starts_with(b"-- child nested=1 flag=override\n"),
        "{captured}"
    );
    assert!(
        captured.ends_with(b"compiler output\ncompiler output\n"),
        "{captured}"
    );
    let inherited = cx.var_or_empty(b"CHILD_OLD");
    assert!(
        inherited.starts_with(b"-- child nested=1 flag=parent\n"),
        "{inherited}"
    );
    assert!(host.stream_bytes(Stream::Stdout).is_empty());
    assert!(
        host.read_dir(Path::new(&format!(
            "{DRIVE}/work/build/CMakeFiles/CMakeScratch"
        )))?
        .is_empty()
    );
    assert!(
        host.metadata(
            Path::new(&format!("{DRIVE}/work/legacy/CMakeFiles/CMakeTmp")),
            Follow::No
        )
        .is_err()
    );
    assert_eq!(
        host.metadata(
            Path::new(&format!("{DRIVE}/work/build/project-new")),
            Follow::No
        )?
        .kind,
        rcmake_host::FileKind::Dir
    );
    let log = host.read(
        Path::new(&format!(
            "{DRIVE}/work/build/CMakeFiles/CMakeConfigureLog.yaml"
        )),
        ReadRange::All { limit: 16_384 },
    )?;
    let log = std::str::from_utf8(&log)?;
    assert_eq!(log.matches("try_compile-v1").count(), 1);
    assert!(log.contains("description: \"Child build\""));
    Ok(())
}

#[test]
fn preserves_msvc_tools_and_source_paths_in_scratch_projects() -> TestResult {
    let host = Arc::new(PureHost::default());
    let root = PathBuf::from(format!("{DRIVE}/work"));
    let compiler = format!("{DRIVE}/work/Probe Tools/cl.exe");
    host.seed_file(Path::new(&compiler), b"")?;
    host.seed_file(
        &root.join("CMakeLists.txt"),
        format!(
            r#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
set(CMAKE_C_COMPILER "{compiler}")
set(CMAKE_LINKER original-link)
set(CMAKE_MT manifest-tool)
set(CMAKE_RC_COMPILER resource-compiler)
set(CMAKE_DEPFILE_FLAGS_C "depfile flags")
set(CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT MultiThreadedDLL)
set(CMAKE_DEBUG_TRY_COMPILE ON)
try_compile(result SOURCE_FROM_CONTENT "probe source.c" "int main(void) {{ return 0; }}"
  CMAKE_FLAGS "-DCMAKE_LINKER=selected-link" NO_CACHE NO_LOG)
"#
        )
        .as_bytes(),
    )?;
    host.set_command_handler(|_, request, _| {
        assert!(!request.pipeline.is_empty());
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let mut interp = interpreter(host.clone(), &root)?;
    let binary = root.join("build space");
    let outcome = interp.run_configure(&root, &binary, &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(interp.cx().var_or_empty(b"result"), "TRUE");
    let scratch = binary.join("CMakeFiles/CMakeScratch");
    let directory = host
        .read_dir(&scratch)?
        .into_iter()
        .next()
        .ok_or("Missing scratch directory.")?;
    let probe = scratch.join(rcmake_host::path::to_native(&directory.name));
    let read = |name| host.read(&probe.join(name), ReadRange::All { limit: 64 * 1024 });
    let lists = String::from_utf8(read("CMakeLists.txt")?)?;
    let source = rcmake_host::path::from_native_cmake(&probe.join("probe source.c"));
    assert!(lists.contains(&source.to_string()), "{lists}");
    let cache = String::from_utf8(read("CMakeCache.txt")?)?;
    for (key, expected) in [
        ("CMAKE_LINKER", "selected-link"),
        ("CMAKE_MT", "manifest-tool"),
        ("CMAKE_RC_COMPILER", "resource-compiler"),
        ("CMAKE_DEPFILE_FLAGS_C", "depfile flags"),
        ("CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT", "MultiThreadedDLL"),
    ] {
        assert!(
            cache.lines().any(|line| {
                line.split_once('=').is_some_and(|(name, value)| {
                    name.split_once(':').is_some_and(|(name, _)| name == key) && value == expected
                })
            }),
            "{key} missing from {cache}"
        );
    }
    Ok(())
}

#[test]
fn sends_child_trace_to_the_parent_destination() -> TestResult {
    use rcmake_eval::{TraceConfig, TraceFormat};

    for format in [TraceFormat::Human, TraceFormat::JsonV1] {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
try_compile(CHILD PROJECT Child SOURCE_DIR /work/child OUTPUT_VARIABLE child_output NO_LOG)
message(STATUS "after probe")
"#,
        )?;
        host.seed_file(
            Path::new("/work/child/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Child NONE)
message(STATUS "inside probe")
"#,
        )?;
        let redirect = (format == TraceFormat::JsonV1).then(|| PathBuf::from("/work/trace"));
        let mut interp = interpreter(host.clone(), Path::new("/work"))?;
        interp.set_trace_config(Some(TraceConfig {
            format,
            redirect: redirect.clone(),
            ..TraceConfig::default()
        }));
        let result = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
        assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
        assert_eq!(
            interp.cx().var_or_empty(b"child_output"),
            "-- inside probe\n"
        );

        let trace = if let Some(path) = redirect {
            assert!(host.stream_bytes(Stream::Stderr).is_empty());
            host.read(&path, ReadRange::All { limit: 16_384 })?
        } else {
            host.stream_bytes(Stream::Stderr).to_vec()
        };
        let trace = std::str::from_utf8(&trace)?;
        let before = trace.find("try_compile").ok_or("Missing parent trace.")?;
        let inside = trace
            .find("/work/child/CMakeLists.txt")
            .ok_or("Missing child trace.")?;
        let after = trace.find("after probe").ok_or("Missing final trace.")?;
        assert!(before < inside && inside < after, "{trace}");
        if format == TraceFormat::JsonV1 {
            assert_eq!(trace.matches(r#""version":"#).count(), 1, "{trace}");
        }
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn builds_real_macho_archive_and_runs_a_probe() -> TestResult {
    use std::time::Duration;

    use rcmake_host::{CommandSpec, Input, NativeHost, Output, RunRequest};

    let host = Arc::new(NativeHost::new()?);
    let temp = host.run(
        &RunRequest {
            pipeline: vec![CommandSpec {
                program: "mktemp".into(),
                args: vec!["-d".into(), "/tmp/rcmake-probe.XXXXXXXX".into()],
            }],
            cwd: "/tmp".into(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Capture,
            timeout: Some(Duration::from_secs(15)),
            tee: false,
        },
        &mut |_, _| {},
    )?;
    assert_eq!(temp.results, [ProcessResult::Exited(0)]);
    let root = PathBuf::from(std::str::from_utf8(&temp.stdout)?.trim());
    struct Temp<'a>(&'a dyn Host, &'a Path);
    impl Drop for Temp<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_dir_all(self.1);
        }
    }
    let _temp = Temp(host.as_ref(), &root);
    host.write(
        &root.join("copied.c"),
        b"int f(void) { return 5; }",
        WriteMode::CreateNew,
    )?;
    host.write(
            &root.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Probe NONE)
set(CMAKE_C_COMPILER /usr/bin/cc)
set(CMAKE_CXX_COMPILER /usr/bin/c++)
set(CMAKE_OSX_ARCHITECTURES arm64)
set(probe_source [=[
#if FOO != 1 || BAR != 2
#error Missing compiler definitions
#endif
_Static_assert(sizeof(TEXT) == 4, "Expected a string with a space");
int main(void) { return 0; }
]=])
try_compile(RES SOURCE_FROM_VAR main.c probe_source
  COMPILE_DEFINITIONS "-v -Wl,-v" "-DFOO=1 -DBAR=2" "-DTEXT=\"a b\""
  CMAKE_FLAGS "-DEXE_LINKER_FLAGS=-v -Wl,-v"
  COPY_FILE "${CMAKE_BINARY_DIR}/out.bin" OUTPUT_VARIABLE out)
try_compile(CACHE_FLAGS SOURCE_FROM_VAR main.c probe_source
  CMAKE_FLAGS [=[-DCOMPILE_DEFINITIONS:STRING=-DFOO=1 -DBAR=2 -DTEXT='"a b"']=]
  OUTPUT_VARIABLE cache_out NO_LOG)
file(READ_MACHO "${CMAKE_BINARY_DIR}/out.bin" ARCHITECTURES arch)
try_compile(BAD SOURCE_FROM_CONTENT bad.c "int main(void){ return missing_symbol; }"
  OUTPUT_VARIABLE error NO_CACHE)
file(WRITE "${CMAKE_BINARY_DIR}/blocked" "file")
try_compile(COPY_RES SOURCE_FROM_CONTENT main.c "int main(void){return 0;}"
  COPY_FILE "${CMAKE_BINARY_DIR}/blocked/out.bin" COPY_FILE_ERROR copy_error NO_LOG)
try_run(RUN_RES COMPILE_RES SOURCE_FROM_CONTENT run.c [=[
#include <stdio.h>
int main(int argc, char **argv) {
  if (argc != 2) return 1;
  printf("arg=%s\n", argv[1]);
  fprintf(stderr, "error stream\n");
  return 7;
}
]=] NO_CACHE ARGS "hello world"
  RUN_OUTPUT_STDOUT_VARIABLE stdout RUN_OUTPUT_STDERR_VARIABLE stderr COMPILE_OUTPUT_VARIABLE build)
try_run(COMBINED_RUN COMBINED_BUILD SOURCE_FROM_CONTENT run.c [=[
#include <stdio.h>
int main(void) {
  fputs("first\n", stdout); fflush(stdout);
  fputs("second\n", stderr);
  return 0;
}
]=] NO_CACHE RUN_OUTPUT_VARIABLE combined NO_LOG)
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
set(code [=[
#if CHECK_VALUE != 3
#error Bad compiler definition
#endif
int g(void) {return CHECK_VALUE;}
]=])
try_compile(ARCHIVE SOURCE_FROM_FILE copied.c "${CMAKE_SOURCE_DIR}/copied.c" SOURCE_FROM_VAR more.c code
  COMPILE_DEFINITIONS -DCHECK_VALUE=3 C_STANDARD 11 C_EXTENSIONS OFF
  COPY_FILE "${CMAKE_BINARY_DIR}/library.a" OUTPUT_VARIABLE archive_out)
set(CMAKE_TRY_COMPILE_TARGET_TYPE EXECUTABLE)
try_compile(CPP SOURCE_FROM_CONTENT main.cpp [=[
template<class T> concept Valid = sizeof(T) > 0;
int main() { static_assert(Valid<int>); }
]=] CXX_STANDARD 20 CXX_EXTENSIONS OFF OUTPUT_VARIABLE cpp_out)
"#,
            WriteMode::CreateNew,
        )?;
    let mut interp = interpreter(host.clone(), &root)?;
    let binary = root.join("build");
    let result = interp.run_configure(&root, &binary, &[]);
    let cx = interp.cx();
    assert_eq!(result.exit_code, 0, "{}", cx.var_or_empty(b"out"));
    assert_eq!(
        cx.var_or_empty(b"RES"),
        "TRUE",
        "{}",
        cx.var_or_empty(b"out")
    );
    assert!(
        cx.var_or_empty(b"out")
            .to_string()
            .contains("clang version")
    );
    assert!(
        cx.var_or_empty(b"out").to_string().contains("PROGRAM:ld"),
        "{}",
        cx.var_or_empty(b"out")
    );
    assert_eq!(
        cx.var_or_empty(b"CACHE_FLAGS"),
        "TRUE",
        "{}",
        cx.var_or_empty(b"cache_out")
    );
    assert_eq!(cx.var_or_empty(b"arch"), "arm64");
    assert_eq!(cx.var_or_empty(b"BAD"), "FALSE");
    let error = cx.var_or_empty(b"error").to_string();
    assert!(
        error.contains("error:") && error.contains("missing_symbol"),
        "{error}"
    );
    assert_eq!(cx.var_or_empty(b"COPY_RES"), "TRUE");
    assert!(!cx.var_or_empty(b"copy_error").is_empty());
    assert_eq!(
        cx.var_or_empty(b"COMPILE_RES"),
        "TRUE",
        "{}",
        cx.var_or_empty(b"build")
    );
    assert_eq!(cx.var_or_empty(b"RUN_RES"), "7");
    assert_eq!(cx.var_or_empty(b"stdout"), "arg=hello world\n");
    assert_eq!(cx.var_or_empty(b"stderr"), "error stream\n");
    assert_eq!(cx.var_or_empty(b"COMBINED_RUN"), "0");
    assert_eq!(cx.var_or_empty(b"combined"), "first\nsecond\n");
    assert_eq!(
        cx.var_or_empty(b"ARCHIVE"),
        "TRUE",
        "{}",
        cx.var_or_empty(b"archive_out")
    );
    assert_eq!(
        host.read(
            &binary.join("library.a"),
            ReadRange::Slice {
                offset: 0,
                len: Some(8)
            }
        )?,
        b"!<arch>\n"
    );
    assert_eq!(
        cx.var_or_empty(b"CPP"),
        "TRUE",
        "{}",
        cx.var_or_empty(b"cpp_out")
    );
    let log = host.read(
        &binary.join("CMakeFiles/CMakeConfigureLog.yaml"),
        ReadRange::All { limit: 256 * 1024 },
    )?;
    let log = std::str::from_utf8(&log)?;
    assert_eq!(log.matches("try_compile-v1").count(), 4);
    assert_eq!(log.matches("try_run-v1").count(), 1);
    assert!(log.contains("variable: \"RUN_RES\""));
    Ok(())
}

#[test]
fn try_compile_can_nest_three_projects_with_the_default_recursion_limit() -> TestResult {
    let host = Arc::new(PureHost::default());
    for level in 0..=3 {
        let mut code = format!("cmake_minimum_required(VERSION 4.5)\nproject(Probe{level} NONE)\n");
        if level < 3 {
            let next = level + 1;
            code.push_str(&format!(
                "try_compile(success PROJECT Probe{next} SOURCE_DIR /work/probe{next} OUTPUT_VARIABLE output NO_LOG)\n\
                 if(NOT success)\n  message(FATAL_ERROR \"${{output}}\")\nendif()\n"
            ));
        }
        host.seed_file(
            Path::new(&format!("/work/probe{level}/CMakeLists.txt")),
            code.as_bytes(),
        )?;
    }
    let mut interp = interpreter(host.clone(), Path::new("/work"))?;
    let result = interp.run_configure(Path::new("/work/probe0"), Path::new("/work/build"), &[]);
    assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
    Ok(())
}
