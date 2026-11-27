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

use rcmake_base::{BString, truth};
use rcmake_buildinfo::{ArtifactReader, ReadLimits, parse_cache};
use rcmake_host::{FileKind, Follow, Host, Input, Output, ProcessResult, RunRequest, Stream, path};
use rcmake_json::{Value, write_compact};
use rcmake_presets::{EnvSnapshot, PresetKind, PresetSettings};

use crate::options::split_flag;
use crate::resolve_cli_path;

const HELP: &str = "\
Usage: rcmake verify [--json] --oracle <path> -- <configure arguments>

Run both rcmake (in compatibility mode) and an explicit upstream CMake binary
against identical staged copies of a project, then compare File API codemodel,
CMakeCache.txt, compile_commands.json, and Ninja dry-run output.

Options:
  --oracle <path>   Path to the reference CMake 4.5.1 executable (required).
  --json            Write one machine-readable JSON report to standard output.
  -h, --help        Show this help.

Notes:
  verify stages a pristine copy of the source tree into a private scratch
  directory and never writes to the caller's source or build directory.
  Configure project code executes twice (once per engine).

Exit status:
  0  Parity across all applicable sections (with CMake 4.5.1).
  1  Observed divergence between rcmake and the oracle.
  2  Invalid input, missing companion, matched failure, or incomplete evidence.
";

#[derive(Debug)]
struct VerifyCli {
    oracle: BString,
    json: bool,
    configure_args: Vec<BString>,
}

#[derive(Debug)]
enum CliAction {
    Help,
    Run(VerifyCli),
}

/// Typed argument forwarded to `rcmake-verify`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ForwardArg {
    /// Verbatim configure argument.
    Literal(BString),
    /// Separate flag plus path relative to the staged source directory.
    FlagSourceRel { flag: BString, rel: BString },
    /// Attached `<prefix><staged_src>/<rel>` argument (for example `-DCMAKE_TOOLCHAIN_FILE:FILEPATH=...`).
    PrefixSourceRel { prefix: BString, rel: BString },
}

#[derive(Debug)]
struct TypedVerifyRequest {
    rcmake_exe: PathBuf,
    oracle_exe: PathBuf,
    source_dir: PathBuf,
    excluded_build_dir: Option<PathBuf>,
    cwd_rel_to_source: Option<BString>,
    args: Vec<ForwardArg>,
    json: bool,
    request_compile_commands: bool,
    compile_commands_user_disabled: bool,
    generator_hint: Option<BString>,
    external_inputs: Vec<BString>,
}

/// Execute `rcmake verify`.
pub(crate) fn run(argv0: Option<&BString>, args: &[BString], host: Arc<dyn Host>) -> i32 {
    let cli = match parse_verify_cli(args) {
        Ok(CliAction::Help) => {
            host.write_out(Stream::Stdout, HELP.as_bytes());
            return 0;
        }
        Ok(CliAction::Run(cli)) => cli,
        Err(message) => return emit_error(host.as_ref(), &message),
    };

    let (rcmake_exe, companion) = match locate_executables(host.as_ref(), argv0) {
        Ok(pair) => pair,
        Err(message) => return emit_error(host.as_ref(), &message),
    };

    let oracle_exe = match resolve_oracle_path(host.as_ref(), &cli.oracle) {
        Ok(path) => path,
        Err(message) => return emit_error(host.as_ref(), &message),
    };

    let request = match build_typed_request(
        host.as_ref(),
        rcmake_exe,
        oracle_exe,
        cli.json,
        &cli.configure_args,
    ) {
        Ok(req) => req,
        Err(message) => return emit_error(host.as_ref(), &message),
    };

    let payload = serialize_request(&request);
    let run_req = RunRequest {
        pipeline: vec![rcmake_host::CommandSpec {
            program: path::from_native(&companion),
            args: vec![BString::from("--request-stdin")],
        }],
        cwd: host.initial_cwd(),
        env: host.env_all(),
        stdin: Input::Bytes(payload.into()),
        stdout: Output::Capture,
        stderr: Output::Capture,
        timeout: None,
        tee: false,
    };

    match host.run(&run_req, &mut |_, _| {}) {
        Ok(output) => {
            if !output.stdout.is_empty() {
                host.write_out(Stream::Stdout, &output.stdout);
            }
            if !output.stderr.is_empty() {
                host.write_out(Stream::Stderr, &output.stderr);
            }
            match output.results.first() {
                Some(ProcessResult::Exited(code)) => *code,
                Some(ProcessResult::Signaled(sig)) => emit_error(
                    host.as_ref(),
                    &format!("rcmake-verify was terminated by signal {sig}."),
                ),
                Some(ProcessResult::TimedOut) => {
                    emit_error(host.as_ref(), "rcmake-verify timed out.")
                }
                Some(ProcessResult::SpawnFailed(err)) => emit_error(
                    host.as_ref(),
                    &format!("Could not start {}: {err}", companion.display()),
                ),
                None => emit_error(host.as_ref(), "rcmake-verify produced no exit status."),
            }
        }
        Err(err) => emit_error(
            host.as_ref(),
            &format!("Could not execute {}: {err}", companion.display()),
        ),
    }
}

