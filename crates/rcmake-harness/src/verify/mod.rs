//! Explicit configure verification companion (`rcmake-verify`).
//!
//! Reads a typed configure-only request over stdin, validates both executables (blocking
//! recursive oracle selection and requiring CMake 4.5.1 for the parity verdict), runs both
//! engines against identical staged absolute paths with isolated scratch environments, and
//! emits a structured parity report.

mod collect;
mod compare;
mod protocol;
mod render;
mod stage;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::Duration;

use rcmake_base::BString;
use rcmake_host::{
    CommandSpec, FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path,
};

use self::collect::{ConfigureOutcome, ProcessOutcome, bounded_tail, collect_run_evidence};
use self::compare::{PairedTimings, SectionReport, Verdict, VerifyReport, compare_runs};
use self::protocol::{PathArg, VerifyRequest};
use self::render::{render_json, render_text};
use self::stage::ScratchWorkspace;

const TARGET_ORACLE_VERSION: &str = rcmake_base::CMAKE_VERSION_STR;
const MAX_STDIN_BYTES: u64 = 4 * 1024 * 1024;
const OVERALL_VERIFY_TIMEOUT: Duration = Duration::from_secs(600);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONFIGURE_OUTPUT_BYTES: usize = 32 << 20;

const HELP: &str = "\
Usage: rcmake-verify --request-stdin
       rcmake-verify --version

Diagnostic companion binary invoked by 'rcmake verify --oracle <path>'.
";

/// Single overall wall-clock deadline shared across engine probes, configures, and dry runs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    start: Duration,
    limit: Duration,
}

impl Deadline {
    pub(crate) fn new(host: &dyn Host, limit: Duration) -> Self {
        Self {
            start: host.monotonic(),
            limit,
        }
    }

    pub(crate) fn remaining(&self, host: &dyn Host) -> Option<Duration> {
        let elapsed = host.monotonic().saturating_sub(self.start);
        self.limit.checked_sub(elapsed).filter(|d| !d.is_zero())
    }

    pub(crate) fn step_timeout(&self, host: &dyn Host, cap: Duration) -> Option<Duration> {
        self.remaining(host).map(|rem| rem.min(cap))
    }
}

/// Run the `rcmake-verify` entry point with the given `Host` and CLI arguments.
pub(crate) fn run(host: &dyn Host, args: &[BString]) -> i32 {
    match args {
        [flag] if flag.as_slice() == b"--version" => {
            host.write_out(
                Stream::Stdout,
                format!("rcmake-verify {}\n", env!("CARGO_PKG_VERSION")).as_bytes(),
            );
            return 0;
        }
        [flag] if flag.as_slice() == b"--help" || flag.as_slice() == b"-h" => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        [flag] if flag.as_slice() == b"--request-stdin" => {}
        _ => {
            host.write_out(
                Stream::Stderr,
                b"rcmake-verify: expected --request-stdin, --version, or --help.\n",
            );
            return 2;
        }
    }

    let raw = match host.read_stdin(MAX_STDIN_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => {
            host.write_out(
                Stream::Stderr,
                format!("rcmake-verify: could not read request from stdin: {err}\n").as_bytes(),
            );
            return 2;
        }
    };

    let req = match VerifyRequest::parse_json(host, &raw) {
        Ok(req) => req,
        Err(err) => {
            host.write_out(Stream::Stderr, format!("rcmake-verify: {err}\n").as_bytes());
            return 2;
        }
    };

    let overall_limit = req
        .timeout_secs
        .map_or(OVERALL_VERIFY_TIMEOUT, Duration::from_secs);
    execute_request_with_deadline(host, &req, overall_limit)
}

fn emit_progress(host: &dyn Host, phase: &str) {
    host.write_out(
        Stream::Stderr,
        format!("rcmake verify: {phase}\n").as_bytes(),
    );
}

