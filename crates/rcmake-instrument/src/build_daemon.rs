//! Direct make-program (`ninja` / `make`) build instrumentation lifecycle.
//!
//! When a user invokes the native build tool directly (instead of `cmake --build`),
//! the build manifest runs `ctest --instrument --start-instrumentation <build-dir>`
//! as its earliest step. That command:
//!
//! 1. Exits immediately if running under `cmake --build` (`IN_CMAKE_BUILD_ENV`).
//! 2. Deduplicates mid-build regeneration reloads against a per-build monitor file
//!    protected by a file lock, keyed to the native build tool's parent PID.
//! 3. Runs the `preBuild` hook once synchronously.
//! 4. Spawns a detached collector (`-E __instrument --wait-and-collect ...`) with
//!    all inherited stdio handles redirected to null so the build tool never blocks
//!    waiting for pipes to close.
//! 5. Once the build tool exits, the detached collector writes one `build` snippet
//!    and runs the `postBuild` hook (cleaning up snippets if subscribed, or leaving
//!    build and compile-trace snippets intact otherwise).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{Host, ReadRange, WriteMode, path};

use crate::files::{absolute, api_dir, epoch_millis, newest_content};
use crate::hooks::run_hook;
use crate::query::read_queries;
use crate::snippet::{SnippetDetails, SystemSample, write_snippet_with_details};
use crate::{
    Hook, IN_CMAKE_BUILD_ENV, InstrumentError, InstrumentOption, Snippet, SnippetRole, invalid,
};

const MONITOR_FILE: &str = ".build-monitor.pid";
const START_LOCK: &str = ".build-start.lock";
const TEST_PARENT_PID_ENV: &[u8] = b"_RCMAKE_INSTRUMENT_TEST_PARENT_PID";

fn read_monitor(host: &dyn Host, file: &Path) -> Option<(u32, u32)> {
    let bytes = host.read(file, ReadRange::All { limit: 128 }).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut lines = text.lines();
    let parent = lines.next()?.trim().parse::<u32>().ok()?;
    let daemon = lines.next()?.trim().parse::<u32>().ok()?;
    Some((parent, daemon))
}

fn write_monitor(
    host: &dyn Host,
    file: &Path,
    parent_pid: u32,
    daemon_pid: u32,
) -> Result<(), InstrumentError> {
    let content = format!("{parent_pid}\n{daemon_pid}\n");
    host.write(file, content.as_bytes(), WriteMode::Truncate)?;
    Ok(())
}

