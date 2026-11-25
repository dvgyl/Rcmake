use std::num::NonZeroUsize;
use std::path::Path;
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
            Path::new(TOOLCHAIN).join("clang"),
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
            if command.program != "/usr/bin/cc" {
                return Err(HostError::unsupported(format!(
                    "Unexpected process: {}",
                    command.program
                )));
            }
            count.fetch_add(1, Ordering::Relaxed);
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
            let out = argument(b"-o")
                .ok_or_else(|| HostError::unsupported("Expected a compiler output."))?;
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
        let root = Path::new("/work/probes");
        let build = Path::new("/work/build");
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
            .ends_with(b"compiler output\n")
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
            .ends_with(b"compiler output\n")
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
try_compile(FIRST SOURCE_FROM_CONTENT main.c [[int main(void) { return 1; }]] NO_CACHE NO_LOG)
try_compile(SECOND SOURCE_FROM_CONTENT main.c [[int main(void) { return 2; }]] NO_CACHE NO_LOG)
try_compile(THIRD SOURCE_FROM_CONTENT main.c [[int main(void) { return 3; }]] NO_CACHE NO_LOG)
";
    let serial = Fixture::new(script)?;
    let parallel = Fixture::new(script)?;
    let names = [b"FIRST".as_slice(), b"SECOND", b"THIRD"];
    let expected = serial.configure(ProbeCacheMode::ReadWrite, 1, &names)?;
    let actual = parallel.configure(ProbeCacheMode::ReadWrite, 4, &names)?;
    assert_eq!(actual, expected);
    assert_eq!(serial.speculative.load(Ordering::Relaxed), 0);
    assert!(parallel.speculative.load(Ordering::Relaxed) >= 2);
    assert_eq!(
        parallel.runs.load(Ordering::Relaxed),
        serial.runs.load(Ordering::Relaxed)
    );
    Ok(())
}
