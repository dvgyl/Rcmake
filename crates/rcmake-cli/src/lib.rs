//! Unified multi-call CLI driver for `rcmake` (`cmake`, `ctest`, `cpack` dispatch via `argv[0]`).

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{BString, CMAKE_VERSION_STR, Limits};
use rcmake_diag::{
    DiagAction, DiagCategory, DiagOptions, DiagState, Diagnostic, Effect, Location, MessageKind,
    RenderOptions, render,
};
use rcmake_eval::{CacheDefine, Interp, InterpConfig, LogLevel, RunMode};
use rcmake_host::{Host, Stream, WriteMode};
use rcmake_model::CacheType;
use rcmake_presets::{EnvSnapshot, PresetKind};

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
    if let Some(first) = rest.first()
        && (first.as_slice() == b"--version" || first.as_slice() == b"-version")
    {
        let banner = format!(
            "{} version {CMAKE_VERSION_STR}\n\nRcmake native Rust drop-in replacement for CMake {CMAKE_VERSION_STR}.\n",
            tool.name()
        );
        host.write_out(Stream::Stdout, banner.as_bytes());
        return 0;
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

fn run_cmake(argv: &[BString], rest: &[BString], host: Arc<dyn Host>) -> i32 {
    let mut defines: Vec<CacheDefine> = Vec::new();
    let mut diag_state = DiagState::default();
    let mut sarif_output: Option<PathBuf> = None;
    let mut log_level: Option<LogLevel> = None;
    let mut log_context = false;
    let mut i = 0usize;

    while let Some(arg) = rest.get(i) {
        let bytes = arg.as_slice();
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
            if let Some(parsed) = parse_define(raw) {
                defines.push(parsed);
            }
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
            let kind = match bytes.strip_prefix(b"--list-presets=") {
                None | Some(b"configure") => PresetKind::Configure,
                Some(b"build") => PresetKind::Build,
                Some(b"test") => PresetKind::Test,
                Some(b"package") => PresetKind::Package,
                Some(b"workflow") => PresetKind::Workflow,
                Some(other) => {
                    emit_cli_error(
                        host.as_ref(),
                        format!("Invalid preset type: {}", String::from_utf8_lossy(other)),
                    );
                    return 1;
                }
            };
            return run_list_presets(host.as_ref(), kind);
        }
        i += 1;
    }

    emit_cli_error(
        host.as_ref(),
        "Requested CLI mode is not yet implemented (scheduled for Milestone 1/2)",
    );
    1
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

    let mut interp = Interp::new(cfg, |_reg| {});
    interp.set_log_level(log_level);
    if log_context {
        interp.set_log_context(true);
    }

    let outcome = interp.run_script(&script_path);
    if let Some(sarif_path) = sarif_output {
        let json = rcmake_json::write_styled(&interp.diag_engine().sarif_json(), "  ");
        let _ = host.write(&sarif_path, json.as_slice(), WriteMode::Truncate);
    }
    outcome.exit_code
}

fn run_list_presets(host: &dyn Host, kind: PresetKind) -> i32 {
    let cwd = host.initial_cwd();
    let presets = match rcmake_presets::load(&cwd, host) {
        Ok(p) => p,
        Err(err) => {
            emit_cli_error(host, err.to_string());
            return 1;
        }
    };
    let env = EnvSnapshot::from_host(host);
    let visible = presets.list_visible(kind, &env);
    let title = match kind {
        PresetKind::Configure => "Available configure presets:",
        PresetKind::Build => "Available build presets:",
        PresetKind::Test => "Available test presets:",
        PresetKind::Package => "Available package presets:",
        PresetKind::Workflow => "Available workflow presets:",
    };
    let mut out = format!("{title}\n\n");
    for item in visible {
        let name = String::from_utf8_lossy(item.name.as_slice());
        if let Some(display) = item.display_name {
            out.push_str(&format!(
                "  \"{name}\" - {}\n",
                String::from_utf8_lossy(display.as_slice())
            ));
        } else {
            out.push_str(&format!("  \"{name}\"\n"));
        }
    }
    host.write_out(Stream::Stdout, out.as_bytes());
    0
}

fn parse_define(raw: &[u8]) -> Option<CacheDefine> {
    let eq = raw.iter().position(|&b| b == b'=')?;
    let lhs = raw.get(..eq)?;
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
}
