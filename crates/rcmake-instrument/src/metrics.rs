//! Collect per-process resource use through platform instrumentation helpers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, HostResult, Input, Output, ProcessResult, ReadRange,
    RunOutput, RunRequest, Stream, WriteMode, path,
};

use crate::snippet::ProcessMetrics;

static NEXT_REPORT: AtomicU64 = AtomicU64::new(0);

/// Run a process and collect resource metrics when the platform provides them.
///
/// # Errors
///
/// Returns a host error if starting or waiting for the process fails.
pub fn run(
    host: &dyn Host,
    request: &RunRequest,
    scratch: &Path,
    collect: bool,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> HostResult<(RunOutput, Option<ProcessMetrics>)> {
    let direct = |on_output: &mut dyn FnMut(Stream, &[u8])| {
        host.run(request, on_output).map(|result| (result, None))
    };
    let [command] = request.pipeline.as_slice() else {
        return direct(on_output);
    };
    if !collect {
        return direct(on_output);
    }
    let system = host.system();
    if system.sysname.starts_with(b"Windows") {
        return run_windows(host, request, command, system.pid, on_output);
    }
    let time = Path::new("/usr/bin/time");
    if !host
        .metadata(time, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return direct(on_output);
    }
    let format = match system.sysname.as_slice() {
        b"Darwin" => Format::Darwin,
        b"Linux" => Format::Linux,
        _ => return direct(on_output),
    };
    let sequence = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
    let report = scratch.join(format!(".metrics-{}-{sequence}", system.pid));
    if host
        .create_dir_all(scratch)
        .and_then(|()| host.write(&report, b"", WriteMode::CreateNew))
        .is_err()
    {
        return direct(on_output);
    }
    let mut args = match format {
        Format::Darwin => vec!["-l".into(), "-o".into(), path::from_native(&report)],
        Format::Linux => vec![
            "-f".into(),
            "%M %U %S".into(),
            "-o".into(),
            path::from_native(&report),
        ],
    };
    args.extend([BString::from("--"), command.program.clone()]);
    args.extend(command.args.iter().cloned());
    let wrapped = RunRequest {
        pipeline: vec![CommandSpec {
            program: path::from_native(time),
            args,
        }],
        ..request.clone()
    };
    let run = host.run(&wrapped, on_output);
    let metrics = host
        .read(&report, ReadRange::All { limit: 8192 })
        .ok()
        .and_then(|data| format.parse(&data));
    let _ = host.remove_file(&report);
    run.map(|output| (output, metrics))
}

const WINDOWS_HELPER_NAME: &str = "metrics-helper.exe";
const WINDOWS_HELPER_HASH_NAME: &str = "metrics-helper.sha256";
const WINDOWS_HELPER_LOCK: &str = "metrics-helper.lock";
const HELPER_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Small C# helper compiled into a trusted per-user cache outside the build
/// tree so Windows process metrics collect Job Object accounting (child +
/// descendant CPU time and peak memory) without `unsafe` Rust code.
///
/// Modes:
/// - `<report> -- <resolved-exe> [args...]`: runs `<resolved-exe>` directly via
///   `CreateProcessW(lpApplicationName, lpCommandLine, ...)` so `CreateProcessW`
///   never searches the helper's directory for a bare executable name.
/// - `<report> --batch <cmd-exe> <script> [args...]`: routes `.bat`/`.cmd`
///   targets through `<cmd-exe> /D /S /V:OFF /C "..."` with `cmd.exe` escaping.
const WINDOWS_HELPER_CS: &str = r#"
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

class RcmakeWinMetrics {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public uint cb;
        public string lpReserved;
        public string lpDesktop;
        public string lpTitle;
        public uint dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public ushort wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION {
        public IntPtr hProcess;
        public IntPtr hThread;
        public uint dwProcessId;
        public uint dwThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_BASIC_ACCOUNTING_INFORMATION {
        public long TotalUserTime;
        public long TotalKernelTime;
        public long ThisPeriodTotalUserTime;
        public long ThisPeriodTotalKernelTime;
        public uint TotalPageFaultCount;
        public uint TotalProcesses;
        public uint ActiveProcesses;
        public uint TotalTerminatedProcesses;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        public long PerProcessUserTimeLimit;
        public long PerJobUserTimeLimit;
        public uint LimitFlags;
        public UIntPtr MinimumWorkingSetSize;
        public UIntPtr MaximumWorkingSetSize;
        public uint ActiveProcessLimit;
        public UIntPtr Affinity;
        public uint PriorityClass;
        public uint SchedulingClass;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct IO_COUNTERS {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
        public IO_COUNTERS IoInfo;
        public UIntPtr ProcessMemoryLimit;
        public UIntPtr JobMemoryLimit;
        public UIntPtr PeakProcessMemoryUsed;
        public UIntPtr PeakJobMemoryUsed;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_MEMORY_COUNTERS {
        public uint cb;
        public uint PageFaultCount;
        public UIntPtr PeakWorkingSetSize;
        public UIntPtr WorkingSetSize;
        public UIntPtr QuotaPeakPagedPoolUsage;
        public UIntPtr QuotaPagedPoolUsage;
        public UIntPtr QuotaPeakNonPagedPoolUsage;
        public UIntPtr QuotaNonPagedPoolUsage;
        public UIntPtr PagefileUsage;
        public UIntPtr PeakPagefileUsage;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(
        string app, StringBuilder cmdLine, IntPtr procAttr, IntPtr threadAttr,
        bool inheritHandles, uint creationFlags, IntPtr env, string cwd,
        ref STARTUPINFO si, out PROCESS_INFORMATION pi);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObjectW(IntPtr attr, string name);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr proc);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint ResumeThread(IntPtr thread);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern uint WaitForSingleObject(IntPtr handle, uint ms);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetExitCodeProcess(IntPtr handle, out uint code);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetProcessTimes(IntPtr handle, out long c, out long e, out long k, out long u);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool QueryInformationJobObject(IntPtr job, int infoClass, IntPtr info, uint len, out uint retLen);

    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr handle);

    [DllImport("psapi.dll", SetLastError = true)]
    static extern bool GetProcessMemoryInfo(IntPtr handle, out PROCESS_MEMORY_COUNTERS counters, uint size);

    static string QuoteArg(string arg, bool forceQuote) {
        if (arg.Length == 0) return "\"\"";
        bool needsQuote = forceQuote;
        if (!needsQuote) {
            foreach (char ch in arg) {
                if (ch == ' ' || ch == '\t' || ch == '"') { needsQuote = true; break; }
            }
        }
        if (!needsQuote) return arg;
        var sb = new StringBuilder();
        sb.Append('"');
        int bs = 0;
        foreach (char ch in arg) {
            if (ch == '\\') { bs++; }
            else if (ch == '"') {
                sb.Append('\\', bs * 2 + 1);
                sb.Append('"');
                bs = 0;
            } else {
                if (bs > 0) { sb.Append('\\', bs); bs = 0; }
                sb.Append(ch);
            }
        }
        if (bs > 0) sb.Append('\\', bs * 2);
        sb.Append('"');
        return sb.ToString();
    }

    static string QuoteCmdArg(string arg) {
        bool hasMeta = false;
        foreach (char ch in arg) {
            if ("&|<>()^".IndexOf(ch) >= 0) { hasMeta = true; break; }
        }
        string quoted = QuoteArg(arg, hasMeta);
        var sb = new StringBuilder(quoted.Length + 4);
        bool inQuotes = false;
        foreach (char ch in quoted) {
            if (ch == '"') {
                inQuotes = !inQuotes;
                sb.Append(ch);
            } else if (!inQuotes && "&|<>()^".IndexOf(ch) >= 0) {
                sb.Append('^');
                sb.Append(ch);
            } else if (ch == '%') {
                sb.Append("%%");
            } else {
                sb.Append(ch);
            }
        }
        return sb.ToString();
    }

    static int Main(string[] args) {
        if (args.Length < 3) return 1;
        string reportPath = args[0];
        string mode = args[1];
        string appName;
        var cmd = new StringBuilder();

        if (mode == "--") {
            appName = args[2];
            for (int i = 2; i < args.Length; i++) {
                if (i > 2) cmd.Append(' ');
                cmd.Append(QuoteArg(args[i], false));
            }
        } else if (mode == "--batch" && args.Length >= 4) {
            appName = args[2];
            cmd.Append(QuoteArg(appName, false));
            cmd.Append(" /D /S /V:OFF /C \"");
            cmd.Append(QuoteArg(args[3], true));
            for (int i = 4; i < args.Length; i++) {
                cmd.Append(' ');
                cmd.Append(QuoteCmdArg(args[i]));
            }
            cmd.Append('"');
        } else {
            return 1;
        }

        var si = new STARTUPINFO();
        si.cb = (uint)Marshal.SizeOf(typeof(STARTUPINFO));
        PROCESS_INFORMATION pi;
        const uint CREATE_SUSPENDED = 0x00000004;
        if (!CreateProcessW(appName, cmd, IntPtr.Zero, IntPtr.Zero, true, CREATE_SUSPENDED, IntPtr.Zero, null, ref si, out pi)) {
            return 1;
        }

        IntPtr job = CreateJobObjectW(IntPtr.Zero, null);
        bool inJob = job != IntPtr.Zero && AssignProcessToJobObject(job, pi.hProcess);
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);
        WaitForSingleObject(pi.hProcess, 0xFFFFFFFF);

        uint exitCode = 1;
        GetExitCodeProcess(pi.hProcess, out exitCode);

        ulong maxRssBytes = 0;
        long user100ns = 0;
        long kernel100ns = 0;

        long ct, et, kt, ut;
        if (GetProcessTimes(pi.hProcess, out ct, out et, out kt, out ut)) {
            user100ns = ut;
            kernel100ns = kt;
        }

        var pmc = new PROCESS_MEMORY_COUNTERS();
        pmc.cb = (uint)Marshal.SizeOf(typeof(PROCESS_MEMORY_COUNTERS));
        if (GetProcessMemoryInfo(pi.hProcess, out pmc, pmc.cb)) {
            ulong ws = pmc.PeakWorkingSetSize.ToUInt64();
            if (ws > maxRssBytes) maxRssBytes = ws;
        }

        if (inJob) {
            uint retLen;
            int accSize = Marshal.SizeOf(typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION));
            IntPtr accBuf = Marshal.AllocHGlobal(accSize);
            try {
                if (QueryInformationJobObject(job, 1, accBuf, (uint)accSize, out retLen)) {
                    var acc = (JOBOBJECT_BASIC_ACCOUNTING_INFORMATION)Marshal.PtrToStructure(accBuf, typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION));
                    if (acc.TotalUserTime > user100ns) user100ns = acc.TotalUserTime;
                    if (acc.TotalKernelTime > kernel100ns) kernel100ns = acc.TotalKernelTime;
                }
            } finally { Marshal.FreeHGlobal(accBuf); }

            int extSize = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
            IntPtr extBuf = Marshal.AllocHGlobal(extSize);
            try {
                if (QueryInformationJobObject(job, 9, extBuf, (uint)extSize, out retLen)) {
                    var ext = (JOBOBJECT_EXTENDED_LIMIT_INFORMATION)Marshal.PtrToStructure(extBuf, typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
                    ulong jobPeak = ext.PeakJobMemoryUsed.ToUInt64();
                    ulong procPeak = ext.PeakProcessMemoryUsed.ToUInt64();
                    if (jobPeak > maxRssBytes) maxRssBytes = jobPeak;
                    if (procPeak > maxRssBytes) maxRssBytes = procPeak;
                }
            } finally { Marshal.FreeHGlobal(extBuf); }
        }

        if (job != IntPtr.Zero) CloseHandle(job);
        CloseHandle(pi.hProcess);

        ulong maxRssKib = maxRssBytes / 1024UL;
        ulong userUs = user100ns > 0 ? (ulong)(user100ns / 10L) : 0UL;
        ulong kernelUs = kernel100ns > 0 ? (ulong)(kernel100ns / 10L) : 0UL;
        try {
            File.WriteAllText(reportPath, string.Format("{0} {1} {2}\n", maxRssKib, userUs, kernelUs));
        } catch {}
        return unchecked((int)exitCode);
    }
}
"#;

struct HelperBinary {
    exe: PathBuf,
    report_dir: PathBuf,
    ephemeral: bool,
}

fn is_real_dir(host: &dyn Host, dir: &Path) -> bool {
    host.metadata(dir, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
}

fn is_real_file(host: &dyn Host, file: &Path) -> bool {
    host.metadata(file, Follow::No)
        .is_ok_and(|meta| meta.kind == FileKind::File && meta.len > 0)
}

fn trusted_cache_dir(host: &dyn Host, source_hash: &str) -> (PathBuf, PathBuf) {
    let base = host
        .env_get(b"LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(|value| {
            path::to_native(&value)
                .join("rcmake")
                .join("metrics-helper")
        })
        .unwrap_or_else(|| {
            let user: String = host
                .env_get(b"USERNAME")
                .map(|value| {
                    value
                        .to_string()
                        .chars()
                        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
                        .collect()
                })
                .filter(|name: &String| !name.is_empty())
                .unwrap_or_else(|| host.system().pid.to_string());
            std::env::temp_dir().join(format!("rcmake-metrics-{user}"))
        });
    let dir = base.join(source_hash);
    (base, dir)
}

fn verify_cached_helper(host: &dyn Host, helper: &Path, hash_file: &Path) -> bool {
    if !is_real_file(host, helper) || !is_real_file(host, hash_file) {
        return false;
    }
    let Ok(expected) = host.read(hash_file, ReadRange::All { limit: 128 }) else {
        return false;
    };
    let Ok(expected) = std::str::from_utf8(&expected) else {
        return false;
    };
    let expected = expected.trim();
    if expected.len() != 64 {
        return false;
    }
    let Ok(bytes) = host.read(
        helper,
        ReadRange::All {
            limit: HELPER_MAX_BYTES,
        },
    ) else {
        return false;
    };
    HashAlgo::Sha256.hex_digest(&bytes) == expected
}

fn compile_windows_helper(host: &dyn Host, work_dir: &Path, output: &Path) -> bool {
    let pid = host.system().pid;
    let seq = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
    let source = work_dir.join(format!("metrics-src-{pid}-{seq}.cs"));
    if host
        .write(&source, WINDOWS_HELPER_CS.as_bytes(), WriteMode::CreateNew)
        .is_err()
    {
        return false;
    }
    let root = host
        .env_get(b"SystemRoot")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| BString::from(r"C:\Windows"));
    let root_path = path::to_native(&root);
    let csc_candidates = [
        root_path.join(r"Microsoft.NET\Framework64\v4.0.30319\csc.exe"),
        root_path.join(r"Microsoft.NET\Framework\v4.0.30319\csc.exe"),
    ];
    let compiled = csc_candidates
        .iter()
        .find(|candidate| {
            host.metadata(candidate, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
        })
        .is_some_and(|csc| {
            let mut out_arg = BString::from("/out:");
            out_arg.extend_from_slice(&path::from_native(output));
            let request = RunRequest {
                pipeline: vec![CommandSpec {
                    program: path::from_native(csc),
                    args: vec![
                        "/nologo".into(),
                        "/optimize+".into(),
                        "/target:exe".into(),
                        out_arg,
                        path::from_native(&source),
                    ],
                }],
                cwd: work_dir.to_path_buf(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Null,
                stderr: Output::Null,
                timeout: Some(std::time::Duration::from_secs(30)),
                tee: false,
                default_error_mode: false,
            };
            host.run(&request, &mut |_, _| {})
                .is_ok_and(|out| matches!(out.results.as_slice(), [ProcessResult::Exited(0)]))
        });
    let _ = host.remove_file(&source);
    compiled && is_real_file(host, output)
}

fn ensure_windows_helper(host: &dyn Host) -> Option<HelperBinary> {
    let source_hash = HashAlgo::Sha256.hex_digest(WINDOWS_HELPER_CS.as_bytes());
    let (base, cache_dir) = trusted_cache_dir(host, &source_hash);
    if host.create_dir_all(&cache_dir).is_ok()
        && is_real_dir(host, &base)
        && is_real_dir(host, &cache_dir)
    {
        let helper = cache_dir.join(WINDOWS_HELPER_NAME);
        let hash_file = cache_dir.join(WINDOWS_HELPER_HASH_NAME);
        if verify_cached_helper(host, &helper, &hash_file) {
            return Some(HelperBinary {
                exe: helper,
                report_dir: cache_dir,
                ephemeral: false,
            });
        }
        if let Ok(_lock) = host.lock_file(&cache_dir.join(WINDOWS_HELPER_LOCK), None)
            && is_real_dir(host, &cache_dir)
        {
            if verify_cached_helper(host, &helper, &hash_file) {
                return Some(HelperBinary {
                    exe: helper,
                    report_dir: cache_dir,
                    ephemeral: false,
                });
            }
            let pid = host.system().pid;
            let seq = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
            let staged = cache_dir.join(format!("metrics-{pid}-{seq}.tmp.exe"));
            if compile_windows_helper(host, &cache_dir, &staged)
                && let Ok(bytes) = host.read(
                    &staged,
                    ReadRange::All {
                        limit: HELPER_MAX_BYTES,
                    },
                )
            {
                let digest = HashAlgo::Sha256.hex_digest(&bytes);
                let _ = host.remove_file(&helper);
                let _ = host.remove_file(&hash_file);
                if host.rename(&staged, &helper).is_ok()
                    && host
                        .write(&hash_file, digest.as_bytes(), WriteMode::CreateNew)
                        .is_ok()
                    && verify_cached_helper(host, &helper, &hash_file)
                {
                    return Some(HelperBinary {
                        exe: helper,
                        report_dir: cache_dir,
                        ephemeral: false,
                    });
                }
            }
            let _ = host.remove_file(&staged);
        }
    }

    // Fallback: compile a fresh single-use helper in a unique temp directory.
    let pid = host.system().pid;
    let seq = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
    let temp_dir = std::env::temp_dir().join(format!("rcmake-metrics-ephemeral-{pid}-{seq}"));
    host.create_private_dir(&temp_dir).ok()?;
    if !is_real_dir(host, &temp_dir) {
        return None;
    }
    let exe = temp_dir.join(WINDOWS_HELPER_NAME);
    if !compile_windows_helper(host, &temp_dir, &exe) {
        let _ = host.remove_dir_all(&temp_dir);
        return None;
    }
    Some(HelperBinary {
        exe,
        report_dir: temp_dir,
        ephemeral: true,
    })
}

fn env_value_ci<'a>(env: &'a [(BString, BString)], name: &[u8]) -> Option<&'a BString> {
    env.iter()
        .find(|(key, _)| key.as_slice().eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

fn is_windows_batch(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
}

fn try_windows_executable(host: &dyn Host, base: &Path, pathext: &[&str]) -> Option<PathBuf> {
    let has_ext = base.extension().is_some();
    if has_ext
        && host
            .metadata(base, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Some(base.to_path_buf());
    }
    for ext in pathext {
        let mut candidate = base.as_os_str().to_os_string();
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if host
            .metadata(&candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Some(candidate);
        }
    }
    if !has_ext
        && host
            .metadata(base, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Some(base.to_path_buf());
    }
    None
}

/// Resolve `program` against explicit path / `cwd` / `PATH` + `PATHEXT` so the
/// helper invokes `CreateProcessW` with an explicit executable path rather than
/// letting Windows search the helper's own directory first.
fn resolve_windows_program(
    host: &dyn Host,
    cwd: &Path,
    env: &[(BString, BString)],
    program: &BString,
) -> Option<PathBuf> {
    let raw_pathext = env_value_ci(env, b"PATHEXT")
        .map(BString::to_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
    let pathext: Vec<&str> = raw_pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty())
        .collect();
    let native = path::to_native(program);
    let has_dir = program
        .as_slice()
        .iter()
        .any(|&byte| matches!(byte, b'/' | b'\\' | b':'));
    if has_dir {
        let candidate = if native.is_absolute() {
            native
        } else {
            cwd.join(native)
        };
        return try_windows_executable(host, &candidate, &pathext);
    }
    if let Some(found) = try_windows_executable(host, &cwd.join(&native), &pathext) {
        return Some(found);
    }
    if let Some(path_var) = env_value_ci(env, b"PATH") {
        for entry in path_var.as_slice().split(|&byte| byte == b';') {
            let trimmed = entry.trim_ascii();
            if trimmed.is_empty() {
                continue;
            }
            let unquoted = trimmed
                .strip_prefix(b"\"")
                .and_then(|rest| rest.strip_suffix(b"\""))
                .unwrap_or(trimmed);
            if unquoted.is_empty() {
                continue;
            }
            let dir = path::to_native(unquoted);
            if let Some(found) = try_windows_executable(host, &dir.join(&native), &pathext) {
                return Some(found);
            }
        }
    }
    None
}

fn resolve_cmd_exe(host: &dyn Host, env: &[(BString, BString)]) -> PathBuf {
    if let Some(comspec) = env_value_ci(env, b"COMSPEC").filter(|value| !value.is_empty()) {
        let candidate = path::to_native(comspec);
        if host
            .metadata(&candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return candidate;
        }
    }
    let root = env_value_ci(env, b"SystemRoot")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| BString::from(r"C:\Windows"));
    path::to_native(&root).join(r"System32\cmd.exe")
}

fn run_windows(
    host: &dyn Host,
    request: &RunRequest,
    command: &CommandSpec,
    pid: u32,
    on_output: &mut dyn FnMut(Stream, &[u8]),
) -> HostResult<(RunOutput, Option<ProcessMetrics>)> {
    let direct = |on_output: &mut dyn FnMut(Stream, &[u8])| {
        host.run(request, on_output).map(|result| (result, None))
    };
    let Some(resolved_program) =
        resolve_windows_program(host, &request.cwd, &request.env, &command.program)
    else {
        return direct(on_output);
    };
    let Some(helper) = ensure_windows_helper(host) else {
        return direct(on_output);
    };
    let sequence = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
    let report = helper
        .report_dir
        .join(format!("metrics-{pid}-{sequence}.txt"));
    if host.write(&report, b"", WriteMode::CreateNew).is_err() {
        if helper.ephemeral {
            let _ = host.remove_dir_all(&helper.report_dir);
        }
        return direct(on_output);
    }
    let mut args = vec![path::from_native(&report)];
    if is_windows_batch(&resolved_program) {
        let cmd_exe = resolve_cmd_exe(host, &request.env);
        args.extend([
            BString::from("--batch"),
            path::from_native(&cmd_exe),
            path::from_native(&resolved_program),
        ]);
    } else {
        args.extend([BString::from("--"), path::from_native(&resolved_program)]);
    }
    args.extend(command.args.iter().cloned());
    let wrapped = RunRequest {
        pipeline: vec![CommandSpec {
            program: path::from_native(&helper.exe),
            args,
        }],
        ..request.clone()
    };
    let run = host.run(&wrapped, on_output);
    let metrics = host
        .read(&report, ReadRange::All { limit: 1024 })
        .ok()
        .and_then(|data| parse_windows_report(&data));
    let _ = host.remove_file(&report);
    if helper.ephemeral {
        let _ = host.remove_dir_all(&helper.report_dir);
    }
    run.map(|output| (output, metrics))
}

fn parse_windows_report(bytes: &[u8]) -> Option<ProcessMetrics> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text.split_ascii_whitespace();
    Some(ProcessMetrics {
        max_rss: fields.next()?.parse().ok()?,
        user_time: fields.next()?.parse().ok()?,
        system_time: fields.next()?.parse().ok()?,
    })
}

