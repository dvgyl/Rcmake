//! Unified multi-call CLI driver for `rcmake` (`cmake`, `ctest`, `cpack` dispatch via `argv[0]`).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{
    BString, CMAKE_VERSION_MAJOR, CMAKE_VERSION_MINOR, CMAKE_VERSION_PATCH, CMAKE_VERSION_STR,
    Limits,
};
use rcmake_diag::{
    DiagAction, DiagCategory, DiagOptions, DiagState, Diagnostic, Effect, Location, MessageKind,
    RenderOptions, render,
};
use rcmake_eval::{CacheDefine, Interp, InterpConfig, LogLevel, RunMode};
use rcmake_host::{Host, Stream, WriteMode};
use rcmake_json::Value;
use rcmake_model::CacheType;
use rcmake_presets::{EnvSnapshot, PresetKind, PresetSettings};

/// Active tool persona selected from the `argv[0]` basename.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `cmake` (or default `rcmake`).
    Cmake,
    /// `ctest`.
    Ctest,
    /// `cpack`.
    Cpack,
}

impl Tool {
    /// Canonical lowercase executable name printed in `--version`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Ctest => "ctest",
            Self::Cpack => "cpack",
        }
    }
}

/// Determine the [`Tool`] persona from the `argv[0]` basename (not the resolved symlink target).
#[must_use]
pub fn tool_from_argv0(argv0: &OsStr) -> Tool {
    let base = Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cmake");
    if base.eq_ignore_ascii_case("ctest") {
        Tool::Ctest
    } else if base.eq_ignore_ascii_case("cpack") {
        Tool::Cpack
    } else {
        Tool::Cmake
    }
}

/// Run the Rcmake CLI with `argv` (including `argv[0]`) against `host`, returning the process exit code.
#[must_use]
pub fn run(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    let tool = argv
        .first()
        .map(|a| {
            let p = rcmake_host::path::to_native(a.as_slice());
            tool_from_argv0(p.as_os_str())
        })
        .unwrap_or(Tool::Cmake);

    let rest = argv.get(1..).unwrap_or(&[]);
    if let Some(first) = rest.first() {
        let fb = first.as_slice();
        if fb == b"--version" || fb == b"-version" || fb == b"/V" {
            let banner = format!(
                "{} version {CMAKE_VERSION_STR}\n\nRcmake native Rust drop-in replacement for CMake {CMAKE_VERSION_STR}.\n",
                tool.name()
            );
            host.write_out(Stream::Stdout, banner.as_bytes());
            return 0;
        }
        if let Some(fmt) = fb
            .strip_prefix(b"--version=")
            .or_else(|| fb.strip_prefix(b"-version="))
            .or_else(|| fb.strip_prefix(b"/V="))
        {
            return run_version_format(tool, fmt, host.as_ref());
        }
    }

    if tool == Tool::Cmake {
        return run_cmake(argv, rest, host);
    }

    emit_cli_error(
        host.as_ref(),
        format!(
            "{} mode is not yet implemented (scheduled for Milestone 3)",
            tool.name()
        ),
    );
    1
}

fn run_version_format(tool: Tool, fmt: &[u8], host: &dyn Host) -> i32 {
    if fmt != b"json-v1" {
        emit_cli_error(
            host,
            format!("Invalid --version format: {}", String::from_utf8_lossy(fmt)),
        );
        return 1;
    }
    let mut prog_ver = BTreeMap::new();
    prog_ver.insert(
        BString::from("major"),
        Value::UInt(u64::from(CMAKE_VERSION_MAJOR)),
    );
    prog_ver.insert(
        BString::from("minor"),
        Value::UInt(u64::from(CMAKE_VERSION_MINOR)),
    );
    prog_ver.insert(
        BString::from("patch"),
        Value::UInt(u64::from(CMAKE_VERSION_PATCH)),
    );
    prog_ver.insert(BString::from("string"), Value::string(CMAKE_VERSION_STR));

    let mut program = BTreeMap::new();
    program.insert(BString::from("name"), Value::string(tool.name()));
    program.insert(BString::from("version"), Value::Object(prog_ver));

    let mut schema_ver = BTreeMap::new();
    schema_ver.insert(BString::from("major"), Value::UInt(1));
    schema_ver.insert(BString::from("minor"), Value::UInt(0));

    let mut root = BTreeMap::new();
    root.insert(BString::from("dependencies"), Value::Array(Vec::new()));
    root.insert(BString::from("program"), Value::Object(program));
    root.insert(BString::from("version"), Value::Object(schema_ver));

    let mut text = rcmake_json::write_styled(&Value::Object(root), "  ");
    text.push(b'\n');
    host.write_out(Stream::Stdout, text.as_slice());
    0
}

