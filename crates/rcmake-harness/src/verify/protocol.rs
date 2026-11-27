//! Typed configure-only request protocol between `rcmake verify` and `rcmake-verify`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_host::{Host, path};
use rcmake_json::{ParseBudget, ReadOptions, Value};

const REQUEST_PARSE_LIMIT: usize = 4 * 1024 * 1024;

/// Path-bearing read-only option value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PathArg {
    /// Path relative to the staged source directory.
    SourceRel(BString),
    /// Read-only absolute path outside the source directory.
    ExternalAbsolute(BString),
}

/// Value of a `-D` cache definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DefineValue {
    /// Verbatim value string.
    Literal(BString),
    /// Path relative to the staged source directory.
    SourceRel(BString),
}

/// Value of `--trace-source`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TraceSourceArg {
    /// Path relative to the staged source directory.
    SourceRel(BString),
    /// Absolute path outside the source directory.
    ExternalAbsolute(BString),
    /// Relative source filename or suffix filter (no parent traversal or root).
    RelativeName(BString),
}

/// Closed set of configure-only boolean flags accepted by `rcmake-verify`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SimpleFlag {
    Fresh,
    WarnUninitialized,
    WarnUnusedVars,
    NoWarnUnusedCli,
    CheckSystemVars,
    CompileNoWarningAsError,
    LinkNoWarningAsError,
    DebugOutput,
    DebugTryCompile,
    DebugFind,
    LogContext,
    Trace,
    TraceExpand,
}

impl SimpleFlag {
    fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "fresh" => Self::Fresh,
            "warn-uninitialized" => Self::WarnUninitialized,
            "warn-unused-vars" => Self::WarnUnusedVars,
            "no-warn-unused-cli" => Self::NoWarnUnusedCli,
            "check-system-vars" => Self::CheckSystemVars,
            "compile-no-warning-as-error" => Self::CompileNoWarningAsError,
            "link-no-warning-as-error" => Self::LinkNoWarningAsError,
            "debug-output" => Self::DebugOutput,
            "debug-trycompile" => Self::DebugTryCompile,
            "debug-find" => Self::DebugFind,
            "log-context" => Self::LogContext,
            "trace" => Self::Trace,
            "trace-expand" => Self::TraceExpand,
            _ => return None,
        })
    }

    const fn as_cli_flag(self) -> &'static str {
        match self {
            Self::Fresh => "--fresh",
            Self::WarnUninitialized => "--warn-uninitialized",
            Self::WarnUnusedVars => "--warn-unused-vars",
            Self::NoWarnUnusedCli => "--no-warn-unused-cli",
            Self::CheckSystemVars => "--check-system-vars",
            Self::CompileNoWarningAsError => "--compile-no-warning-as-error",
            Self::LinkNoWarningAsError => "--link-no-warning-as-error",
            Self::DebugOutput => "--debug-output",
            Self::DebugTryCompile => "--debug-trycompile",
            Self::DebugFind => "--debug-find",
            Self::LogContext => "--log-context",
            Self::Trace => "--trace",
            Self::TraceExpand => "--trace-expand",
        }
    }
}

/// Typed configure-only option forwarded from `rcmake verify` to `rcmake-verify`.
///
/// CLI flags are synthesized solely from these variants in the receiver, preventing
/// direct `--request-stdin` callers from injecting non-configure actions, output
/// redirects, or `-S`/`-B` overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigureOption {
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
    Flag(SimpleFlag),
    Warning(BString),
    LogLevel(BString),
    DebugFindPkg(BString),
    DebugFindVar(BString),
    TraceFormat(BString),
    TraceSource(TraceSourceArg),
    ProjectFile(BString),
}

/// Typed configure-only request read from stdin by `rcmake-verify`.
#[derive(Clone, Debug)]
pub(crate) struct VerifyRequest {
    pub(crate) rcmake_exe: PathBuf,
    pub(crate) oracle_exe: Option<PathBuf>,
    pub(crate) source_dir: PathBuf,
    pub(crate) excluded_build_dir: Option<PathBuf>,
    pub(crate) cwd_rel_to_source: Option<BString>,
    pub(crate) args: Vec<ConfigureOption>,
    pub(crate) json: bool,
    pub(crate) request_compile_commands: bool,
    pub(crate) compile_commands_user_disabled: bool,
    pub(crate) generator_hint: Option<BString>,
    pub(crate) external_inputs: Vec<BString>,
    pub(crate) timeout_secs: Option<u64>,
}