pub(crate) fn execute_request_with_deadline(
    host: &dyn Host,
    req: &VerifyRequest,
    overall_limit: Duration,
) -> i32 {
    let deadline = Deadline::new(host, overall_limit);
    let external_inputs: Vec<String> = req
        .external_inputs
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();

    let oracle_exe = match resolve_oracle_from_request(host, req) {
        Ok(exe) => exe,
        Err(msg) => {
            return emit_invalid_input(
                host,
                req,
                "",
                "unknown".to_owned(),
                "unknown".to_owned(),
                external_inputs,
                msg,
            );
        }
    };
    let oracle_disp = oracle_exe.display().to_string();

    let (oracle_version, rcmake_version) = match validate_engines(host, req, &oracle_exe, &deadline)
    {
        Ok(versions) => versions,
        Err((o_ver, r_ver, msg)) => {
            return emit_invalid_input(host, req, &oracle_disp, o_ver, r_ver, external_inputs, msg);
        }
    };

    emit_progress(host, "staging source");
    let ws =
        match ScratchWorkspace::create(host, &req.source_dir, req.excluded_build_dir.as_deref()) {
            Ok(ws) => ws,
            Err(msg) => {
                return emit_invalid_input(
                    host,
                    req,
                    &oracle_disp,
                    oracle_version,
                    rcmake_version,
                    external_inputs,
                    msg,
                );
            }
        };

    let cli_args = req.materialize_args(
        ws.staged_src(),
        ws.staged_bin(),
        req.request_compile_commands,
    );

    let oracle_cwd = match ws.reset_for_run(req.cwd_rel_to_source.as_ref()) {
        Ok(cwd) => cwd,
        Err(msg) => {
            let _ = ws.cleanup();
            return emit_invalid_input(
                host,
                req,
                &oracle_disp,
                oracle_version,
                rcmake_version,
                external_inputs,
                msg,
            );
        }
    };

    let oracle_env = ws.child_env(false);
    if let Err(msg) = validate_staged_presets(host, req, ws.staged_src(), &oracle_env) {
        let _ = ws.cleanup();
        return emit_invalid_input(
            host,
            req,
            &oracle_disp,
            oracle_version,
            rcmake_version,
            external_inputs,
            msg,
        );
    }

    emit_progress(host, "running oracle configure");
    let oracle_outcome = run_configure(
        host,
        &oracle_exe,
        &cli_args,
        &oracle_cwd,
        &oracle_env,
        &deadline,
    );
    let oracle_evidence = collect_run_evidence(
        host,
        &req.source_dir,
        ws.staged_src(),
        ws.staged_bin(),
        ws.root(),
        &oracle_env,
        oracle_outcome,
        req.request_compile_commands,
        req.compile_commands_user_disabled,
        &deadline,
    );

    // Recreate the staged paths and scratch caches so rcmake runs against an identical tree.
    let rcmake_cwd = match ws.reset_for_run(req.cwd_rel_to_source.as_ref()) {
        Ok(cwd) => cwd,
        Err(msg) => {
            let _ = ws.cleanup();
            return emit_invalid_input(
                host,
                req,
                &oracle_disp,
                oracle_version,
                rcmake_version,
                external_inputs,
                msg,
            );
        }
    };
    let rcmake_env = ws.child_env(true);
    emit_progress(host, "running rcmake configure");
    let rcmake_outcome = run_configure(
        host,
        &req.rcmake_exe,
        &cli_args,
        &rcmake_cwd,
        &rcmake_env,
        &deadline,
    );
    let rcmake_evidence = collect_run_evidence(
        host,
        &req.source_dir,
        ws.staged_src(),
        ws.staged_bin(),
        ws.root(),
        &rcmake_env,
        rcmake_outcome,
        req.request_compile_commands,
        req.compile_commands_user_disabled,
        &deadline,
    );

    emit_progress(host, "comparing artifacts");
    let mut report = compare_runs(
        host,
        &oracle_exe,
        oracle_version,
        &req.rcmake_exe,
        rcmake_version,
        ws.staged_src(),
        ws.staged_bin(),
        ws.staged_home(),
        ws.staged_tmp(),
        req.request_compile_commands,
        req.compile_commands_user_disabled,
        external_inputs,
        &oracle_evidence,
        &rcmake_evidence,
    );

    let cleanup_warning = ws.cleanup().err();
    if let Some(warn) = &cleanup_warning {
        host.write_out(
            Stream::Stderr,
            format!("rcmake verify: warning: {warn}\n").as_bytes(),
        );
    }
    report.cleanup_warning = cleanup_warning;

    let rendered = if req.json {
        render_json(&report)
    } else {
        render_text(&report)
    };
    host.write_out(Stream::Stdout, rendered.as_bytes());
    report.verdict.exit_code()
}

