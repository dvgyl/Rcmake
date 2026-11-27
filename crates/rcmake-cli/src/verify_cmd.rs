//! Native `rcmake verify` subcommand.
//!
//! Parses and validates a configure-only verification request, rewrites source-relative
//! read paths, rejects non-configure actions and unsupported output redirects before
//! launch, and delegates execution to the sibling `rcmake-verify` companion binary.
//! This module never discovers or executes upstream `cmake`.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_buildinfo::{ArtifactReader, ReadLimits, parse_cache};
use rcmake_host::{FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path};
use rcmake_json::{Value, write_compact};
use rcmake_presets::{EnvSnapshot, PresetKind, PresetSettings};

use crate::options::split_flag;
use crate::resolve_cli_path;

/// Default wall-clock timeout in seconds for the full verify workflow.
const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 600;
/// Extra grace seconds granted to the launcher above the companion's overall deadline so the
/// companion can finish cleanly and emit its structured report.
const VERIFY_LAUNCHER_GRACE_SECS: u64 = 60;

const HELP: &str = "\
Usage: rcmake verify [--json] [--timeout <secs>] [--oracle <path>] -- <configure arguments>

Run both rcmake (in compatibility mode) and an explicit upstream CMake binary
against identical staged copies of a project, then compare File API codemodel,
CMakeCache.txt, compile_commands.json, and Ninja dry-run output.

Options:
  --oracle <path>   Path to the reference CMake 4.5.1 executable
                    (optional when the companion reads the oracle environment variable).
  --timeout <secs>  Overall timeout in seconds (default: 600; env: RCMAKE_VERIFY_TIMEOUT).
  --json            Write one machine-readable JSON report to standard output.
  -h, --help        Show this help.

Notes:
  verify stages a pristine copy of the source tree into a private scratch
  directory and never writes to the caller's source or build directory.
  Configure project code executes twice (once per engine).
  Progress phase messages are written to standard error; standard output
  contains only the final report (pure JSON when --json is set).

Exit status:
  0  Parity across all applicable sections (with CMake 4.5.1).
  1  Observed divergence between rcmake and the oracle.
  2  Invalid input, missing companion, matched failure, or incomplete evidence.
";

#[derive(Debug)]
struct VerifyCli {
    oracle: Option<BString>,
    timeout_secs: u64,
    json: bool,
    configure_args: Vec<BString>,
}