impl VerifyRequest {
    pub(crate) fn parse_json(host: &dyn Host, bytes: &[u8]) -> Result<Self, String> {
        let budget = ParseBudget::artifact(REQUEST_PARSE_LIMIT);
        let root = rcmake_json::parse_bounded(bytes, &ReadOptions::default(), &budget)
            .map_err(|err| format!("Invalid verify request JSON: {}", err.message))?;
        let version = root
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| "Missing schema_version in verify request.".to_owned())?;
        if version != 1 {
            return Err(format!(
                "Unsupported verify request schema_version {version}."
            ));
        }

        let req_path = |key: &str| -> Result<PathBuf, String> {
            let raw = root
                .get(key)
                .and_then(Value::as_bytes)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("Missing required field '{key}' in verify request."))?;
            if !is_host_absolute(host, raw) {
                return Err(format!("Field '{key}' must be an absolute path."));
            }
            Ok(path::to_native(raw))
        };

        let rcmake_exe = req_path("rcmake_exe")?;
        let oracle_exe = match root.get("oracle_exe") {
            None | Some(Value::Null) => None,
            Some(val) => {
                let raw = val
                    .as_bytes()
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| "Field 'oracle_exe' must be a non-empty string.".to_owned())?;
                if !is_host_absolute(host, raw) {
                    return Err("Field 'oracle_exe' must be an absolute path.".into());
                }
                Some(path::to_native(raw))
            }
        };
        let source_dir = req_path("source_dir")?;

        let excluded_build_dir = match root.get("excluded_build_dir") {
            None | Some(Value::Null) => None,
            Some(val) => {
                let raw = val
                    .as_bytes()
                    .ok_or_else(|| "Field 'excluded_build_dir' must be a string.".to_owned())?;
                if raw.is_empty() {
                    None
                } else {
                    if !is_host_absolute(host, raw) {
                        return Err("Field 'excluded_build_dir' must be an absolute path.".into());
                    }
                    Some(path::to_native(raw))
                }
            }
        };

        let cwd_rel_to_source = match root.get("cwd_rel_to_source") {
            None | Some(Value::Null) => None,
            Some(val) => {
                let raw = val
                    .as_bytes()
                    .ok_or_else(|| "Field 'cwd_rel_to_source' must be a string.".to_owned())?;
                if !raw.is_empty() {
                    validate_source_relative(raw)?;
                }
                Some(BString::from(raw))
            }
        };

        let json = root.get("json").and_then(Value::as_bool).unwrap_or(false);
        let timeout_secs = match root.get("timeout_secs") {
            None | Some(Value::Null) => None,
            Some(val) => {
                let secs = val
                    .as_u64()
                    .filter(|&s| s > 0)
                    .ok_or_else(|| "Field 'timeout_secs' must be a positive integer.".to_owned())?;
                Some(secs)
            }
        };
        let mut request_compile_commands = root
            .get("request_compile_commands")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let mut compile_commands_user_disabled = root
            .get("compile_commands_user_disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut generator_hint = root
            .get("generator_hint")
            .and_then(Value::as_bytes)
            .filter(|v| !v.is_empty())
            .map(BString::from);

        let mut external_seen: BTreeSet<BString> = BTreeSet::new();
        let mut external_inputs = Vec::new();
        if let Some(items) = root.get("external_inputs").and_then(Value::as_array) {
            for item in items {
                let raw = item
                    .as_bytes()
                    .ok_or_else(|| "Field 'external_inputs' must contain strings.".to_owned())?;
                let val = BString::from(raw);
                if external_seen.insert(val.clone()) {
                    external_inputs.push(val);
                }
            }
        }

        let mut args = Vec::new();
        let mut seen_project_file = false;
        let mut explicit_compdb: Option<bool> = None;

        if let Some(raw_args) = root.get("args") {
            let items = raw_args
                .as_array()
                .ok_or_else(|| "Field 'args' must be an array.".to_owned())?;
            for item in items {
                let opt = parse_configure_option(host, item)?;
                match &opt {
                    ConfigureOption::Generator(g) => {
                        generator_hint = Some(g.clone());
                    }
                    ConfigureOption::ProjectFile(_) => {
                        if seen_project_file {
                            return Err("Multiple project_file options are not allowed.".into());
                        }
                        seen_project_file = true;
                    }
                    ConfigureOption::Define {
                        name,
                        value: DefineValue::Literal(v),
                        ..
                    } if name.as_slice() == b"CMAKE_EXPORT_COMPILE_COMMANDS" => {
                        explicit_compdb = Some(truth::is_on(v));
                    }
                    ConfigureOption::Undefine(pat)
                        if pat.as_slice() == b"CMAKE_EXPORT_COMPILE_COMMANDS" =>
                    {
                        explicit_compdb = None;
                    }
                    ConfigureOption::PresetsFile(PathArg::ExternalAbsolute(p))
                    | ConfigureOption::PreloadScript(PathArg::ExternalAbsolute(p))
                    | ConfigureOption::Toolchain(PathArg::ExternalAbsolute(p))
                    | ConfigureOption::TraceSource(TraceSourceArg::ExternalAbsolute(p))
                        if external_seen.insert(p.clone()) =>
                    {
                        external_inputs.push(p.clone());
                    }
                    _ => {}
                }
                args.push(opt);
            }
        }

        if let Some(enabled) = explicit_compdb {
            compile_commands_user_disabled = !enabled;
            request_compile_commands = false;
        }

        Ok(Self {
            rcmake_exe,
            oracle_exe,
            source_dir,
            excluded_build_dir,
            cwd_rel_to_source,
            args,
            json,
            request_compile_commands,
            compile_commands_user_disabled,
            generator_hint,
            external_inputs,
            timeout_secs,
        })
    }

    pub(crate) fn materialize_args(
        &self,
        staged_src: &Path,
        staged_bin: &Path,
        inject_compile_commands: bool,
    ) -> Vec<BString> {
        let mut out = Vec::with_capacity(self.args.len() * 2 + 6);
        out.push(BString::from("-S"));
        out.push(path::from_native_cmake(staged_src));
        out.push(BString::from("-B"));
        out.push(path::from_native_cmake(staged_bin));

        for opt in &self.args {
            match opt {
                ConfigureOption::Generator(v) => {
                    out.push(BString::from("-G"));
                    out.push(v.clone());
                }
                ConfigureOption::Architecture(v) => {
                    out.push(BString::from("-A"));
                    out.push(v.clone());
                }
                ConfigureOption::Toolset(v) => {
                    out.push(BString::from("-T"));
                    out.push(v.clone());
                }
                ConfigureOption::Preset(v) => {
                    out.push(BString::from("--preset"));
                    out.push(v.clone());
                }
                ConfigureOption::PresetsFile(p) => {
                    out.push(BString::from("--presets-file"));
                    out.push(materialize_path_arg(staged_src, p));
                }
                ConfigureOption::PreloadScript(p) => {
                    out.push(BString::from("-C"));
                    out.push(materialize_path_arg(staged_src, p));
                }
                ConfigureOption::Toolchain(p) => {
                    out.push(BString::from("--toolchain"));
                    out.push(materialize_path_arg(staged_src, p));
                }
                ConfigureOption::InstallPrefix(v) => {
                    out.push(BString::from("--install-prefix"));
                    out.push(v.clone());
                }
                ConfigureOption::Define { name, ty, value } => {
                    let mut combined = BString::from("-D");
                    combined.extend_from_slice(name);
                    if let Some(ty) = ty {
                        combined.push(b':');
                        combined.extend_from_slice(ty);
                    }
                    combined.push(b'=');
                    match value {
                        DefineValue::Literal(v) => combined.extend_from_slice(v),
                        DefineValue::SourceRel(rel) => {
                            let joined = staged_src.join(path::to_native(rel));
                            combined.extend_from_slice(&path::from_native_cmake(&joined));
                        }
                    }
                    out.push(combined);
                }
                ConfigureOption::Undefine(pat) => {
                    out.push(BString::from("-U"));
                    out.push(pat.clone());
                }
                ConfigureOption::Flag(flag) => {
                    out.push(BString::from(flag.as_cli_flag()));
                }
                ConfigureOption::Warning(w) => {
                    let mut combined = BString::from("-W");
                    combined.extend_from_slice(w);
                    out.push(combined);
                }
                ConfigureOption::LogLevel(level) => {
                    let mut combined = BString::from("--log-level=");
                    combined.extend_from_slice(level);
                    out.push(combined);
                }
                ConfigureOption::DebugFindPkg(pkg) => {
                    let mut combined = BString::from("--debug-find-pkg=");
                    combined.extend_from_slice(pkg);
                    out.push(combined);
                }
                ConfigureOption::DebugFindVar(var) => {
                    let mut combined = BString::from("--debug-find-var=");
                    combined.extend_from_slice(var);
                    out.push(combined);
                }
                ConfigureOption::TraceFormat(fmt) => {
                    let mut combined = BString::from("--trace-format=");
                    combined.extend_from_slice(fmt);
                    out.push(combined);
                }
                ConfigureOption::TraceSource(src) => {
                    let mut combined = BString::from("--trace-source=");
                    match src {
                        TraceSourceArg::SourceRel(rel) => {
                            let joined = staged_src.join(path::to_native(rel));
                            combined.extend_from_slice(&path::from_native_cmake(&joined));
                        }
                        TraceSourceArg::ExternalAbsolute(abs) => {
                            combined.extend_from_slice(abs);
                        }
                        TraceSourceArg::RelativeName(rel) => {
                            combined.extend_from_slice(rel);
                        }
                    }
                    out.push(combined);
                }
                ConfigureOption::ProjectFile(name) => {
                    let mut combined = BString::from("--project-file=");
                    combined.extend_from_slice(name);
                    out.push(combined);
                }
            }
        }

        if inject_compile_commands {
            out.push(BString::from("-DCMAKE_EXPORT_COMPILE_COMMANDS:BOOL=ON"));
        }
        out
    }

    pub(crate) fn preset_name(&self) -> Option<&BString> {
        self.args.iter().rev().find_map(|opt| match opt {
            ConfigureOption::Preset(name) => Some(name),
            _ => None,
        })
    }

    pub(crate) fn presets_file(&self) -> Option<&PathArg> {
        self.args.iter().rev().find_map(|opt| match opt {
            ConfigureOption::PresetsFile(path_arg) => Some(path_arg),
            _ => None,
        })
    }
}

