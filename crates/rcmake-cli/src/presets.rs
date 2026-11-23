use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::{CacheDefine, LogLevel, TraceConfig, TraceFormat as EvalTraceFormat};
use rcmake_host::{Host, Stream, WriteMode};
use rcmake_model::CacheType;
use rcmake_presets::{
    ConfigureSettings, EnvSnapshot, PresetKind, PresetSettings, Presets, ResolvedPreset, Strategy,
    TraceFormat as PresetTraceFormat, TraceMode,
};

use crate::options::{self, CacheEdit, Options};
use crate::{emit_cli_error, resolve_cli_path};

const KINDS: [PresetKind; 5] = [
    PresetKind::Configure,
    PresetKind::Build,
    PresetKind::Test,
    PresetKind::Package,
    PresetKind::Workflow,
];

const INVALID_LIST_MODE: &str = "Invalid value specified for --list-presets.\nValid values are configure, build, test, package, workflow, all, defined, or any of the type values suffixed with -defined. When no value is passed the default is configure.";
const EMPTY_LIST_MODE: &str = "Invalid value used with --list-presets";

#[derive(Clone, Copy)]
pub(crate) struct Listing {
    kind: Option<PresetKind>,
    defined: bool,
}

impl Listing {
    pub(crate) fn available(kind: PresetKind) -> Self {
        Self {
            kind: Some(kind),
            defined: false,
        }
    }

    pub(crate) fn parse(value: Option<&[u8]>) -> Result<Self, &'static str> {
        match value {
            None => Ok(Self::available(PresetKind::Configure)),
            Some(b"") => Err(EMPTY_LIST_MODE),
            Some(b"defined") => Self::for_kind(PresetKind::Configure, value),
            Some(value) => {
                let suffix = value.strip_suffix(b"-defined");
                let kind = match suffix.unwrap_or(value) {
                    b"all" => None,
                    label => Some(
                        KINDS
                            .into_iter()
                            .find(|kind| kind.label().as_bytes() == label)
                            .ok_or(INVALID_LIST_MODE)?,
                    ),
                };
                Ok(Self {
                    kind,
                    defined: suffix.is_some(),
                })
            }
        }
    }

    pub(crate) fn for_kind(kind: PresetKind, value: Option<&[u8]>) -> Result<Self, &'static str> {
        let mut listing = Self::available(kind);
        match value {
            None => {}
            Some(b"defined") => listing.defined = true,
            Some(b"") => return Err(EMPTY_LIST_MODE),
            Some(_) if kind == PresetKind::Workflow => {
                return Err(
                    "Invalid value specified for --list-presets.\nThe only supported value in workflow mode is \"defined\".",
                );
            }
            Some(_) => {
                return Err(
                    "Invalid value specified for --list-presets.\nThe only supported value is \"defined\".",
                );
            }
        }
        Ok(listing)
    }
}

pub(crate) fn load(host: &dyn Host, src: &Path, custom: Option<&Path>) -> Result<Presets, i32> {
    read(host, src, custom).map_err(|message| {
        emit_cli_error(host, message);
        1
    })
}

fn read(host: &dyn Host, src: &Path, custom: Option<&Path>) -> Result<Presets, String> {
    let result = if let Some(file) = custom {
        rcmake_presets::load_file(src, file, host).map_err(|err| err.message)
    } else {
        rcmake_presets::load(src, host).map_err(|err| err.message)
    };
    result.map_err(|err| {
        if err.starts_with("Could not read presets from ") {
            err
        } else {
            format!("Could not read presets from {}:\n{err}", src.display())
        }
    })
}

pub(crate) fn resolve_for_mode(
    host: &dyn Host,
    kind: PresetKind,
    name: &[u8],
    custom: Option<&Path>,
) -> Result<ResolvedPreset, i32> {
    let cwd = host.initial_cwd();
    let presets = load(host, &cwd, custom)?;
    presets
        .resolve(kind, name, &EnvSnapshot::from_host(host))
        .map_err(|err| {
            emit_cli_error(host, err.message);
            1
        })
}

pub(crate) fn list(host: &dyn Host, src: &Path, custom: Option<&Path>, listing: Listing) -> i32 {
    let presets = match load(host, src, custom) {
        Ok(presets) => presets,
        Err(code) => return code,
    };
    print_list(host, &presets, &EnvSnapshot::from_host(host), listing);
    0
}