enum ListPresetsMode {
    One(PresetKind),
    All,
}

fn run_cmake(argv: &[BString], rest: &[BString], host: Arc<dyn Host>) -> i32 {
    let mut defines: Vec<CacheDefine> = Vec::new();
    let mut preload_scripts: Vec<PathBuf> = Vec::new();
    let mut diag_state = DiagState::default();
    let mut sarif_output: Option<PathBuf> = None;
    let mut log_level: Option<LogLevel> = None;
    let mut log_context = false;
    let mut view_only = false;
    let mut source_dir: Option<PathBuf> = None;
    let mut binary_dir: Option<PathBuf> = None;
    let mut generator: Option<BString> = None;
    let mut preset_name: Option<BString> = None;
    let mut list_presets: Option<ListPresetsMode> = None;
    let mut positional: Vec<BString> = Vec::new();
    let mut i = 0usize;

    while let Some(arg) = rest.get(i) {
        let bytes = arg.as_slice();
        if bytes == b"-E" {
            let sub_args = rest.get(i + 1..).unwrap_or(&[]);
            let cwd = host.initial_cwd();
            let exe_display = host
                .current_exe()
                .map(|p| rcmake_host::path::from_native(&p))
                .unwrap_or_else(|_| BString::from("cmake"));
            let caps = build_capabilities_json;
            let ctx = rcmake_tool_e::ToolCtx {
                host: host.as_ref(),
                cwd: &cwd,
                exe_display: exe_display.as_slice(),
                capabilities: &caps,
            };
            return rcmake_tool_e::run(sub_args, &ctx);
        }
        if bytes == b"-P" {
            let Some(script_arg) = rest.get(i + 1) else {
                emit_cli_error(host.as_ref(), "No script specified for argument -P");
                return 1;
            };
            return run_script_mode(
                argv,
                script_arg.as_slice(),
                defines,
                diag_state,
                sarif_output,
                log_level,
                log_context,
                host,
            );
        }
        if bytes == b"--build" || bytes == b"--install" {
            emit_cli_error(
                host.as_ref(),
                format!(
                    "{} mode is not yet implemented (scheduled for Milestone 2)",
                    String::from_utf8_lossy(bytes)
                ),
            );
            return 1;
        }
        if bytes == b"-N" {
            view_only = true;
            i += 1;
            continue;
        }
        if let Some(c_arg) = take_opt_value(rest, &mut i, b"-C") {
            let Some(val) = c_arg else {
                emit_cli_error(host.as_ref(), "No file specified for -C");
                return 1;
            };
            preload_scripts.push(rcmake_host::path::to_native(val));
            continue;
        }
        if let Some(s_arg) = take_opt_value(rest, &mut i, b"-S") {
            let Some(val) = s_arg else {
                emit_cli_error(host.as_ref(), "No source directory specified for -S");
                return 1;
            };
            source_dir = Some(resolve_cli_path(host.as_ref(), val));
            continue;
        }
        if let Some(b_arg) = take_opt_value(rest, &mut i, b"-B") {
            let Some(val) = b_arg else {
                emit_cli_error(host.as_ref(), "No build directory specified for -B");
                return 1;
            };
            binary_dir = Some(resolve_cli_path(host.as_ref(), val));
            continue;
        }
        if let Some(g_arg) = take_opt_value(rest, &mut i, b"-G") {
            let Some(val) = g_arg else {
                emit_cli_error(host.as_ref(), "No generator specified for -G");
                return 1;
            };
            generator = Some(BString::from(val));
            continue;
        }
        if let Some(def) = bytes.strip_prefix(b"-D") {
            let raw = if def.is_empty() {
                i += 1;
                match rest.get(i) {
                    Some(next) => next.as_slice(),
                    None => {
                        emit_cli_error(host.as_ref(), "No argument for -D");
                        return 1;
                    }
                }
            } else {
                def
            };
            let Some(parsed) = parse_define(raw) else {
                let raw_s = String::from_utf8_lossy(raw);
                host.write_out(
                    Stream::Stderr,
                    format!(
                        "CMake Error: Parse error in command line argument: {raw_s}\n Should be: VAR:type=value\n\nCMake Error: Run 'cmake --help' for all supported options.\n"
                    )
                    .as_bytes(),
                );
                return 1;
            };
            defines.push(parsed);
            i += 1;
            continue;
        }
        if bytes.starts_with(b"-U") {
            if bytes == b"-U" {
                i += 1;
            }
            i += 1;
            continue;
        }
        if bytes == b"--preset" {
            i += 1;
            let Some(next) = rest.get(i) else {
                emit_cli_error(host.as_ref(), "No preset specified for --preset");
                return 1;
            };
            preset_name = Some(next.clone());
            i += 1;
            continue;
        }
        if let Some(p) = bytes.strip_prefix(b"--preset=") {
            preset_name = Some(BString::from(p));
            i += 1;
            continue;
        }
        if bytes == b"--log-context" {
            log_context = true;
            i += 1;
            continue;
        }
        if let Some(level_bytes) = bytes.strip_prefix(b"--log-level=") {
            if let Some(lvl) = LogLevel::parse(level_bytes) {
                log_level = Some(lvl);
            } else {
                emit_cli_error(
                    host.as_ref(),
                    format!(
                        "Value {} for --log-level is not a valid log level.",
                        String::from_utf8_lossy(level_bytes)
                    ),
                );
                return 1;
            }
            i += 1;
            continue;
        }
        if bytes == b"--log-level" {
            i += 1;
            let Some(next) = rest.get(i) else {
                emit_cli_error(host.as_ref(), "No argument for --log-level");
                return 1;
            };
            if let Some(lvl) = LogLevel::parse(next.as_slice()) {
                log_level = Some(lvl);
            } else {
                emit_cli_error(
                    host.as_ref(),
                    format!(
                        "Value {} for --log-level is not a valid log level.",
                        String::from_utf8_lossy(next.as_slice())
                    ),
                );
                return 1;
            }
            i += 1;
            continue;
        }
        if let Some(path_bytes) = bytes.strip_prefix(b"--sarif-output=") {
            let p = resolve_cli_path(host.as_ref(), path_bytes);
            sarif_output = Some(p);
            i += 1;
            continue;
        }
        if apply_warning_flag(bytes, &mut diag_state) {
            i += 1;
            continue;
        }
        if bytes == b"--list-presets" || bytes.starts_with(b"--list-presets=") {
            let mode = match bytes.strip_prefix(b"--list-presets=") {
                None | Some(b"configure") => ListPresetsMode::One(PresetKind::Configure),
                Some(b"build") => ListPresetsMode::One(PresetKind::Build),
                Some(b"test") => ListPresetsMode::One(PresetKind::Test),
                Some(b"package") => ListPresetsMode::One(PresetKind::Package),
                Some(b"workflow") => ListPresetsMode::One(PresetKind::Workflow),
                Some(b"all") => ListPresetsMode::All,
                Some(_) => {
                    emit_cli_error(host.as_ref(), "Invalid value specified for --list-presets.");
                    return 1;
                }
            };
            list_presets = Some(mode);
            i += 1;
            continue;
        }
        if bytes == b"--no-warn-unused-cli"
            || bytes == b"--debug-output"
            || bytes == b"--trace"
            || bytes == b"--trace-expand"
            || bytes.starts_with(b"--trace-")
        {
            i += 1;
            continue;
        }
        if bytes == b"-H" || bytes.starts_with(b"-H") {
            let val = if bytes == b"-H" {
                i += 1;
                rest.get(i).map(|a| a.as_slice())
            } else {
                bytes.strip_prefix(b"-H")
            };
            if let Some(v) = val {
                source_dir = Some(resolve_cli_path(host.as_ref(), v));
            }
            i += 1;
            continue;
        }
        if !bytes.starts_with(b"-") {
            positional.push(arg.clone());
        }
        i += 1;
    }

    if let Some(mode) = list_presets {
        let dir = source_dir
            .or_else(|| {
                positional
                    .first()
                    .map(|p| resolve_cli_path(host.as_ref(), p.as_slice()))
            })
            .unwrap_or_else(|| host.initial_cwd());
        return run_list_presets(host.as_ref(), &dir, mode);
    }

    if view_only && !preload_scripts.is_empty() {
        return run_view_preload(
            argv,
            &preload_scripts,
            defines,
            diag_state,
            sarif_output,
            log_level,
            log_context,
            host,
        );
    }

    if preset_name.is_some()
        || source_dir.is_some()
        || binary_dir.is_some()
        || !positional.is_empty()
    {
        return run_configure_mode(
            argv,
            source_dir,
            binary_dir,
            positional,
            preset_name,
            generator,
            &preload_scripts,
            defines,
            diag_state,
            sarif_output,
            log_level,
            log_context,
            host,
        );
    }

    emit_cli_error(
        host.as_ref(),
        "Requested CLI mode is not yet implemented (scheduled for Milestone 1/2)",
    );
    1
}