enum Format {
    Darwin,
    Linux,
}

impl Format {
    fn parse(self, bytes: &[u8]) -> Option<ProcessMetrics> {
        let report = std::str::from_utf8(bytes).ok()?;
        let seconds = |text: &str| {
            let seconds: f64 = text.replace(',', ".").parse().ok()?;
            (seconds.is_finite() && seconds >= 0.0)
                .then_some((seconds * 1_000_000.0).round() as u64)
        };
        let mut lines = report.lines();
        match self {
            Self::Linux => {
                let mut fields = lines.next_back()?.split_ascii_whitespace();
                Some(ProcessMetrics {
                    max_rss: fields.next()?.parse().ok()?,
                    user_time: seconds(fields.next()?)?,
                    system_time: seconds(fields.next()?)?,
                })
            }
            Self::Darwin => {
                let mut fields = lines
                    .find(|line| line.contains(" real "))?
                    .split_ascii_whitespace();
                let user_time = seconds(fields.nth(2)?)?;
                let system_time = seconds(fields.nth(1)?)?;
                let rss: u64 = lines
                    .find(|line| line.ends_with("maximum resident set size"))?
                    .split_ascii_whitespace()
                    .next()?
                    .parse()
                    .ok()?;
                Some(ProcessMetrics {
                    max_rss: rss / 1024,
                    user_time,
                    system_time,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessMetrics, parse_windows_report};

    #[test]
    fn parses_windows_metrics_report() {
        assert_eq!(
            parse_windows_report(b"65536 12345 678\n"),
            Some(ProcessMetrics {
                max_rss: 65_536,
                user_time: 12_345,
                system_time: 678,
            })
        );
        assert_eq!(parse_windows_report(b""), None);
        assert_eq!(parse_windows_report(b"123 456"), None);
    }
}