pub(crate) fn list_for_tool(host: &dyn Host, kind: PresetKind, args: &[BString]) -> Option<i32> {
    if kind == PresetKind::Test && args.first().is_some_and(|arg| arg == "--instrument") {
        return None;
    }
    let tokens = args
        .iter()
        .map(|arg| arg.as_slice())
        .take_while(|arg| *arg != b"--");
    if !tokens
        .clone()
        .any(|arg| options::split_flag(arg, b'=').0 == b"--list-presets")
    {
        return None;
    }

    let mut file = None;
    let mut listing = Listing::available(kind);
    let mut tokens = tokens.peekable();
    while let Some(arg) = tokens.next() {
        let (flag, inline) = options::split_flag(arg, b'=');
        match flag {
            _ if kind == PresetKind::Test
                && (matches!(
                    flag,
                    b"-h"
                        | b"-H"
                        | b"--help"
                        | b"-help"
                        | b"-usage"
                        | b"/?"
                        | b"--version"
                        | b"-version"
                        | b"/V"
                        | b"/version"
                ) || flag.starts_with(b"--help-")) =>
            {
                return None;
            }
            b"--preset" | b"--presets-file" => {
                let value = inline.or_else(|| options::take_value(&mut tokens));
                if let Some(value) = value.filter(|value| !value.is_empty()) {
                    if flag == b"--presets-file" {
                        file = Some(resolve_cli_path(host, value));
                    }
                } else {
                    let flag = String::from_utf8_lossy(flag);
                    if kind == PresetKind::Test {
                        emit_cli_error(host, format!("'{flag}' requires an argument"));
                        return Some(1);
                    }
                    let label = if flag == "--preset" { "preset" } else { "file" };
                    emit_cli_error(host, format!("No {label} specified for {flag}"));
                }
            }
            b"--list-presets" => {
                let value = inline.or_else(|| options::take_value(&mut tokens));
                match Listing::for_kind(kind, value) {
                    Ok(mode) => listing = mode,
                    Err(message) => {
                        emit_cli_error(host, message);
                        return Some(1);
                    }
                }
            }
            _ => {}
        }
    }

    let cwd = host.initial_cwd();
    match read(host, &cwd, file.as_deref()) {
        Ok(presets) => {
            print_list(host, &presets, &EnvSnapshot::from_host(host), listing);
            Some(0)
        }
        Err(message) => {
            let label = if kind == PresetKind::Package {
                "CPack"
            } else {
                "CMake"
            };
            host.write_out(
                Stream::Stderr,
                format!("{label} Error: {message}\n").as_bytes(),
            );
            Some(1)
        }
    }
}

pub(crate) fn print_list(host: &dyn Host, presets: &Presets, env: &EnvSnapshot, listing: Listing) {
    let mut output = String::new();
    for kind in KINDS {
        if listing.kind.is_some_and(|selected| selected != kind) {
            continue;
        }
        let entries: Vec<_> = presets
            .list_defined(kind)
            .filter_map(|preset| {
                let reason = unavailable(presets, env, kind, &preset.name);
                (listing.defined || reason.is_none()).then_some((preset, reason))
            })
            .collect();
        let Some(width) = entries.iter().map(|(preset, _)| preset.name.len()).max() else {
            continue;
        };
        if !output.is_empty() {
            output.push('\n');
        }
        let heading = if listing.defined {
            "Defined"
        } else {
            "Available"
        };
        output.push_str(&format!("{heading} {} presets:\n\n", kind.label()));
        for (index, (preset, reason)) in entries.iter().enumerate() {
            let name = &preset.name;
            output.push_str(&format!("  \"{name}\""));
            if let Some(display) = preset.display_name.as_ref().filter(|name| !name.is_empty()) {
                let pad = width - name.len() + 1;
                output.push_str(&format!("{:pad$}- {display}", ""));
            }
            output.push('\n');
            if let Some(reason) = reason {
                output.push_str(&format!("    Unavailable: {reason}\n"));
                if index + 1 < entries.len() {
                    output.push('\n');
                }
            }
        }
    }
    options::out(host, &output);
}

fn unavailable(
    presets: &Presets,
    env: &EnvSnapshot,
    kind: PresetKind,
    name: &[u8],
) -> Option<String> {
    let resolved = match presets.resolve_definition(kind, name, env) {
        Ok(preset) => preset,
        Err(err) if err.message.starts_with("Cannot use hidden ") => return Some("hidden".into()),
        Err(err) if err.message.starts_with("Cannot use disabled ") => {
            let condition = presets.preset_condition(kind, name);
            return Some(condition.map_or_else(
                || "condition evaluated to false".into(),
                |condition| {
                    format!(
                        "condition evaluated to false: {}",
                        rcmake_json::write_compact(condition)
                    )
                },
            ));
        }
        Err(_) => return Some("unsupported macro expansion".into()),
    };
    if let Some(configure) = resolved.configure_preset
        && let Some(reason) = unavailable(presets, env, PresetKind::Configure, &configure)
    {
        return Some(format!("configure preset \"{configure}\": {reason}"));
    }
    match resolved.settings {
        PresetSettings::Configure(_) => resolved
            .generator
            .filter(|generator| !options::is_generator(generator))
            .map(|generator| format!("generator \"{generator}\" is not available")),
        PresetSettings::Package(settings) => settings
            .generators
            .is_some_and(|names| {
                names
                    .iter()
                    .any(|name| rcmake_cpack::generators::create(name).is_none())
            })
            .then(|| "one or more package generators are not available".into()),
        PresetSettings::Workflow(workflow) => {
            workflow.steps.iter().enumerate().find_map(|(index, step)| {
                let reason = unavailable(presets, env, step.kind, &step.name)?;
                Some(format!(
                    "step {}: {} preset \"{}\": {reason}",
                    index + 1,
                    step.kind.label(),
                    step.name,
                ))
            })
        }
        _ => None,
    }
}