fn emit_error(host: &dyn Host, message: &str) -> i32 {
    host.write_out(
        Stream::Stderr,
        format!("rcmake verify: {message}\n").as_bytes(),
    );
    2
}

fn parse_verify_cli(args: &[BString]) -> Result<CliAction, String> {
    let mut oracle: Option<BString> = None;
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

    let Some(oracle) = oracle else {
        return Err("Missing required --oracle <path> option. Run 'rcmake verify --help'.".into());
    };
    Ok(CliAction::Run(VerifyCli {
        oracle,
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
        "Companion binary '{companion_name}' was not found next to {}. Install or build rcmake-verify (for example: cargo build -p rcmake-harness --bin rcmake-verify).",
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
    Literal(BString),
    PreloadScript(PathBuf),
    ToolchainFlag(PathBuf),
    InstallPrefixFlag(PathBuf),
    PresetsFileFlag(PathBuf),
    Define {
        raw: BString,
        name: BString,
        ty: Option<BString>,
        value: BString,
    },
    Undefine(BString),
}

fn build_typed_request(
    host: &dyn Host,
    rcmake_exe: PathBuf,
    oracle_exe: PathBuf,
    json: bool,
    configure_args: &[BString],
) -> Result<TypedVerifyRequest, String> {
    let mut dirs = Vec::new();
    let mut forwarded = Vec::new();
    let mut preset: Option<BString> = None;
    let mut presets_file: Option<PathBuf> = None;
    let mut generator_hint: Option<BString> = None;
    let mut explicit_compdb_state: Option<bool> = None;
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

        // Reject non-configure actions and unsupported output/path redirects before launch (P1-13).
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
                forwarded.push(ParsedForward::Literal(BString::from("-G")));
                forwarded.push(ParsedForward::Literal(BString::from(val)));
            }
            b"-A" | b"-T" => {
                let val = take_req("Missing value for -A or -T.")?;
                forwarded.push(ParsedForward::Literal(BString::from(flag)));
                forwarded.push(ParsedForward::Literal(BString::from(val)));
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
                forwarded.push(ParsedForward::Literal(BString::from("--preset")));
                forwarded.push(ParsedForward::Literal(BString::from(val)));
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
                forwarded.push(ParsedForward::Define {
                    raw: BString::from(raw_def),
                    name,
                    ty,
                    value,
                });
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
                forwarded.push(ParsedForward::Literal(BString::from(flag)));
            }
            b"-W" => {
                let val = take_req("-W must be followed with [no-]<name>.")?;
                forwarded.push(ParsedForward::Literal(
                    [b"-W".as_slice(), val].concat().into(),
                ));
            }
            b"--log-level" | b"--loglevel" | b"--debug-find-pkg" | b"--debug-find-var"
            | b"--trace-format" | b"--trace-source" | b"--project-file" => {
                let val = take_req("Missing value for configure option.")?;
                forwarded.push(ParsedForward::Literal([flag, b"=", val].concat().into()));
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
            excluded_build_dir = Some(if native.is_absolute() {
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

    // Rewrite path-bearing read arguments against staged source, recording external inputs.
    let mut external_inputs = Vec::new();
    let mut args = Vec::new();
    for item in forwarded {
        match item {
            ParsedForward::Literal(val) => args.push(ForwardArg::Literal(val)),
            ParsedForward::Undefine(pat) => {
                args.push(ForwardArg::Literal(BString::from("-U")));
                args.push(ForwardArg::Literal(pat));
            }
            ParsedForward::PreloadScript(script) => {
                push_path_arg(
                    host,
                    &source_canonical,
                    &script,
                    b"-C",
                    &mut args,
                    &mut external_inputs,
                    true,
                );
            }
            ParsedForward::ToolchainFlag(tc) => {
                push_path_arg(
                    host,
                    &source_canonical,
                    &tc,
                    b"--toolchain",
                    &mut args,
                    &mut external_inputs,
                    true,
                );
            }
            ParsedForward::PresetsFileFlag(file) => {
                push_path_arg(
                    host,
                    &source_canonical,
                    &file,
                    b"--presets-file",
                    &mut args,
                    &mut external_inputs,
                    true,
                );
            }
            ParsedForward::InstallPrefixFlag(dir) => {
                args.push(ForwardArg::Literal(BString::from("--install-prefix")));
                args.push(ForwardArg::Literal(path::from_native_cmake(&dir)));
            }
            ParsedForward::Define {
                raw,
                name,
                ty,
                value,
            } => {
                let path_like = name.as_slice() == b"CMAKE_TOOLCHAIN_FILE"
                    || ty.as_deref().is_some_and(|t| {
                        t.eq_ignore_ascii_case(b"PATH") || t.eq_ignore_ascii_case(b"FILEPATH")
                    });
                if path_like && !value.is_empty() {
                    let resolved = resolve_setup_candidate(host, &source_canonical, &value);
                    if let Some(rel) = relative_under(host, &source_canonical, &resolved) {
                        let mut prefix = BString::from("-D");
                        prefix.extend_from_slice(&name);
                        if let Some(ty) = &ty {
                            prefix.push(b':');
                            prefix.extend_from_slice(ty);
                        }
                        prefix.push(b'=');
                        args.push(ForwardArg::PrefixSourceRel { prefix, rel });
                    } else {
                        if name.as_slice() == b"CMAKE_TOOLCHAIN_FILE" {
                            external_inputs.push(path::from_native_cmake(&resolved));
                        }
                        let mut full = BString::from("-D");
                        full.extend_from_slice(&name);
                        if let Some(ty) = &ty {
                            full.push(b':');
                            full.extend_from_slice(ty);
                        }
                        full.push(b'=');
                        full.extend_from_slice(&path::from_native_cmake(&resolved));
                        args.push(ForwardArg::Literal(full));
                    }
                } else {
                    args.push(ForwardArg::Literal(BString::from("-D")));
                    args.push(ForwardArg::Literal(raw));
                }
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
        request_compile_commands,
        compile_commands_user_disabled,
        generator_hint,
        external_inputs,
    })
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
        Some(colon) => (
            BString::from(lhs.get(..colon).unwrap_or_default()),
            Some(BString::from(lhs.get(colon + 1..).unwrap_or_default())),
        ),
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
    if native.is_absolute() {
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

fn push_path_arg(
    host: &dyn Host,
    source_root: &Path,
    candidate: &Path,
    flag: &[u8],
    out: &mut Vec<ForwardArg>,
    external_inputs: &mut Vec<BString>,
    track_external: bool,
) {
    if let Some(rel) = relative_under(host, source_root, candidate) {
        out.push(ForwardArg::FlagSourceRel {
            flag: BString::from(flag),
            rel,
        });
    } else {
        let abs = path::from_native_cmake(candidate);
        if track_external {
            external_inputs.push(abs.clone());
        }
        out.push(ForwardArg::Literal(BString::from(flag)));
        out.push(ForwardArg::Literal(abs));
    }
}

fn serialize_request(req: &TypedVerifyRequest) -> BString {
    let mut root = BTreeMap::new();
    root.insert(BString::from("schema_version"), Value::UInt(1));
    root.insert(
        BString::from("rcmake_exe"),
        Value::string(path::from_native(&req.rcmake_exe)),
    );
    root.insert(
        BString::from("oracle_exe"),
        Value::string(path::from_native(&req.oracle_exe)),
    );
    root.insert(
        BString::from("source_dir"),
        Value::string(path::from_native(&req.source_dir)),
    );
    if let Some(excluded) = &req.excluded_build_dir {
        root.insert(
            BString::from("excluded_build_dir"),
            Value::string(path::from_native(excluded)),
        );
    }
    if let Some(cwd_rel) = &req.cwd_rel_to_source {
        root.insert(
            BString::from("cwd_rel_to_source"),
            Value::string(cwd_rel.clone()),
        );
    }
    root.insert(BString::from("json"), Value::Bool(req.json));
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
                ForwardArg::Literal(value) => {
                    obj.insert(BString::from("kind"), Value::string("literal"));
                    obj.insert(BString::from("value"), Value::string(value.clone()));
                }
                ForwardArg::FlagSourceRel { flag, rel } => {
                    obj.insert(BString::from("kind"), Value::string("flag_source_rel"));
                    obj.insert(BString::from("flag"), Value::string(flag.clone()));
                    obj.insert(BString::from("rel"), Value::string(rel.clone()));
                }
                ForwardArg::PrefixSourceRel { prefix, rel } => {
                    obj.insert(BString::from("kind"), Value::string("prefix_source_rel"));
                    obj.insert(BString::from("prefix"), Value::string(prefix.clone()));
                    obj.insert(BString::from("rel"), Value::string(rel.clone()));
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

        // Non-configure actions and path-bearing redirects -> exit 2 before launching companion
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
    fn rewrites_source_relative_preload_and_toolchain_and_passes_typed_request()
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

        let captured = Arc::new(std::sync::Mutex::new(Vec::<(BString, Vec<u8>)>::new()));
        let sink = Arc::clone(&captured);
        host.set_command_handler(move |_host, req, _| {
            let bytes = match &req.stdin {
                Input::Bytes(b) => b.clone(),
                _ => Vec::new(),
            };
            if let Ok(mut guard) = sink.lock() {
                guard.push((req.pipeline[0].program.clone(), bytes));
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
        assert_eq!(calls[0].0, "/bin/rcmake-verify");
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
            a.get("kind").and_then(Value::as_str) == Some("flag_source_rel")
                && a.get("flag").and_then(Value::as_str) == Some("-C")
                && a.get("rel").and_then(Value::as_str) == Some("init.cmake")
        }));
        assert!(args.iter().any(|a| {
            a.get("kind").and_then(Value::as_str) == Some("flag_source_rel")
                && a.get("flag").and_then(Value::as_str) == Some("--toolchain")
                && a.get("rel").and_then(Value::as_str) == Some("tc.cmake")
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
}