fn materialize_path_arg(staged_src: &Path, arg: &PathArg) -> BString {
    match arg {
        PathArg::SourceRel(rel) => path::from_native_cmake(&staged_src.join(path::to_native(rel))),
        PathArg::ExternalAbsolute(abs) => abs.clone(),
    }
}

fn parse_configure_option(host: &dyn Host, item: &Value) -> Result<ConfigureOption, String> {
    let kind = item
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing option kind in verify request.".to_owned())?;

    let req_non_empty = |field: &str| -> Result<BString, String> {
        let bytes = item
            .get(field)
            .and_then(Value::as_bytes)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("Option '{kind}' is missing non-empty field '{field}'."))?;
        reject_nul(bytes, field)?;
        Ok(BString::from(bytes))
    };

    match kind {
        "generator" => Ok(ConfigureOption::Generator(req_non_empty("value")?)),
        "architecture" => Ok(ConfigureOption::Architecture(req_non_empty("value")?)),
        "toolset" => Ok(ConfigureOption::Toolset(req_non_empty("value")?)),
        "preset" => Ok(ConfigureOption::Preset(req_non_empty("value")?)),
        "presets_file" => Ok(ConfigureOption::PresetsFile(parse_path_arg(
            host, item, kind,
        )?)),
        "preload_script" => Ok(ConfigureOption::PreloadScript(parse_path_arg(
            host, item, kind,
        )?)),
        "toolchain" => Ok(ConfigureOption::Toolchain(parse_path_arg(
            host, item, kind,
        )?)),
        "install_prefix" => Ok(ConfigureOption::InstallPrefix(req_non_empty("value")?)),
        "define" => {
            let name = req_non_empty("name")?;
            if name.contains(&b'=') || name.contains(&b':') {
                return Err(format!(
                    "Invalid define variable name '{}' in verify request.",
                    String::from_utf8_lossy(&name)
                ));
            }
            let ty = match item.get("type") {
                None | Some(Value::Null) => None,
                Some(val) => {
                    let bytes = val
                        .as_bytes()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| "Define 'type' must be a non-empty string.".to_owned())?;
                    if !bytes.iter().all(|&b| b.is_ascii_alphabetic() || b == b'_') {
                        return Err(format!(
                            "Invalid define cache type '{}' in verify request.",
                            String::from_utf8_lossy(bytes)
                        ));
                    }
                    Some(BString::from(bytes))
                }
            };
            let value_kind = item
                .get("value_kind")
                .and_then(Value::as_str)
                .unwrap_or("literal");
            let raw_val = item
                .get("value")
                .and_then(Value::as_bytes)
                .ok_or_else(|| "Option 'define' is missing field 'value'.".to_owned())?;
            reject_nul(raw_val, "value")?;
            let value = match value_kind {
                "literal" => DefineValue::Literal(BString::from(raw_val)),
                "source_rel" => {
                    validate_source_relative_nonempty(raw_val)?;
                    DefineValue::SourceRel(BString::from(raw_val))
                }
                other => {
                    return Err(format!("Unknown define value_kind '{other}'."));
                }
            };
            Ok(ConfigureOption::Define { name, ty, value })
        }
        "undefine" => Ok(ConfigureOption::Undefine(req_non_empty("pattern")?)),
        "flag" => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "Option 'flag' is missing string field 'name'.".to_owned())?;
            let flag = SimpleFlag::from_wire(name).ok_or_else(|| {
                format!("Unsupported or disallowed configure flag '{name}' in verify request.")
            })?;
            Ok(ConfigureOption::Flag(flag))
        }
        "warning" => {
            let val = req_non_empty("value")?;
            validate_warning_value(&val)?;
            Ok(ConfigureOption::Warning(val))
        }
        "log_level" => {
            let val = req_non_empty("value")?;
            validate_log_level(&val)?;
            Ok(ConfigureOption::LogLevel(val))
        }
        "debug_find_pkg" => {
            let val = req_non_empty("value")?;
            if val.contains(&b'/') || val.contains(&b'\\') {
                return Err("Option 'debug_find_pkg' must not contain path separators.".into());
            }
            Ok(ConfigureOption::DebugFindPkg(val))
        }
        "debug_find_var" => {
            let val = req_non_empty("value")?;
            if val.contains(&b'/') || val.contains(&b'\\') {
                return Err("Option 'debug_find_var' must not contain path separators.".into());
            }
            Ok(ConfigureOption::DebugFindVar(val))
        }
        "trace_format" => {
            let val = req_non_empty("value")?;
            if !matches!(val.as_slice(), b"human" | b"json-v1") {
                return Err(format!(
                    "Invalid trace_format '{}': expected 'human' or 'json-v1'.",
                    String::from_utf8_lossy(&val)
                ));
            }
            Ok(ConfigureOption::TraceFormat(val))
        }
        "trace_source" => {
            let path_kind = item
                .get("path_kind")
                .and_then(Value::as_str)
                .ok_or_else(|| "Option 'trace_source' is missing field 'path_kind'.".to_owned())?;
            let raw = req_non_empty("value")?;
            match path_kind {
                "source_rel" => {
                    validate_source_relative_nonempty(&raw)?;
                    Ok(ConfigureOption::TraceSource(TraceSourceArg::SourceRel(raw)))
                }
                "external_abs" => {
                    if !is_host_absolute(host, &raw) {
                        return Err(
                            "Option 'trace_source' external_abs must be an absolute path.".into(),
                        );
                    }
                    Ok(ConfigureOption::TraceSource(
                        TraceSourceArg::ExternalAbsolute(raw),
                    ))
                }
                "relative_name" => {
                    validate_source_relative_nonempty(&raw)?;
                    Ok(ConfigureOption::TraceSource(TraceSourceArg::RelativeName(
                        raw,
                    )))
                }
                other => Err(format!("Unknown trace_source path_kind '{other}'.")),
            }
        }
        "project_file" => {
            let val = req_non_empty("value")?;
            validate_plain_filename(&val)?;
            Ok(ConfigureOption::ProjectFile(val))
        }
        other => Err(format!(
            "Unsupported or disallowed verify option kind '{other}'."
        )),
    }
}