#[derive(Default)]
pub(crate) struct Application {
    pub binary: Option<PathBuf>,
    pub warnings: Vec<BString>,
    pub log: String,
}

pub(crate) fn apply(host: &dyn Host, src: &Path, opts: &mut Options) -> Result<Application, i32> {
    let Some(name) = &opts.preset else {
        return Ok(Application::default());
    };
    let presets = load(host, src, opts.presets_file.as_deref())?;
    let env = EnvSnapshot::from_host(host);
    let resolved = presets
        .resolve(PresetKind::Configure, name, &env)
        .map_err(|err| {
            let reason = if err.message.starts_with("No such configure preset") {
                Some("No such configure preset")
            } else if err.message.starts_with("Cannot use hidden preset") {
                Some("Cannot use hidden configure preset")
            } else if err.message.contains("disabled")
                || err.message.contains("condition evaluates to false")
            {
                Some("Cannot use disabled configure preset")
            } else {
                None
            };
            if let Some(reason) = reason {
                emit_cli_error(host, format!("{reason} in {}: \"{name}\"", src.display()));
                print_list(
                    host,
                    &presets,
                    &env,
                    Listing::available(PresetKind::Configure),
                );
            } else if err.message.contains("unsupported vendor macro") {
                emit_cli_error(
                    host,
                    format!(
                        "Could not evaluate configure preset \"{name}\": Invalid macro expansion"
                    ),
                );
            } else {
                let msg = if err.message.starts_with("Could not read presets from ") {
                    err.message
                } else {
                    format!(
                        "Could not read presets from {}:\n{}",
                        src.display(),
                        err.message
                    )
                };
                emit_cli_error(host, msg);
            }
            1
        })?;
    if opts.generator.is_none() {
        opts.generator = resolved.generator;
    }
    let binary = resolved
        .binary_dir
        .map(|dir| src.join(rcmake_host::path::to_native(&dir)));
    let PresetSettings::Configure(settings) = resolved.settings else {
        return Ok(Application {
            binary,
            ..Application::default()
        });
    };

    let mut definitions = BTreeMap::new();
    if let Some(vars) = &settings.cache_variables {
        for (name, entry) in vars {
            let ty = entry.kind.as_ref().and_then(|ty| CacheType::parse(ty));
            definitions.insert(name.clone(), (ty, entry.value.clone()));
        }
    }
    for (name, value, ty) in [
        (
            "CMAKE_TOOLCHAIN_FILE",
            settings.toolchain_file.as_ref(),
            CacheType::Filepath,
        ),
        (
            "CMAKE_INSTALL_PREFIX",
            settings.install_dir.as_ref(),
            CacheType::Path,
        ),
    ] {
        if let Some(value) = value {
            definitions.insert(name.into(), (Some(ty), value.clone()));
        }
    }
    for edit in &opts.cache_edits {
        if let CacheEdit::Define(def) = edit {
            definitions.remove(&def.name);
        }
    }
    let report: Vec<_> = definitions
        .iter()
        .filter(|(name, _)| {
            !opts.cache_edits.iter().any(|edit| {
                matches!(edit, CacheEdit::Remove(pattern) if crate::configure::matches_glob(pattern, name))
            })
        })
        .collect();
    let mut log = String::new();
    if !report.is_empty() {
        log.push_str("Preset CMake variables:\n\n");
        for (name, (ty, value)) in report {
            log.push_str(&format!("  {name}"));
            if let Some(ty) = ty {
                log.push_str(&format!(":{}", ty.as_str()));
            }
            log.push_str(&format!("=\"{value}\"\n"));
        }
        log.push('\n');
    }
    if resolved.environment.values().any(Option::is_some) {
        log.push_str("Preset environment variables:\n\n");
        for (key, value) in &resolved.environment {
            if let Some(value) = value {
                log.push_str(&format!("  {key}=\"{value}\"\n"));
            }
        }
        log.push('\n');
    }
    if !opts
        .log_level
        .is_some_and(|level| level >= LogLevel::Verbose)
    {
        log.clear();
    }
    for (key, value) in resolved.environment {
        if let Some(value) = value {
            host.env_set(&key, Some(&value));
        }
    }
    opts.cache_edits.splice(
        0..0,
        definitions
            .into_iter()
            .map(|(name, (ty, value))| CacheEdit::Define(CacheDefine { name, ty, value })),
    );
    if opts.graphviz.is_none() {
        opts.graphviz = settings
            .graphviz
            .as_ref()
            .map(|file| resolve_cli_path(host, file));
    }
    for (selection, slot) in [
        (&settings.architecture, &mut opts.architecture),
        (&settings.toolset, &mut opts.toolset),
    ] {
        if slot.is_none()
            && let Some(selection) = selection
            && selection.strategy != Some(Strategy::External)
        {
            *slot = selection.value.clone();
        }
    }
    if let Some(debug) = &settings.debug {
        if debug.output == Some(true) {
            options::out(host, "Running with debug output on.\n");
        }
        if debug.try_compile == Some(true) {
            if !opts.debug_trycompile {
                options::out(host, "debug trycompile on\n");
            }
            opts.debug_trycompile = true;
        }
        if debug.find == Some(true) {
            opts.cache_edits.insert(
                0,
                CacheEdit::Define(CacheDefine {
                    name: "CMAKE_FIND_DEBUG_MODE".into(),
                    ty: Some(CacheType::Bool),
                    value: "ON".into(),
                }),
            );
        }
    }
    if let Some(trace) = &settings.trace
        && opts.trace.is_none()
        && trace.mode != Some(TraceMode::Off)
        && (trace.mode.is_some()
            || trace.format.is_some()
            || trace.source.is_some()
            || trace.redirect.is_some())
    {
        let mut cfg = TraceConfig::default();
        if trace.mode == Some(TraceMode::Expand) {
            cfg.expand = true;
            options::out(
                host,
                "Put cmake in trace mode, but with variables expanded.\n",
            );
        } else if trace.mode == Some(TraceMode::On) {
            options::out(host, "Put cmake in trace mode.\n");
        }
        if let Some(fmt) = trace.format {
            options::out(
                host,
                "Put cmake in trace mode and sets the trace output format.\n",
            );
            cfg.format = match fmt {
                PresetTraceFormat::Human => EvalTraceFormat::Human,
                PresetTraceFormat::JsonV1 => EvalTraceFormat::JsonV1,
            };
        }
        if let Some(sources) = &trace.source {
            cfg.sources.extend(sources.iter().cloned());
        }
        if let Some(redirect) = &trace.redirect {
            let absolute = resolve_cli_path(host, redirect);
            if let Err(err) = host.write(&absolute, b"", WriteMode::Truncate) {
                emit_cli_error(
                    host,
                    format!(
                        "Error opening trace file {}: {err}",
                        String::from_utf8_lossy(redirect)
                    ),
                );
                return Err(1);
            }
            options::out(
                host,
                &format!(
                    "Put cmake in trace mode and redirect trace output to a file instead of stderr.\nTrace will be written to {}\n",
                    absolute.display()
                ),
            );
            cfg.redirect = Some(absolute);
        }
        opts.trace = Some(cfg);
    }
    Ok(Application {
        binary,
        warnings: warning_flags(&settings),
        log,
    })
}