/// Resolve the requested configure preset via the production `rcmake-presets` loader against
/// the staged source tree and exact child environment, rejecting unresolved presets or any
/// preset that enables `graphviz` or `trace.redirect` before either engine executes.
fn validate_staged_presets(
    host: &dyn Host,
    req: &VerifyRequest,
    staged_src: &Path,
    child_env: &[(BString, BString)],
) -> Result<(), String> {
    let Some(preset_name) = req.preset_name() else {
        return Ok(());
    };
    let presets = match req.presets_file() {
        Some(PathArg::SourceRel(rel)) => {
            let file = staged_src.join(path::to_native(rel));
            rcmake_presets::load_file(staged_src, &file, host)
        }
        Some(PathArg::ExternalAbsolute(abs)) => {
            let file = path::to_native(abs);
            rcmake_presets::load_file(staged_src, &file, host)
        }
        None => rcmake_presets::load(staged_src, host),
    }
    .map_err(|err| format!("Failed to load configure presets: {err}"))?;

    let env = rcmake_presets::EnvSnapshot {
        vars: child_env.iter().cloned().collect(),
    };
    let resolved = rcmake_presets::resolve(
        &presets,
        rcmake_presets::PresetKind::Configure,
        preset_name,
        &env,
    )
    .map_err(|err| {
        format!(
            "Failed to resolve configure preset '{}': {err}",
            String::from_utf8_lossy(preset_name)
        )
    })?;

    if let rcmake_presets::PresetSettings::Configure(cfg) = &resolved.settings {
        if cfg.graphviz.is_some() {
            return Err(format!(
                "Configure preset '{}' enables graphviz output, which writes outside the verify scratch tree.",
                String::from_utf8_lossy(preset_name)
            ));
        }
        if cfg
            .trace
            .as_ref()
            .and_then(|trace| trace.redirect.as_ref())
            .is_some()
        {
            return Err(format!(
                "Configure preset '{}' enables trace.redirect, which writes outside the verify scratch tree.",
                String::from_utf8_lossy(preset_name)
            ));
        }
    }
    Ok(())
}

fn resolve_oracle_from_request(host: &dyn Host, req: &VerifyRequest) -> Result<PathBuf, String> {
    if let Some(exe) = &req.oracle_exe {
        return Ok(exe.clone());
    }
    let raw = host
        .env_get(b"RCMAKE_ORACLE_CMAKE")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            "Missing required --oracle <path> option (or set RCMAKE_ORACLE_CMAKE). Run 'rcmake verify --help'."
                .to_owned()
        })?;
    if raw.contains(&b'/') || raw.contains(&b'\\') {
        let native = path::to_native(&raw);
        return Ok(if native.is_absolute() {
            native
        } else {
            host.initial_cwd().join(native)
        });
    }
    let windows = host.system().sysname.eq_ignore_ascii_case(b"Windows");
    let sep = if windows { b';' } else { b':' };
    if let Some(path_var) = host.env_get(b"PATH") {
        let name = String::from_utf8_lossy(&raw);
        let with_exe = if windows && !name.to_ascii_lowercase().ends_with(".exe") {
            Some(format!("{name}.exe"))
        } else {
            None
        };
        for dir in path_var.split(|&b| b == sep).filter(|d| !d.is_empty()) {
            let base = path::to_native(dir);
            for leaf in std::iter::once(name.as_ref()).chain(with_exe.as_deref()) {
                let candidate = base.join(leaf);
                if host
                    .metadata(&candidate, Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::File)
                {
                    return Ok(candidate);
                }
            }
        }
    }
    Ok(host.initial_cwd().join(path::to_native(&raw)))
}