fn remove_optional(host: &dyn Host, file: &Path) {
    let _ = match host.remove_file(file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
}

/// Find the PID of the native build tool (`ninja`, `make`, etc.) that invoked
/// `--start-instrumentation`.
fn native_parent_pid(host: &dyn Host) -> Option<u32> {
    if let Some(pid) = host
        .env_get(TEST_PARENT_PID_ENV)
        .and_then(|value| value.to_string().parse::<u32>().ok())
        .filter(|&pid| pid > 0)
    {
        return Some(pid);
    }
    if host.system().sysname.as_slice() == b"PureOS" {
        return None;
    }
    platform::parent_build_pid(host)
}

/// Handle `--start-instrumentation <build-dir>`.
pub(crate) fn start(host: &dyn Host, args: &[BString]) -> Result<i32, InstrumentError> {
    if host.env_get(IN_CMAKE_BUILD_ENV).is_some() {
        return Ok(0);
    }
    let build_arg = args
        .first()
        .ok_or_else(|| invalid("Missing build directory for --start-instrumentation."))?;
    let build = absolute(host, &path::to_native(build_arg));
    let queries = read_queries(host, &build)?;
    if queries.is_empty() {
        return Ok(0);
    }
    let api = api_dir(&build);
    host.create_dir_all(&api)?;
    let _lock = host.lock_file(&api.join(START_LOCK), None)?;
    let monitor = api.join(MONITOR_FILE);
    let synthetic = host.system().sysname.as_slice() == b"PureOS";
    let parent_pid = native_parent_pid(host);
    if !synthetic && parent_pid.is_none() {
        // Parent build-tool PID lookup failed on a real OS host: abort daemon
        // startup cleanly rather than prematurely running `postBuild` or writing
        // a `build` snippet before the native build finishes.
        return Ok(0);
    }
    if let (Some(parent), Some((recorded_parent, daemon_pid))) =
        (parent_pid, read_monitor(host, &monitor))
        && recorded_parent == parent
        && platform::is_alive(daemon_pid)
    {
        return Ok(0);
    }

    run_hook(host, &build, Hook::PreBuild)?;

    let dynamic = queries.iter().any(|query| {
        query
            .options
            .contains(&InstrumentOption::DynamicSystemInformation)
    });
    let before = dynamic.then(|| SystemSample::collect(host));
    let time_start = epoch_millis(host);

    let Some(parent_pid) = parent_pid else {
        // Synthetic host (PureHost unit tests without an OS parent PID):
        // finalize synchronously instead of spawning a detached OS process.
        finalize_build(host, &build, time_start, before)?;
        return Ok(0);
    };

    let exe = host.current_exe()?;
    let mut daemon_args: Vec<BString> = vec![
        "-E".into(),
        "__instrument".into(),
        "--wait-and-collect".into(),
        "--build-dir".into(),
        path::from_native_cmake(&build),
        "--parent-pid".into(),
        parent_pid.to_string().into(),
        "--time-start".into(),
        time_start.to_string().into(),
    ];
    if let Some(sample) = before {
        daemon_args.extend(["--before-mem".into(), sample.memory_used.to_string().into()]);
        if let Some(cpu) = sample.cpu_load {
            daemon_args.extend(["--before-cpu".into(), cpu.to_string().into()]);
        }
    }
    let daemon_pid = platform::spawn_detached(&exe, &build, &daemon_args, &host.env_all())?;
    write_monitor(host, &monitor, parent_pid, daemon_pid)?;
    Ok(0)
}

/// Handle `--wait-and-collect --build-dir <dir> --parent-pid <pid> --time-start <ms> ...`.
pub(crate) fn wait_and_collect(host: &dyn Host, args: &[BString]) -> Result<i32, InstrumentError> {
    let mut build: Option<PathBuf> = None;
    let mut parent_pid: Option<u32> = None;
    let mut time_start: Option<u64> = None;
    let mut before_mem: Option<u64> = None;
    let mut before_cpu: Option<f64> = None;

    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| invalid(format!("Missing value for {flag}.")))?;
        match flag.as_slice() {
            b"--build-dir" => build = Some(absolute(host, &path::to_native(value))),
            b"--parent-pid" => {
                parent_pid = Some(
                    value
                        .to_string()
                        .parse::<u32>()
                        .map_err(|_| invalid(format!("Invalid --parent-pid: {value}")))?,
                );
            }
            b"--time-start" => {
                time_start = Some(
                    value
                        .to_string()
                        .parse::<u64>()
                        .map_err(|_| invalid(format!("Invalid --time-start: {value}")))?,
                );
            }
            b"--before-mem" => {
                before_mem = value.to_string().parse::<u64>().ok();
            }
            b"--before-cpu" => {
                before_cpu = value.to_string().parse::<f64>().ok();
            }
            _ => {
                return Err(invalid(format!(
                    "Unknown --wait-and-collect option: {flag}"
                )));
            }
        }
    }

    let build = build.ok_or_else(|| invalid("Missing --build-dir for --wait-and-collect."))?;
    let parent_pid =
        parent_pid.ok_or_else(|| invalid("Missing --parent-pid for --wait-and-collect."))?;
    let time_start = time_start.unwrap_or_else(|| epoch_millis(host));
    let before = before_mem.map(|memory_used| SystemSample {
        memory_used,
        cpu_load: before_cpu,
    });

    if host.system().sysname.as_slice() != b"PureOS" {
        platform::wait_for_exit(host, parent_pid);
    }

    let result = finalize_build(host, &build, time_start, before);

    let api = api_dir(&build);
    if let Ok(_lock) = host.lock_file(&api.join(START_LOCK), Some(Duration::from_secs(5))) {
        let monitor = api.join(MONITOR_FILE);
        if read_monitor(host, &monitor)
            .is_some_and(|(_, daemon_pid)| daemon_pid == host.system().pid)
        {
            remove_optional(host, &monitor);
        }
    }

    result?;
    Ok(0)
}