#[derive(Debug)]
enum CliAction {
    Help,
    Run(VerifyCli),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathArg {
    SourceRel(BString),
    ExternalAbsolute(BString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DefineValue {
    Literal(BString),
    SourceRel(BString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TraceSourceArg {
    SourceRel(BString),
    ExternalAbsolute(BString),
    RelativeName(BString),
}

/// Typed configure-only option serialized to `rcmake-verify`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfigureOption {
    Generator(BString),
    Architecture(BString),
    Toolset(BString),
    Preset(BString),
    PresetsFile(PathArg),
    PreloadScript(PathArg),
    Toolchain(PathArg),
    InstallPrefix(BString),
    Define {
        name: BString,
        ty: Option<BString>,
        value: DefineValue,
    },
    Undefine(BString),
    Flag(&'static str),
    Warning(BString),
    LogLevel(BString),
    DebugFindPkg(BString),
    DebugFindVar(BString),
    TraceFormat(BString),
    TraceSource(TraceSourceArg),
    ProjectFile(BString),
}

#[derive(Debug)]
struct TypedVerifyRequest {
    rcmake_exe: PathBuf,
    oracle_exe: Option<PathBuf>,
    source_dir: PathBuf,
    excluded_build_dir: Option<PathBuf>,
    cwd_rel_to_source: Option<BString>,
    args: Vec<ConfigureOption>,
    json: bool,
    timeout_secs: u64,
    request_compile_commands: bool,
    compile_commands_user_disabled: bool,
    generator_hint: Option<BString>,
    external_inputs: Vec<BString>,
}

fn wants_json_output(args: &[BString]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_slice() != b"--")
        .any(|arg| arg.as_slice() == b"--json")
}

/// Execute `rcmake verify`.
pub(crate) fn run(argv0: Option<&BString>, args: &[BString], host: Arc<dyn Host>) -> i32 {
    let json_requested = wants_json_output(args);
    let cli = match parse_verify_cli(host.as_ref(), args) {
        Ok(CliAction::Help) => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        Ok(CliAction::Run(cli)) => cli,
        Err(message) => return emit_error(host.as_ref(), json_requested, None, None, &message),
    };

    let cli_oracle_disp = cli
        .oracle
        .as_deref()
        .map(|raw| String::from_utf8_lossy(raw).into_owned());
    let (rcmake_exe, companion) = match locate_executables(host.as_ref(), argv0) {
        Ok(pair) => pair,
        Err(message) => {
            return emit_error(
                host.as_ref(),
                cli.json,
                None,
                cli_oracle_disp.as_deref(),
                &message,
            );
        }
    };

    let oracle_exe = match &cli.oracle {
        Some(raw) => match resolve_oracle_path(host.as_ref(), raw) {
            Ok(path) => Some(path),
            Err(message) => {
                return emit_error(
                    host.as_ref(),
                    cli.json,
                    Some(&rcmake_exe.display().to_string()),
                    cli_oracle_disp.as_deref(),
                    &message,
                );
            }
        },
        None => None,
    };

    let rcmake_disp = rcmake_exe.display().to_string();
    let oracle_disp = oracle_exe.as_ref().map(|path| path.display().to_string());
    let request = match build_typed_request(
        host.as_ref(),
        rcmake_exe,
        oracle_exe,
        cli.json,
        cli.timeout_secs,
        &cli.configure_args,
    ) {
        Ok(req) => req,
        Err(message) => {
            return emit_error(
                host.as_ref(),
                cli.json,
                Some(&rcmake_disp),
                oracle_disp.as_deref(),
                &message,
            );
        }
    };

    let payload = serialize_request(&request);
    let launcher_timeout =
        Duration::from_secs(cli.timeout_secs.saturating_add(VERIFY_LAUNCHER_GRACE_SECS));
    let run_req = RunRequest {
        pipeline: vec![rcmake_host::CommandSpec {
            program: path::from_native(&companion),
            args: vec![BString::from("--request-stdin")],
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Bytes(payload.into()),
        stdout: Output::Capture,
        stderr: Output::Stream,
        timeout: Some(launcher_timeout),
        tee: false,
    };

    let host_ref = host.as_ref();
    match host.run(&run_req, &mut |stream, chunk| {
        if stream == Stream::Stderr && !chunk.is_empty() {
            host_ref.write_out(Stream::Stderr, chunk);
        }
    }) {
        Ok(output) => {
            if !output.stdout.is_empty() {
                host.write_out(Stream::Stdout, &output.stdout);
            }
            if !output.stderr.is_empty() {
                host.write_out(Stream::Stderr, &output.stderr);
            }
            let emitted_stdout = !output.stdout.is_empty();
            match output.results.first() {
                Some(ProcessResult::Exited(code)) => *code,
                Some(ProcessResult::Signaled(sig)) => emit_error(
                    host.as_ref(),
                    cli.json && !emitted_stdout,
                    Some(&rcmake_disp),
                    oracle_disp.as_deref(),
                    &format!("rcmake-verify was terminated by signal {sig}."),
                ),
                Some(ProcessResult::TimedOut) => emit_error(
                    host.as_ref(),
                    cli.json && !emitted_stdout,
                    Some(&rcmake_disp),
                    oracle_disp.as_deref(),
                    &format!(
                        "rcmake-verify timed out after {} seconds.",
                        cli.timeout_secs
                    ),
                ),
                Some(ProcessResult::SpawnFailed(err)) => emit_error(
                    host.as_ref(),
                    cli.json && !emitted_stdout,
                    Some(&rcmake_disp),
                    oracle_disp.as_deref(),
                    &format!("Could not start {}: {err}", companion.display()),
                ),
                None => emit_error(
                    host.as_ref(),
                    cli.json && !emitted_stdout,
                    Some(&rcmake_disp),
                    oracle_disp.as_deref(),
                    "rcmake-verify produced no exit status.",
                ),
            }
        }
        Err(err) => emit_error(
            host.as_ref(),
            cli.json,
            Some(&rcmake_disp),
            oracle_disp.as_deref(),
            &format!("Could not execute {}: {err}", companion.display()),
        ),
    }
}

fn emit_error(
    host: &dyn Host,
    json: bool,
    rcmake_exe: Option<&str>,
    oracle_exe: Option<&str>,
    message: &str,
) -> i32 {
    if json {
        let skipped = || {
            let mut sec = BTreeMap::new();
            sec.insert(BString::from("status"), Value::string("skipped"));
            sec.insert(BString::from("compared"), Value::UInt(0));
            sec.insert(BString::from("differences"), Value::UInt(0));
            sec.insert(BString::from("examples"), Value::Array(Vec::new()));
            sec.insert(
                BString::from("notes"),
                Value::Array(vec![Value::string("Not executed due to invalid input.")]),
            );
            Value::Object(sec)
        };
        let mut oracle_obj = BTreeMap::new();
        oracle_obj.insert(
            BString::from("executable"),
            Value::string(oracle_exe.unwrap_or("")),
        );
        oracle_obj.insert(BString::from("version"), Value::string("unknown"));
        oracle_obj.insert(
            BString::from("expected_version"),
            Value::string(rcmake_base::CMAKE_VERSION_STR),
        );

        let mut rcmake_obj = BTreeMap::new();
        rcmake_obj.insert(
            BString::from("executable"),
            Value::string(rcmake_exe.unwrap_or("")),
        );
        rcmake_obj.insert(BString::from("version"), Value::string("unknown"));
        rcmake_obj.insert(BString::from("persona"), Value::string("compat"));

        let mut engines = BTreeMap::new();
        engines.insert(BString::from("oracle"), Value::Object(oracle_obj));
        engines.insert(BString::from("rcmake"), Value::Object(rcmake_obj));

        let mut scope = BTreeMap::new();
        scope.insert(BString::from("generator"), Value::Null);
        scope.insert(
            BString::from("compile_commands_injected"),
            Value::Bool(false),
        );
        scope.insert(BString::from("external_inputs"), Value::Array(Vec::new()));
        scope.insert(
            BString::from("volatile_cache_exceptions"),
            Value::Array(Vec::new()),
        );

        let mut timings = BTreeMap::new();
        timings.insert(
            BString::from("observation"),
            Value::string("single_paired_observation"),
        );
        timings.insert(
            BString::from("run_order"),
            Value::Array(vec![Value::string("oracle"), Value::string("rcmake")]),
        );
        timings.insert(BString::from("oracle_ms"), Value::UInt(0));
        timings.insert(BString::from("rcmake_ms"), Value::UInt(0));
        timings.insert(BString::from("speedup"), Value::Null);

        let mut sections = BTreeMap::new();
        sections.insert(BString::from("configure"), skipped());
        sections.insert(BString::from("codemodel"), skipped());
        sections.insert(BString::from("cache"), skipped());
        sections.insert(BString::from("compile_commands"), skipped());
        sections.insert(BString::from("ninja_dry_run"), skipped());

        let mut root = BTreeMap::new();
        root.insert(BString::from("schema_version"), Value::UInt(1));
        root.insert(BString::from("verdict"), Value::string("invalid_input"));
        root.insert(BString::from("exit_code"), Value::UInt(2));
        root.insert(BString::from("engines"), Value::Object(engines));
        root.insert(BString::from("scope"), Value::Object(scope));
        root.insert(BString::from("timings"), Value::Object(timings));
        root.insert(BString::from("sections"), Value::Object(sections));
        root.insert(BString::from("error"), Value::string(message));

        let mut out = rcmake_json::write_styled(&Value::Object(root), "  ");
        out.push(b'\n');
        host.write_out(Stream::Stdout, out.as_slice());
    } else {
        host.write_out(
            Stream::Stderr,
            format!("rcmake verify: {message}\n").as_bytes(),
        );
    }
    2
}

fn parse_timeout_secs(raw: &[u8], source: &str) -> Result<u64, String> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| format!("Invalid {source}: expected a positive integer number of seconds."))?;
    let secs = text.trim().parse::<u64>().map_err(|_| {
        format!("Invalid {source} '{text}': expected a positive integer number of seconds.")
    })?;
    if secs == 0 {
        return Err(format!(
            "Invalid {source} '0': timeout must be at least 1 second."
        ));
    }
    Ok(secs)
}

fn parse_verify_cli(host: &dyn Host, args: &[BString]) -> Result<CliAction, String> {
    let mut oracle: Option<BString> = None;
    let mut timeout_secs: Option<u64> = None;
    let mut json = false;
    let mut configure_args = Vec::new();
    let mut iter = args.iter().peekable();
    let mut after_sep = false;

    while let Some(arg) = iter.next() {
        if after_sep {
            configure_args.push(arg.clone());
            continue;
        }
        let raw = arg.as_slice();
        if raw == b"--" {
            after_sep = true;
            continue;
        }
        if raw == b"-h" || raw == b"--help" {
            return Ok(CliAction::Help);
        }
        let (flag, inline) = if raw.starts_with(b"--") {
            split_flag(raw, b'=')
        } else {
            (raw, None)
        };
        match flag {
            b"--json" => {
                if inline.is_some() {
                    return Err("Use --json without a value.".into());
                }
                json = true;
            }
            b"--oracle" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().map(|v| v.as_slice()),
                };
                let value = value
                    .filter(|v| !v.is_empty() && *v != b"--")
                    .ok_or_else(|| "Supply a path for --oracle.".to_owned())?;
                if oracle.is_some() {
                    return Err("Specify --oracle only once.".into());
                }
                oracle = Some(BString::from(value));
            }
            b"--timeout" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().map(|v| v.as_slice()),
                };
                let value = value
                    .filter(|v| !v.is_empty() && *v != b"--")
                    .ok_or_else(|| {
                        "Supply a positive integer in seconds for --timeout.".to_owned()
                    })?;
                if timeout_secs.is_some() {
                    return Err("Specify --timeout only once.".into());
                }
                timeout_secs = Some(parse_timeout_secs(value, "--timeout")?);
            }
            _ if raw.starts_with(b"--") && !is_known_cmake_long_flag(flag) => {
                return Err(format!(
                    "Unknown verify option {}. Pass configure arguments after '--'.",
                    String::from_utf8_lossy(raw)
                ));
            }
            _ => {
                // Allow configure arguments directly if `--` was omitted after verify flags.
                configure_args.push(arg.clone());
                configure_args.extend(iter.cloned());
                break;
            }
        }
    }

    let timeout_secs = match timeout_secs {
        Some(secs) => secs,
        None => match host
            .env_get(b"RCMAKE_VERIFY_TIMEOUT")
            .filter(|v| !v.is_empty())
        {
            Some(env_val) => parse_timeout_secs(&env_val, "RCMAKE_VERIFY_TIMEOUT")?,
            None => DEFAULT_VERIFY_TIMEOUT_SECS,
        },
    };
    Ok(CliAction::Run(VerifyCli {
        oracle,
        timeout_secs,
        json,
        configure_args,
    }))
}

fn is_known_cmake_long_flag(flag: &[u8]) -> bool {
    matches!(
        flag,
        b"--fresh"
            | b"--toolchain"
            | b"--install-prefix"
            | b"--preset"
            | b"--presets-file"
            | b"--project-file"
            | b"--warn-uninitialized"
            | b"--warn-unused-vars"
            | b"--no-warn-unused-cli"
            | b"--check-system-vars"
            | b"--compile-no-warning-as-error"
            | b"--link-no-warning-as-error"
            | b"--debug-output"
            | b"--debug-trycompile"
            | b"--debug-find"
            | b"--debug-find-pkg"
            | b"--debug-find-var"
            | b"--log-level"
            | b"--loglevel"
            | b"--log-context"
            | b"--trace"
            | b"--trace-expand"
            | b"--trace-format"
            | b"--trace-source"
            | b"--trace-redirect"
            | b"--graphviz"
            | b"--sarif-output"
            | b"--profiling-format"
            | b"--profiling-output"
            | b"--build"
            | b"--install"
            | b"--open"
            | b"--workflow"
            | b"--find-package"
            | b"--system-information"
            | b"--list-presets"
            | b"--debugger"
            | b"--debugger-pipe"
            | b"--debugger-dap-log"
    )
}