fn emit_invalid_input(
    host: &dyn Host,
    req: &VerifyRequest,
    oracle_exe: &str,
    oracle_version: String,
    rcmake_version: String,
    external_inputs: Vec<String>,
    message: String,
) -> i32 {
    if req.json {
        let skipped = SectionReport::skipped("Not executed due to invalid input.");
        let report = VerifyReport {
            verdict: Verdict::InvalidInput,
            oracle_exe: oracle_exe.to_owned(),
            oracle_version,
            rcmake_exe: req.rcmake_exe.display().to_string(),
            rcmake_version,
            generator: req
                .generator_hint
                .as_deref()
                .map(|g| String::from_utf8_lossy(g).into_owned()),
            compile_commands_injected: req.request_compile_commands,
            external_inputs,
            timings: PairedTimings {
                run_order: ["oracle", "rcmake"],
                oracle_ns: 0,
                rcmake_ns: 0,
                speedup: None,
            },
            configure: skipped.clone(),
            codemodel: skipped.clone(),
            cache: skipped.clone(),
            compile_commands: skipped.clone(),
            ninja_dry_run: skipped,
            error: Some(message),
            cleanup_warning: None,
        };
        host.write_out(Stream::Stdout, render_json(&report).as_bytes());
    } else {
        host.write_out(
            Stream::Stderr,
            format!("rcmake verify: {message}\n").as_bytes(),
        );
    }
    Verdict::InvalidInput.exit_code()
}

fn validate_engines(
    host: &dyn Host,
    req: &VerifyRequest,
    oracle_exe: &Path,
    deadline: &Deadline,
) -> Result<(String, String), (String, String, String)> {
    let mut oracle_ver_out = "unknown".to_owned();
    let mut rcmake_ver_out = "unknown".to_owned();

    if !host
        .metadata(&req.rcmake_exe, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Err((
            oracle_ver_out,
            rcmake_ver_out,
            format!(
                "rcmake executable '{}' does not exist.",
                req.rcmake_exe.display()
            ),
        ));
    }
    if !host
        .metadata(oracle_exe, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::File)
    {
        return Err((
            oracle_ver_out,
            rcmake_ver_out,
            format!(
                "Oracle executable '{}' does not exist.",
                oracle_exe.display()
            ),
        ));
    }

    let rcmake_canon = host
        .canonicalize(&req.rcmake_exe)
        .unwrap_or_else(|_| req.rcmake_exe.clone());
    let oracle_canon = host
        .canonicalize(oracle_exe)
        .unwrap_or_else(|_| oracle_exe.to_path_buf());
    if rcmake_canon == oracle_canon {
        return Err((
            oracle_ver_out,
            rcmake_ver_out,
            format!(
                "Oracle path '{}' resolves to the same executable as rcmake. Supply an upstream CMake {} executable.",
                oracle_exe.display(),
                TARGET_ORACLE_VERSION
            ),
        ));
    }

    let rcmake_banner = probe_version(host, &req.rcmake_exe, deadline).map_err(|err| {
        (
            oracle_ver_out.clone(),
            rcmake_ver_out.clone(),
            format!(
                "Could not read version from rcmake '{}': {err}",
                req.rcmake_exe.display()
            ),
        )
    })?;
    rcmake_ver_out = extract_version_line(&rcmake_banner);

    let oracle_banner = probe_version(host, oracle_exe, deadline).map_err(|err| {
        (
            oracle_ver_out.clone(),
            rcmake_ver_out.clone(),
            format!(
                "Could not read version from oracle '{}': {err}",
                oracle_exe.display()
            ),
        )
    })?;
    oracle_ver_out = extract_version_line(&oracle_banner);

    if oracle_banner.to_ascii_lowercase().contains("rcmake") {
        return Err((
            oracle_ver_out,
            rcmake_ver_out,
            format!(
                "Oracle executable '{}' identifies as rcmake. Supply an upstream CMake {} executable.",
                oracle_exe.display(),
                TARGET_ORACLE_VERSION
            ),
        ));
    }

    let Some(parsed_oracle) = extract_cmake_semver(&oracle_banner) else {
        return Err((
            oracle_ver_out,
            rcmake_ver_out,
            format!(
                "Oracle executable '{}' did not report a CMake version ('{}').",
                oracle_exe.display(),
                extract_version_line(&oracle_banner)
            ),
        ));
    };

    if parsed_oracle != TARGET_ORACLE_VERSION {
        return Err((
            parsed_oracle.clone(),
            rcmake_ver_out,
            format!(
                "Oracle executable '{}' version mismatch: detected CMake {parsed_oracle}, expected CMake {TARGET_ORACLE_VERSION}.",
                oracle_exe.display()
            ),
        ));
    }

    Ok((parsed_oracle, rcmake_ver_out))
}

