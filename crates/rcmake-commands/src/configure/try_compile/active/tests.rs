use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_eval::{ProbeCacheMode, Services};
use rcmake_host::{
    Host, HostError, HostResult, ProcessResult, PureHost, ReadRange, RunOutput, path,
};

use super::super::{capture::CapturingHost, tests::interpreter};
use crate::ConfigureRuntime;

const TOOLCHAIN: &str = "/Library/Developer/CommandLineTools/usr/bin";
const SDK: &str = "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk";
const PROJECT: &str = "\
cmake_minimum_required(VERSION 4.5)
project(Probes NONE)
set(CMAKE_SYSTEM_NAME Darwin)
set(CMAKE_C_COMPILER /usr/bin/cc)
set(CMAKE_C_COMPILER_ID AppleClang)
set(CMAKE_OSX_SYSROOT /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk)
set(CMAKE_C_COMPILE_OBJECT \"<CMAKE_C_COMPILER> -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk -o <OBJECT> -c <SOURCE>\")
set(CMAKE_C_LINK_EXECUTABLE \"<CMAKE_C_COMPILER> -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk -o <TARGET> <OBJECTS>\")
";

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    inner: Arc<PureHost>,
    host: Arc<dyn Host>,
    runs: Arc<AtomicUsize>,
    speculative: Arc<AtomicUsize>,
}

impl Fixture {
    fn new(commands: &str) -> HostResult<Self> {
        let root = Path::new("/work/probes");
        let executable = root.join("bin/rcmake");
        let inner = Arc::new(PureHost::new(root.into(), executable.clone()));
        let lists = format!("{PROJECT}{commands}");
        inner.seed_file(&root.join("CMakeLists.txt"), lists.as_bytes())?;
        for file in [
            executable,
            "/usr/bin/xcrun".into(),
            "/usr/bin/cc".into(),
            "/usr/bin/c++".into(),
            Path::new(TOOLCHAIN).join("clang"),
            Path::new(TOOLCHAIN).join("clang++"),
            Path::new(TOOLCHAIN).join("ld"),
            Path::new(SDK).join("SDKSettings.json"),
            Path::new(SDK).join("usr/lib/libSystem.tbd"),
        ] {
            inner.seed_file(&file, b"fixture")?;
        }
        inner.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/probe-cache"));
        inner.sleep(Duration::from_secs(4));
        let runs = Arc::new(AtomicUsize::new(0));
        let speculative = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&runs);
        let worker_count = Arc::clone(&speculative);
        inner.set_command_handler(move |host, request, _| {
            let command = request
                .pipeline
                .first()
                .ok_or_else(|| HostError::unsupported("Expected a process."))?;
            if command.program == "/usr/bin/xcrun" {
                let tool = command
                    .args
                    .last()
                    .ok_or_else(|| HostError::unsupported("Expected a tool name."))?;
                return Ok(success(format!("{TOOLCHAIN}/{tool}\n").into_bytes()));
            }
            if command.args.iter().any(|arg| arg == "-E")
                && command.args.iter().any(|arg| arg == "-v")
            {
                let mut output = success(Vec::new());
                output.stderr = format!(
                    "#include <...> search starts here:\n {SDK}/usr/include\nEnd of search list.\n"
                )
                .into_bytes();
                return Ok(output);
            }
            if command.program == format!("{TOOLCHAIN}/clang").as_str()
                || command.program == format!("{TOOLCHAIN}/clang++").as_str()
            {
                let data = match command.args.as_slice() {
                    [flag] if flag == "-print-resource-dir" => {
                        b"/fixture/clang/resource\n".to_vec()
                    }
                    [flag] if flag == "-print-target-triple" => b"arm64-apple-darwin\n".to_vec(),
                    [flag] if flag == "-v" => Vec::new(),
                    _ => return Err(HostError::unsupported("Unexpected compiler query.")),
                };
                return Ok(success(data));
            }
            if command.program != "/usr/bin/cc" && command.program != "/usr/bin/c++" {
                return Err(HostError::unsupported(format!(
                    "Unexpected process: {}",
                    command.program
                )));
            }
            count.fetch_add(1, Ordering::Relaxed);
            if std::thread::current().name() == Some("rcmake-probe") {
                worker_count.fetch_add(1, Ordering::Relaxed);
                if host.env_get(b"RCMAKE_MEASURE_PROBE_INTERVALS").is_some()
                    && command.args.iter().any(|arg| arg == "-c")
                {
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
            let argument = |flag: &[u8]| {
                command.args.windows(2).find_map(|pair| match pair {
                    [key, value] if key.as_slice() == flag => {
                        Some(request.cwd.join(path::to_native(value)))
                    }
                    _ => None,
                })
            };
            let out = argument(b"-o")
                .ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
            host.seed_file(&out, b"test binary")?;
            if let Some(deps) = argument(b"-MF") {
                let source = command
                    .args
                    .iter()
                    .find(|arg| arg.ends_with(b".c") || arg.ends_with(b".cpp"))
                    .ok_or_else(|| HostError::unsupported("Expected a source file."))?;
                host.seed_file(&deps, format!("out.o: {source}\n").as_bytes())?;
            }
            if command.args.iter().any(|arg| arg == "-dependency_info") {
                let deps = command
                    .args
                    .last()
                    .ok_or_else(|| HostError::unsupported("Expected linker evidence."))?;
                let bytes = format!("\0fixture\0\u{10}{SDK}/usr/lib/libSystem.tbd\0");
                host.seed_file(&request.cwd.join(path::to_native(deps)), bytes.as_bytes())?;
            }
            Ok(success(b"compiler output\n".to_vec()))
        })?;

        let system = rcmake_host::SystemInfo {
            sysname: "Darwin".into(),
            cpu_count: 4,
            total_memory: 16 << 30,
            ..inner.system()
        };
        let host = CapturingHost::for_test(inner.clone(), root, system);
        Ok(Self {
            inner,
            host,
            runs,
            speculative,
        })
    }

    fn configure(
        &self,
        mode: ProbeCacheMode,
        jobs: usize,
        names: &[&[u8]],
    ) -> TestResult<Vec<BString>> {
        self.configure_in("/work/build", mode, jobs, names)
    }

    fn configure_in(
        &self,
        build: &str,
        mode: ProbeCacheMode,
        jobs: usize,
        names: &[&[u8]],
    ) -> TestResult<Vec<BString>> {
        let root = Path::new("/work/probes");
        let build = Path::new(build);
        let mut services = Services::default();
        services.speed.probe_cache = mode;
        services.speed.probe_jobs = NonZeroUsize::new(jobs);
        let mut runtime = ConfigureRuntime::new(self.host.clone(), root, build, &services, true);
        let mut interp = interpreter(runtime.host(), root)?;
        self.inner.sleep(Duration::from_secs(4));
        let outcome = runtime.run(&mut interp, &[]);
        runtime.finish(outcome.exit_code == 0);
        assert_eq!(outcome.exit_code, 0);
        let values = names
            .iter()
            .map(|name| interp.cx().var_or_empty(name))
            .collect();
        Ok(values)
    }
}

fn success(stdout: Vec<u8>) -> RunOutput {
    RunOutput {
        stdout,
        results: vec![ProcessResult::Exited(0)],
        ..Default::default()
    }
}

#[test]
fn warm_probe_hits_in_a_different_build_directory() -> TestResult {
    let fixture = Fixture::new(
        "try_compile(OK SOURCE_FROM_CONTENT main.c [[int main(void) { return 0; }]] NO_CACHE)\n",
    )?;
    for build in ["/work/cold", "/work/warm"] {
        assert_eq!(
            fixture.configure_in(build, ProbeCacheMode::ReadWrite, 1, &[b"OK"])?,
            vec![BString::from("TRUE")]
        );
        assert_eq!(fixture.runs.load(Ordering::Relaxed), 2);
    }
    Ok(())
}

#[test]
fn external_source_content_invalidates_a_relocated_probe() -> TestResult {
    let source = Path::new("/work/probes/external.c");
    let fixture = Fixture::new("try_compile(OK SOURCES /work/probes/external.c NO_CACHE)\n")?;
    fixture
        .inner
        .seed_file(source, b"int main(void) { return 0; }\n")?;
    fixture.inner.sleep(Duration::from_secs(4));
    for build in ["/work/cold", "/work/warm"] {
        let values = fixture.configure_in(build, ProbeCacheMode::ReadWrite, 1, &[b"OK"])?;
        assert_eq!(values, [BString::from("TRUE")]);
        assert_eq!(fixture.runs.load(Ordering::Relaxed), 2);
    }
    fixture
        .inner
        .seed_file(source, b"int main(void) { return 1; }\n")?;
    fixture.inner.sleep(Duration::from_secs(4));
    let values = fixture.configure_in("/work/edited", ProbeCacheMode::ReadWrite, 1, &[b"OK"])?;
    assert_eq!(values, [BString::from("TRUE")]);
    assert_eq!(fixture.runs.load(Ordering::Relaxed), 4);
    Ok(())
}

#[test]
fn warm_probe_replays_output_and_copy_without_a_compiler() -> TestResult {
    let fixture = Fixture::new(
        "try_compile(OK SOURCE_FROM_CONTENT main.c [[int main(void) { return 0; }]] COPY_FILE copied OUTPUT_VARIABLE OUT NO_CACHE)\n",
    )?;
    let check = || fixture.configure(ProbeCacheMode::ReadWrite, 1, &[b"OK", b"OUT"]);
    let first = check()?;
    assert_eq!(first.first().ok_or("Missing probe result.")?, "TRUE");
    assert!(
        first
            .last()
            .ok_or("Missing output variable.")?
            .ends_with(b"compiler output\n\n")
    );
    assert_eq!(fixture.runs.load(Ordering::Relaxed), 2);
    let copy = Path::new("/work/probes/copied");
    fixture.inner.remove_file(copy)?;
    let second = check()?;
    assert_eq!(second.first(), first.first());
    assert!(
        second
            .last()
            .ok_or("Missing output variable.")?
            .ends_with(b"compiler output\n\n")
    );
    assert_eq!(fixture.runs.load(Ordering::Relaxed), 2);
    assert_eq!(
        fixture.inner.read(copy, ReadRange::All { limit: 64 })?,
        b"test binary"
    );
    let readonly = fixture.configure(ProbeCacheMode::ReadOnly, 1, &[b"OK", b"OUT"])?;
    assert_eq!(readonly.first(), second.first());
    assert_eq!(fixture.runs.load(Ordering::Relaxed), 2);
    let verified = fixture.configure(ProbeCacheMode::Verify, 1, &[b"OK", b"OUT"])?;
    assert_eq!(verified.first(), second.first());
    assert_eq!(fixture.runs.load(Ordering::Relaxed), 4);
    Ok(())
}

#[test]
fn speculative_jobs_match_serial_probe_results() -> TestResult {
    let script = "\
set(CMAKE_CXX_COMPILER /usr/bin/c++)
set(CMAKE_CXX_COMPILER_ID AppleClang)
set(CMAKE_CXX_COMPILE_OBJECT \"<CMAKE_CXX_COMPILER> -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk -o <OBJECT> -c <SOURCE>\")
set(CMAKE_CXX_LINK_EXECUTABLE \"<CMAKE_CXX_COMPILER> -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk -o <TARGET> <OBJECTS>\")
try_compile(FIRST SOURCE_FROM_CONTENT main.c [[int main(void) { return 1; }]] NO_CACHE NO_LOG)
try_compile(SECOND SOURCE_FROM_CONTENT second.cpp [[int main() { return 2; }]] NO_CACHE NO_LOG)
try_compile(THIRD SOURCE_FROM_CONTENT third.c [[int main(void) { return 3; }]] NO_CACHE NO_LOG)
try_compile(FOURTH SOURCE_FROM_CONTENT fourth.c [[int main(void) { return 4; }]] NO_CACHE NO_LOG)
try_compile(FIFTH SOURCE_FROM_CONTENT fifth.cpp [[int main() { return 5; }]] NO_CACHE NO_LOG)
";
    let serial = Fixture::new(script)?;
    let names = [
        b"FIRST".as_slice(),
        b"SECOND",
        b"THIRD",
        b"FOURTH",
        b"FIFTH",
    ];
    let expected = serial.configure(ProbeCacheMode::ReadWrite, 1, &names)?;
    assert_eq!(serial.speculative.load(Ordering::Relaxed), 0);
    for mode in [ProbeCacheMode::ReadWrite, ProbeCacheMode::Off] {
        let parallel = Fixture::new(script)?;
        parallel
            .inner
            .env_set(b"RCMAKE_MEASURE_PROBE_INTERVALS", Some(b"1"));
        assert_eq!(parallel.configure(mode, 4, &names)?, expected);
        assert!(parallel.speculative.load(Ordering::Relaxed) >= 2);
        assert_eq!(
            parallel.runs.load(Ordering::Relaxed),
            serial.runs.load(Ordering::Relaxed)
        );
        if mode == ProbeCacheMode::Off {
            let records = parallel.inner.read(
                Path::new("/work/build/CMakeFiles/rcmake-probe-intervals.txt"),
                ReadRange::All { limit: 8192 },
            )?;
            let intervals = std::str::from_utf8(&records)?
                .lines()
                .filter_map(|line| {
                    let fields: Vec<_> = line.split_ascii_whitespace().collect();
                    let [scratch, start, end, "0"] = fields.as_slice() else {
                        return None;
                    };
                    Some((
                        *scratch,
                        start.parse::<u64>().ok()?,
                        end.parse::<u64>().ok()?,
                    ))
                })
                .collect::<Vec<_>>();
            assert!(
                intervals.iter().any(|(name, start, end)| {
                    intervals.iter().any(|(other, other_start, other_end)| {
                        name != other && start < other_end && other_start < end
                    })
                }),
                "{}",
                String::from_utf8_lossy(&records)
            );
            assert!(
                parallel
                    .inner
                    .metadata(Path::new("/work/probe-cache"), rcmake_host::Follow::No)
                    .is_err()
            );
        }
    }
    Ok(())
}

#[test]
fn optional_sdk_header_insertion_invalidates_a_warm_probe() -> TestResult {
    check_sdk_insertion(
        r#"
try_compile(OK SOURCE_FROM_CONTENT main.c [[
#if __has_include(<nested/optional.h>)
#include <nested/optional.h>
#endif
int main(void) { return 0; }
]] NO_CACHE)
"#,
    )
}

#[test]
fn pragma_operators_use_header_search_verification() -> TestResult {
    check_sdk_insertion(
        r#"
try_compile(OK SOURCE_FROM_CONTENT main.c [[
_Pragma("pack(1)")
int main(void) { return 0; }
]] NO_CACHE)
"#,
    )
}

fn check_sdk_insertion(project: &str) -> TestResult {
    let fixture = Fixture::new(project)?;
    let include = Path::new(SDK).join("usr/include/nested");
    fixture.inner.create_dir_all(&include)?;
    for (index, expected_runs) in [2, 2, 4, 4].into_iter().enumerate() {
        if index == 2 {
            fixture
                .inner
                .seed_file(&include.join("optional.h"), b"new")?;
        }
        assert_eq!(
            fixture.configure_in(
                &format!("/work/header-{index}"),
                ProbeCacheMode::ReadWrite,
                1,
                &[b"OK"]
            )?,
            [BString::from("TRUE")]
        );
        assert_eq!(fixture.runs.load(Ordering::Relaxed), expected_runs);
    }
    Ok(())
}

#[test]
fn uncertified_ninja_wrapper_bypasses_warm_probe_cache_while_native_ninja_hits() -> TestResult {
    use rcmake_model::GeneratorKind;

    let root = Path::new("/work/probes");
    let executable = root.join("bin/rcmake");
    let inner = Arc::new(PureHost::new(root.into(), executable.clone()));
    let lists = format!(
        "set(CMAKE_MAKE_PROGRAM /bin/ninja CACHE FILEPATH \"\")\n\
         {PROJECT}\
         try_compile(OK SOURCE_FROM_CONTENT main.c [[int main(void) {{ return 0; }}]] NO_CACHE)\n"
    );
    inner.seed_file(&root.join("CMakeLists.txt"), lists.as_bytes())?;
    for file in [
        executable,
        "/usr/bin/xcrun".into(),
        "/usr/bin/cc".into(),
        "/usr/bin/c++".into(),
        Path::new(TOOLCHAIN).join("clang"),
        Path::new(TOOLCHAIN).join("clang++"),
        Path::new(TOOLCHAIN).join("ld"),
        Path::new(SDK).join("SDKSettings.json"),
        Path::new(SDK).join("usr/lib/libSystem.tbd"),
    ] {
        inner.seed_file(&file, b"fixture")?;
    }
    let ninja_path = Path::new("/bin/ninja");
    // Seed a certified native ELF Ninja binary.
    inner.seed_file(ninja_path, b"\x7fELF-ninja-v1")?;
    inner.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/probe-cache"));
    inner.sleep(Duration::from_secs(4));

    let compiler_runs = Arc::new(AtomicUsize::new(0));
    let reject_child_ninja = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rejected_child_runs = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&compiler_runs);
    let reject_child = Arc::clone(&reject_child_ninja);
    let rejected_runs = Arc::clone(&rejected_child_runs);
    inner.set_command_handler(move |host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Expected a process."))?;
        if command.program == "/bin/ninja" {
            assert_eq!(command.args.as_slice(), [BString::from("--version")]);
            if reject_child.load(Ordering::Relaxed)
                && request.cwd.to_string_lossy().contains("CMakeScratch")
            {
                rejected_runs.fetch_add(1, Ordering::Relaxed);
                return Ok(RunOutput {
                    stderr: b"wrapper rejected child cwd\n".to_vec(),
                    results: vec![ProcessResult::Exited(1)],
                    ..Default::default()
                });
            }
            return Ok(success(b"1.12.1\n".to_vec()));
        }
        if command.program == "/usr/bin/xcrun" {
            let tool = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected a tool name."))?;
            return Ok(success(format!("{TOOLCHAIN}/{tool}\n").into_bytes()));
        }
        if command.args.iter().any(|arg| arg == "-E") && command.args.iter().any(|arg| arg == "-v")
        {
            let mut output = success(Vec::new());
            output.stderr = format!(
                "#include <...> search starts here:\n {SDK}/usr/include\nEnd of search list.\n"
            )
            .into_bytes();
            return Ok(output);
        }
        if command.program == format!("{TOOLCHAIN}/clang").as_str()
            || command.program == format!("{TOOLCHAIN}/clang++").as_str()
        {
            let data = match command.args.as_slice() {
                [flag] if flag == "-print-resource-dir" => b"/fixture/clang/resource\n".to_vec(),
                [flag] if flag == "-print-target-triple" => b"arm64-apple-darwin\n".to_vec(),
                [flag] if flag == "-v" => Vec::new(),
                _ => return Err(HostError::unsupported("Unexpected compiler query.")),
            };
            return Ok(success(data));
        }
        if command.program != "/usr/bin/cc" {
            return Err(HostError::unsupported(format!(
                "Unexpected process: {}",
                command.program
            )));
        }
        count.fetch_add(1, Ordering::Relaxed);
        let argument = |flag: &[u8]| {
            command.args.windows(2).find_map(|pair| match pair {
                [key, value] if key.as_slice() == flag => {
                    Some(request.cwd.join(path::to_native(value)))
                }
                _ => None,
            })
        };
        let out =
            argument(b"-o").ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
        host.seed_file(&out, b"test binary")?;
        if let Some(deps) = argument(b"-MF") {
            let source = command
                .args
                .iter()
                .find(|arg| arg.ends_with(b".c"))
                .ok_or_else(|| HostError::unsupported("Expected a source file."))?;
            host.seed_file(&deps, format!("out.o: {source}\n").as_bytes())?;
        }
        if command.args.iter().any(|arg| arg == "-dependency_info") {
            let deps = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected linker evidence."))?;
            let bytes = format!("\0fixture\0\u{10}{SDK}/usr/lib/libSystem.tbd\0");
            host.seed_file(&request.cwd.join(path::to_native(deps)), bytes.as_bytes())?;
        }
        Ok(success(b"compiler output\n".to_vec()))
    })?;

    let system = rcmake_host::SystemInfo {
        sysname: "Darwin".into(),
        cpu_count: 4,
        total_memory: 16 << 30,
        ..inner.system()
    };
    let host = CapturingHost::for_test(inner.clone(), root, system);
    let run_ninja_configure = |build: &str| -> TestResult<(i32, BString)> {
        let mut services = Services::default();
        services.speed.probe_cache = ProbeCacheMode::ReadWrite;
        services.speed.probe_jobs = NonZeroUsize::new(1);
        let mut runtime =
            ConfigureRuntime::new(host.clone(), root, Path::new(build), &services, true);
        let mut interp = interpreter(runtime.host(), root)?;
        interp.cx().project().generator = Some(GeneratorKind::Ninja);
        inner.sleep(Duration::from_secs(4));
        let outcome = runtime.run(&mut interp, &[]);
        runtime.finish(outcome.exit_code == 0);
        Ok((outcome.exit_code, interp.cx().var_or_empty(b"OK")))
    };

    // 1. Cold run with native ELF Ninja v1 publishes a warm probe cache entry.
    assert_eq!(
        run_ninja_configure("/work/cold")?,
        (0, BString::from("TRUE"))
    );
    let after_cold = compiler_runs.load(Ordering::Relaxed);
    assert_eq!(after_cold, 2);

    // 2. Same native ELF Ninja v1 binary hits the warm cache (zero compiler calls) after
    //    verifying `--version` live in the child scratch directory.
    assert_eq!(
        run_ninja_configure("/work/warm-same-native")?,
        (0, BString::from("TRUE"))
    );
    assert_eq!(compiler_runs.load(Ordering::Relaxed), after_cold);

    // 3. Replacing Ninja with a DIFFERENT native ELF binary (such as a compiled wrapper) MUST
    //    invalidate warm probe reuse because the Ninja digest is bound in probe identity.
    inner.seed_file(ninja_path, b"\x7fELF-compiled-wrapper-v2")?;
    assert_eq!(
        run_ninja_configure("/work/warm-compiled-wrapper-ok")?,
        (0, BString::from("TRUE"))
    );
    let after_compiled_ok = compiler_runs.load(Ordering::Relaxed);
    assert_eq!(after_compiled_ok, after_cold + 2);

    // 4. A compiled ELF wrapper that accepts caller cwd but rejects child CMakeScratch cwd
    //    MUST fail live in the child directory and must NOT return a stale cached TRUE.
    reject_child_ninja.store(true, Ordering::Relaxed);
    assert_eq!(
        run_ninja_configure("/work/warm-compiled-wrapper-reject")?,
        (1, BString::from("FALSE"))
    );
    assert!(rejected_child_runs.load(Ordering::Relaxed) >= 1);
    assert_eq!(compiler_runs.load(Ordering::Relaxed), after_compiled_ok);

    // 5. Replacing Ninja with an uncertified shell script wrapper MUST bypass warm cache lookup
    //    and execute the child live in its own working directory.
    reject_child_ninja.store(false, Ordering::Relaxed);
    inner.seed_file(ninja_path, b"#!/bin/sh\necho 1.12.1\n")?;
    assert_eq!(
        run_ninja_configure("/work/warm-wrapper-ok")?,
        (0, BString::from("TRUE"))
    );
    let after_wrapper = compiler_runs.load(Ordering::Relaxed);
    assert_eq!(after_wrapper, after_compiled_ok + 2);

    // 6. A shell wrapper that accepts caller cwd but rejects child cwd must NOT return stale TRUE.
    let rejected_before_shell = rejected_child_runs.load(Ordering::Relaxed);
    reject_child_ninja.store(true, Ordering::Relaxed);
    assert_eq!(
        run_ninja_configure("/work/warm-wrapper-reject")?,
        (1, BString::from("FALSE"))
    );
    assert_eq!(
        rejected_child_runs.load(Ordering::Relaxed),
        rejected_before_shell + 1
    );
    assert_eq!(compiler_runs.load(Ordering::Relaxed), after_wrapper);
    Ok(())
}

#[test]
fn speculative_ninja_workers_never_spawn_ninja_version_checks() -> TestResult {
    use rcmake_model::GeneratorKind;

    let root = Path::new("/work/probes");
    let executable = root.join("bin/rcmake");
    let inner = Arc::new(PureHost::new(root.into(), executable.clone()));
    // Two back-to-back probes so Probe 1 queues Probe 2 into the speculative worker pool,
    // plus a third probe after a failing guard that is never reached by the evaluator.
    let lists = format!(
        "set(CMAKE_MAKE_PROGRAM /bin/ninja CACHE FILEPATH \"\")\n\
         {PROJECT}\
         try_compile(OK1 SOURCE_FROM_CONTENT a.c [[int main(void) {{ return 0; }}]] NO_CACHE)\n\
         try_compile(OK2 SOURCE_FROM_CONTENT b.c [[int main(void) {{ return 1; }}]] NO_CACHE)\n"
    );
    inner.seed_file(&root.join("CMakeLists.txt"), lists.as_bytes())?;
    for file in [
        executable,
        "/usr/bin/xcrun".into(),
        "/usr/bin/cc".into(),
        "/usr/bin/c++".into(),
        Path::new(TOOLCHAIN).join("clang"),
        Path::new(TOOLCHAIN).join("clang++"),
        Path::new(TOOLCHAIN).join("ld"),
        Path::new(SDK).join("SDKSettings.json"),
        Path::new(SDK).join("usr/lib/libSystem.tbd"),
    ] {
        inner.seed_file(&file, b"fixture")?;
    }
    inner.seed_file(Path::new("/bin/ninja"), b"\x7fELF-ninja-spec-test")?;
    inner.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/probe-cache-spec"));
    inner.sleep(Duration::from_secs(4));

    let ninja_cwds = Arc::new(std::sync::Mutex::new(Vec::<std::path::PathBuf>::new()));
    let ninja_cwds_clone = Arc::clone(&ninja_cwds);
    let speculative_cc_runs = Arc::new(AtomicUsize::new(0));
    let speculative_cc_clone = Arc::clone(&speculative_cc_runs);
    inner.set_command_handler(move |host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Expected a process."))?;
        if command.program == "/bin/ninja" {
            if let Ok(mut guard) = ninja_cwds_clone.lock() {
                guard.push(request.cwd.clone());
            }
            return Ok(success(b"1.12.1\n".to_vec()));
        }
        if command.program == "/usr/bin/xcrun" {
            let tool = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected a tool name."))?;
            return Ok(success(format!("{TOOLCHAIN}/{tool}\n").into_bytes()));
        }
        if command.args.iter().any(|arg| arg == "-E") && command.args.iter().any(|arg| arg == "-v")
        {
            let mut output = success(Vec::new());
            output.stderr = format!(
                "#include <...> search starts here:\n {SDK}/usr/include\nEnd of search list.\n"
            )
            .into_bytes();
            return Ok(output);
        }
        if command.program == format!("{TOOLCHAIN}/clang").as_str()
            || command.program == format!("{TOOLCHAIN}/clang++").as_str()
        {
            let data = match command.args.as_slice() {
                [flag] if flag == "-print-resource-dir" => b"/fixture/clang/resource\n".to_vec(),
                [flag] if flag == "-print-target-triple" => b"arm64-apple-darwin\n".to_vec(),
                [flag] if flag == "-v" => Vec::new(),
                _ => return Err(HostError::unsupported("Unexpected compiler query.")),
            };
            return Ok(success(data));
        }
        if command.program != "/usr/bin/cc" {
            return Err(HostError::unsupported(format!(
                "Unexpected process: {}",
                command.program
            )));
        }
        if request.cwd.to_string_lossy().contains("Spec-") {
            speculative_cc_clone.fetch_add(1, Ordering::Relaxed);
        } else {
            // Pause briefly on foreground Probe 1 so the worker executes Probe 2 in Spec-*.
            std::thread::sleep(Duration::from_millis(25));
        }
        let argument = |flag: &[u8]| {
            command.args.windows(2).find_map(|pair| match pair {
                [key, value] if key.as_slice() == flag => {
                    Some(request.cwd.join(path::to_native(value)))
                }
                _ => None,
            })
        };
        let out =
            argument(b"-o").ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
        host.seed_file(&out, b"test binary")?;
        if let Some(deps) = argument(b"-MF") {
            let source = command
                .args
                .iter()
                .find(|arg| arg.ends_with(b".c"))
                .ok_or_else(|| HostError::unsupported("Expected a source file."))?;
            host.seed_file(&deps, format!("out.o: {source}\n").as_bytes())?;
        }
        if command.args.iter().any(|arg| arg == "-dependency_info") {
            let deps = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected linker evidence."))?;
            let bytes = format!("\0fixture\0\u{10}{SDK}/usr/lib/libSystem.tbd\0");
            host.seed_file(&request.cwd.join(path::to_native(deps)), bytes.as_bytes())?;
        }
        Ok(success(b"compiler output\n".to_vec()))
    })?;

    let system = rcmake_host::SystemInfo {
        sysname: "Darwin".into(),
        cpu_count: 4,
        total_memory: 16 << 30,
        ..inner.system()
    };
    let host = CapturingHost::for_test(inner.clone(), root, system);
    let mut services = Services::default();
    services.speed.probe_cache = ProbeCacheMode::ReadWrite;
    services.speed.probe_jobs = NonZeroUsize::new(2);
    let mut runtime =
        ConfigureRuntime::new(host, root, Path::new("/work/spec-build"), &services, true);
    let mut interp = interpreter(runtime.host(), root)?;
    interp.cx().project().generator = Some(GeneratorKind::Ninja);
    inner.sleep(Duration::from_secs(4));
    let outcome = runtime.run(&mut interp, &[]);
    runtime.finish(outcome.exit_code == 0);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(interp.cx().var_or_empty(b"OK1"), "TRUE");
    assert_eq!(interp.cx().var_or_empty(b"OK2"), "TRUE");

    assert!(
        speculative_cc_runs.load(Ordering::Relaxed) >= 1,
        "expected speculative worker to compile Probe 2 in a Spec-* directory"
    );
    let cwds = ninja_cwds
        .lock()
        .map_err(|_| HostError::unsupported("lock poisoned"))?;
    assert!(
        !cwds
            .iter()
            .any(|dir| dir.to_string_lossy().contains("Spec-")),
        "speculative workers must never execute ninja --version in Spec-* scratch dirs: {cwds:?}"
    );
    Ok(())
}

#[test]
fn foreach_probe_loop_runs_speculatively_and_matches_serial() -> TestResult {
    let script = "\
foreach(item IN ITEMS alpha beta gamma delta)
  string(MAKE_C_IDENTIFIER \"${item}\" ident)
  string(TOUPPER \"${ident}\" upper)
  set(src \"int main(void) { return 0; } /* ${item} */\\n\")
  try_compile(HAVE_${upper} SOURCE_FROM_VAR \"probe_${ident}.c\" src NO_CACHE NO_LOG)
endforeach()
";
    let names = [
        b"HAVE_ALPHA".as_slice(),
        b"HAVE_BETA",
        b"HAVE_GAMMA",
        b"HAVE_DELTA",
    ];
    let serial = Fixture::new(script)?;
    let expected = serial.configure(ProbeCacheMode::ReadWrite, 1, &names)?;
    assert_eq!(expected, vec![BString::from("TRUE"); 4]);
    assert_eq!(serial.speculative.load(Ordering::Relaxed), 0);

    for mode in [ProbeCacheMode::ReadWrite, ProbeCacheMode::Off] {
        let parallel = Fixture::new(script)?;
        parallel
            .inner
            .env_set(b"RCMAKE_MEASURE_PROBE_INTERVALS", Some(b"1"));
        assert_eq!(parallel.configure(mode, 4, &names)?, expected);
        assert!(
            parallel.speculative.load(Ordering::Relaxed) >= 2,
            "expected foreach iterations to compile speculatively in mode {mode:?}"
        );
        assert_eq!(
            parallel.runs.load(Ordering::Relaxed),
            serial.runs.load(Ordering::Relaxed)
        );
    }
    Ok(())
}

#[test]
fn expanded_var_straight_line_probes_run_speculatively() -> TestResult {
    let script = "\
set(PREFIX HAVE)
set(BODY_A \"int main(void) { return 1; }\")
set(BODY_B \"int main(void) { return 2; }\")
try_compile(${PREFIX}_A SOURCE_FROM_VAR a.c BODY_A NO_CACHE NO_LOG)
string(TOUPPER \"b\" SUFFIX_B)
try_compile(${PREFIX}_${SUFFIX_B} SOURCE_FROM_CONTENT b.c \"${BODY_B}\" NO_CACHE NO_LOG)
try_compile(${PREFIX}_C SOURCE_FROM_CONTENT c.c \"int main(void) { return 3; }\" NO_CACHE NO_LOG)
";
    let names = [b"HAVE_A".as_slice(), b"HAVE_B", b"HAVE_C"];
    let serial = Fixture::new(script)?;
    let expected = serial.configure(ProbeCacheMode::ReadWrite, 1, &names)?;
    assert_eq!(expected, vec![BString::from("TRUE"); 3]);

    let parallel = Fixture::new(script)?;
    assert_eq!(
        parallel.configure(ProbeCacheMode::ReadWrite, 4, &names)?,
        expected
    );
    assert!(parallel.speculative.load(Ordering::Relaxed) >= 1);
    assert_eq!(
        parallel.runs.load(Ordering::Relaxed),
        serial.runs.load(Ordering::Relaxed)
    );
    Ok(())
}

#[test]
fn mutating_foreach_probe_loop_declines_speculation() -> TestResult {
    let script = "\
set(ACC \"int v0 = 0;\")
foreach(item IN ITEMS one two three)
  set(ACC \"${ACC}\\nint v_${item} = 1;\")
  try_compile(HAVE_${item} SOURCE_FROM_CONTENT \"${item}.c\" \"${ACC}\\nint main(void){return 0;}\" NO_CACHE NO_LOG)
endforeach()
";
    let names = [b"HAVE_one".as_slice(), b"HAVE_two", b"HAVE_three"];
    let serial = Fixture::new(script)?;
    let expected = serial.configure(ProbeCacheMode::ReadWrite, 1, &names)?;
    let parallel = Fixture::new(script)?;
    assert_eq!(
        parallel.configure(ProbeCacheMode::ReadWrite, 4, &names)?,
        expected
    );
    assert_eq!(parallel.speculative.load(Ordering::Relaxed), 0);
    assert_eq!(
        parallel.runs.load(Ordering::Relaxed),
        serial.runs.load(Ordering::Relaxed)
    );
    Ok(())
}

#[test]
fn oracle_mode_and_single_job_disable_speculation_completely() -> TestResult {
    let script = "\
foreach(item IN ITEMS alpha beta gamma)
  try_compile(HAVE_${item} SOURCE_FROM_CONTENT \"${item}.c\" \"int main(void) { return 0; } /* ${item} */\" NO_CACHE NO_LOG)
endforeach()
";
    let names = [b"HAVE_alpha".as_slice(), b"HAVE_beta", b"HAVE_gamma"];

    let single = Fixture::new(script)?;
    assert_eq!(
        single.configure(ProbeCacheMode::ReadWrite, 1, &names)?,
        vec![BString::from("TRUE"); 3]
    );
    assert_eq!(single.speculative.load(Ordering::Relaxed), 0);

    let oracle = Fixture::new(script)?;
    oracle.inner.env_set(b"RCMAKE_ORACLE_MODE", Some(b"1"));
    assert_eq!(
        oracle.configure(ProbeCacheMode::ReadWrite, 4, &names)?,
        vec![BString::from("TRUE"); 3]
    );
    assert_eq!(oracle.speculative.load(Ordering::Relaxed), 0);
    assert_eq!(
        oracle.runs.load(Ordering::Relaxed),
        single.runs.load(Ordering::Relaxed)
    );
    Ok(())
}

#[test]
fn speculative_preparation_preserves_unread_cli_variable_parity() -> TestResult {
    let script = "\
foreach(item IN ITEMS alpha beta gamma)
  string(MAKE_C_IDENTIFIER \"${item}\" ident)
  string(TOUPPER \"${ident}\" upper)
  set(src \"int main(void) { return 0; } /* ${item} */\\n\")
  try_compile(HAVE_${upper} SOURCE_FROM_VAR \"probe_${ident}.c\" src NO_CACHE NO_LOG)
endforeach()
";
    let tracked = [
        "CMAKE_CUDA_COMPILER",
        "CMAKE_CXX_COMPILER",
        "CMAKE_CXX_SOURCE_FILE_EXTENSIONS",
        "CMAKE_C_STANDARD",
        "UNUSED_CLI_OPTION",
    ];
    let run_with_jobs = |jobs: usize| -> TestResult<(Vec<BString>, usize)> {
        let fixture = Fixture::new(script)?;
        let root = Path::new("/work/probes");
        let build = Path::new("/work/build");
        let mut services = Services::default();
        services.speed.probe_cache = ProbeCacheMode::ReadWrite;
        services.speed.probe_jobs = NonZeroUsize::new(jobs);
        let mut runtime = ConfigureRuntime::new(fixture.host.clone(), root, build, &services, true);
        let mut interp = interpreter(runtime.host(), root)?;
        interp.track_variable_reads(tracked.iter().map(|&name| BString::from(name)));
        fixture.inner.sleep(Duration::from_secs(4));
        let outcome = runtime.run(&mut interp, &[]);
        runtime.finish(outcome.exit_code == 0);
        assert_eq!(outcome.exit_code, 0);
        let unread = interp.unread_variables().map(BString::from).collect();
        Ok((unread, fixture.speculative.load(Ordering::Relaxed)))
    };

    let (serial_unread, serial_spec) = run_with_jobs(1)?;
    let (parallel_unread, parallel_spec) = run_with_jobs(4)?;
    assert_eq!(serial_spec, 0);
    assert!(
        parallel_spec >= 1,
        "expected speculative compilation in parallel run"
    );
    assert_eq!(
        parallel_unread, serial_unread,
        "speculative preparation must not change unread CLI variables"
    );
    assert!(
        parallel_unread
            .iter()
            .any(|name| name == "UNUSED_CLI_OPTION")
    );
    assert!(
        parallel_unread
            .iter()
            .any(|name| name == "CMAKE_CXX_SOURCE_FILE_EXTENSIONS")
    );
    Ok(())
}

#[test]
fn check_include_file_foreach_runs_speculatively_and_matches_serial() -> TestResult {
    let script = "\
include(CheckIncludeFile)
foreach(hdr IN ITEMS a.h b.h c.h d.h)
  string(MAKE_C_IDENTIFIER \"${hdr}\" ident)
  string(TOUPPER \"${ident}\" upper)
  check_include_file(\"${hdr}\" HAVE_${upper})
endforeach()
";
    let seed_modules = |fixture: &Fixture| -> HostResult<()> {
        let modules = Path::new("/work/probes/cmake/Modules");
        fixture.inner.create_dir_all(&modules.join("Internal"))?;
        fixture.inner.seed_file(
            &modules.join("CheckIncludeFile.cmake"),
            include_bytes!("../../../../../../modules/cmake-4.5/Modules/CheckIncludeFile.cmake"),
        )?;
        fixture.inner.seed_file(
            &modules.join("CheckIncludeFile.c.in"),
            include_bytes!("../../../../../../modules/cmake-4.5/Modules/CheckIncludeFile.c.in"),
        )?;
        fixture.inner.seed_file(
            &modules.join("Internal/CheckCommon.cmake"),
            include_bytes!(
                "../../../../../../modules/cmake-4.5/Modules/Internal/CheckCommon.cmake"
            ),
        )?;
        Ok(())
    };
    let names = [
        b"HAVE_A_H".as_slice(),
        b"HAVE_B_H",
        b"HAVE_C_H",
        b"HAVE_D_H",
    ];
    let serial = Fixture::new(script)?;
    seed_modules(&serial)?;
    let expected = serial.configure(ProbeCacheMode::ReadWrite, 1, &names)?;
    assert_eq!(expected, vec![BString::from("1"); 4]);
    assert_eq!(serial.speculative.load(Ordering::Relaxed), 0);

    let parallel = Fixture::new(script)?;
    seed_modules(&parallel)?;
    assert_eq!(
        parallel.configure(ProbeCacheMode::ReadWrite, 4, &names)?,
        expected
    );
    assert!(
        parallel.speculative.load(Ordering::Relaxed) >= 2,
        "expected stock check_include_file foreach iterations to compile speculatively"
    );
    assert_eq!(
        parallel.runs.load(Ordering::Relaxed),
        serial.runs.load(Ordering::Relaxed)
    );
    Ok(())
}

#[test]
fn speculative_rejects_compiler_wrapper_with_external_telemetry_writes() -> TestResult {
    let script = "\
set(CMAKE_C_COMPILER /work/probes/cc-wrapper)
foreach(item IN ITEMS alpha beta gamma)
  try_compile(HAVE_${item} SOURCE_FROM_CONTENT \"${item}.c\" \"int main(void) { return 0; } /* ${item} */\" NO_CACHE NO_LOG)
endforeach()
";
    let root = Path::new("/work/probes");
    let executable = root.join("bin/rcmake");
    let inner = Arc::new(PureHost::new(root.into(), executable.clone()));
    let lists = format!("{PROJECT}{script}");
    inner.seed_file(&root.join("CMakeLists.txt"), lists.as_bytes())?;
    inner.seed_file(
        &root.join("cc-wrapper"),
        b"#!/bin/sh\nprintf 'hit\\n' >> \"$RCMAKE_PROBE_LOG\"\nexec /usr/bin/cc \"$@\"\n",
    )?;
    for file in [
        executable,
        PathBuf::from("/bin/sh"),
        "/usr/bin/xcrun".into(),
        "/usr/bin/cc".into(),
        Path::new(TOOLCHAIN).join("clang"),
        Path::new(TOOLCHAIN).join("ld"),
        Path::new(SDK).join("SDKSettings.json"),
        Path::new(SDK).join("usr/lib/libSystem.tbd"),
    ] {
        inner.seed_file(&file, b"fixture")?;
    }
    inner.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/probe-cache-wrap"));
    inner.env_set(b"RCMAKE_PROBE_LOG", Some(b"/work/probe.log"));
    inner.sleep(Duration::from_secs(4));

    let speculative = Arc::new(AtomicUsize::new(0));
    let worker_count = Arc::clone(&speculative);
    inner.set_command_handler(move |host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Expected a process."))?;
        if command.program == "/usr/bin/xcrun" {
            let tool = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected a tool name."))?;
            return Ok(success(format!("{TOOLCHAIN}/{tool}\n").into_bytes()));
        }
        if command.args.iter().any(|arg| arg == "-E") && command.args.iter().any(|arg| arg == "-v")
        {
            let mut output = success(Vec::new());
            output.stderr = format!(
                "#include <...> search starts here:\n {SDK}/usr/include\nEnd of search list.\n"
            )
            .into_bytes();
            return Ok(output);
        }
        if command.program == format!("{TOOLCHAIN}/clang").as_str() {
            let data = match command.args.as_slice() {
                [flag] if flag == "-print-resource-dir" => b"/fixture/clang/resource\n".to_vec(),
                [flag] if flag == "-print-target-triple" => b"arm64-apple-darwin\n".to_vec(),
                [flag] if flag == "-v" => Vec::new(),
                _ => return Err(HostError::unsupported("Unexpected compiler query.")),
            };
            return Ok(success(data));
        }
        if command.program != "/work/probes/cc-wrapper" {
            return Err(HostError::unsupported(format!(
                "Unexpected process: {}",
                command.program
            )));
        }
        if std::thread::current().name() == Some("rcmake-probe") {
            worker_count.fetch_add(1, Ordering::Relaxed);
        }
        let argument = |flag: &[u8]| {
            command.args.windows(2).find_map(|pair| match pair {
                [key, value] if key.as_slice() == flag => {
                    Some(request.cwd.join(path::to_native(value)))
                }
                _ => None,
            })
        };
        let out =
            argument(b"-o").ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
        host.seed_file(&out, b"test binary")?;
        if let Some(deps) = argument(b"-MF") {
            let source = command
                .args
                .iter()
                .find(|arg| arg.ends_with(b".c"))
                .ok_or_else(|| HostError::unsupported("Expected a source file."))?;
            host.seed_file(&deps, format!("out.o: {source}\n").as_bytes())?;
        }
        if command.args.iter().any(|arg| arg == "-dependency_info") {
            let deps = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected linker evidence."))?;
            let bytes = format!("\0fixture\0\u{10}{SDK}/usr/lib/libSystem.tbd\0");
            host.seed_file(&request.cwd.join(path::to_native(deps)), bytes.as_bytes())?;
        }
        Ok(success(b"compiler output\n".to_vec()))
    })?;

    let system = rcmake_host::SystemInfo {
        sysname: "Darwin".into(),
        cpu_count: 4,
        total_memory: 16 << 30,
        ..inner.system()
    };
    let host = CapturingHost::for_test(inner.clone(), root, system);
    let mut services = Services::default();
    services.speed.probe_cache = ProbeCacheMode::ReadWrite;
    services.speed.probe_jobs = NonZeroUsize::new(4);
    let mut runtime =
        ConfigureRuntime::new(host, root, Path::new("/work/build-wrap"), &services, true);
    let mut interp = interpreter(runtime.host(), root)?;
    inner.sleep(Duration::from_secs(4));
    let outcome = runtime.run(&mut interp, &[]);
    runtime.finish(outcome.exit_code == 0);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(interp.cx().var_or_empty(b"HAVE_alpha"), "TRUE");
    assert_eq!(interp.cx().var_or_empty(b"HAVE_beta"), "TRUE");
    assert_eq!(interp.cx().var_or_empty(b"HAVE_gamma"), "TRUE");
    assert_eq!(
        speculative.load(Ordering::Relaxed),
        0,
        "compiler wrapper with external telemetry log must never execute speculatively"
    );
    Ok(())
}

#[test]
fn speculative_defers_uncached_language_discovery_to_foreground() -> TestResult {
    // Probe 1 is C; Probe 2 is C++ (not yet discovered in `session.tools`). Speculation
    // must NOT trigger C++ compiler discovery while preparing Probe 1's lookahead.
    let script = "\
set(CMAKE_CXX_COMPILER /usr/bin/c++)
set(CMAKE_CXX_COMPILER_ID AppleClang)
set(CMAKE_CXX_COMPILE_OBJECT \"<CMAKE_CXX_COMPILER> -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk -o <OBJECT> -c <SOURCE>\")
set(CMAKE_CXX_LINK_EXECUTABLE \"<CMAKE_CXX_COMPILER> -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk -o <TARGET> <OBJECTS>\")
try_compile(FIRST SOURCE_FROM_CONTENT main.c [[int main(void) { return 1; }]] NO_CACHE NO_LOG)
try_compile(SECOND SOURCE_FROM_CONTENT second.cpp [[int main() { return 2; }]] NO_CACHE NO_LOG)
";
    let root = Path::new("/work/probes");
    let executable = root.join("bin/rcmake");
    let inner = Arc::new(PureHost::new(root.into(), executable.clone()));
    let lists = format!("{PROJECT}{script}");
    inner.seed_file(&root.join("CMakeLists.txt"), lists.as_bytes())?;
    for file in [
        executable,
        "/usr/bin/xcrun".into(),
        "/usr/bin/cc".into(),
        "/usr/bin/c++".into(),
        Path::new(TOOLCHAIN).join("clang"),
        Path::new(TOOLCHAIN).join("clang++"),
        Path::new(TOOLCHAIN).join("ld"),
        Path::new(SDK).join("SDKSettings.json"),
        Path::new(SDK).join("usr/lib/libSystem.tbd"),
    ] {
        inner.seed_file(&file, b"fixture")?;
    }
    inner.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/probe-cache-defer"));
    inner.sleep(Duration::from_secs(4));

    let c_compiled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cxx_discovered_before_c = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c_flag = Arc::clone(&c_compiled);
    let cxx_early = Arc::clone(&cxx_discovered_before_c);
    inner.set_command_handler(move |host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Expected a process."))?;
        if command.program == "/usr/bin/xcrun" {
            let tool = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected a tool name."))?;
            if tool == "clang++" && !c_flag.load(Ordering::Acquire) {
                cxx_early.store(true, Ordering::Release);
            }
            return Ok(success(format!("{TOOLCHAIN}/{tool}\n").into_bytes()));
        }
        if command.args.iter().any(|arg| arg == "-E") && command.args.iter().any(|arg| arg == "-v")
        {
            let mut output = success(Vec::new());
            output.stderr = format!(
                "#include <...> search starts here:\n {SDK}/usr/include\nEnd of search list.\n"
            )
            .into_bytes();
            return Ok(output);
        }
        if command.program == format!("{TOOLCHAIN}/clang").as_str()
            || command.program == format!("{TOOLCHAIN}/clang++").as_str()
        {
            if command.program == format!("{TOOLCHAIN}/clang++").as_str()
                && !c_flag.load(Ordering::Acquire)
            {
                cxx_early.store(true, Ordering::Release);
            }
            let data = match command.args.as_slice() {
                [flag] if flag == "-print-resource-dir" => b"/fixture/clang/resource\n".to_vec(),
                [flag] if flag == "-print-target-triple" => b"arm64-apple-darwin\n".to_vec(),
                [flag] if flag == "-v" => Vec::new(),
                _ => return Err(HostError::unsupported("Unexpected compiler query.")),
            };
            return Ok(success(data));
        }
        if command.program == "/usr/bin/cc" {
            c_flag.store(true, Ordering::Release);
        } else if command.program != "/usr/bin/c++" {
            return Err(HostError::unsupported(format!(
                "Unexpected process: {}",
                command.program
            )));
        }
        let argument = |flag: &[u8]| {
            command.args.windows(2).find_map(|pair| match pair {
                [key, value] if key.as_slice() == flag => {
                    Some(request.cwd.join(path::to_native(value)))
                }
                _ => None,
            })
        };
        let out =
            argument(b"-o").ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
        host.seed_file(&out, b"test binary")?;
        if let Some(deps) = argument(b"-MF") {
            let source = command
                .args
                .iter()
                .find(|arg| arg.ends_with(b".c") || arg.ends_with(b".cpp"))
                .ok_or_else(|| HostError::unsupported("Expected a source file."))?;
            host.seed_file(&deps, format!("out.o: {source}\n").as_bytes())?;
        }
        if command.args.iter().any(|arg| arg == "-dependency_info") {
            let deps = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected linker evidence."))?;
            let bytes = format!("\0fixture\0\u{10}{SDK}/usr/lib/libSystem.tbd\0");
            host.seed_file(&request.cwd.join(path::to_native(deps)), bytes.as_bytes())?;
        }
        Ok(success(b"compiler output\n".to_vec()))
    })?;

    let system = rcmake_host::SystemInfo {
        sysname: "Darwin".into(),
        cpu_count: 4,
        total_memory: 16 << 30,
        ..inner.system()
    };
    let host = CapturingHost::for_test(inner.clone(), root, system);
    let mut services = Services::default();
    services.speed.probe_cache = ProbeCacheMode::ReadWrite;
    services.speed.probe_jobs = NonZeroUsize::new(4);
    let mut runtime =
        ConfigureRuntime::new(host, root, Path::new("/work/build-defer"), &services, true);
    let mut interp = interpreter(runtime.host(), root)?;
    inner.sleep(Duration::from_secs(4));
    let outcome = runtime.run(&mut interp, &[]);
    runtime.finish(outcome.exit_code == 0);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(interp.cx().var_or_empty(b"FIRST"), "TRUE");
    assert_eq!(interp.cx().var_or_empty(b"SECOND"), "TRUE");
    assert!(
        !cxx_discovered_before_c.load(Ordering::Acquire),
        "uncached C++ toolchain discovery must wait until the foreground C++ probe"
    );
    Ok(())
}

#[test]
fn paused_speculative_worker_leaves_no_visible_files_in_build_or_source_tree() -> TestResult {
    use std::sync::{Mutex, mpsc};

    // Pause the speculative worker for `second.c` while foreground execution runs
    // `file(GLOB_RECURSE ...)` across both `${CMAKE_BINARY_DIR}` and `${CMAKE_SOURCE_DIR}`.
    let script = "\
try_compile(FIRST SOURCE_FROM_CONTENT first.c [[int main(void) { return 1; }]] NO_CACHE NO_LOG)
try_compile(SECOND SOURCE_FROM_CONTENT second.c [[int main(void) { return 2; }]] NO_CACHE NO_LOG)
file(GLOB_RECURSE BUILD_FILES \"${CMAKE_BINARY_DIR}/*\")
file(GLOB_RECURSE SOURCE_FILES \"${CMAKE_SOURCE_DIR}/*\")
";
    let root = Path::new("/work/probes");
    let build = Path::new("/work/build-isolated");
    let executable = root.join("bin/rcmake");
    let inner = Arc::new(PureHost::new(root.into(), executable.clone()));
    let lists = format!("{PROJECT}{script}");
    inner.seed_file(&root.join("CMakeLists.txt"), lists.as_bytes())?;
    for file in [
        executable,
        "/usr/bin/xcrun".into(),
        "/usr/bin/cc".into(),
        Path::new(TOOLCHAIN).join("clang"),
        Path::new(TOOLCHAIN).join("ld"),
        Path::new(SDK).join("SDKSettings.json"),
        Path::new(SDK).join("usr/lib/libSystem.tbd"),
    ] {
        inner.seed_file(&file, b"fixture")?;
    }
    inner.env_set(b"RCMAKE_PROBE_CACHE_DIR", Some(b"/work/probe-cache-iso"));
    inner.sleep(Duration::from_secs(4));

    let (worker_started_tx, worker_started_rx) = mpsc::channel::<std::path::PathBuf>();
    let (release_worker_tx, release_worker_rx) = mpsc::channel::<()>();
    let worker_started_rx = Arc::new(Mutex::new(worker_started_rx));
    let release_worker_rx = Arc::new(Mutex::new(release_worker_rx));
    let seen_in_build_while_paused = Arc::new(Mutex::new(Vec::<BString>::new()));
    let seen_build_clone = Arc::clone(&seen_in_build_while_paused);
    let worker_cwd = Arc::new(Mutex::new(Option::<std::path::PathBuf>::None));
    let worker_cwd_clone = Arc::clone(&worker_cwd);
    let started_tx = Mutex::new(worker_started_tx);
    let release_tx = Mutex::new(Some(release_worker_tx));
    let build_for_handler = build.to_path_buf();

    inner.set_command_handler(move |host, request, _| {
        let command = request
            .pipeline
            .first()
            .ok_or_else(|| HostError::unsupported("Expected a process."))?;
        if command.program == "/usr/bin/xcrun" {
            let tool = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected a tool name."))?;
            return Ok(success(format!("{TOOLCHAIN}/{tool}\n").into_bytes()));
        }
        if command.args.iter().any(|arg| arg == "-E") && command.args.iter().any(|arg| arg == "-v")
        {
            let mut output = success(Vec::new());
            output.stderr = format!(
                "#include <...> search starts here:\n {SDK}/usr/include\nEnd of search list.\n"
            )
            .into_bytes();
            return Ok(output);
        }
        if command.program == format!("{TOOLCHAIN}/clang").as_str() {
            let data = match command.args.as_slice() {
                [flag] if flag == "-print-resource-dir" => b"/fixture/clang/resource\n".to_vec(),
                [flag] if flag == "-print-target-triple" => b"arm64-apple-darwin\n".to_vec(),
                [flag] if flag == "-v" => Vec::new(),
                _ => return Err(HostError::unsupported("Unexpected compiler query.")),
            };
            return Ok(success(data));
        }
        if command.program != "/usr/bin/cc" {
            return Err(HostError::unsupported(format!(
                "Unexpected process: {}",
                command.program
            )));
        }
        let is_worker = std::thread::current().name() == Some("rcmake-probe");
        let is_compile = command.args.iter().any(|arg| arg == "-c");
        if is_worker && is_compile {
            if let Ok(mut guard) = worker_cwd_clone.lock() {
                *guard = Some(request.cwd.clone());
            }
            if let Ok(tx) = started_tx.lock() {
                let _ = tx.send(request.cwd.clone());
            }
            if let Ok(rx) = release_worker_rx.lock() {
                let _ = rx.recv_timeout(Duration::from_secs(2));
            }
        } else if !is_worker && is_compile {
            // Wait until the worker has created its Spec-* dir and paused inside it, then
            // inspect the visible build tree's CMakeScratch directory directly.
            if let Ok(rx) = worker_started_rx.lock()
                && rx.recv_timeout(Duration::from_secs(2)).is_ok()
            {
                if let Ok(entries) =
                    host.read_dir(&build_for_handler.join("CMakeFiles/CMakeScratch"))
                    && let Ok(mut seen) = seen_build_clone.lock()
                {
                    seen.extend(entries.into_iter().map(|entry| entry.name));
                }
                if let Ok(mut guard) = release_tx.lock()
                    && let Some(tx) = guard.take()
                {
                    let _ = tx.send(());
                }
            }
        }
        let argument = |flag: &[u8]| {
            command.args.windows(2).find_map(|pair| match pair {
                [key, value] if key.as_slice() == flag => {
                    Some(request.cwd.join(path::to_native(value)))
                }
                _ => None,
            })
        };
        let out =
            argument(b"-o").ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
        host.seed_file(&out, b"test binary")?;
        if let Some(deps) = argument(b"-MF") {
            let source = command
                .args
                .iter()
                .find(|arg| arg.ends_with(b".c"))
                .ok_or_else(|| HostError::unsupported("Expected a source file."))?;
            host.seed_file(&deps, format!("out.o: {source}\n").as_bytes())?;
        }
        if command.args.iter().any(|arg| arg == "-dependency_info") {
            let deps = command
                .args
                .last()
                .ok_or_else(|| HostError::unsupported("Expected linker evidence."))?;
            let bytes = format!("\0fixture\0\u{10}{SDK}/usr/lib/libSystem.tbd\0");
            host.seed_file(&request.cwd.join(path::to_native(deps)), bytes.as_bytes())?;
        }
        Ok(success(b"compiler output\n".to_vec()))
    })?;

    let system = rcmake_host::SystemInfo {
        sysname: "Darwin".into(),
        cpu_count: 4,
        total_memory: 16 << 30,
        ..inner.system()
    };
    let host = CapturingHost::for_test(inner.clone(), root, system);
    let mut services = Services::default();
    services.speed.probe_cache = ProbeCacheMode::ReadWrite;
    services.speed.probe_jobs = NonZeroUsize::new(2);
    let mut runtime = ConfigureRuntime::new(host, root, build, &services, true);
    let mut interp = interpreter(runtime.host(), root)?;
    inner.sleep(Duration::from_secs(4));
    let outcome = runtime.run(&mut interp, &[]);
    runtime.finish(outcome.exit_code == 0);
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(interp.cx().var_or_empty(b"FIRST"), "TRUE");
    assert_eq!(interp.cx().var_or_empty(b"SECOND"), "TRUE");

    let spec_dir = worker_cwd
        .lock()
        .map_err(|_| HostError::unsupported("lock poisoned"))?
        .clone()
        .ok_or("expected speculative worker to run")?;
    assert!(
        !spec_dir.starts_with(build) && !spec_dir.starts_with(root),
        "speculative scratch {spec_dir:?} must be outside build ({build:?}) and source ({root:?})"
    );
    let seen = seen_in_build_while_paused
        .lock()
        .map_err(|_| HostError::unsupported("lock poisoned"))?;
    assert!(
        !seen
            .iter()
            .any(|name| name.starts_with(b"Spec-") || name.ends_with(b".claim")),
        "visible build CMakeScratch must never contain Spec-* files while worker runs: {seen:?}"
    );
    let build_files = interp.cx().var_or_empty(b"BUILD_FILES");
    let source_files = interp.cx().var_or_empty(b"SOURCE_FILES");
    assert!(
        !build_files.windows(5).any(|w| w == b"Spec-")
            && !source_files.windows(5).any(|w| w == b"Spec-"),
        "file(GLOB_RECURSE) observed speculative scratch files: build={build_files} source={source_files}"
    );
    // Session scratch directory must also be removed once ProbePool exits.
    if let Some(session_parent) = spec_dir.parent() {
        assert!(
            inner
                .metadata(session_parent, rcmake_host::Follow::No)
                .is_err(),
            "session scratch directory {session_parent:?} should be cleaned up on drop"
        );
    }
    Ok(())
}