fn locate_executables(
    host: &dyn Host,
    argv0: Option<&BString>,
) -> Result<(PathBuf, PathBuf), String> {
    let exe = host
        .current_exe()
        .ok()
        .or_else(|| argv0.map(|a| resolve_cli_path(host, a)))
        .ok_or_else(|| "Could not determine the rcmake executable path.".to_owned())?;
    let companion_name = if host.system().sysname.eq_ignore_ascii_case(b"Windows") {
        "rcmake-verify.exe"
    } else {
        "rcmake-verify"
    };
    let mut candidates = vec![exe.with_file_name(companion_name)];
    if let Ok(canonical) = host.canonicalize(&exe) {
        let sibling = canonical.with_file_name(companion_name);
        if !candidates.contains(&sibling) {
            candidates.push(sibling);
        }
    }
    for candidate in &candidates {
        if host
            .metadata(candidate, Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::File)
        {
            return Ok((exe, candidate.clone()));
        }
    }
    Err(format!(
        "Companion binary '{companion_name}' was not found next to {}. Reinstall rcmake from a complete release bundle that includes {companion_name} (from a source checkout, build it with: cargo build -p rcmake-harness --bin rcmake-verify).",
        exe.display()
    ))
}

fn resolve_oracle_path(host: &dyn Host, raw: &[u8]) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("Supply a non-empty path for --oracle.".into());
    }
    if raw.contains(&b'/') || raw.contains(&b'\\') {
        return Ok(resolve_cli_path(host, raw));
    }
    // Search PATH when a bare executable name is supplied.
    let windows = host.system().sysname.eq_ignore_ascii_case(b"Windows");
    let sep = if windows { b';' } else { b':' };
    if let Some(path_var) = host.env_get(b"PATH") {
        let name = String::from_utf8_lossy(raw);
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
    Ok(resolve_cli_path(host, raw))
}

#[derive(Clone, Debug)]
enum ParsedDir {
    Source(PathBuf),
    Binary(PathBuf),
    Positional(PathBuf),
}

#[derive(Clone, Debug)]
enum ParsedForward {
    Generator(BString),
    Architecture(BString),
    Toolset(BString),
    Preset(BString),
    PreloadScript(PathBuf),
    ToolchainFlag(PathBuf),
    InstallPrefixFlag(PathBuf),
    PresetsFileFlag(PathBuf),
    Define {
        name: BString,
        ty: Option<BString>,
        value: BString,
    },
    Undefine(BString),
    Flag(&'static str),
    Warning(BString),
    LogLevel(BString),
    DebugFindPkg(BString),
    DebugFindVar(BString),
    TraceFormat(BString),
    TraceSource(BString),
    ProjectFile(BString),
}

fn build_typed_request(
    host: &dyn Host,
    rcmake_exe: PathBuf,
    oracle_exe: Option<PathBuf>,
    json: bool,
    timeout_secs: u64,
    configure_args: &[BString],
) -> Result<TypedVerifyRequest, String> {
    let mut dirs = Vec::new();
    let mut forwarded = Vec::new();
    let mut preset: Option<BString> = None;
    let mut presets_file: Option<PathBuf> = None;
    let mut generator_hint: Option<BString> = None;
    let mut explicit_compdb_state: Option<bool> = None;
    let mut seen_project_file = false;
    let mut iter = configure_args.iter().peekable();

    // Reject leading non-configure subcommands or mode flags immediately.
    if let Some(first) = configure_args.first()
        && matches!(
            first.as_slice(),
            b"fmt"
                | b"lint"
                | b"verify"
                | b"doctor"
                | b"cache"
                | b"explain"
                | b"diff-config"
                | b"ctest"
                | b"cpack"
                | b"ccmake"
                | b"lsp"
        )
        && !host
            .metadata(&resolve_cli_path(host, first), Follow::Yes)
            .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return Err(format!(
            "Unsupported action '{}' for verify. Only project configure invocations are supported.",
            String::from_utf8_lossy(first)
        ));
    }

    while let Some(raw) = iter.next() {
        let bytes = raw.as_slice();
        if bytes.is_empty() {
            continue;
        }
        if !bytes.starts_with(b"-") {
            dirs.push(ParsedDir::Positional(resolve_cli_path(host, bytes)));
            continue;
        }
        let (flag, inline) = if let Some(rest) = bytes.strip_prefix(b"--install-prefix ") {
            (
                b"--install-prefix".as_slice(),
                Some(rest.trim_ascii_start()),
            )
        } else if bytes.starts_with(b"--") {
            split_flag(bytes, b'=')
        } else {
            let (f, tail) = bytes.split_at(bytes.len().min(2));
            (f, (!tail.is_empty()).then_some(tail))
        };

        // Reject non-configure actions and unsupported output/path redirects before launch.
        if let Some(reason) = rejected_configure_option(bytes, flag) {
            return Err(reason);
        }

        let mut take_req = |missing: &str| -> Result<&[u8], String> {
            inline
                .or_else(|| {
                    iter.next_if(|next| !next.starts_with(b"-"))
                        .map(|v| v.as_slice())
                })
                .filter(|v| !v.is_empty())
                .ok_or_else(|| missing.to_owned())
        };

        match flag {
            b"-S" | b"-H" => {
                let val = take_req("No source directory specified for -S.")?;
                dirs.push(ParsedDir::Source(resolve_cli_path(host, val)));
            }
            b"-B" => {
                let val = take_req("No build directory specified for -B.")?;
                dirs.push(ParsedDir::Binary(resolve_cli_path(host, val)));
            }
            b"-G" => {
                let val = take_req("No generator specified for -G.")?;
                generator_hint = Some(BString::from(val));
                forwarded.push(ParsedForward::Generator(BString::from(val)));
            }
            b"-A" => {
                let val = take_req("Missing value for -A.")?;
                forwarded.push(ParsedForward::Architecture(BString::from(val)));
            }
            b"-T" => {
                let val = take_req("Missing value for -T.")?;
                forwarded.push(ParsedForward::Toolset(BString::from(val)));
            }
            b"-C" => {
                let val = match inline {
                    Some(tail) => tail.strip_prefix(b"=").unwrap_or(tail).trim_ascii_start(),
                    None => iter
                        .next_if(|next| !next.starts_with(b"-"))
                        .map(|v| v.as_slice())
                        .ok_or_else(|| "-C must be followed by a file name.".to_owned())?,
                };
                if val.is_empty() {
                    return Err("No file name specified for -C.".into());
                }
                forwarded.push(ParsedForward::PreloadScript(resolve_cli_path(host, val)));
            }
            b"--toolchain" => {
                let val = take_req("No file specified for --toolchain.")?;
                forwarded.push(ParsedForward::ToolchainFlag(resolve_cli_path(host, val)));
            }
            b"--install-prefix" => {
                let val = take_req("No directory specified for --install-prefix.")?;
                forwarded.push(ParsedForward::InstallPrefixFlag(resolve_cli_path(
                    host, val,
                )));
            }
            b"--preset" => {
                let val = take_req("No preset specified for --preset.")?;
                preset = Some(BString::from(val));
                forwarded.push(ParsedForward::Preset(BString::from(val)));
            }
            b"--presets-file" => {
                let val = take_req("No file specified for --presets-file.")?;
                let resolved = resolve_cli_path(host, val);
                presets_file = Some(resolved.clone());
                forwarded.push(ParsedForward::PresetsFileFlag(resolved));
            }
            b"-D" => {
                let raw_def = inline
                    .or_else(|| iter.next().map(|v| v.as_slice()))
                    .ok_or_else(|| "-D must be followed with VAR=VALUE.".to_owned())?;
                let (name, ty, value) = split_define(raw_def)?;
                if name.as_slice() == b"CMAKE_EXPORT_COMPILE_COMMANDS" {
                    explicit_compdb_state = Some(truth::is_on(&value));
                }
                forwarded.push(ParsedForward::Define { name, ty, value });
            }
            b"-U" => {
                let pat = take_req("-U must be followed with VAR.")?;
                if pat == b"CMAKE_EXPORT_COMPILE_COMMANDS" {
                    explicit_compdb_state = None;
                }
                forwarded.push(ParsedForward::Undefine(BString::from(pat)));
            }
            b"--fresh"
            | b"--warn-uninitialized"
            | b"--warn-unused-vars"
            | b"--no-warn-unused-cli"
            | b"--check-system-vars"
            | b"--compile-no-warning-as-error"
            | b"--link-no-warning-as-error"
            | b"--debug-output"
            | b"--debug-trycompile"
            | b"--debug-find"
            | b"--log-context"
            | b"--trace"
            | b"--trace-expand" => {
                if inline.is_some() {
                    return Err(format!(
                        "Option {} does not take a value.",
                        String::from_utf8_lossy(flag)
                    ));
                }
                let wire_name = simple_flag_wire_name(flag).ok_or_else(|| {
                    format!(
                        "Unsupported configure flag '{}' for verify.",
                        String::from_utf8_lossy(flag)
                    )
                })?;
                forwarded.push(ParsedForward::Flag(wire_name));
            }
            b"-W" => {
                let val = take_req("-W must be followed with [no-]<name>.")?;
                validate_warning_value(val)?;
                forwarded.push(ParsedForward::Warning(BString::from(val)));
            }
            b"--log-level" | b"--loglevel" => {
                let val = take_req("Missing value for --log-level.")?;
                validate_log_level(val)?;
                forwarded.push(ParsedForward::LogLevel(BString::from(val)));
            }
            b"--debug-find-pkg" => {
                let val = take_req("Missing value for --debug-find-pkg.")?;
                if val.contains(&b'/') || val.contains(&b'\\') {
                    return Err("--debug-find-pkg must not contain path separators.".into());
                }
                forwarded.push(ParsedForward::DebugFindPkg(BString::from(val)));
            }
            b"--debug-find-var" => {
                let val = take_req("Missing value for --debug-find-var.")?;
                if val.contains(&b'/') || val.contains(&b'\\') {
                    return Err("--debug-find-var must not contain path separators.".into());
                }
                forwarded.push(ParsedForward::DebugFindVar(BString::from(val)));
            }
            b"--trace-format" => {
                let val = take_req("Missing value for --trace-format.")?;
                if !matches!(val, b"human" | b"json-v1") {
                    return Err(format!(
                        "Invalid --trace-format '{}': expected 'human' or 'json-v1'.",
                        String::from_utf8_lossy(val)
                    ));
                }
                forwarded.push(ParsedForward::TraceFormat(BString::from(val)));
            }
            b"--trace-source" => {
                let val = take_req("Missing value for --trace-source.")?;
                if val.contains(&0) {
                    return Err("--trace-source must not contain NUL bytes.".into());
                }
                forwarded.push(ParsedForward::TraceSource(BString::from(val)));
            }
            b"--project-file" => {
                let val = take_req("Missing value for --project-file.")?;
                if seen_project_file {
                    return Err("Specify --project-file only once.".into());
                }
                validate_plain_filename(val)?;
                seen_project_file = true;
                forwarded.push(ParsedForward::ProjectFile(BString::from(val)));
            }
            _ => {
                return Err(format!(
                    "Unsupported or unclassified configure option '{}' for verify.",
                    String::from_utf8_lossy(bytes)
                ));
            }
        }
    }