fn probe_version(host: &dyn Host, exe: &Path, deadline: &Deadline) -> Result<String, String> {
    let Some(timeout) = deadline.step_timeout(host, VERSION_TIMEOUT) else {
        return Err("overall verify deadline exceeded".to_owned());
    };
    let req = RunRequest {
        pipeline: vec![CommandSpec {
            program: path::from_native(exe),
            args: vec![BString::from("--version")],
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(timeout),
        tee: false,
        default_error_mode: false,
    };
    let out = host
        .run_bounded(&req, 64 * 1024)
        .map_err(|err| err.to_string())?;
    match out.results.first() {
        Some(ProcessResult::Exited(0)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Some(ProcessResult::Exited(code)) => Err(format!("exited with status {code}")),
        Some(ProcessResult::TimedOut) => Err("timed out".to_owned()),
        Some(ProcessResult::Signaled(sig)) => Err(format!("terminated by signal {sig}")),
        Some(ProcessResult::SpawnFailed(err)) => Err(err.to_string()),
        None => Err("no exit status".to_owned()),
    }
}

fn extract_version_line(banner: &str) -> String {
    let mut lines = banner.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("unknown");
    if let Some(second) = lines.next()
        && second.to_ascii_lowercase().starts_with("rcmake")
        && !first.to_ascii_lowercase().contains("rcmake")
    {
        return format!("{first} ({second})");
    }
    first.to_owned()
}

fn extract_cmake_semver(banner: &str) -> Option<String> {
    for line in banner.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("cmake version ") {
            let token = rest.split_whitespace().next()?;
            if !token.is_empty() {
                return Some(token.to_owned());
            }
        }
    }
    None
}

fn run_configure(
    host: &dyn Host,
    exe: &Path,
    args: &[BString],
    cwd: &Path,
    env: &[(BString, BString)],
    deadline: &Deadline,
) -> ConfigureOutcome {
    let Some(timeout) = deadline.remaining(host) else {
        return ConfigureOutcome {
            status: ProcessOutcome::TimedOut,
            elapsed_ns: 0,
            stderr_tail: "Overall verify deadline exceeded before configure started.".to_owned(),
        };
    };
    let req = RunRequest {
        pipeline: vec![CommandSpec {
            program: path::from_native(exe),
            args: args.to_vec(),
        }],
        cwd: cwd.to_path_buf(),
        env: env.to_vec(),
        stdin: Input::Null,
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: Some(timeout),
        tee: false,
        default_error_mode: false,
    };
    let start = host.monotonic();
    let result = host.run_bounded(&req, MAX_CONFIGURE_OUTPUT_BYTES);
    let elapsed = host.monotonic().saturating_sub(start);
    let elapsed_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);

    match result {
        Ok(out) => {
            let status = match out.results.first() {
                Some(ProcessResult::Exited(code)) => ProcessOutcome::Exited(*code),
                Some(ProcessResult::Signaled(sig)) => ProcessOutcome::Signaled(*sig),
                Some(ProcessResult::TimedOut) => ProcessOutcome::TimedOut,
                Some(ProcessResult::SpawnFailed(err)) => {
                    ProcessOutcome::SpawnFailed(err.to_string())
                }
                None => ProcessOutcome::SpawnFailed("no process exit status".to_owned()),
            };
            ConfigureOutcome {
                status,
                elapsed_ns,
                stderr_tail: bounded_tail(&out.stderr),
            }
        }
        Err(err) => ConfigureOutcome {
            status: ProcessOutcome::SpawnFailed(err.to_string()),
            elapsed_ns,
            stderr_tail: err.to_string(),
        },
    }
}
