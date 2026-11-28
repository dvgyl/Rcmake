use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Follow, Host, ProcessResult, PureHost, ReadRange, RunOutput, Stream, WriteMode};
use rcmake_model::CacheType;

const DRIVE: &str = if cfg!(windows) { "C:" } else { "" };

type TestResult = Result<(), Box<dyn std::error::Error>>;

pub(super) fn interpreter(
    host: Arc<dyn Host>,
    cwd: &Path,
) -> Result<Interp, Box<dyn std::error::Error>> {
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
        let file = modules.join(name);
        if host.metadata(&file, Follow::No).is_err() {
            host.write(&file, body.as_bytes(), WriteMode::CreateNew)?;
        }
    }
    let mut interp = Interp::new(
        InterpConfig {
            services: Default::default(),
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
try_compile(NEW SOURCES /work/main.c CMAKE_FLAGS "-DCOMPILE_DEFINITIONS=--probe-driver-flag" LINK_LIBRARIES " m " OUTPUT_VARIABLE SOURCE_OUT NO_CACHE NO_LOG)
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
    let source = cx.var_or_empty(b"SOURCE_OUT");
    assert!(source.starts_with(b"Change Dir: '"), "{source}");
    assert!(
        source.to_string().contains("\n\nRun Build Command(s): "),
        "{source}"
    );
    let captured = cx.var_or_empty(b"CHILD_OUT");
    assert!(captured.starts_with(b"Change Dir: '"), "{captured}");
    assert!(
        captured.to_string().contains("\n\nRun Build Command(s): "),
        "{captured}"
    );
    assert!(
        captured
            .to_string()
            .contains("-- child nested=1 flag=override\n"),
        "{captured}"
    );
    assert!(
        captured.ends_with(b"compiler output\ncompiler output\n\n"),
        "{captured}"
    );
    let inherited = cx.var_or_empty(b"CHILD_OLD");
    assert!(
        inherited
            .to_string()
            .contains("-- child nested=1 flag=parent\n"),
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
    // One document holds every event, in the CMake 4.5.1 field order.
    assert_eq!(log.matches("\n---\nevents:\n").count(), 1, "{log}");
    let event = log
        .split("  -\n")
        .find(|event| event.contains("try_compile-v1"))
        .ok_or("missing try_compile event")?;
    let order = [
        "kind:",
        "backtrace:",
        "description:",
        "directories:",
        "buildResult:",
    ]
    .map(|key| event.find(key));
    assert!(order.iter().all(Option::is_some), "{event}");
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{event}");
    let result = event
        .split("buildResult:\n")
        .nth(1)
        .ok_or("missing build result")?;
    let fields = ["variable:", "cached:", "stdout: |", "exitCode:"].map(|key| result.find(key));
    assert!(fields.iter().all(Option::is_some), "{event}");
    assert!(fields.windows(2).all(|pair| pair[0] < pair[1]), "{event}");
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

    let work = format!("{DRIVE}/work");
    let root = Path::new(&work);
    for format in [TraceFormat::Human, TraceFormat::JsonV1] {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            &root.join("CMakeLists.txt"),
            format!(
                r#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
try_compile(CHILD PROJECT Child SOURCE_DIR "{work}/child" OUTPUT_VARIABLE child_output NO_LOG)
message(STATUS "after probe")
"#
            )
            .as_bytes(),
        )?;
        host.seed_file(
            &root.join("child/CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Child NONE)
message(STATUS "inside probe")
"#,
        )?;
        let redirect = (format == TraceFormat::JsonV1).then(|| root.join("trace"));
        let mut interp = interpreter(host.clone(), root)?;
        interp.set_trace_config(Some(TraceConfig {
            format,
            redirect: redirect.clone(),
            ..TraceConfig::default()
        }));
        let result = interp.run_configure(root, &root.join("build"), &[]);
        assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
        let output = interp.cx().var_or_empty(b"child_output");
        assert!(output.starts_with(b"Change Dir: '"), "{output}");
        assert!(output.ends_with(b"-- inside probe\n\n"), "{output}");

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
            default_error_mode: false,
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
    let work = format!("{DRIVE}/work");
    let root = Path::new(&work);
    for level in 0..=3 {
        let mut code = format!("cmake_minimum_required(VERSION 4.5)\nproject(Probe{level} NONE)\n");
        if level < 3 {
            let next = level + 1;
            code.push_str(&format!(
                "try_compile(success PROJECT Probe{next} SOURCE_DIR \"{work}/probe{next}\" OUTPUT_VARIABLE output NO_LOG)\n\
                 if(NOT success)\n  message(FATAL_ERROR \"${{output}}\")\nendif()\n"
            ));
        }
        host.seed_file(
            &root.join(format!("probe{level}/CMakeLists.txt")),
            code.as_bytes(),
        )?;
    }
    let mut interp = interpreter(host.clone(), root)?;
    let result = interp.run_configure(&root.join("probe0"), &root.join("build"), &[]);
    assert_eq!(result.exit_code, 0, "{}", host.stream_bytes(Stream::Stderr));
    Ok(())
}

#[test]
fn multi_config_try_compile_honors_caller_config_and_xcode_debug_default() -> TestResult {
    use std::sync::Mutex;

    use rcmake_model::GeneratorKind;

    let run_case = |generator: GeneratorKind,
                    caller_config: Option<&str>,
                    config_types: &str|
     -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let host = Arc::new(PureHost::default());
        let seen_cmds = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = seen_cmds.clone();
        host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
        host.seed_file(
            Path::new(&format!("{DRIVE}/work/child/main.c")),
            b"int main(void) { return 0; }\n",
        )?;
        // Child explicitly sets CMAKE_BUILD_TYPE=MinSizeRel; multi-config try_compile must NOT
        // let child CMAKE_BUILD_TYPE override caller configuration or Xcode Debug default.
        host.seed_file(
            Path::new(&format!("{DRIVE}/work/child/CMakeLists.txt")),
            b"cmake_minimum_required(VERSION 4.5)\n\
              set(CMAKE_BUILD_TYPE MinSizeRel)\n\
              set(CMAKE_C_FLAGS_DEBUG \"-DFROM_DEBUG\")\n\
              set(CMAKE_C_FLAGS_RELEASE \"-DFROM_RELEASE\")\n\
              set(CMAKE_C_FLAGS_RELWITHDEBINFO \"-DFROM_RELWITHDEBINFO\")\n\
              set(CMAKE_C_FLAGS_MINSIZEREL \"-DFROM_MINSIZEREL\")\n\
              project(Child C)\n\
              add_executable(app main.c)\n",
        )?;
        host.set_command_handler(move |fs, req, _| {
            if let Some(cmd) = req.pipeline.first() {
                let rendered = format!(
                    "{} {}",
                    cmd.program,
                    cmd.args
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if let Ok(mut guard) = seen_clone.lock() {
                    guard.push(rendered);
                }
                if let Some(pos) = cmd.args.iter().position(|a| a == "-o")
                    && let Some(out) = cmd.args.get(pos + 1)
                {
                    let target = req.cwd.join(rcmake_host::path::to_native(out));
                    if let Some(parent) = target.parent() {
                        let _ = fs.create_dir_all(parent);
                    }
                    let _ = fs.write(&target, b"artifact", WriteMode::Truncate);
                }
                if cmd.args.as_slice() == [rcmake_base::BString::from("--version")] {
                    return Ok(RunOutput {
                        stdout: b"1.12.1\n".to_vec(),
                        results: vec![ProcessResult::Exited(0)],
                        ..RunOutput::default()
                    });
                }
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let root = PathBuf::from(format!("{DRIVE}/work"));
        let mut script = format!(
            "cmake_minimum_required(VERSION 4.5)\n\
             set(CMAKE_CONFIGURATION_TYPES \"{config_types}\")\n\
             set(CMAKE_C_COMPILER \"{DRIVE}/work/probe-cc\")\n\
             set(CMAKE_C_SOURCE_FILE_EXTENSIONS c)\n\
             set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT> <FLAGS>\")\n\
             set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> <OBJECTS> -o <TARGET>\")\n\
             set(CMAKE_C_FLAGS_DEBUG \"-DFROM_DEBUG\")\n\
             set(CMAKE_C_FLAGS_RELEASE \"-DFROM_RELEASE\")\n\
             set(CMAKE_C_FLAGS_RELWITHDEBINFO \"-DFROM_RELWITHDEBINFO\")\n"
        );
        if let Some(cfg) = caller_config {
            script.push_str(&format!("set(CMAKE_TRY_COMPILE_CONFIGURATION \"{cfg}\")\n"));
        }
        script.push_str(&format!(
            "try_compile(OK PROJECT Child SOURCE_DIR \"{DRIVE}/work/child\" BINARY_DIR \"{DRIVE}/work/child-build\" TARGET app NO_LOG)\n\
             if(NOT OK)\n  message(FATAL_ERROR \"probe failed\")\nendif()\n"
        ));
        host.seed_file(&root.join("CMakeLists.txt"), script.as_bytes())?;
        let mut interp = interpreter(host.clone(), &root)?;
        interp.cx().project().generator = Some(generator);
        let outcome = interp.run_configure(&root, &root.join("build"), &[]);
        assert_eq!(
            outcome.exit_code,
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        let cmds = seen_cmds.lock().map_err(|_| "lock poisoned")?.clone();
        Ok(cmds)
    };

    // 1. Xcode with unset CMAKE_TRY_COMPILE_CONFIGURATION defaults to Debug even when Release is
    //    first in CMAKE_CONFIGURATION_TYPES and child sets CMAKE_BUILD_TYPE=MinSizeRel.
    let xcode_default = run_case(GeneratorKind::Xcode, None, "Release;Debug")?;
    assert!(
        xcode_default.iter().any(|c| c.contains("-DFROM_DEBUG")),
        "{xcode_default:?}"
    );
    assert!(
        !xcode_default.iter().any(|c| c.contains("MinSizeRel")),
        "{xcode_default:?}"
    );

    // 2. Xcode with explicit CMAKE_TRY_COMPILE_CONFIGURATION=RelWithDebInfo honors caller config
    //    over child CMAKE_BUILD_TYPE=MinSizeRel.
    let xcode_explicit = run_case(
        GeneratorKind::Xcode,
        Some("RelWithDebInfo"),
        "Release;Debug",
    )?;
    assert!(
        xcode_explicit
            .iter()
            .any(|c| c.contains("-DFROM_RELWITHDEBINFO")),
        "{xcode_explicit:?}"
    );
    assert!(
        !xcode_explicit.iter().any(|c| c.contains("MinSizeRel")),
        "{xcode_explicit:?}"
    );

    // 3. NinjaMultiConfig PROJECT-signature probe with no explicit CMAKE_TRY_COMPILE_CONFIGURATION
    //    resolves its implicit default from the configured child project (CMAKE_DEFAULT_BUILD_TYPE
    //    or child's first CMAKE_CONFIGURATION_TYPES), not the caller's configuration list.
    {
        let run_child_ninja_multi =
            |child_extra: &str| -> Result<Vec<String>, Box<dyn std::error::Error>> {
                let host = Arc::new(PureHost::default());
                let seen_cmds = Arc::new(Mutex::new(Vec::<String>::new()));
                let seen_clone = seen_cmds.clone();
                host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
                host.seed_file(
                    Path::new(&format!("{DRIVE}/work/child/main.c")),
                    b"int main(void) { return 0; }\n",
                )?;
                host.seed_file(
                    Path::new(&format!("{DRIVE}/work/child/CMakeLists.txt")),
                    format!(
                        "cmake_minimum_required(VERSION 4.5)\n\
                     {child_extra}\n\
                     set(CMAKE_C_FLAGS_DEBUG \"-DFROM_DEBUG\")\n\
                     set(CMAKE_C_FLAGS_RELEASE \"-DFROM_RELEASE\")\n\
                     set(CMAKE_C_FLAGS_RELWITHDEBINFO \"-DFROM_RELWITHDEBINFO\")\n\
                     set(CMAKE_C_FLAGS_MINSIZEREL \"-DFROM_MINSIZEREL\")\n\
                     project(Child C)\n\
                     add_executable(app main.c)\n"
                    )
                    .as_bytes(),
                )?;
                host.set_command_handler(move |fs, req, _| {
                    if let Some(cmd) = req.pipeline.first() {
                        let rendered = format!(
                            "{} {}",
                            cmd.program,
                            cmd.args
                                .iter()
                                .map(|a| a.to_string())
                                .collect::<Vec<_>>()
                                .join(" ")
                        );
                        if let Ok(mut guard) = seen_clone.lock() {
                            guard.push(rendered);
                        }
                        if let Some(pos) = cmd.args.iter().position(|a| a == "-o")
                            && let Some(out) = cmd.args.get(pos + 1)
                        {
                            let target = req.cwd.join(rcmake_host::path::to_native(out));
                            if let Some(parent) = target.parent() {
                                let _ = fs.create_dir_all(parent);
                            }
                            let _ = fs.write(&target, b"artifact", WriteMode::Truncate);
                        }
                        if cmd.args.as_slice() == [rcmake_base::BString::from("--version")] {
                            return Ok(RunOutput {
                                stdout: b"1.12.1\n".to_vec(),
                                results: vec![ProcessResult::Exited(0)],
                                ..RunOutput::default()
                            });
                        }
                    }
                    Ok(RunOutput {
                        results: vec![ProcessResult::Exited(0)],
                        ..RunOutput::default()
                    })
                })?;
                let root = PathBuf::from(format!("{DRIVE}/work"));
                host.seed_file(
                &root.join("CMakeLists.txt"),
                format!(
                    "cmake_minimum_required(VERSION 4.5)\n\
                     set(CMAKE_CONFIGURATION_TYPES \"Debug;MinSizeRel\")\n\
                     set(CMAKE_DEFAULT_BUILD_TYPE \"Debug\")\n\
                     set(CMAKE_C_COMPILER \"{DRIVE}/work/probe-cc\")\n\
                     set(CMAKE_C_SOURCE_FILE_EXTENSIONS c)\n\
                     set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT> <FLAGS>\")\n\
                     set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> <OBJECTS> -o <TARGET>\")\n\
                     try_compile(OK PROJECT Child SOURCE_DIR \"{DRIVE}/work/child\" BINARY_DIR \"{DRIVE}/work/child-build\" TARGET app NO_LOG)\n\
                     if(NOT OK)\n  message(FATAL_ERROR \"probe failed\")\nendif()\n"
                )
                .as_bytes(),
            )?;
                let mut interp = interpreter(host.clone(), &root)?;
                interp.cx().project().generator = Some(GeneratorKind::NinjaMultiConfig);
                let outcome = interp.run_configure(&root, &root.join("build"), &[]);
                assert_eq!(
                    outcome.exit_code,
                    0,
                    "{}",
                    host.stream_bytes(Stream::Stderr)
                );
                let cmds = seen_cmds.lock().map_err(|_| "lock poisoned")?.clone();
                Ok(cmds)
            };

        let child_default_type = run_child_ninja_multi(
            "set(CMAKE_CONFIGURATION_TYPES \"Release;RelWithDebInfo\")\nset(CMAKE_DEFAULT_BUILD_TYPE \"RelWithDebInfo\")",
        )?;
        assert!(
            child_default_type
                .iter()
                .any(|c| c.contains("-DFROM_RELWITHDEBINFO")),
            "{child_default_type:?}"
        );
        assert!(
            !child_default_type
                .iter()
                .any(|c| c.contains("-DFROM_DEBUG")),
            "{child_default_type:?}"
        );

        let child_first_type =
            run_child_ninja_multi("set(CMAKE_CONFIGURATION_TYPES \"Release;Debug\")")?;
        assert!(
            child_first_type
                .iter()
                .any(|c| c.contains("-DFROM_RELEASE")),
            "{child_first_type:?}"
        );
        assert!(
            !child_first_type.iter().any(|c| c.contains("-DFROM_DEBUG")),
            "{child_first_type:?}"
        );

        // Child with CMAKE_DEFAULT_BUILD_TYPE=Debug, CMAKE_CROSS_CONFIGS=all, and
        // CMAKE_DEFAULT_CONFIGS=Release builds Release (the default target config), not Debug.
        let child_default_configs = run_child_ninja_multi(
            "set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
             set(CMAKE_DEFAULT_BUILD_TYPE \"Debug\")\n\
             set(CMAKE_CROSS_CONFIGS \"all\")\n\
             set(CMAKE_DEFAULT_CONFIGS \"Release\")",
        )?;
        assert!(
            child_default_configs
                .iter()
                .any(|c| c.contains("-DFROM_RELEASE")),
            "{child_default_configs:?}"
        );
        assert!(
            !child_default_configs
                .iter()
                .any(|c| c.contains("-DFROM_DEBUG")),
            "{child_default_configs:?}"
        );

        // Child with CMAKE_DEFAULT_CONFIGS=all builds every cross configuration.
        let child_default_all = run_child_ninja_multi(
            "set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
             set(CMAKE_DEFAULT_BUILD_TYPE \"Debug\")\n\
             set(CMAKE_CROSS_CONFIGS \"all\")\n\
             set(CMAKE_DEFAULT_CONFIGS \"all\")",
        )?;
        assert!(
            child_default_all.iter().any(|c| c.contains("-DFROM_DEBUG")),
            "{child_default_all:?}"
        );
        assert!(
            child_default_all
                .iter()
                .any(|c| c.contains("-DFROM_RELEASE")),
            "{child_default_all:?}"
        );

        // Child with CMAKE_DEFAULT_BUILD_TYPE=Debug, CMAKE_CROSS_CONFIGS=all, and
        // CMAKE_DEFAULT_CONFIGS=Release builds two child-defined Debug generator executables
        // (`gen_tool_a` and `gen_tool_b`) that share one generated input (`shared.c`) BEFORE
        // invoking them in the (Debug, Release) cross graph, deduplicating the shared producer
        // step so it executes exactly once across the entire probe build.
        {
            let host = Arc::new(PureHost::default());
            let seen_cmds = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_clone = seen_cmds.clone();
            host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
            host.seed_file(Path::new(&format!("{DRIVE}/work/make-shared")), b"")?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/gen_tool_a.c")),
                b"int main(void) { return 0; }\n",
            )?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/gen_tool_b.c")),
                b"int main(void) { return 0; }\n",
            )?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/CMakeLists.txt")),
                format!(
                    "cmake_minimum_required(VERSION 4.5)\n\
                     set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
                     set(CMAKE_DEFAULT_BUILD_TYPE \"Debug\")\n\
                     set(CMAKE_CROSS_CONFIGS \"all\")\n\
                     set(CMAKE_DEFAULT_CONFIGS \"Release\")\n\
                     set(CMAKE_C_FLAGS_DEBUG \"-DFROM_DEBUG\")\n\
                     set(CMAKE_C_FLAGS_RELEASE \"-DFROM_RELEASE\")\n\
                     project(Child C)\n\
                     add_custom_command(\n\
                       OUTPUT \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/shared.c\"\n\
                       COMMAND \"{DRIVE}/work/make-shared\" \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/shared.c\"\n\
                     )\n\
                     add_library(shared_dep STATIC \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/shared.c\")\n\
                     add_executable(gen_tool_a gen_tool_a.c)\n\
                     target_link_libraries(gen_tool_a PRIVATE shared_dep)\n\
                     add_executable(gen_tool_b gen_tool_b.c)\n\
                     target_link_libraries(gen_tool_b PRIVATE shared_dep)\n\
                     add_custom_command(\n\
                       OUTPUT \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/gen_a.c\"\n\
                       COMMAND gen_tool_a \"--cmd=$<COMMAND_CONFIG:$<CONFIG>>\" \"--out=$<OUTPUT_CONFIG:$<CONFIG>>\" \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/gen_a.c\"\n\
                     )\n\
                     add_custom_command(\n\
                       OUTPUT \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/gen_b.c\"\n\
                       COMMAND gen_tool_b \"--cmd=$<COMMAND_CONFIG:$<CONFIG>>\" \"--out=$<OUTPUT_CONFIG:$<CONFIG>>\" \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/gen_b.c\"\n\
                     )\n\
                     add_executable(app \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/gen_a.c\" \"${{CMAKE_CURRENT_BINARY_DIR}}/$<CONFIG>/gen_b.c\")\n"
                )
                .as_bytes(),
            )?;
            host.set_command_handler(move |fs, req, _| {
                if let Some(cmd) = req.pipeline.first() {
                    let rendered = format!(
                        "{} {}",
                        cmd.program,
                        cmd.args
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    if let Ok(mut guard) = seen_clone.lock() {
                        guard.push(rendered);
                    }
                    let program_str = cmd.program.to_string();
                    if program_str.ends_with("make-shared")
                        && let Some(out) = cmd.args.last()
                    {
                        let target = req.cwd.join(rcmake_host::path::to_native(out));
                        if let Some(parent) = target.parent() {
                            let _ = fs.create_dir_all(parent);
                        }
                        let _ = fs.write(
                            &target,
                            b"int shared_sym(void) { return 1; }\n",
                            WriteMode::Truncate,
                        );
                    }
                    if program_str.ends_with("gen_tool_a")
                        || program_str.ends_with("gen_tool_a.exe")
                        || program_str.ends_with("gen_tool_b")
                        || program_str.ends_with("gen_tool_b.exe")
                    {
                        let tool_path = req.cwd.join(rcmake_host::path::to_native(&cmd.program));
                        if fs.metadata(&tool_path, rcmake_host::Follow::Yes).is_err() {
                            return Ok(RunOutput {
                                stderr: format!(
                                    "missing generator tool: {}\n",
                                    tool_path.display()
                                )
                                .into_bytes(),
                                results: vec![ProcessResult::Exited(127)],
                                ..RunOutput::default()
                            });
                        }
                        if let Some(out) = cmd.args.last() {
                            let target = req.cwd.join(rcmake_host::path::to_native(out));
                            if let Some(parent) = target.parent() {
                                let _ = fs.create_dir_all(parent);
                            }
                            let body = if program_str.contains("gen_tool_a") {
                                b"int main(void) { return 0; }\n".as_slice()
                            } else {
                                b"int helper_b(void) { return 0; }\n".as_slice()
                            };
                            let _ = fs.write(&target, body, WriteMode::Truncate);
                        }
                    }
                    if let Some(pos) = cmd.args.iter().position(|a| a == "-o")
                        && let Some(out) = cmd.args.get(pos + 1)
                    {
                        let target = req.cwd.join(rcmake_host::path::to_native(out));
                        if let Some(parent) = target.parent() {
                            let _ = fs.create_dir_all(parent);
                        }
                        let _ = fs.write(&target, b"artifact", WriteMode::Truncate);
                    }
                    if cmd.args.as_slice() == [rcmake_base::BString::from("--version")] {
                        return Ok(RunOutput {
                            stdout: b"1.12.1\n".to_vec(),
                            results: vec![ProcessResult::Exited(0)],
                            ..RunOutput::default()
                        });
                    }
                }
                Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                })
            })?;
            let root = PathBuf::from(format!("{DRIVE}/work"));
            host.seed_file(
                &root.join("CMakeLists.txt"),
                format!(
                    "cmake_minimum_required(VERSION 4.5)\n\
                     set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
                     set(CMAKE_MAKE_PROGRAM \"{DRIVE}/work/ninja\")\n\
                     set(CMAKE_C_COMPILER \"{DRIVE}/work/probe-cc\")\n\
                     set(CMAKE_C_SOURCE_FILE_EXTENSIONS c)\n\
                     set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT> <FLAGS>\")\n\
                     set(CMAKE_C_CREATE_STATIC_LIBRARY \"<CMAKE_C_COMPILER> -o <TARGET> <OBJECTS>\")\n\
                     set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> <OBJECTS> -o <TARGET>\")\n\
                     try_compile(OK PROJECT Child SOURCE_DIR \"{DRIVE}/work/child\" BINARY_DIR \"{DRIVE}/work/child-build\" TARGET app NO_LOG)\n\
                     if(NOT OK)\n  message(FATAL_ERROR \"cross-config probe failed\")\nendif()\n"
                )
                .as_bytes(),
            )?;
            let mut interp = interpreter(host.clone(), &root)?;
            interp.cx().project().generator = Some(GeneratorKind::NinjaMultiConfig);
            let outcome = interp.run_configure(&root, &root.join("build"), &[]);
            assert_eq!(
                outcome.exit_code,
                0,
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
            let cmds = seen_cmds.lock().map_err(|_| "lock poisoned")?.clone();
            let shared_runs = cmds.iter().filter(|c| c.contains("make-shared")).count();
            assert_eq!(
                shared_runs, 1,
                "shared Debug producer step must run exactly once across both tools, got {cmds:?}"
            );
            let shared_idx = cmds.iter().position(|c| c.contains("make-shared"));
            let build_gen_a = cmds
                .iter()
                .position(|c| c.contains("gen_tool_a.c") && c.contains("-DFROM_DEBUG"));
            let build_gen_b = cmds
                .iter()
                .position(|c| c.contains("gen_tool_b.c") && c.contains("-DFROM_DEBUG"));
            let run_gen_a = cmds.iter().position(|c| {
                c.contains("Debug/gen_tool_a")
                    && c.contains("--cmd=Debug")
                    && c.contains("--out=Release")
            });
            let run_gen_b = cmds.iter().position(|c| {
                c.contains("Debug/gen_tool_b")
                    && c.contains("--cmd=Debug")
                    && c.contains("--out=Release")
            });
            let build_app = cmds
                .iter()
                .position(|c| c.contains("Release/gen_a.c") && c.contains("-DFROM_RELEASE"));
            assert!(
                shared_idx.is_some()
                    && build_gen_a.is_some()
                    && build_gen_b.is_some()
                    && run_gen_a.is_some()
                    && run_gen_b.is_some()
                    && build_app.is_some()
                    && shared_idx < build_gen_a
                    && shared_idx < build_gen_b
                    && build_gen_a < run_gen_a
                    && build_gen_b < run_gen_b
                    && run_gen_a < build_app
                    && run_gen_b < build_app,
                "expected shared Debug producer once -> Debug tools -> Release custom commands -> Release app, got {cmds:?}"
            );
        }

        // Explicit CMAKE_TRY_COMPILE_CONFIGURATION absent from child CMAKE_CONFIGURATION_TYPES
        // fails membership validation instead of lowering an unconfigured build file.
        {
            let host = Arc::new(PureHost::default());
            host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/main.c")),
                b"int main(void) { return 0; }\n",
            )?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/CMakeLists.txt")),
                b"cmake_minimum_required(VERSION 4.5)\n\
                  set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
                  project(Child C)\n\
                  add_executable(app main.c)\n",
            )?;
            host.set_command_handler(move |_, req, _| {
                if let Some(cmd) = req.pipeline.first()
                    && cmd.args.as_slice() == [rcmake_base::BString::from("--version")]
                {
                    return Ok(RunOutput {
                        stdout: b"1.12.1\n".to_vec(),
                        results: vec![ProcessResult::Exited(0)],
                        ..RunOutput::default()
                    });
                }
                Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                })
            })?;
            let root = PathBuf::from(format!("{DRIVE}/work"));
            host.seed_file(
                &root.join("CMakeLists.txt"),
                format!(
                    "cmake_minimum_required(VERSION 4.5)\n\
                     set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
                     set(CMAKE_TRY_COMPILE_CONFIGURATION \"NonExistent\")\n\
                     set(CMAKE_MAKE_PROGRAM \"{DRIVE}/work/ninja\")\n\
                     set(CMAKE_C_COMPILER \"{DRIVE}/work/probe-cc\")\n\
                     set(CMAKE_C_SOURCE_FILE_EXTENSIONS c)\n\
                     set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT> <FLAGS>\")\n\
                     set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> <OBJECTS> -o <TARGET>\")\n\
                     try_compile(OK PROJECT Child SOURCE_DIR \"{DRIVE}/work/child\" BINARY_DIR \"{DRIVE}/work/child-build\" TARGET app OUTPUT_VARIABLE LOG NO_LOG)\n"
                )
                .as_bytes(),
            )?;
            let mut interp = interpreter(host.clone(), &root)?;
            interp.cx().project().generator = Some(GeneratorKind::NinjaMultiConfig);
            let outcome = interp.run_configure(&root, &root.join("build"), &[]);
            assert_eq!(
                outcome.exit_code,
                1,
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
            assert_eq!(interp.cx().var_or_empty(b"OK"), "FALSE");
            assert!(
                interp
                    .cx()
                    .var_or_empty(b"LOG")
                    .to_string()
                    .contains("not present in CMAKE_CONFIGURATION_TYPES"),
                "{}",
                interp.cx().var_or_empty(b"LOG")
            );
        }

        // Child whose CMAKE_DEFAULT_BUILD_TYPE is absent from CMAKE_CONFIGURATION_TYPES fails
        // membership validation instead of lowering an absent configuration.
        {
            let host = Arc::new(PureHost::default());
            host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/main.c")),
                b"int main(void) { return 0; }\n",
            )?;
            host.seed_file(
                Path::new(&format!("{DRIVE}/work/child/CMakeLists.txt")),
                b"cmake_minimum_required(VERSION 4.5)\n\
                  set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
                  set(CMAKE_DEFAULT_BUILD_TYPE \"RelWithDebInfo\")\n\
                  project(Child C)\n\
                  add_executable(app main.c)\n",
            )?;
            host.set_command_handler(move |_, req, _| {
                if let Some(cmd) = req.pipeline.first()
                    && cmd.args.as_slice() == [rcmake_base::BString::from("--version")]
                {
                    return Ok(RunOutput {
                        stdout: b"1.12.1\n".to_vec(),
                        results: vec![ProcessResult::Exited(0)],
                        ..RunOutput::default()
                    });
                }
                Ok(RunOutput {
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                })
            })?;
            let root = PathBuf::from(format!("{DRIVE}/work"));
            host.seed_file(
                &root.join("CMakeLists.txt"),
                format!(
                    "cmake_minimum_required(VERSION 4.5)\n\
                     set(CMAKE_CONFIGURATION_TYPES \"Debug;Release\")\n\
                     set(CMAKE_MAKE_PROGRAM \"{DRIVE}/work/ninja\")\n\
                     set(CMAKE_C_COMPILER \"{DRIVE}/work/probe-cc\")\n\
                     set(CMAKE_C_SOURCE_FILE_EXTENSIONS c)\n\
                     set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT> <FLAGS>\")\n\
                     set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> <OBJECTS> -o <TARGET>\")\n\
                     try_compile(OK PROJECT Child SOURCE_DIR \"{DRIVE}/work/child\" BINARY_DIR \"{DRIVE}/work/child-build\" TARGET app OUTPUT_VARIABLE LOG NO_LOG)\n"
                )
                .as_bytes(),
            )?;
            let mut interp = interpreter(host.clone(), &root)?;
            interp.cx().project().generator = Some(GeneratorKind::NinjaMultiConfig);
            let outcome = interp.run_configure(&root, &root.join("build"), &[]);
            assert_eq!(
                outcome.exit_code,
                1,
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
            assert!(
                host.stream_bytes(Stream::Stderr)
                    .to_string()
                    .contains("Failed to generate test project build system."),
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
            assert_eq!(interp.cx().var_or_empty(b"OK"), "FALSE");
            assert!(
                interp
                    .cx()
                    .var_or_empty(b"LOG")
                    .to_string()
                    .contains("not present in CMAKE_CONFIGURATION_TYPES"),
                "{}",
                interp.cx().var_or_empty(b"LOG")
            );
        }
    }

    // 4. Source-file try_compile on Xcode honors CMAKE_TRY_COMPILE_CONFIGURATION for CMP0066 flags.
    {
        let host = Arc::new(PureHost::default());
        let seen_cmds = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = seen_cmds.clone();
        host.seed_file(Path::new(&format!("{DRIVE}/work/probe-cc")), b"")?;
        host.seed_file(
            Path::new(&format!("{DRIVE}/work/src.c")),
            b"int main(void) { return 0; }\n",
        )?;
        host.set_command_handler(move |fs, req, _| {
            if let Some(cmd) = req.pipeline.first() {
                let rendered = format!(
                    "{} {}",
                    cmd.program,
                    cmd.args
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                if let Ok(mut guard) = seen_clone.lock() {
                    guard.push(rendered);
                }
                if let Some(pos) = cmd.args.iter().position(|a| a == "-o")
                    && let Some(out) = cmd.args.get(pos + 1)
                {
                    let target = req.cwd.join(rcmake_host::path::to_native(out));
                    if let Some(parent) = target.parent() {
                        let _ = fs.create_dir_all(parent);
                    }
                    let _ = fs.write(&target, b"artifact", WriteMode::Truncate);
                }
                if cmd.args.as_slice() == [rcmake_base::BString::from("--version")] {
                    return Ok(RunOutput {
                        stdout: b"1.12.1\n".to_vec(),
                        results: vec![ProcessResult::Exited(0)],
                        ..RunOutput::default()
                    });
                }
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let root = PathBuf::from(format!("{DRIVE}/work"));
        host.seed_file(
            &root.join("CMakeLists.txt"),
            format!(
                "cmake_minimum_required(VERSION 4.5)\n\
                 set(CMAKE_CONFIGURATION_TYPES \"Release;Debug\")\n\
                 set(CMAKE_TRY_COMPILE_CONFIGURATION \"RelWithDebInfo\")\n\
                 set(CMAKE_C_COMPILER \"{DRIVE}/work/probe-cc\")\n\
                 set(CMAKE_C_SOURCE_FILE_EXTENSIONS c)\n\
                 set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT> <FLAGS>\")\n\
                 set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> <OBJECTS> -o <TARGET>\")\n\
                 set(CMAKE_C_FLAGS_DEBUG \"-DCALLER_DEBUG\")\n\
                 set(CMAKE_C_FLAGS_RELWITHDEBINFO \"-DCALLER_RELWITHDEBINFO\")\n\
                 try_compile(OK SOURCES \"{DRIVE}/work/src.c\" NO_LOG)\n\
                 if(NOT OK)\n  message(FATAL_ERROR \"source probe failed\")\nendif()\n"
            )
            .as_bytes(),
        )?;
        let mut interp = interpreter(host.clone(), &root)?;
        interp.cx().project().generator = Some(GeneratorKind::Xcode);
        let outcome = interp.run_configure(&root, &root.join("build"), &[]);
        assert_eq!(
            outcome.exit_code,
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        let cmds = seen_cmds.lock().map_err(|_| "lock poisoned")?.clone();
        assert!(
            cmds.iter().any(|c| c.contains("-DCALLER_RELWITHDEBINFO")),
            "{cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("-DCALLER_DEBUG")),
            "{cmds:?}"
        );
    }
    Ok(())
}

#[cfg(all(feature = "native", any(unix, windows)))]
#[test]
fn ninja_verified_query_keeps_observe_and_speculate_probes_pure() -> TestResult {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rcmake_host::{HostError, RunRequest};
    use rcmake_model::GeneratorKind;
    use rcmake_store::{InputStamp, Trace, file_digest};

    use super::journal::{DependencyClosure, JournalHost, JournalMode, ProcessVerifier};
    use crate::configure::build_tool_facts::{BuildToolFacts, VerifiedNinja};

    let launches = Arc::new(AtomicUsize::new(0));
    let host = Arc::new(PureHost::default());
    let root = PathBuf::from(format!("{DRIVE}/work"));
    let ninja_path = root.join("bin/ninja");
    host.seed_file(&ninja_path, b"\x7fELFninja-fixture")?;
    host.set_permissions(&ninja_path, 0o755)?;
    host.seed_file(
        &root.join("child/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\nproject(Child NONE)\n",
    )?;
    // Seed shared modules on `host` before creating any JournalHost so module setup does not
    // look like an external write from inside the journaled probe.
    let _seed = interpreter(host.clone(), &root)?;
    host.seed_file(
        &root.join("cmake/Modules/CMakeDetermineSystem.cmake"),
        b"set(CMAKE_SYSTEM_LOADED 1)\n",
    )?;
    let ninja_str = rcmake_host::path::from_native(&ninja_path);
    let ninja_for_handler = ninja_str.clone();
    let launches_clone = launches.clone();
    host.set_command_handler(move |_, req, _| {
        launches_clone.fetch_add(1, Ordering::Relaxed);
        let [cmd] = req.pipeline.as_slice() else {
            return Err(HostError::unsupported("unexpected pipeline"));
        };
        if cmd.program == ninja_for_handler
            && cmd.args.as_slice() == [rcmake_base::BString::from("--version")]
        {
            return Ok(RunOutput {
                stdout: b"1.12.1\n".to_vec(),
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            });
        }
        Err(HostError::unsupported("unexpected command"))
    })?;

    let facts = BuildToolFacts::default();
    let digest = file_digest(host.as_ref(), &ninja_path)?;
    // A caller-dir observation must NOT match a child scratch directory.
    let caller_verified = facts
        .sample_ninja(
            host.as_ref(),
            &ninja_path,
            digest,
            host.env_all(),
            root.clone(),
        )
        .ok_or("expected caller verified ninja")?;
    assert_eq!(launches.load(Ordering::Relaxed), 1);

    struct NinjaVerifier {
        ninja: VerifiedNinja,
        hits: AtomicUsize,
    }
    impl ProcessVerifier for NinjaVerifier {
        fn recorded_platform_query(&self, request: &RunRequest) -> Option<RunOutput> {
            self.ninja.matches(request).then(|| {
                self.hits.fetch_add(1, Ordering::Relaxed);
                self.ninja.output.clone()
            })
        }
        fn preflight(&self, _: &RunRequest) -> Option<Trace> {
            None
        }
        fn dependencies(
            &self,
            _: &RunRequest,
            _: &RunOutput,
            _: &[InputStamp],
        ) -> Option<DependencyClosure> {
            None
        }
    }

    let mut total_hits = 0usize;
    for mode in [JournalMode::Observe, JournalMode::Speculate] {
        let scratch = root.join(format!("scratch-{mode:?}"));
        host.create_dir_all(&scratch)?;
        assert!(
            !caller_verified.matches(&RunRequest {
                pipeline: vec![rcmake_host::CommandSpec {
                    program: ninja_str.clone(),
                    args: vec!["--version".into()],
                }],
                cwd: scratch.clone(),
                env: host.env_all(),
                stdin: rcmake_host::Input::Null,
                stdout: rcmake_host::Output::Capture,
                stderr: rcmake_host::Output::Capture,
                timeout: Some(std::time::Duration::from_secs(5)),
                tee: false,
                default_error_mode: false,
            }),
            "caller-dir VerifiedNinja must not replay into a different scratch directory"
        );
        let verified = facts
            .sample_ninja(
                host.as_ref(),
                &ninja_path,
                digest,
                host.env_all(),
                scratch.clone(),
            )
            .ok_or("expected scratch verified ninja")?;
        let verifier = Arc::new(NinjaVerifier {
            ninja: verified,
            hits: AtomicUsize::new(0),
        });
        let journal = Arc::new(JournalHost::from_shared(
            host.clone(),
            &scratch,
            verifier.clone(),
            mode,
        )?);
        let mut child = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: journal.clone(),
                mode: RunMode::Configure,
                cmake_root: root.join("cmake"),
                cmake_command: root.join("cmake/bin/cmake"),
                cwd: root.clone(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        {
            let mut cx = child.cx();
            cx.project().generator = Some(GeneratorKind::Ninja);
            cx.set_var(b"CMAKE_SYSTEM", cx.host().system().sysname);
            cx.set_var(
                b"CMAKE_MAKE_PROGRAM",
                rcmake_host::path::from_native(&ninja_path),
            );
        }
        let outcome = child.run_configure(&root.join("child"), &scratch, &[]);
        assert_eq!(
            outcome.exit_code,
            0,
            "mode={mode:?}: {}",
            host.stream_bytes(Stream::Stderr)
        );
        drop(child);
        let finished = Arc::try_unwrap(journal)
            .map_err(|_| HostError::unsupported("journal still held"))?
            .finish();
        assert_eq!(finished.impure(), None, "mode={mode:?} marked impure");
        total_hits += verifier.hits.load(Ordering::Relaxed);
    }
    assert_eq!(
        total_hits, 2,
        "both Observe and Speculate probes must replay their scratch-live Ninja observation via recorded_platform_query"
    );
    assert_eq!(
        launches.load(Ordering::Relaxed),
        3,
        "one live verification per distinct directory (caller + Observe scratch + Speculate scratch)"
    );
    Ok(())
}

#[cfg(all(feature = "native", target_os = "macos"))]
mod platform_queries {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rcmake_host::{HostError, HostResult, RunRequest};
    use rcmake_store::{InputStamp, Trace};

    use super::super::initialize_probe;
    use super::super::journal::{DependencyClosure, JournalHost, JournalMode, ProcessVerifier};
    use super::{Host, Path, ProcessResult, PureHost, RunOutput, Stream, TestResult, interpreter};
    use crate::configure::platform_queries::PlatformQueries;
    use std::sync::Arc;

    const MODULE: &str = r#"
execute_process(COMMAND /usr/bin/sw_vers -productVersion
    WORKING_DIRECTORY "${CMAKE_BINARY_DIR}" OUTPUT_VARIABLE version)
if(NOT version STREQUAL "15.0")
    message(FATAL_ERROR "Unexpected platform query result: ${version}")
endif()
"#;

    fn query_output(request: &RunRequest) -> Option<RunOutput> {
        let [command] = request.pipeline.as_slice() else {
            return None;
        };
        (command.program == "/usr/bin/sw_vers"
            && command.args.len() == 1
            && command
                .args
                .first()
                .is_some_and(|arg| arg == "-productVersion"))
        .then(|| RunOutput {
            stdout: b"15.0".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    }

    fn host(launches: &Arc<AtomicUsize>) -> HostResult<Arc<PureHost>> {
        host_with_stderr(launches, b"")
    }

    /// A host whose platform query also writes `stderr` to its standard error.
    fn host_with_stderr(
        launches: &Arc<AtomicUsize>,
        stderr: &'static [u8],
    ) -> HostResult<Arc<PureHost>> {
        let host = Arc::new(PureHost::default());
        let tool = Path::new("/usr/bin/sw_vers");
        host.seed_file(tool, b"fixture")?;
        host.set_permissions(tool, 0o755)?;
        host.seed_file(
            Path::new("/work/cmake/Modules/Platform/Darwin-Initialize.cmake"),
            MODULE.as_bytes(),
        )?;
        host.seed_file(
            Path::new("/work/child/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Child NONE)\ninclude(Platform/Darwin-Initialize)\n",
        )?;
        let launches = launches.clone();
        host.set_command_handler(move |_, request, _| {
            launches.fetch_add(1, Ordering::Relaxed);
            let mut output = query_output(request)
                .ok_or_else(|| HostError::unsupported("Unexpected process."))?;
            output.stderr = stderr.to_vec();
            Ok(output)
        })?;
        Ok(host)
    }

    const NOTE: &str = "sw_vers note\n";

    #[test]
    fn inherited_query_stderr_in_a_probe_goes_to_the_probe_output() -> TestResult {
        let launches = Arc::new(AtomicUsize::new(0));
        let host = host_with_stderr(&launches, NOTE.as_bytes())?;
        let root = Path::new("/work");
        host.seed_file(
            &root.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
try_compile(FIRST PROJECT Child SOURCE_DIR /work/child BINARY_DIR /work/first OUTPUT_VARIABLE out)
file(WRITE /work/probe-output.txt "${out}")
"#,
        )?;
        let mut parent = interpreter(host.clone(), root)?;
        parent.add_session_data(Arc::new(PlatformQueries::default()));
        let outcome = parent.run_configure(root, &root.join("build"), &[]);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert_eq!(outcome.exit_code, 0, "{stderr}");
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        let output = host.read(
            Path::new("/work/probe-output.txt"),
            rcmake_host::ReadRange::All { limit: 1 << 20 },
        )?;
        let output = String::from_utf8_lossy(&output);
        assert_eq!(output.matches(NOTE).count(), 1, "{output}");
        assert!(!stderr.contains(NOTE), "{stderr}");
        Ok(())
    }

    #[test]
    fn inherited_query_stderr_at_top_level_goes_to_standard_error() -> TestResult {
        let launches = Arc::new(AtomicUsize::new(0));
        let host = host_with_stderr(&launches, NOTE.as_bytes())?;
        let root = Path::new("/work");
        host.seed_file(
            &root.join("CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Parent NONE)\ninclude(Platform/Darwin-Initialize)\n",
        )?;
        let mut parent = interpreter(host.clone(), root)?;
        parent.add_session_data(Arc::new(PlatformQueries::default()));
        let outcome = parent.run_configure(root, &root.join("build"), &[]);
        let stderr = host.stream_bytes(Stream::Stderr).to_string();
        assert_eq!(outcome.exit_code, 0, "{stderr}");
        assert_eq!(stderr.matches(NOTE).count(), 1, "{stderr}");
        Ok(())
    }

    #[test]
    fn second_unjournaled_probe_launches_no_platform_queries() -> TestResult {
        let launches = Arc::new(AtomicUsize::new(0));
        let host = host(&launches)?;
        let root = Path::new("/work");
        host.seed_file(
            &root.join("CMakeLists.txt"),
            br#"
cmake_minimum_required(VERSION 4.5)
project(Parent NONE)
try_compile(FIRST PROJECT Child SOURCE_DIR /work/child BINARY_DIR /work/first)
try_compile(SECOND PROJECT Child SOURCE_DIR /work/child BINARY_DIR /work/second)
if(NOT FIRST OR NOT SECOND)
    message(FATAL_ERROR "Both probes must configure successfully.")
endif()
"#,
        )?;
        let mut parent = interpreter(host.clone(), root)?;
        parent.add_session_data(Arc::new(PlatformQueries::default()));
        let outcome = parent.run_configure(root, &root.join("build"), &[]);
        assert_eq!(
            outcome.exit_code,
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        Ok(())
    }

    struct PlatformVerifier(AtomicUsize);

    impl ProcessVerifier for PlatformVerifier {
        fn recorded_platform_query(&self, request: &RunRequest) -> Option<RunOutput> {
            let output = query_output(request)?;
            self.0.fetch_add(1, Ordering::Relaxed);
            Some(output)
        }

        fn preflight(&self, _: &RunRequest) -> Option<Trace> {
            None
        }

        fn dependencies(
            &self,
            _: &RunRequest,
            _: &RunOutput,
            _: &[InputStamp],
        ) -> Option<DependencyClosure> {
            None
        }
    }

    #[test]
    fn journal_checks_match_with_and_without_the_parent_memo() -> TestResult {
        let launches = Arc::new(AtomicUsize::new(0));
        let host = host(&launches)?;
        let root = Path::new("/work");
        let source = root.join("child");
        let scratch = root.join("scratch");
        host.create_dir_all(&scratch)?;

        let mut with_memo = interpreter(host.clone(), root)?;
        with_memo.add_session_data(Arc::new(PlatformQueries::default()));
        assert_eq!(
            with_memo
                .run_configure(&source, &root.join("seed"), &[])
                .exit_code,
            0
        );
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        let mut without_memo = interpreter(host.clone(), root)?;
        let verifier = Arc::new(PlatformVerifier(AtomicUsize::new(0)));
        let mut checks = Vec::new();
        for parent in [&mut with_memo, &mut without_memo] {
            let journal = Arc::new(JournalHost::from_shared(
                host.clone(),
                &scratch,
                verifier.clone(),
                JournalMode::Observe,
            )?);
            let mut child = interpreter(journal.clone(), root)?;
            let cx = parent.cx();
            super::super::capture::CapturingHost::capture(journal.clone(), &scratch, |capture| {
                initialize_probe(&cx, &mut child, true, &capture);
            });
            let outcome = child.run_configure(&source, &scratch, &[]);
            assert_eq!(
                outcome.exit_code,
                0,
                "{}",
                host.stream_bytes(Stream::Stderr)
            );
            drop(child);
            let journal = Arc::try_unwrap(journal)
                .map_err(|_| HostError::unsupported("The child retained the journal."))?
                .finish();
            let observed = journal.checks();
            assert!(observed.encoded_size_bound() > 16);
            checks.push(observed.clone());
        }
        assert_eq!(launches.load(Ordering::Relaxed), 1);
        assert_eq!(verifier.0.load(Ordering::Relaxed), 2);
        assert_eq!(checks.first(), checks.get(1));
        Ok(())
    }
}

#[test]
fn forwards_cmake_module_path_before_project_in_sources_signature() -> TestResult {
    let host = Arc::new(PureHost::default());
    let root = PathBuf::from(format!("{DRIVE}/work"));
    host.seed_file(&root.join("probe-cc"), b"")?;
    host.seed_file(&root.join("main.c"), b"int main(void) { return 0; }\n")?;
    host.seed_file(
        &root.join("CMakeLists.txt"),
        &format!(
            "cmake_minimum_required(VERSION 4.5)\n\
             project(Parent NONE)\n\
             set(CMAKE_C_COMPILER {DRIVE}/work/probe-cc)\n\
             set(CMAKE_MODULE_PATH \"{DRIVE}/work/custom_modules;{DRIVE}/work/more_modules\")\n\
             set(CMAKE_DEBUG_TRY_COMPILE ON)\n\
             try_compile(OK SOURCES {DRIVE}/work/main.c NO_CACHE)\n"
        )
        .into_bytes(),
    )?;
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let mut interp = interpreter(host.clone(), &root)?;
    let outcome = interp.run_configure(&root, &root.join("build"), &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let scratch_root = root.join("build/CMakeFiles/CMakeScratch");
    let entries = host.read_dir(&scratch_root)?;
    let scratch_dir = scratch_root.join(rcmake_host::path::to_native(
        &entries.first().ok_or("missing scratch dir")?.name,
    ));
    let lists = host.read(
        &scratch_dir.join("CMakeLists.txt"),
        ReadRange::All { limit: 1 << 20 },
    )?;
    let lists_text = String::from_utf8_lossy(&lists);
    let expected_line = format!(
        "set(CMAKE_MODULE_PATH \"{DRIVE}/work/custom_modules;{DRIVE}/work/more_modules\")\n"
    );
    let mp_pos = lists_text
        .find(&expected_line)
        .ok_or_else(|| format!("missing CMAKE_MODULE_PATH in:\n{lists_text}"))?;
    let proj_pos = lists_text
        .find("project(CMAKE_TRY_COMPILE")
        .ok_or("missing project()")?;
    assert!(
        mp_pos < proj_pos,
        "CMAKE_MODULE_PATH must be emitted before project():\n{lists_text}"
    );
    // Also verify `--debug-trycompile` emits forward-slash CMake paths (Windows parity).
    let stderr = String::from_utf8_lossy(host.stream_bytes(Stream::Stderr).as_slice()).into_owned();
    let expected_cmake_dir = rcmake_host::path::from_native_cmake(&scratch_dir);
    assert!(
        stderr.contains(std::str::from_utf8(&expected_cmake_dir)?),
        "expected CMake-style path in debug-trycompile output:\n{stderr}"
    );
    // Also verify CMAKE_MODULE_PATH is recorded in the configure log YAML even when not listed
    // in CMAKE_TRY_COMPILE_PLATFORM_VARIABLES.
    let log_bytes = host.read(
        &root.join("build/CMakeFiles/CMakeConfigureLog.yaml"),
        ReadRange::All { limit: 1 << 20 },
    )?;
    let log_text = String::from_utf8_lossy(&log_bytes);
    let expected_yaml =
        format!("CMAKE_MODULE_PATH: \"{DRIVE}/work/custom_modules;{DRIVE}/work/more_modules\"");
    assert!(
        log_text.contains(&expected_yaml),
        "expected CMAKE_MODULE_PATH in CMakeConfigureLog.yaml:\n{log_text}"
    );
    Ok(())
}

#[test]
fn project_signature_evaluates_file_generate_before_building() -> TestResult {
    let host = Arc::new(PureHost::default());
    let root = PathBuf::from(format!("{DRIVE}/work"));
    host.seed_file(&root.join("probe-cc"), b"")?;
    host.seed_file(
        &root.join("sub/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\n\
          project(GenChild C)\n\
          file(GENERATE OUTPUT \"${CMAKE_CURRENT_BINARY_DIR}/gen_main.c\" CONTENT \"int main(void) { return $<BOOL:1>; }\\n\")\n\
          add_executable(genapp \"${CMAKE_CURRENT_BINARY_DIR}/gen_main.c\")\n",
    )?;
    host.seed_file(
        &root.join("CMakeLists.txt"),
        &format!(
            "cmake_minimum_required(VERSION 4.5)\n\
             project(Parent NONE)\n\
             set(CMAKE_C_COMPILER {DRIVE}/work/probe-cc)\n\
             try_compile(GEN_OK PROJECT GenChild SOURCE_DIR {DRIVE}/work/sub BINARY_DIR {DRIVE}/work/build/gen TARGET genapp NO_CACHE NO_LOG)\n"
        )
        .into_bytes(),
    )?;
    let saw_generated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = Arc::clone(&saw_generated);
    let expected_gen = root.join("build/gen/gen_main.c");
    host.set_command_handler(move |h, _, _| {
        if let Ok(bytes) = h.read(&expected_gen, ReadRange::All { limit: 1024 })
            && bytes == b"int main(void) { return 1; }\n"
        {
            seen.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let mut interp = interpreter(host.clone(), &root)?;
    let outcome = interp.run_configure(&root, &root.join("build"), &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert_eq!(interp.cx().var(b"GEN_OK"), Some("TRUE".into()));
    assert!(
        saw_generated.load(std::sync::atomic::Ordering::Relaxed),
        "file(GENERATE) output must be written before build_child executes"
    );
    Ok(())
}

#[test]
fn project_signature_file_generate_rejects_lexically_duplicate_outputs() -> TestResult {
    let host = Arc::new(PureHost::default());
    let root = PathBuf::from(format!("{DRIVE}/work"));
    host.seed_file(&root.join("probe-cc"), b"")?;
    host.seed_file(
        &root.join("sub/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 4.5)\n\
          project(DupChild C)\n\
          file(GENERATE OUTPUT \"${CMAKE_CURRENT_BINARY_DIR}/out.c\" CONTENT \"int main(void) { return 0; }\\n\")\n\
          file(GENERATE OUTPUT \"${CMAKE_CURRENT_BINARY_DIR}/sub/../out.c\" CONTENT \"int main(void) { return 0; }\\n\")\n\
          add_executable(dupapp \"${CMAKE_CURRENT_BINARY_DIR}/out.c\")\n",
    )?;
    host.seed_file(
        &root.join("CMakeLists.txt"),
        &format!(
            "cmake_minimum_required(VERSION 4.5)\n\
             project(Parent NONE)\n\
             set(CMAKE_C_COMPILER {DRIVE}/work/probe-cc)\n\
             try_compile(DUP_OK PROJECT DupChild SOURCE_DIR {DRIVE}/work/sub BINARY_DIR {DRIVE}/work/build/dup TARGET dupapp OUTPUT_VARIABLE DUP_OUT NO_CACHE NO_LOG)\n"
        )
        .into_bytes(),
    )?;
    host.set_command_handler(|_, _, _| {
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    let mut interp = interpreter(host.clone(), &root)?;
    let outcome = interp.run_configure(&root, &root.join("build"), &[]);
    assert_ne!(outcome.exit_code, 0);
    let output = interp.cx().var(b"DUP_OUT").unwrap_or_default();
    assert!(
        String::from_utf8_lossy(&output).contains("multiple different commands"),
        "expected duplicate output error, got:\n{output}"
    );
    Ok(())
}