    // Resolve source and excluded binary directories relative to caller's cwd (never write to them).
    let (source_dir, mut excluded_build_dir) = resolve_verify_dirs(host, &dirs)?;

    // Inspect configure preset (if given) before staging to resolve effective binaryDir,
    // generator, export flag, and reject preset output redirects (graphviz, trace.redirect).
    if let Some(preset_name) = &preset {
        let presets = match presets_file.as_deref() {
            Some(file) => rcmake_presets::load_file(&source_dir, file, host),
            None => rcmake_presets::load(&source_dir, host),
        }
        .map_err(|err| {
            format!(
                "Could not read presets from {}: {}",
                source_dir.display(),
                err.message
            )
        })?;
        let env = EnvSnapshot::from_host(host);
        let resolved = presets
            .resolve(PresetKind::Configure, preset_name, &env)
            .map_err(|err| err.message)?;
        if generator_hint.is_none() {
            generator_hint = resolved.generator;
        }
        if excluded_build_dir.is_none()
            && let Some(bin) = resolved.binary_dir
        {
            let native = path::to_native(&bin);
            excluded_build_dir = Some(if is_host_absolute(host, &bin) || native.is_absolute() {
                native
            } else {
                source_dir.join(native)
            });
        }
        if let PresetSettings::Configure(cfg) = resolved.settings {
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
            if explicit_compdb_state.is_none()
                && let Some(vars) = &cfg.cache_variables
                && let Some(entry) = vars.get(b"CMAKE_EXPORT_COMPILE_COMMANDS".as_slice())
            {
                explicit_compdb_state = Some(truth::is_on(&entry.value));
            }
        }
    }

    let source_canonical = host.canonicalize(&source_dir).map_err(|err| {
        format!(
            "Source directory {} is unreadable: {err}",
            source_dir.display()
        )
    })?;
    if !host
        .metadata(&source_canonical, Follow::Yes)
        .is_ok_and(|meta| meta.kind == FileKind::Dir)
    {
        return Err(format!(
            "Source path {} is not a directory.",
            source_dir.display()
        ));
    }

    let cwd_rel_to_source = host.canonicalize(&host.initial_cwd()).ok().and_then(|cwd| {
        cwd.strip_prefix(&source_canonical)
            .ok()
            .map(path::from_native_cmake)
    });

    // Classify and rewrite path-bearing read options against staged source, recording external inputs.
    let mut external_inputs = Vec::new();
    let mut args = Vec::new();
    for item in forwarded {
        match item {
            ParsedForward::Generator(v) => args.push(ConfigureOption::Generator(v)),
            ParsedForward::Architecture(v) => args.push(ConfigureOption::Architecture(v)),
            ParsedForward::Toolset(v) => args.push(ConfigureOption::Toolset(v)),
            ParsedForward::Preset(v) => args.push(ConfigureOption::Preset(v)),
            ParsedForward::Undefine(pat) => args.push(ConfigureOption::Undefine(pat)),
            ParsedForward::Flag(name) => args.push(ConfigureOption::Flag(name)),
            ParsedForward::Warning(w) => args.push(ConfigureOption::Warning(w)),
            ParsedForward::LogLevel(l) => args.push(ConfigureOption::LogLevel(l)),
            ParsedForward::DebugFindPkg(p) => args.push(ConfigureOption::DebugFindPkg(p)),
            ParsedForward::DebugFindVar(v) => args.push(ConfigureOption::DebugFindVar(v)),
            ParsedForward::TraceFormat(f) => args.push(ConfigureOption::TraceFormat(f)),
            ParsedForward::ProjectFile(name) => args.push(ConfigureOption::ProjectFile(name)),
            ParsedForward::PreloadScript(script) => {
                args.push(ConfigureOption::PreloadScript(classify_path_arg(
                    host,
                    &source_canonical,
                    &script,
                    &mut external_inputs,
                )));
            }
            ParsedForward::ToolchainFlag(tc) => {
                args.push(ConfigureOption::Toolchain(classify_path_arg(
                    host,
                    &source_canonical,
                    &tc,
                    &mut external_inputs,
                )));
            }
            ParsedForward::PresetsFileFlag(file) => {
                args.push(ConfigureOption::PresetsFile(classify_path_arg(
                    host,
                    &source_canonical,
                    &file,
                    &mut external_inputs,
                )));
            }
            ParsedForward::InstallPrefixFlag(dir) => {
                args.push(ConfigureOption::InstallPrefix(path::from_native_cmake(
                    &dir,
                )));
            }
            ParsedForward::TraceSource(raw) => {
                let classified =
                    classify_trace_source(host, &source_canonical, &raw, &mut external_inputs)?;
                args.push(ConfigureOption::TraceSource(classified));
            }
            ParsedForward::Define { name, ty, value } => {
                let explicit_path_var = matches!(
                    name.as_slice(),
                    b"CMAKE_TOOLCHAIN_FILE"
                        | b"CMAKE_PROJECT_INCLUDE"
                        | b"CMAKE_PROJECT_INCLUDE_BEFORE"
                        | b"CMAKE_PROJECT_TOP_LEVEL_INCLUDES"
                        | b"CMAKE_USER_MAKE_RULES_OVERRIDE"
                );
                let typed_path = ty.as_deref().is_some_and(|t| {
                    t.eq_ignore_ascii_case(b"PATH") || t.eq_ignore_ascii_case(b"FILEPATH")
                });
                let single_path_like = !value.is_empty()
                    && !value.contains(&b';')
                    && (explicit_path_var || typed_path);
                let typed_value = if single_path_like {
                    let resolved = resolve_setup_candidate(host, &source_canonical, &value);
                    if let Some(rel) =
                        relative_under(host, &source_canonical, &resolved).filter(|r| !r.is_empty())
                    {
                        DefineValue::SourceRel(rel)
                    } else if name.as_slice() == b"CMAKE_TOOLCHAIN_FILE" || typed_path {
                        let abs = path::from_native_cmake(&resolved);
                        if name.as_slice() == b"CMAKE_TOOLCHAIN_FILE" {
                            external_inputs.push(abs.clone());
                        }
                        DefineValue::Literal(abs)
                    } else {
                        DefineValue::Literal(value)
                    }
                } else {
                    DefineValue::Literal(value)
                };
                args.push(ConfigureOption::Define {
                    name,
                    ty,
                    value: typed_value,
                });
            }
        }
    }