fn parse_path_arg(host: &dyn Host, item: &Value, kind: &str) -> Result<PathArg, String> {
    let path_kind = item
        .get("path_kind")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Option '{kind}' is missing field 'path_kind'."))?;
    let raw = item
        .get("value")
        .and_then(Value::as_bytes)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("Option '{kind}' is missing non-empty field 'value'."))?;
    reject_nul(raw, "value")?;
    match path_kind {
        "source_rel" => {
            validate_source_relative_nonempty(raw)?;
            Ok(PathArg::SourceRel(BString::from(raw)))
        }
        "external_abs" => {
            if !is_host_absolute(host, raw) {
                return Err(format!(
                    "Option '{kind}' external_abs path '{}' must be absolute.",
                    String::from_utf8_lossy(raw)
                ));
            }
            Ok(PathArg::ExternalAbsolute(BString::from(raw)))
        }
        other => Err(format!("Unknown path_kind '{other}' for option '{kind}'.")),
    }
}

fn reject_nul(bytes: &[u8], field: &str) -> Result<(), String> {
    if bytes.contains(&0) {
        return Err(format!("Field '{field}' must not contain NUL bytes."));
    }
    Ok(())
}

/// Return whether `raw` is an absolute path for `host`'s operating system.
///
/// Uses `host.system().sysname` rather than compile-time `cfg!(windows)` so
/// POSIX `PureHost` unit tests behave consistently on Windows CI runners.
pub(crate) fn is_host_absolute(host: &dyn Host, raw: &[u8]) -> bool {
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

/// Validate that `rel` stays lexically inside the staged source directory.
/// Empty `rel` is accepted (representing the staged source root).
pub(crate) fn validate_source_relative(rel: &[u8]) -> Result<(), String> {
    if rel.is_empty() {
        return Ok(());
    }
    if rel.contains(&0) {
        return Err("Source-relative path must not contain NUL bytes.".into());
    }
    if rel.starts_with(b"/") || rel.starts_with(b"\\") {
        return Err(format!(
            "Source-relative path '{}' must stay inside the staged source directory.",
            String::from_utf8_lossy(rel)
        ));
    }
    if matches!(rel, [drive, b':', ..] if drive.is_ascii_alphabetic()) {
        return Err(format!(
            "Source-relative path '{}' must not contain a drive prefix.",
            String::from_utf8_lossy(rel)
        ));
    }
    for part in rel.split(|&b| b == b'/' || b == b'\\') {
        if part == b".." {
            return Err(format!(
                "Source-relative path '{}' must stay inside the staged source directory.",
                String::from_utf8_lossy(rel)
            ));
        }
    }
    Ok(())
}

fn validate_source_relative_nonempty(rel: &[u8]) -> Result<(), String> {
    if rel.is_empty() || rel == b"." {
        return Err("Source-relative path must not be empty.".into());
    }
    validate_source_relative(rel)
}

pub(crate) fn validate_plain_filename(name: &[u8]) -> Result<(), String> {
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

fn validate_warning_value(val: &[u8]) -> Result<(), String> {
    let rest = val.strip_prefix(b"no-").unwrap_or(val);
    let name = rest.strip_prefix(b"error=").unwrap_or(rest);
    if !matches!(name, b"dev" | b"deprecated") {
        return Err(format!(
            "Unsupported warning flag '-W{}' in verify request.",
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
            "Invalid log level '{}' in verify request.",
            String::from_utf8_lossy(val)
        ));
    }
    Ok(())
}