fn warning_flags(settings: &ConfigureSettings) -> Vec<BString> {
    let w = settings.warnings.as_ref();
    let e = settings.errors.as_ref();
    let mut flags = Vec::new();
    for (category, warning, error) in [
        ("author", w.and_then(|w| w.author), e.and_then(|e| e.author)),
        (
            "deprecated",
            w.and_then(|w| w.deprecated),
            e.and_then(|e| e.deprecated),
        ),
        (
            "experimental",
            w.and_then(|w| w.experimental),
            e.and_then(|e| e.experimental),
        ),
        ("policy", w.and_then(|w| w.policy), e.and_then(|e| e.policy)),
        ("strict", w.and_then(|w| w.strict), e.and_then(|e| e.strict)),
        (
            "non-target-directive",
            w.and_then(|w| w.non_target_directive),
            e.and_then(|e| e.non_target_directive),
        ),
        (
            "install-absolute-destination",
            w.and_then(|w| w.install_absolute_destination),
            e.and_then(|e| e.install_absolute_destination),
        ),
        (
            "uninitialized",
            w.and_then(|w| w.uninitialized),
            e.and_then(|e| e.uninitialized),
        ),
        (
            "unused-cli",
            w.and_then(|w| w.unused_cli),
            e.and_then(|e| e.unused_cli),
        ),
    ] {
        if let Some(enabled) = warning {
            let prefix = if enabled { "-W" } else { "-Wno-" };
            flags.push(format!("{prefix}{category}").into());
        }
        if let Some(enabled) = error {
            let prefix = if enabled { "-Werror=" } else { "-Wno-error=" };
            flags.push(format!("{prefix}{category}").into());
        }
    }
    flags
}