    let compile_commands_user_disabled = explicit_compdb_state == Some(false);
    let request_compile_commands = explicit_compdb_state.is_none();

    Ok(TypedVerifyRequest {
        rcmake_exe,
        oracle_exe,
        source_dir: source_canonical,
        excluded_build_dir,
        cwd_rel_to_source,
        args,
        json,
        timeout_secs,
        request_compile_commands,
        compile_commands_user_disabled,
        generator_hint,
        external_inputs,
    })
}

fn simple_flag_wire_name(flag: &[u8]) -> Option<&'static str> {
    Some(match flag {
        b"--fresh" => "fresh",
        b"--warn-uninitialized" => "warn-uninitialized",
        b"--warn-unused-vars" => "warn-unused-vars",
        b"--no-warn-unused-cli" => "no-warn-unused-cli",
        b"--check-system-vars" => "check-system-vars",
        b"--compile-no-warning-as-error" => "compile-no-warning-as-error",
        b"--link-no-warning-as-error" => "link-no-warning-as-error",
        b"--debug-output" => "debug-output",
        b"--debug-trycompile" => "debug-trycompile",
        b"--debug-find" => "debug-find",
        b"--log-context" => "log-context",
        b"--trace" => "trace",
        b"--trace-expand" => "trace-expand",
        _ => return None,
    })
}

fn validate_warning_value(val: &[u8]) -> Result<(), String> {
    let rest = val.strip_prefix(b"no-").unwrap_or(val);
    let name = rest.strip_prefix(b"error=").unwrap_or(rest);
    if !matches!(name, b"dev" | b"deprecated") {
        return Err(format!(
            "Unsupported warning flag '-W{}' for verify.",
            String::from_utf8_lossy(val)
        ));
    }
    Ok(())
}

fn validate_log_level(val: &[u8]) -> Result<(), String> {
    let upper = val.to_ascii_uppercase();
    if !matches!(
        upper.as_slice(),
        b"ERROR" | b"WARNING" | b"NOTICE" | b"STATUS" | b"VERBOSE" | b"DEBUG" | b"TRACE"
    ) {
        return Err(format!(
            "Invalid --log-level '{}': expected ERROR, WARNING, NOTICE, STATUS, VERBOSE, DEBUG, or TRACE.",
            String::from_utf8_lossy(val)
        ));
    }
    Ok(())
}