fn finalize_build(
    host: &dyn Host,
    build: &Path,
    time_start: u64,
    before: Option<SystemSample>,
) -> Result<(), InstrumentError> {
    let queries = read_queries(host, build)?;
    if queries.is_empty() {
        return Ok(());
    }
    let data = api_dir(build).join("data");
    let end = epoch_millis(host);
    let duration = end.saturating_sub(time_start);
    let dynamic_enabled = queries.iter().any(|query| {
        query
            .options
            .contains(&InstrumentOption::DynamicSystemInformation)
    });
    let dynamic = if dynamic_enabled {
        let before = before.unwrap_or_else(|| SystemSample::collect(host));
        Some((before, SystemSample::collect(host)))
    } else {
        None
    };
    let snippet = Snippet {
        role: SnippetRole::Build,
        command: BString::default(),
        working_dir: path::from_native_cmake(build),
        result: None,
        time_start,
        duration,
        target: None,
        outputs: Vec::new(),
        source: None,
        language: None,
        test_name: None,
        config: None,
        stdout: None,
        stderr: None,
    };
    let details = SnippetDetails {
        cmake_content: newest_content(host, &data)?,
        dynamic,
        ..SnippetDetails::default()
    };
    write_snippet_with_details(host, &data, &snippet, &details)?;
    run_hook(host, build, Hook::PostBuild)?;
    Ok(())
}

#[cfg(unix)]
mod platform {
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use rcmake_base::BString;
    use rcmake_host::{Host, HostError, path};
    use rustix::io::Errno;
    use rustix::process::{Pid, getppid, test_kill_process};

    use crate::InstrumentError;

    pub(super) fn parent_build_pid(_host: &dyn Host) -> Option<u32> {
        let ppid = getppid()?.as_raw_nonzero().get();
        u32::try_from(ppid).ok().filter(|&pid| pid > 1)
    }

    pub(super) fn is_alive(pid: u32) -> bool {
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        let Some(pid) = Pid::from_raw(raw) else {
            return false;
        };
        if is_zombie(raw) {
            return false;
        }
        match test_kill_process(pid) {
            Ok(()) => true,
            Err(Errno::PERM) => true,
            Err(_) => false,
        }
    }