fn take_opt_value<'a>(
    rest: &'a [BString],
    idx: &mut usize,
    flag: &[u8],
) -> Option<Option<&'a [u8]>> {
    let bytes = rest.get(*idx)?.as_slice();
    if bytes == flag {
        *idx += 1;
        let val = rest.get(*idx).map(|a| a.as_slice());
        if val.is_some() {
            *idx += 1;
        }
        Some(val)
    } else if let Some(tail) = bytes.strip_prefix(flag) {
        *idx += 1;
        Some(Some(tail))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn run_view_preload(
    argv: &[BString],
    preload_scripts: &[PathBuf],
    defines: Vec<CacheDefine>,
    diag_state: DiagState,
    sarif_output: Option<PathBuf>,
    log_level: Option<LogLevel>,
    log_context: bool,
    host: Arc<dyn Host>,
) -> i32 {
    let cwd = host.initial_cwd();
    let (cmake_command, cmake_root) = discover_cmake_paths(host.as_ref());
    let cfg = InterpConfig {
        host: Arc::clone(&host),
        mode: RunMode::Configure,
        cmake_root,
        cmake_command,
        cwd,
        limits: Limits::default(),
        diag: DiagOptions {
            initial_state: diag_state,
            sarif: sarif_output.is_some(),
        },
        defines,
        script_args: argv.to_vec(),
    };
    let mut interp = Interp::new(cfg, rcmake_commands::register_all);
    interp.set_log_level(log_level);
    if log_context {
        interp.set_log_context(true);
    }
    for script in preload_scripts {
        let _ = interp.run_preload_script(script);
        if interp.fatal_occurred() {
            break;
        }
    }
    write_sarif_if_requested(host.as_ref(), sarif_output.as_deref(), &interp);
    interp.exit_code()
}

#[allow(clippy::too_many_arguments)]
fn run_configure_mode(
    argv: &[BString],
    mut source_dir: Option<PathBuf>,
    mut binary_dir: Option<PathBuf>,
    positional: Vec<BString>,
    preset_name: Option<BString>,
    mut generator: Option<BString>,
    preload_scripts: &[PathBuf],
    mut defines: Vec<CacheDefine>,
    diag_state: DiagState,
    sarif_output: Option<PathBuf>,
    log_level: Option<LogLevel>,
    log_context: bool,
    host: Arc<dyn Host>,
) -> i32 {
    let cwd = host.initial_cwd();
    if source_dir.is_none()
        && let Some(first) = positional.first()
    {
        source_dir = Some(resolve_cli_path(host.as_ref(), first.as_slice()));
    }
    let src = source_dir.unwrap_or_else(|| cwd.clone());

    if let Some(pname) = preset_name {
        let presets = match rcmake_presets::load(&src, host.as_ref()) {
            Ok(p) => p,
            Err(err) => {
                emit_cli_error(host.as_ref(), err.to_string());
                return 1;
            }
        };
        let env = EnvSnapshot::from_host(host.as_ref());
        let resolved = match rcmake_presets::resolve(
            &presets,
            PresetKind::Configure,
            pname.as_slice(),
            &env,
        ) {
            Ok(r) => r,
            Err(err) => {
                emit_cli_error(host.as_ref(), err.to_string());
                return 1;
            }
        };
        if binary_dir.is_none()
            && let Some(bd) = &resolved.binary_dir
        {
            binary_dir = Some(resolve_cli_path(host.as_ref(), bd.as_slice()));
        }
        if generator.is_none() {
            generator = resolved.generator.clone();
        }
        for (k, v) in &resolved.environment {
            host.env_set(k.as_slice(), v.as_ref().map(|b| b.as_slice()));
        }
        if let PresetSettings::Configure(cfg) = &resolved.settings
            && let Some(vars) = &cfg.cache_variables
        {
            for (name, cv) in vars {
                let ty = cv
                    .kind
                    .as_ref()
                    .and_then(|t| CacheType::parse(t.as_slice()));
                defines.push(CacheDefine {
                    name: name.clone(),
                    ty,
                    value: cv.value.clone(),
                });
            }
        }
    }

    if let Some(generator_name) = generator {
        defines.push(CacheDefine {
            name: BString::from("CMAKE_GENERATOR"),
            ty: Some(CacheType::Internal),
            value: generator_name,
        });
    }

    let bin = binary_dir.unwrap_or_else(|| cwd.clone());
    let (cmake_command, cmake_root) = discover_cmake_paths(host.as_ref());

    let cfg = InterpConfig {
        host: Arc::clone(&host),
        mode: RunMode::Configure,
        cmake_root,
        cmake_command,
        cwd,
        limits: Limits::default(),
        diag: DiagOptions {
            initial_state: diag_state,
            sarif: sarif_output.is_some(),
        },
        defines,
        script_args: argv.to_vec(),
    };

    let mut interp = Interp::new(cfg, rcmake_commands::register_all);
    interp.set_log_level(log_level);
    if log_context {
        interp.set_log_context(true);
    }

    let outcome = interp.run_configure(&src, &bin, preload_scripts);
    if !write_sarif_if_requested(host.as_ref(), sarif_output.as_deref(), &interp) {
        return 1;
    }
    outcome.exit_code
}

#[allow(clippy::too_many_arguments)]
fn run_script_mode(
    argv: &[BString],
    script_arg: &[u8],
    defines: Vec<CacheDefine>,
    diag_state: DiagState,
    sarif_output: Option<PathBuf>,
    log_level: Option<LogLevel>,
    log_context: bool,
    host: Arc<dyn Host>,
) -> i32 {
    let cwd = host.initial_cwd();
    let script_path = resolve_cli_path(host.as_ref(), script_arg);
    let (cmake_command, cmake_root) = discover_cmake_paths(host.as_ref());

    let cfg = InterpConfig {
        host: Arc::clone(&host),
        mode: RunMode::Script,
        cmake_root,
        cmake_command,
        cwd,
        limits: Limits::default(),
        diag: DiagOptions {
            initial_state: diag_state,
            sarif: sarif_output.is_some(),
        },
        defines,
        script_args: argv.to_vec(),
    };

    let mut interp = Interp::new(cfg, rcmake_commands::register_all);
    interp.set_log_level(log_level);
    if log_context {
        interp.set_log_context(true);
    }

    let outcome = interp.run_script(&script_path);
    if !write_sarif_if_requested(host.as_ref(), sarif_output.as_deref(), &interp) {
        return 1;
    }
    outcome.exit_code
}

fn write_sarif_if_requested(host: &dyn Host, path: Option<&Path>, interp: &Interp) -> bool {
    let Some(sarif_path) = path else {
        return true;
    };
    let json = rcmake_json::write_styled(&interp.diag_engine().sarif_json(), "  ");
    if let Err(err) = host.write(sarif_path, json.as_slice(), WriteMode::Truncate) {
        emit_cli_error(
            host,
            format!(
                "Failed to write SARIF output to \"{}\": {err}",
                sarif_path.display()
            ),
        );
        return false;
    }
    true
}

fn run_list_presets(host: &dyn Host, source_dir: &Path, mode: ListPresetsMode) -> i32 {
    let presets = match rcmake_presets::load(source_dir, host) {
        Ok(p) => p,
        Err(err) => {
            emit_cli_error(host, err.to_string());
            return 1;
        }
    };
    let env = EnvSnapshot::from_host(host);
    let kinds: &[PresetKind] = match mode {
        ListPresetsMode::One(k) => match k {
            PresetKind::Configure => &[PresetKind::Configure],
            PresetKind::Build => &[PresetKind::Build],
            PresetKind::Test => &[PresetKind::Test],
            PresetKind::Package => &[PresetKind::Package],
            PresetKind::Workflow => &[PresetKind::Workflow],
        },
        ListPresetsMode::All => &[
            PresetKind::Configure,
            PresetKind::Build,
            PresetKind::Test,
            PresetKind::Package,
            PresetKind::Workflow,
        ],
    };

    let mut out = String::new();
    let mut printed_any = false;
    for &kind in kinds {
        let visible = presets.list_visible(kind, &env);
        if matches!(mode, ListPresetsMode::All) && visible.is_empty() {
            continue;
        }
        if printed_any {
            out.push('\n');
        }
        printed_any = true;

        let title = match kind {
            PresetKind::Configure => "Available configure presets:",
            PresetKind::Build => "Available build presets:",
            PresetKind::Test => "Available test presets:",
            PresetKind::Package => "Available package presets:",
            PresetKind::Workflow => "Available workflow presets:",
        };
        out.push_str(title);
        out.push_str("\n\n");

        let max_len = visible
            .iter()
            .map(|item| item.name.len())
            .max()
            .unwrap_or(0);
        for item in visible {
            let name = String::from_utf8_lossy(item.name.as_slice());
            if let Some(display) = item.display_name {
                let pad = max_len.saturating_sub(item.name.len()) + 1;
                out.push_str("  \"");
                out.push_str(&name);
                out.push('"');
                for _ in 0..pad {
                    out.push(' ');
                }
                out.push_str("- ");
                out.push_str(&String::from_utf8_lossy(display.as_slice()));
                out.push('\n');
            } else {
                out.push_str(&format!("  \"{name}\"\n"));
            }
        }
    }
    host.write_out(Stream::Stdout, out.as_bytes());
    0
}

fn parse_define(raw: &[u8]) -> Option<CacheDefine> {
    let eq = raw.iter().position(|&b| b == b'=')?;
    let lhs = raw.get(..eq)?;
    if lhs.is_empty() {
        return None;
    }
    let value = BString::from(raw.get(eq + 1..).unwrap_or(&[]));
    if let Some(colon) = lhs.iter().position(|&b| b == b':') {
        let name = BString::from(lhs.get(..colon)?);
        let ty = CacheType::parse(lhs.get(colon + 1..).unwrap_or(&[]));
        Some(CacheDefine { name, ty, value })
    } else {
        Some(CacheDefine {
            name: BString::from(lhs),
            ty: None,
            value,
        })
    }
}

fn apply_warning_flag(flag: &[u8], state: &mut DiagState) -> bool {
    let Some(rest) = flag.strip_prefix(b"-W") else {
        return false;
    };
    let (negated, rest) = match rest.strip_prefix(b"no-") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let (error_level, cat_name) = match rest.strip_prefix(b"error=") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let cat = match cat_name {
        b"dev" | b"author" => DiagCategory::Author,
        b"deprecated" => DiagCategory::Deprecated,
        b"policy" => DiagCategory::Policy,
        b"strict" => DiagCategory::Strict,
        b"uninitialized" => DiagCategory::Uninitialized,
        b"unused-cli" => DiagCategory::UnusedCli,
        _ => return false,
    };
    match (negated, error_level) {
        (false, false) => state.promote(cat, DiagAction::Warn, true),
        (true, false) => state.set(cat, DiagAction::Ignore, true),
        (false, true) => state.promote(cat, DiagAction::SendError, true),
        (true, true) => state.demote(cat, DiagAction::Warn, true),
    }
    true
}

fn resolve_cli_path(host: &dyn Host, arg: &[u8]) -> PathBuf {
    let native = rcmake_host::path::to_native(arg);
    if native.is_absolute() {
        native
    } else {
        host.initial_cwd().join(native)
    }
}

fn discover_cmake_paths(host: &dyn Host) -> (PathBuf, PathBuf) {
    let exe = host
        .current_exe()
        .unwrap_or_else(|_| PathBuf::from("/usr/local/bin/cmake"));
    let real_exe = host.canonicalize(&exe).unwrap_or(exe);
    let bin_dir = real_exe
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
    let cmake_command = bin_dir.join("cmake");
    let cmake_root = bin_dir
        .parent()
        .map(|p| p.join("share/cmake-4.5"))
        .unwrap_or_else(|| PathBuf::from("/usr/local/share/cmake-4.5"));
    (cmake_command, cmake_root)
}

fn build_capabilities_json() -> Value {
    let mut version = BTreeMap::new();
    version.insert(BString::from("isDirty"), Value::Bool(false));
    version.insert(
        BString::from("major"),
        Value::UInt(u64::from(CMAKE_VERSION_MAJOR)),
    );
    version.insert(
        BString::from("minor"),
        Value::UInt(u64::from(CMAKE_VERSION_MINOR)),
    );
    version.insert(
        BString::from("patch"),
        Value::UInt(u64::from(CMAKE_VERSION_PATCH)),
    );
    version.insert(BString::from("string"), Value::string(CMAKE_VERSION_STR));
    version.insert(BString::from("suffix"), Value::string(""));

    let mut root = BTreeMap::new();
    root.insert(BString::from("debugger"), Value::Bool(false));
    root.insert(BString::from("fileApi"), Value::object());
    root.insert(BString::from("generators"), Value::Array(Vec::new()));
    root.insert(BString::from("serverMode"), Value::Bool(false));
    root.insert(BString::from("tls"), Value::Bool(false));
    root.insert(BString::from("version"), Value::Object(version));
    Value::Object(root)
}

fn emit_cli_error(host: &dyn Host, body: impl Into<BString>) {
    let diag = Diagnostic {
        kind: MessageKind::Error,
        effect: Effect::Fatal,
        category: None,
        policy: None,
        location: Location::None,
        body: body.into(),
    };
    let rendered = render(&diag, &RenderOptions::default());
    host.write_out(Stream::Stderr, rendered.as_slice());
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_host::PureHost;

    #[test]
    fn argv0_dispatch_and_version_banner() {
        assert_eq!(tool_from_argv0(OsStr::new("/tmp/bin/cmake")), Tool::Cmake);
        assert_eq!(tool_from_argv0(OsStr::new("ctest")), Tool::Ctest);
        assert_eq!(tool_from_argv0(OsStr::new("/opt/cpack")), Tool::Cpack);
        assert_eq!(tool_from_argv0(OsStr::new("rcmake")), Tool::Cmake);

        let host = Arc::new(PureHost::default());
        let code = run(
            &[BString::from("cmake"), BString::from("--version")],
            host.clone(),
        );
        assert_eq!(code, 0);
        let out = host.stream_bytes(Stream::Stdout);
        assert!(out.starts_with(b"cmake version 4.5.1\n\n"));
    }

    #[test]
    fn script_mode_executes_with_defines() {
        let host = Arc::new(PureHost::default());
        host.seed_file(
            Path::new("/work/hello.cmake"),
            b"message(STATUS \"hi ${WHO}\")\n",
        )
        .unwrap();
        let code = run(
            &[
                BString::from("cmake"),
                BString::from("-DWHO=Rcmake"),
                BString::from("-P"),
                BString::from("hello.cmake"),
            ],
            host.clone(),
        );
        assert_eq!(code, 0);
        assert_eq!(
            host.stream_bytes(Stream::Stdout),
            BString::from("-- hi Rcmake\n")
        );
    }

    #[test]
    fn malformed_define_is_rejected() {
        let host = Arc::new(PureHost::default());
        let code = run(
            &[
                BString::from("cmake"),
                BString::from("-DFOO"),
                BString::from("-P"),
                BString::from("hello.cmake"),
            ],
            host.clone(),
        );
        assert_eq!(code, 1);
        assert!(
            host.stream_bytes(Stream::Stderr)
                .starts_with(b"CMake Error: Parse error in command line argument: FOO\n")
        );
    }
}