fn validate_plain_filename(name: &[u8]) -> Result<(), String> {
    if name.is_empty()
        || name == b"."
        || name == b".."
        || name.contains(&0)
        || name.contains(&b'/')
        || name.contains(&b'\\')
        || name.contains(&b':')
    {
        return Err(format!(
            "Option '--project-file' value '{}' must be a plain file name with no path separators.",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

fn is_host_absolute(host: &dyn Host, raw: &[u8]) -> bool {
    if raw.is_empty() || raw.contains(&0) {
        return false;
    }
    if host.system().sysname.eq_ignore_ascii_case(b"Windows") {
        let fwd: Vec<u8> = raw
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect();
        Lexical::windows(&fwd).absolute()
    } else {
        raw.starts_with(b"/")
    }
}

fn classify_trace_source(
    host: &dyn Host,
    source_root: &Path,
    raw: &[u8],
    external_inputs: &mut Vec<BString>,
) -> Result<TraceSourceArg, String> {
    let is_abs = is_host_absolute(host, raw);
    let has_sep = raw.contains(&b'/') || raw.contains(&b'\\');
    if is_abs || has_sep {
        let resolved = resolve_setup_candidate(host, source_root, raw);
        if let Some(rel) = relative_under(host, source_root, &resolved).filter(|r| !r.is_empty()) {
            return Ok(TraceSourceArg::SourceRel(rel));
        }
        if is_abs || raw.split(|&b| b == b'/' || b == b'\\').any(|p| p == b"..") {
            let abs = path::from_native_cmake(&resolved);
            external_inputs.push(abs.clone());
            return Ok(TraceSourceArg::ExternalAbsolute(abs));
        }
    }
    if raw == b"."
        || raw == b".."
        || matches!(raw, [drive, b':', ..] if drive.is_ascii_alphabetic())
        || raw
            .split(|&b| b == b'/' || b == b'\\')
            .any(|part| part == b"..")
    {
        return Err(format!(
            "Invalid --trace-source filter '{}': relative filter must not escape the source tree.",
            String::from_utf8_lossy(raw)
        ));
    }
    Ok(TraceSourceArg::RelativeName(BString::from(raw)))
}

fn rejected_configure_option(raw: &[u8], flag: &[u8]) -> Option<String> {
    if matches!(
        flag,
        b"-E"
            | b"-P"
            | b"--build"
            | b"--install"
            | b"--open"
            | b"--workflow"
            | b"--find-package"
            | b"--system-information"
            | b"--list-presets"
            | b"-i"
            | b"-N"
            | b"--print-config-dir"
            | b"--version"
            | b"-version"
            | b"--help"
            | b"-h"
            | b"-help"
            | b"--check-build-system"
            | b"--check-stamp-file"
            | b"--check-stamp-list"
            | b"--regenerate-during-build"
    ) || flag.starts_with(b"--help-")
        || matches!(
            raw,
            b"-L" | b"-LA" | b"-LH" | b"-LAH" | b"-LR" | b"-LRA" | b"-LRH" | b"-LRAH"
        )
    {
        return Some(format!(
            "Option '{}' is not a project configure action and is rejected by verify.",
            String::from_utf8_lossy(raw)
        ));
    }
    if matches!(
        flag,
        b"--trace-redirect"
            | b"--profiling-output"
            | b"--profiling-format"
            | b"--sarif-output"
            | b"--graphviz"
            | b"--debugger"
            | b"--debugger-pipe"
            | b"--debugger-dap-log"
    ) {
        return Some(format!(
            "Option '{}' writes outside the verify scratch tree or requires interactive transport and is rejected.",
            String::from_utf8_lossy(flag)
        ));
    }
    if matches!(
        flag,
        b"--hermetic"
            | b"--profile"
            | b"--diagnostics"
            | b"--fetch-lock"
            | b"--no-probe-cache"
            | b"--export-compile-commands"
    ) {
        return Some(format!(
            "Native rcmake extension '{}' cannot be forwarded to verify.",
            String::from_utf8_lossy(flag)
        ));
    }
    None
}

fn split_define(raw: &[u8]) -> Result<(BString, Option<BString>, BString), String> {
    let Some(eq) = raw.iter().position(|&b| b == b'=') else {
        return Err(format!(
            "Invalid -D definition '{}': expected VAR[:TYPE]=VALUE.",
            String::from_utf8_lossy(raw)
        ));
    };
    let lhs = raw.get(..eq).unwrap_or_default();
    let value = BString::from(raw.get(eq + 1..).unwrap_or_default());
    let (name, ty) = match lhs.iter().position(|&b| b == b':') {
        Some(colon) => {
            let ty_bytes = lhs.get(colon + 1..).unwrap_or_default();
            if ty_bytes.is_empty()
                || !ty_bytes
                    .iter()
                    .all(|&b| b.is_ascii_alphabetic() || b == b'_')
            {
                return Err(format!(
                    "Invalid -D cache type in '{}'.",
                    String::from_utf8_lossy(raw)
                ));
            }
            (
                BString::from(lhs.get(..colon).unwrap_or_default()),
                Some(BString::from(ty_bytes)),
            )
        }
        None => (BString::from(lhs), None),
    };
    if name.is_empty() {
        return Err(format!(
            "Invalid -D definition '{}': missing variable name.",
            String::from_utf8_lossy(raw)
        ));
    }
    Ok((name, ty, value))
}

fn resolve_verify_dirs(
    host: &dyn Host,
    args: &[ParsedDir],
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut source: Option<PathBuf> = None;
    let mut binary: Option<PathBuf> = None;
    let explicit_binary = args.iter().any(|a| matches!(a, ParsedDir::Binary(_)));
    for arg in args {
        match arg {
            ParsedDir::Source(p) => source = Some(p.clone()),
            ParsedDir::Binary(p) => binary = Some(p.clone()),
            ParsedDir::Positional(pos) => {
                let dir = match pos.file_name().and_then(|n| n.to_str()) {
                    Some("CMakeCache.txt" | "CMakeLists.txt") => pos.parent().unwrap_or(pos),
                    _ => pos,
                };
                let has_cache = host
                    .metadata(&dir.join("CMakeCache.txt"), Follow::Yes)
                    .is_ok_and(|m| m.kind == FileKind::File);
                let has_lists = host
                    .metadata(&dir.join("CMakeLists.txt"), Follow::Yes)
                    .is_ok_and(|m| m.kind == FileKind::File);
                if has_cache && !explicit_binary {
                    binary = Some(dir.to_path_buf());
                } else if has_lists || source.is_none() {
                    source = Some(dir.to_path_buf());
                } else if !explicit_binary && binary.is_none() {
                    binary = Some(dir.to_path_buf());
                }
            }
        }
    }
    let cwd = host.initial_cwd();
    if source.is_none()
        && let Some(bin_dir) = binary.as_deref().or(Some(cwd.as_path()))
        && let Some(mut reader) = ArtifactReader::new(host, bin_dir, ReadLimits::default())
        && let Ok(bytes) = reader.read(Path::new("CMakeCache.txt"))
    {
        let cache = parse_cache(&bytes);
        if let Some(home) = cache.get(b"CMAKE_HOME_DIRECTORY".as_slice())
            && !home.value.is_empty()
        {
            source = Some(path::to_native(&home.value));
        }
    }
    Ok((source.unwrap_or(cwd), binary))
}

fn resolve_setup_candidate(host: &dyn Host, source_root: &Path, raw: &[u8]) -> PathBuf {
    let native = path::to_native(raw);
    if is_host_absolute(host, raw) || native.is_absolute() {
        return native;
    }
    let from_cwd = resolve_cli_path(host, raw);
    if host.metadata(&from_cwd, Follow::Yes).is_ok() {
        return from_cwd;
    }
    let from_src = source_root.join(&native);
    if host.metadata(&from_src, Follow::Yes).is_ok() {
        return from_src;
    }
    from_cwd
}

fn relative_under(host: &dyn Host, source_root: &Path, candidate: &Path) -> Option<BString> {
    if let Ok(canon) = host.canonicalize(candidate)
        && let Ok(rel) = canon.strip_prefix(source_root)
    {
        return Some(path::from_native_cmake(rel));
    }
    candidate
        .strip_prefix(source_root)
        .ok()
        .map(path::from_native_cmake)
}

fn classify_path_arg(
    host: &dyn Host,
    source_root: &Path,
    candidate: &Path,
    external_inputs: &mut Vec<BString>,
) -> PathArg {
    if let Some(rel) = relative_under(host, source_root, candidate).filter(|r| !r.is_empty()) {
        PathArg::SourceRel(rel)
    } else {
        let abs = path::from_native_cmake(candidate);
        external_inputs.push(abs.clone());
        PathArg::ExternalAbsolute(abs)
    }
}

fn insert_path_arg_fields(obj: &mut BTreeMap<BString, Value>, arg: &PathArg) {
    match arg {
        PathArg::SourceRel(rel) => {
            obj.insert(BString::from("path_kind"), Value::string("source_rel"));
            obj.insert(BString::from("value"), Value::string(rel.clone()));
        }
        PathArg::ExternalAbsolute(abs) => {
            obj.insert(BString::from("path_kind"), Value::string("external_abs"));
            obj.insert(BString::from("value"), Value::string(abs.clone()));
        }
    }
}

fn serialize_request(req: &TypedVerifyRequest) -> BString {
    let mut root = BTreeMap::new();
    root.insert(BString::from("schema_version"), Value::UInt(1));
    root.insert(
        BString::from("rcmake_exe"),
        Value::string(path::from_native_cmake(&req.rcmake_exe)),
    );
    root.insert(
        BString::from("oracle_exe"),
        match &req.oracle_exe {
            Some(oracle) => Value::string(path::from_native_cmake(oracle)),
            None => Value::Null,
        },
    );
    root.insert(
        BString::from("source_dir"),
        Value::string(path::from_native_cmake(&req.source_dir)),
    );
    if let Some(excluded) = &req.excluded_build_dir {
        root.insert(
            BString::from("excluded_build_dir"),
            Value::string(path::from_native_cmake(excluded)),
        );
    }
    if let Some(cwd_rel) = &req.cwd_rel_to_source {
        root.insert(
            BString::from("cwd_rel_to_source"),
            Value::string(cwd_rel.clone()),
        );
    }
    root.insert(BString::from("json"), Value::Bool(req.json));
    root.insert(BString::from("timeout_secs"), Value::UInt(req.timeout_secs));
    root.insert(
        BString::from("request_compile_commands"),
        Value::Bool(req.request_compile_commands),
    );
    root.insert(
        BString::from("compile_commands_user_disabled"),
        Value::Bool(req.compile_commands_user_disabled),
    );
    if let Some(generator) = &req.generator_hint {
        root.insert(
            BString::from("generator_hint"),
            Value::string(generator.clone()),
        );
    }
    root.insert(
        BString::from("external_inputs"),
        Value::Array(
            req.external_inputs
                .iter()
                .map(|p| Value::string(p.clone()))
                .collect(),
        ),
    );
    let args = req
        .args
        .iter()
        .map(|arg| {
            let mut obj = BTreeMap::new();
            match arg {
                ConfigureOption::Generator(v) => {
                    obj.insert(BString::from("kind"), Value::string("generator"));
                    obj.insert(BString::from("value"), Value::string(v.clone()));
                }
                ConfigureOption::Architecture(v) => {
                    obj.insert(BString::from("kind"), Value::string("architecture"));
                    obj.insert(BString::from("value"), Value::string(v.clone()));
                }
                ConfigureOption::Toolset(v) => {
                    obj.insert(BString::from("kind"), Value::string("toolset"));
                    obj.insert(BString::from("value"), Value::string(v.clone()));
                }
                ConfigureOption::Preset(v) => {
                    obj.insert(BString::from("kind"), Value::string("preset"));
                    obj.insert(BString::from("value"), Value::string(v.clone()));
                }
                ConfigureOption::PresetsFile(p) => {
                    obj.insert(BString::from("kind"), Value::string("presets_file"));
                    insert_path_arg_fields(&mut obj, p);
                }
                ConfigureOption::PreloadScript(p) => {
                    obj.insert(BString::from("kind"), Value::string("preload_script"));
                    insert_path_arg_fields(&mut obj, p);
                }
                ConfigureOption::Toolchain(p) => {
                    obj.insert(BString::from("kind"), Value::string("toolchain"));
                    insert_path_arg_fields(&mut obj, p);
                }
                ConfigureOption::InstallPrefix(v) => {
                    obj.insert(BString::from("kind"), Value::string("install_prefix"));
                    obj.insert(BString::from("value"), Value::string(v.clone()));
                }
                ConfigureOption::Define { name, ty, value } => {
                    obj.insert(BString::from("kind"), Value::string("define"));
                    obj.insert(BString::from("name"), Value::string(name.clone()));
                    if let Some(ty) = ty {
                        obj.insert(BString::from("type"), Value::string(ty.clone()));
                    }
                    match value {
                        DefineValue::Literal(v) => {
                            obj.insert(BString::from("value_kind"), Value::string("literal"));
                            obj.insert(BString::from("value"), Value::string(v.clone()));
                        }
                        DefineValue::SourceRel(rel) => {
                            obj.insert(BString::from("value_kind"), Value::string("source_rel"));
                            obj.insert(BString::from("value"), Value::string(rel.clone()));
                        }
                    }
                }
                ConfigureOption::Undefine(pat) => {
                    obj.insert(BString::from("kind"), Value::string("undefine"));
                    obj.insert(BString::from("pattern"), Value::string(pat.clone()));
                }
                ConfigureOption::Flag(name) => {
                    obj.insert(BString::from("kind"), Value::string("flag"));
                    obj.insert(BString::from("name"), Value::string(*name));
                }
                ConfigureOption::Warning(w) => {
                    obj.insert(BString::from("kind"), Value::string("warning"));
                    obj.insert(BString::from("value"), Value::string(w.clone()));
                }
                ConfigureOption::LogLevel(l) => {
                    obj.insert(BString::from("kind"), Value::string("log_level"));
                    obj.insert(BString::from("value"), Value::string(l.clone()));
                }
                ConfigureOption::DebugFindPkg(p) => {
                    obj.insert(BString::from("kind"), Value::string("debug_find_pkg"));
                    obj.insert(BString::from("value"), Value::string(p.clone()));
                }
                ConfigureOption::DebugFindVar(v) => {
                    obj.insert(BString::from("kind"), Value::string("debug_find_var"));
                    obj.insert(BString::from("value"), Value::string(v.clone()));
                }
                ConfigureOption::TraceFormat(f) => {
                    obj.insert(BString::from("kind"), Value::string("trace_format"));
                    obj.insert(BString::from("value"), Value::string(f.clone()));
                }
                ConfigureOption::TraceSource(src) => {
                    obj.insert(BString::from("kind"), Value::string("trace_source"));
                    match src {
                        TraceSourceArg::SourceRel(rel) => {
                            obj.insert(BString::from("path_kind"), Value::string("source_rel"));
                            obj.insert(BString::from("value"), Value::string(rel.clone()));
                        }
                        TraceSourceArg::ExternalAbsolute(abs) => {
                            obj.insert(BString::from("path_kind"), Value::string("external_abs"));
                            obj.insert(BString::from("value"), Value::string(abs.clone()));
                        }
                        TraceSourceArg::RelativeName(rel) => {
                            obj.insert(BString::from("path_kind"), Value::string("relative_name"));
                            obj.insert(BString::from("value"), Value::string(rel.clone()));
                        }
                    }
                }
                ConfigureOption::ProjectFile(name) => {
                    obj.insert(BString::from("kind"), Value::string("project_file"));
                    obj.insert(BString::from("value"), Value::string(name.clone()));
                }
            }
            Value::Object(obj)
        })
        .collect();
    root.insert(BString::from("args"), Value::Array(args));
    write_compact(&Value::Object(root))
}

#[cfg(test)]
mod tests {
    use rcmake_host::{PureHost, RunOutput};

    use super::*;

    fn bargs(items: &[&str]) -> Vec<BString> {
        items.iter().copied().map(BString::from).collect()
    }

    #[test]
    fn rejects_missing_oracle_and_non_configure_actions() -> Result<(), Box<dyn std::error::Error>>
    {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        host.seed_file(Path::new("/bin/rcmake"), b"")?;
        host.seed_file(Path::new("/bin/rcmake-verify"), b"")?;
        host.seed_file(Path::new("/work/CMakeLists.txt"), b"project(x NONE)\n")?;

        // Missing --oracle -> exit 2
        assert_eq!(run(None, &bargs(&["--", "-S", "."]), host.clone()), 2);

        // Non-configure actions, path redirects, and unsafe project-file paths -> exit 2 before launch
        for bad in [
            &["--oracle", "/opt/cmake", "--", "--build", "out"][..],
            &["--oracle", "/opt/cmake", "--", "--install", "out"][..],
            &["--oracle", "/opt/cmake", "--", "-P", "script.cmake"][..],
            &["--oracle", "/opt/cmake", "--", "-E", "echo", "hi"][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "--workflow",
                "--preset",
                "w",
            ][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "-S",
                ".",
                "--trace-redirect=/tmp/out.txt",
            ][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "-S",
                ".",
                "--graphviz=g.dot",
            ][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "-S",
                ".",
                "--sarif-output=out.sarif",
            ][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "-S",
                ".",
                "--export-compile-commands",
            ][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "-S",
                ".",
                "--project-file=../evil.txt",
            ][..],
            &[
                "--oracle",
                "/opt/cmake",
                "--",
                "-S",
                ".",
                "--project-file=/abs/CMakeLists.txt",
            ][..],
        ] {
            let code = run(None, &bargs(bad), host.clone());
            assert_eq!(code, 2, "expected exit 2 for {bad:?}");
        }
        Ok(())
    }

    #[test]
    fn missing_companion_produces_actionable_error() {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        let code = run(
            None,
            &bargs(&["--oracle", "/opt/cmake", "--", "-S", "."]),
            host.clone(),
        );
        assert_eq!(code, 2);
        let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
        assert!(stderr.contains("rcmake-verify"), "{stderr}");
    }

    #[test]
    fn rewrites_source_relative_options_and_passes_typed_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        host.seed_file(Path::new("/bin/rcmake"), b"")?;
        host.seed_file(Path::new("/bin/rcmake-verify"), b"")?;
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"project(demo NONE)\n",
        )?;
        host.seed_file(
            Path::new("/work/src/init.cmake"),
            b"set(FOO 1 CACHE STRING \"\")\n",
        )?;
        host.seed_file(Path::new("/work/src/tc.cmake"), b"")?;
        host.seed_file(Path::new("/work/src/sub/mod.cmake"), b"")?;

        let captured = Arc::new(std::sync::Mutex::new(Vec::<(
            BString,
            Vec<u8>,
            Option<Duration>,
        )>::new()));
        let sink = Arc::clone(&captured);
        host.set_command_handler(move |_host, req, _| {
            let bytes = match &req.stdin {
                Input::Bytes(b) => b.clone(),
                _ => Vec::new(),
            };
            if let Ok(mut guard) = sink.lock() {
                guard.push((req.pipeline[0].program.clone(), bytes, req.timeout));
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: b"{\"verdict\":\"parity\"}\n".to_vec(),
                stderr: Vec::new(),
            })
        })?;

        let code = run(
            None,
            &bargs(&[
                "--json",
                "--oracle",
                "/opt/cmake-4.5.1/bin/cmake",
                "--",
                "-S",
                "src",
                "-B",
                "src/build",
                "-G",
                "Ninja",
                "-C",
                "src/init.cmake",
                "--toolchain",
                "src/tc.cmake",
                "--trace-source=/work/src/sub/mod.cmake",
                "--project-file=AltLists.txt",
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=OFF",
            ]),
            host.clone(),
        );
        assert_eq!(
            code,
            0,
            "{}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );
        let calls = captured.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            rcmake_base::path_lex::cmake_style(&calls[0].0),
            "/bin/rcmake-verify"
        );
        assert_eq!(
            calls[0].2,
            Some(Duration::from_secs(
                DEFAULT_VERIFY_TIMEOUT_SECS + VERIFY_LAUNCHER_GRACE_SECS
            ))
        );
        let parsed = rcmake_json::parse(&calls[0].1, &rcmake_json::ReadOptions::default())?;
        assert_eq!(
            parsed.get("schema_version").and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            parsed.get("source_dir").and_then(Value::as_str),
            Some("/work/src")
        );
        assert_eq!(
            parsed.get("excluded_build_dir").and_then(Value::as_str),
            Some("/work/src/build")
        );
        assert_eq!(
            parsed
                .get("compile_commands_user_disabled")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            parsed
                .get("request_compile_commands")
                .and_then(Value::as_bool),
            Some(false)
        );
        let args = parsed.get("args").and_then(Value::as_array).expect("args");
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("generator")
                && a.get("value").and_then(Value::as_str) == Some("Ninja")
        }));
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("preload_script")
                && a.get("path_kind").and_then(Value::as_str) == Some("source_rel")
                && a.get("value").and_then(Value::as_str) == Some("init.cmake")
        }));
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("toolchain")
                && a.get("path_kind").and_then(Value::as_str) == Some("source_rel")
                && a.get("value").and_then(Value::as_str) == Some("tc.cmake")
        }));
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("trace_source")
                && a.get("path_kind").and_then(Value::as_str) == Some("source_rel")
                && a.get("value").and_then(Value::as_str) == Some("sub/mod.cmake")
        }));
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("project_file")
                && a.get("value").and_then(Value::as_str) == Some("AltLists.txt")
        }));
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("define")
                && a.get("name").and_then(Value::as_str) == Some("CMAKE_EXPORT_COMPILE_COMMANDS")
                && a.get("value_kind").and_then(Value::as_str) == Some("literal")
                && a.get("value").and_then(Value::as_str) == Some("OFF")
        }));
        Ok(())
    }

    #[test]
    fn ordinary_configure_never_launches_verify_companion() -> Result<(), Box<dyn std::error::Error>>
    {
        let host = crate::driver_tests::project_host();
        host.seed_file(
            Path::new("/work/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(ordinary LANGUAGES NONE)\n",
        )?;
        let launched = Arc::new(std::sync::Mutex::new(Vec::<BString>::new()));
        let sink = Arc::clone(&launched);
        host.set_command_handler(move |_host, req, _| {
            if let Ok(mut guard) = sink.lock() {
                for spec in &req.pipeline {
                    guard.push(spec.program.clone());
                }
            }
            let stdout = if req
                .pipeline
                .first()
                .is_some_and(|spec| spec.args.as_slice() == [BString::from("--version")])
            {
                b"1.12.1\n".to_vec()
            } else {
                Vec::new()
            };
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout,
                stderr: Vec::new(),
            })
        })?;
        let argv = [
            "rcmake",
            "-S",
            ".",
            "-B",
            "out",
            "-G",
            "Ninja",
            "-DCMAKE_MAKE_PROGRAM=ninja",
        ]
        .map(BString::from);
        assert_eq!(
            crate::run(&argv, host.clone()),
            0,
            "{}",
            String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
        );
        let calls = launched.lock().expect("lock");
        assert!(
            calls
                .iter()
                .all(|p| !p.to_string().contains("rcmake-verify")),
            "ordinary configure must never launch rcmake-verify: {calls:?}"
        );
        Ok(())
    }

    #[test]
    fn supports_oracle_env_timeout_flag_env_and_json_prelaunch_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        // 1. Pre-launch --json error when companion is missing emits valid schema_version=1 JSON on stdout.
        let no_comp = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        no_comp.seed_file(Path::new("/bin/rcmake"), b"")?;
        no_comp.seed_file(Path::new("/work/CMakeLists.txt"), b"project(x NONE)\n")?;
        let code = run(
            None,
            &bargs(&["--json", "--oracle", "/opt/cmake", "--", "-S", "."]),
            no_comp.clone(),
        );
        assert_eq!(code, 2);
        assert!(no_comp.stream_bytes(Stream::Stderr).is_empty());
        let doc = rcmake_json::parse(
            &no_comp.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )?;
        assert_eq!(doc.get("schema_version").and_then(Value::as_u64), Some(1));
        assert_eq!(
            doc.get("verdict").and_then(Value::as_str),
            Some("invalid_input")
        );
        assert_eq!(doc.get("exit_code").and_then(Value::as_u64), Some(2));
        let err_msg = doc.get("error").and_then(Value::as_str).unwrap_or("");
        assert!(err_msg.contains("Reinstall rcmake"), "{err_msg}");

        // 2. Pre-launch --json error on unknown verify option emits valid JSON report on stdout.
        let bad_flag = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        bad_flag.seed_file(Path::new("/bin/rcmake"), b"")?;
        bad_flag.seed_file(Path::new("/bin/rcmake-verify"), b"")?;
        assert_eq!(
            run(
                None,
                &bargs(&["--json", "--bogus-flag", "--", "-S", "."]),
                bad_flag.clone()
            ),
            2
        );
        let doc_bad_flag = rcmake_json::parse(
            &bad_flag.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )?;
        assert_eq!(
            doc_bad_flag.get("verdict").and_then(Value::as_str),
            Some("invalid_input")
        );

        // 3. Omitting --oracle delegates resolution to rcmake-verify (oracle_exe: null) while
        // RCMAKE_VERIFY_TIMEOUT and --timeout override thread through.
        let host = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        host.seed_file(Path::new("/bin/rcmake"), b"")?;
        host.seed_file(Path::new("/bin/rcmake-verify"), b"")?;
        host.seed_file(Path::new("/work/CMakeLists.txt"), b"project(x NONE)\n")?;
        host.env_set(b"RCMAKE_VERIFY_TIMEOUT", Some(b"900"));

        let captured = Arc::new(std::sync::Mutex::new((Vec::<u8>::new(), None::<Duration>)));
        let cap_ref = Arc::clone(&captured);
        host.set_command_handler(move |_host, req, _| {
            if let Input::Bytes(bytes) = &req.stdin
                && let Ok(mut guard) = cap_ref.lock()
            {
                *guard = (bytes.to_vec(), req.timeout);
            }
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: b"{\"verdict\":\"pass\"}\n".to_vec(),
                stderr: Vec::new(),
            })
        })?;

        assert_eq!(
            run(None, &bargs(&["--json", "--", "-S", "."]), host.clone()),
            0
        );
        {
            let (raw, timeout) = captured.lock().expect("lock").clone();
            assert_eq!(
                timeout,
                Some(Duration::from_secs(900 + VERIFY_LAUNCHER_GRACE_SECS))
            );
            let parsed = rcmake_json::parse(&raw, &rcmake_json::ReadOptions::default())?;
            assert_eq!(parsed.get("oracle_exe"), Some(&Value::Null));
            assert_eq!(
                parsed.get("timeout_secs").and_then(Value::as_u64),
                Some(900)
            );
        }

        // Explicit --timeout overrides RCMAKE_VERIFY_TIMEOUT.
        assert_eq!(
            run(
                None,
                &bargs(&["--json", "--timeout", "180", "--", "-S", "."]),
                host.clone()
            ),
            0
        );
        {
            let (raw, timeout) = captured.lock().expect("lock").clone();
            assert_eq!(
                timeout,
                Some(Duration::from_secs(180 + VERIFY_LAUNCHER_GRACE_SECS))
            );
            let parsed = rcmake_json::parse(&raw, &rcmake_json::ReadOptions::default())?;
            assert_eq!(
                parsed.get("timeout_secs").and_then(Value::as_u64),
                Some(180)
            );
        }

        // Invalid --timeout=0 fails pre-launch cleanly with JSON invalid_input.
        let bad_to = Arc::new(PureHost::new(
            PathBuf::from("/work"),
            PathBuf::from("/bin/rcmake"),
        ));
        bad_to.seed_file(Path::new("/bin/rcmake"), b"")?;
        bad_to.seed_file(Path::new("/bin/rcmake-verify"), b"")?;
        assert_eq!(
            run(
                None,
                &bargs(&[
                    "--json",
                    "--timeout=0",
                    "--oracle",
                    "/opt/cmake",
                    "--",
                    "-S",
                    "."
                ]),
                bad_to.clone()
            ),
            2
        );
        let bad_doc = rcmake_json::parse(
            &bad_to.stream_bytes(Stream::Stdout),
            &rcmake_json::ReadOptions::default(),
        )?;
        assert_eq!(
            bad_doc.get("verdict").and_then(Value::as_str),
            Some("invalid_input")
        );
        Ok(())
    }
}
