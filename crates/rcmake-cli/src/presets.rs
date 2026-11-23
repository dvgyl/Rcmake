use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_eval::{CacheDefine, LogLevel};
use rcmake_host::{Host, ReadRange};
use rcmake_model::CacheType;
use rcmake_presets::{
    ConfigureSettings, EnvSnapshot, PresetKind, PresetSettings, Presets, Strategy,
};

use crate::options::{self, CacheEdit, Options};
use crate::{emit_cli_error, resolve_cli_path};

const KINDS: [(PresetKind, &str); 5] = [
    (PresetKind::Configure, "configure"),
    (PresetKind::Build, "build"),
    (PresetKind::Test, "test"),
    (PresetKind::Package, "package"),
    (PresetKind::Workflow, "workflow"),
];

fn load(host: &dyn Host, src: &Path, custom: Option<&Path>) -> Result<Presets, i32> {
    let result = if let Some(file) = custom {
        load_custom(host, src, file)
    } else {
        rcmake_presets::load(src, host).map_err(|err| err.message)
    };
    result.map_err(|err| {
        emit_cli_error(
            host,
            format!("Could not read presets from {}:\n{err}", src.display()),
        );
        1
    })
}

fn load_custom(host: &dyn Host, src: &Path, file: &Path) -> Result<Presets, String> {
    let data = host
        .read(
            file,
            ReadRange::All {
                limit: 16 * 1024 * 1024,
            },
        )
        .map_err(|err| {
            if err.kind == std::io::ErrorKind::NotFound {
                format!("File not found: {}", file.display())
            } else {
                format!("{}: {err}", file.display())
            }
        })?;
    let document = rcmake_json::parse(&data, &Default::default())
        .map_err(|err| format!("JSON Parse Error: {}:\n{err}", file.display()))?;
    let values = BTreeMap::from([(src.join("CMakePresets.json"), document)]);
    rcmake_presets::load_from_values(src, host, &values).map_err(|err| err.message)
}

pub(crate) fn list(host: &dyn Host, src: &Path, custom: Option<&Path>, mode: &[u8]) -> i32 {
    let presets = match load(host, src, custom) {
        Ok(presets) => presets,
        Err(code) => return code,
    };
    print_list(host, &presets, &EnvSnapshot::from_host(host), mode);
    0
}

fn print_list(host: &dyn Host, presets: &Presets, env: &EnvSnapshot, mode: &[u8]) {
    let mut output = String::new();
    for (kind, label) in KINDS {
        if mode != b"all" && mode != label.as_bytes() {
            continue;
        }
        let mut entries = presets.list_visible(kind, env);
        if kind == PresetKind::Configure {
            entries.retain(|item| {
                presets
                    .resolve(kind, &item.name, env)
                    .is_ok_and(|resolved| {
                        resolved
                            .generator
                            .is_none_or(|generator| options::is_generator(&generator))
                    })
            });
        }
        if entries.is_empty() {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("Available {label} presets:\n\n"));
        let width = entries
            .iter()
            .map(|entry| entry.name.len())
            .max()
            .unwrap_or(0);
        for item in entries {
            if let Some(display) = item.display_name {
                let pad = width.saturating_sub(item.name.len()) + 1;
                output.push_str(&format!("  \"{}\"{:pad$}- {display}\n", item.name, ""));
            } else {
                output.push_str(&format!("  \"{}\"\n", item.name));
            }
        }
    }
    options::out(host, &output);
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
                print_list(host, &presets, &env, b"configure");
            } else if err.message.contains("unsupported vendor macro") {
                emit_cli_error(
                    host,
                    format!(
                        "Could not evaluate configure preset \"{name}\": Invalid macro expansion"
                    ),
                );
            } else {
                emit_cli_error(
                    host,
                    format!(
                        "Could not read presets from {}:\n{}",
                        src.display(),
                        err.message
                    ),
                );
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
            options::out(host, "debug trycompile on\n");
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