    #[cfg(target_os = "linux")]
    #[expect(
        clippy::disallowed_methods,
        reason = "Inspect /proc/<pid>/stat to avoid treating a zombie parent build process as alive."
    )]
    fn is_zombie(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_ascii_whitespace().next())
            == Some("Z")
    }

    #[cfg(not(target_os = "linux"))]
    #[expect(
        clippy::disallowed_methods,
        reason = "Inspect process state with /bin/ps on non-Linux Unix hosts to detect zombies."
    )]
    fn is_zombie(pid: i32) -> bool {
        let Ok(output) = Command::new("/bin/ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        std::str::from_utf8(&output.stdout)
            .ok()
            .is_some_and(|state| state.trim_ascii().starts_with('Z'))
    }

    pub(super) fn wait_for_exit(host: &dyn Host, pid: u32) {
        while is_alive(pid) {
            host.sleep(Duration::from_millis(10));
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "Detached build collector must close inherited stdio handles and outlive the parent process without blocking Host::run."
    )]
    pub(super) fn spawn_detached(
        exe: &Path,
        cwd: &Path,
        args: &[BString],
        env: &[(BString, BString)],
    ) -> Result<u32, InstrumentError> {
        let mut command = Command::new(exe);
        command
            .args(args.iter().map(|arg| path::to_native(arg)))
            .current_dir(cwd)
            .env_clear()
            .envs(
                env.iter()
                    .map(|(k, v)| (path::to_native(k), path::to_native(v))),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command
            .spawn()
            .map_err(|error| HostError::from_io(error, Some(exe.to_path_buf())))?;
        Ok(child.id())
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use rcmake_base::BString;
    use rcmake_host::{
        CommandSpec, Host, HostError, Input, Output, ProcessResult, RunRequest, path,
    };

    use crate::InstrumentError;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    fn powershell_path(host: &dyn Host) -> BString {
        let root = host
            .env_get(b"SystemRoot")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| BString::from(r"C:\Windows"));
        let mut full = path::to_native(&root);
        full.push(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        path::from_native(&full)
    }

    pub(super) fn parent_build_pid(host: &dyn Host) -> Option<u32> {
        let my_pid = host.system().pid;
        let script = format!(
            "$cur = {my_pid}; for ($i = 0; $i -lt 6; $i++) {{ \
             $p = Get-CimInstance Win32_Process -Filter \"ProcessId=$cur\" -ErrorAction SilentlyContinue; \
             if (-not $p) {{ break }}; \
             $pp = [int]$p.ParentProcessId; \
             if ($pp -le 0) {{ break }}; \
             $par = Get-CimInstance Win32_Process -Filter \"ProcessId=$pp\" -ErrorAction SilentlyContinue; \
             if (-not $par) {{ Write-Output $pp; break }}; \
             $name = $par.Name.ToLowerInvariant(); \
             if ($name -in @('ctest.exe','cmake.exe','rcmake.exe','cmd.exe')) {{ $cur = $pp; continue }}; \
             Write-Output $pp; break }}"
        );
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: powershell_path(host),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    script.into(),
                ],
            }],
            cwd: host.initial_cwd(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Capture,
            stderr: Output::Null,
            timeout: Some(Duration::from_secs(10)),
            tee: false,
            default_error_mode: false,
        };
        let output = host.run(&request, &mut |_, _| {}).ok()?;
        if !matches!(output.results.as_slice(), [ProcessResult::Exited(0)]) {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&pid| pid > 0)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "Check whether a Windows PID is still alive without unsafe Win32 calls."
    )]
    fn query_alive(pid: u32) -> Option<bool> {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = std::str::from_utf8(&output.stdout).ok()?;
        let needle = format!("\"{pid}\"");
        Some(text.lines().any(|line| line.contains(&needle)))
    }

    pub(super) fn is_alive(pid: u32) -> bool {
        query_alive(pid).unwrap_or(false)
    }

    pub(super) fn wait_for_exit(host: &dyn Host, pid: u32) {
        // Distinguish "target process already exited" (exit 0) from PowerShell
        // startup/runtime errors (non-zero or spawn failure) so we never finalize
        // early while the native build is still running.
        let script = format!(
            "try {{ $p = Get-Process -Id {pid} -ErrorAction Stop; $p.WaitForExit(); exit 0 }} \
             catch [System.ArgumentException] {{ exit 0 }} \
             catch {{ if ($_.FullyQualifiedErrorId -like 'NoProcessFoundForGivenId*') {{ exit 0 }} else {{ exit 2 }} }}"
        );
        let request = RunRequest {
            pipeline: vec![CommandSpec {
                program: powershell_path(host),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    script.into(),
                ],
            }],
            cwd: host.initial_cwd(),
            env: host.env_all(),
            stdin: Input::Null,
            stdout: Output::Null,
            stderr: Output::Null,
            timeout: None,
            tee: false,
            default_error_mode: false,
        };
        let waited_ok = host
            .run(&request, &mut |_, _| {})
            .is_ok_and(|out| matches!(out.results.as_slice(), [ProcessResult::Exited(0)]));
        if waited_ok {
            return;
        }
        // Fallback: poll with `tasklist` until the process is confirmed gone.
        let mut consecutive_errors = 0u32;
        loop {
            match query_alive(pid) {
                Some(true) => {
                    consecutive_errors = 0;
                    host.sleep(Duration::from_millis(50));
                }
                Some(false) => break,
                None => {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    if consecutive_errors >= 5 {
                        break;
                    }
                    host.sleep(Duration::from_millis(50));
                }
            }
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "Detached build collector must close inherited stdio handles and outlive the parent process without blocking Host::run."
    )]
    pub(super) fn spawn_detached(
        exe: &Path,
        cwd: &Path,
        args: &[BString],
        env: &[(BString, BString)],
    ) -> Result<u32, InstrumentError> {
        let mut command = Command::new(exe);
        command
            .args(args.iter().map(|arg| path::to_native(arg)))
            .current_dir(cwd)
            .env_clear()
            .envs(
                env.iter()
                    .map(|(k, v)| (path::to_native(k), path::to_native(v))),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        let child = command
            .spawn()
            .map_err(|error| HostError::from_io(error, Some(exe.to_path_buf())))?;
        Ok(child.id())
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::path::Path;

    use rcmake_base::BString;
    use rcmake_host::Host;

    use crate::InstrumentError;

    pub(super) fn parent_build_pid(_host: &dyn Host) -> Option<u32> {
        None
    }

    pub(super) fn is_alive(_pid: u32) -> bool {
        false
    }

    pub(super) fn wait_for_exit(_host: &dyn Host, _pid: u32) {}

    pub(super) fn spawn_detached(
        _exe: &Path,
        _cwd: &Path,
        _args: &[BString],
        _env: &[(BString, BString)],
    ) -> Result<u32, InstrumentError> {
        Ok(0)
    }
}
