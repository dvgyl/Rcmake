//! Run the configure, build, test, and package steps of a workflow preset.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, Stream};
use rcmake_instrument::{Hook, hooks::run_hook};
use rcmake_presets::{
    EnvSnapshot, PresetKind, Presets, PresetsError, ResolvedPreset, WorkflowStep,
};

use crate::{options, resolve_cli_path};

const USAGE: &str = "Usage: cmake --workflow <options>\n\
Options:\n  --preset <preset>        = Workflow preset to execute.\n  \
--presets-file <file>    = Path to a presets file.\n  \
--list-presets[=defined] = List available or all defined workflow presets.\n  \
--fresh                  = Configure a fresh build tree, removing any existing cache file.\n";
const FLAGS: &[&[u8]] = &[
    b"--preset",
    b"--presets-file",
    b"--list-presets",
    b"--fresh",
];

enum Listing {
    Available,
    Defined,
}

#[derive(Default)]
struct Options {
    preset: Option<BString>,
    file: Option<PathBuf>,
    listing: Option<Listing>,
    fresh: bool,
}

impl Options {
    fn parse(args: &[BString], host: &dyn Host) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.iter().enumerate().peekable();
        while let Some((index, arg)) = args.next() {
            let mut parts = arg.splitn(2, |&byte| byte == b'=');
            let flag = parts.next().unwrap_or_default();
            let inline = parts.next();
            let mut value = || {
                inline
                    .or_else(|| {
                        args.next_if(|(_, arg)| !arg.starts_with(b"-"))
                            .map(|(_, arg)| arg.as_slice())
                    })
                    .filter(|value| !value.is_empty())
            };
            match flag {
                b"--preset" => {
                    options.preset = Some(
                        value()
                            .ok_or("CMake Error: No preset specified for --preset")?
                            .into(),
                    );
                }
                b"--presets-file" => {
                    let file =
                        value().ok_or("CMake Error: No file specified for --presets-file")?;
                    options.file = Some(resolve_cli_path(host, file));
                }
                b"--list-presets" => {
                    options.listing = Some(match value() {
                        None if inline.is_none() => Listing::Available,
                        None => return Err("CMake Error: Invalid value used with --list-presets".into()),
                        Some(b"defined") => Listing::Defined,
                        Some(_) => return Err("Invalid value specified for --list-presets.\nThe only supported value in workflow mode is \"defined\".".into()),
                    });
                }
                b"--fresh" if inline.is_none() => options.fresh = true,
                _ if index == 0 && !arg.starts_with(b"-") && !arg.is_empty() => {
                    options.preset = Some(arg.clone());
                }
                _ if index == 0 => {
                    return Err("CMake Error: No preset specified for --preset".into());
                }
                _ => return Err(options::unknown_mode_option(arg, FLAGS)),
            }
        }
        if options.preset.is_none() && options.listing.is_none() {
            return Err(String::new());
        }
        Ok(options)
    }
}

fn error(host: &dyn Host, message: &str) {
    host.write_out(
        Stream::Stderr,
        format!("CMake Error: {message}\n").as_bytes(),
    );
}

/// Run the workflow and return the exit code of the first step that fails.
pub(crate) fn run(args: &[BString], host: Arc<dyn Host>) -> i32 {
    let options = match Options::parse(args, host.as_ref()) {
        Ok(options) => options,
        Err(message) => {
            if !message.is_empty() {
                host.write_out(Stream::Stderr, format!("{message}\n").as_bytes());
            }
            host.write_out(Stream::Stderr, USAGE.as_bytes());
            return 1;
        }
    };
    let cwd = host.initial_cwd();
    let presets = match &options.file {
        Some(file) => rcmake_presets::load_file(&cwd, file, host.as_ref()),
        None => rcmake_presets::load(&cwd, host.as_ref()),
    };
    let presets = match presets {
        Ok(presets) => presets,
        Err(err) => {
            let message = if err.message.starts_with("Could not read presets from ") {
                err.message
            } else {
                format!("Could not read presets from {}:\n{err}", cwd.display())
            };
            error(host.as_ref(), &message);
            return 1;
        }
    };
    let env = EnvSnapshot::from_host(host.as_ref());
    if let Some(listing) = &options.listing {
        print_presets(host.as_ref(), &presets, &env, listing);
        return 0;
    }
    let Some(name) = &options.preset else {
        return 1;
    };
    let Some((_, workflow)) = presets
        .workflow_definitions()
        .find(|(preset, _)| &preset.name == name)
    else {
        error(
            host.as_ref(),
            &format!("No such workflow preset in {}: \"{name}\"", cwd.display()),
        );
        print_presets(host.as_ref(), &presets, &env, &Listing::Available);
        return 1;
    };
    let binary_dir = match validate(&workflow.steps, &presets, &env, &cwd) {
        Ok(dir) => dir,
        Err(message) => {
            error(host.as_ref(), &message);
            return 1;
        }
    };

    let mut exit = 0;
    for (index, step) in workflow.steps.iter().enumerate() {
        let prefix = if index == 0 { "" } else { "\n" };
        let kind = step.kind.label();
        let name = &step.name;
        host.write_out(
            Stream::Stdout,
            format!(
                "{prefix}Executing workflow step {} of {}: {kind} preset \"{name}\"\n\n",
                index + 1,
                workflow.steps.len()
            )
            .as_bytes(),
        );
        exit = run_step(step, &options, &host, &env);
        if exit != 0 {
            error(
                host.as_ref(),
                &format!(
                    "Workflow step {} failed with exit code {exit}",
                    step_label(step)
                ),
            );
            break;
        }
    }
    if let Err(err) = run_hook(host.as_ref(), &binary_dir, Hook::PostCMakeWorkflow) {
        host.write_out(Stream::Stderr, format!("CMake Warning: {err}\n").as_bytes());
    }
    exit
}

fn step_label(step: &WorkflowStep) -> String {
    format!("of type \"{}\" named \"{}\"", step.kind.label(), step.name)
}

fn validate(
    steps: &[WorkflowStep],
    presets: &Presets,
    env: &EnvSnapshot,
    cwd: &Path,
) -> Result<PathBuf, String> {
    let mut binary_dir = cwd.to_path_buf();
    for step in steps {
        let preset = presets.resolve(step.kind, &step.name, env).map_err(|err| {
            let label = step_label(step);
            let reason = match err.message.as_str() {
                message if message.starts_with("Cannot use hidden ") => {
                    "Cannot use hidden preset for"
                }
                message if message.starts_with("Cannot use disabled ") => {
                    "Cannot use disabled preset for"
                }
                message if message.starts_with("No such ") => "No such preset for",
                _ => {
                    return format!(
                        "Could not evaluate workflow step {label}: Invalid macro expansion"
                    );
                }
            };
            format!("{reason} workflow step {label} in {}", cwd.display())
        })?;
        if step.kind == PresetKind::Configure
            && let Some(dir) = preset.binary_dir
        {
            binary_dir = cwd.join(rcmake_host::path::to_native(&dir));
        }
    }
    Ok(binary_dir)
}

fn print_presets(host: &dyn Host, presets: &Presets, env: &EnvSnapshot, listing: &Listing) {
    let defined = matches!(listing, Listing::Defined);
    let entries: Vec<_> = presets
        .workflow_definitions()
        .filter_map(|(preset, workflow)| {
            let reason = workflow.steps.iter().enumerate().find_map(|(index, step)| {
                let reason = unavailable(presets, env, step)?;
                Some(format!(
                    "step {}: {} preset \"{}\": {reason}",
                    index + 1,
                    step.kind.label(),
                    step.name
                ))
            });
            (defined || reason.is_none()).then_some((preset, reason))
        })
        .collect();
    let Some(width) = entries.iter().map(|(preset, _)| preset.name.len()).max() else {
        return;
    };
    let heading = if defined { "Defined" } else { "Available" };
    let mut output = format!("{heading} workflow presets:\n\n");
    for (index, (preset, reason)) in entries.iter().enumerate() {
        let name = &preset.name;
        output.push_str(&format!("  \"{name}\""));
        if let Some(display) = preset
            .display_name
            .as_ref()
            .filter(|display| !display.is_empty())
        {
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
    host.write_out(Stream::Stdout, output.as_bytes());
}

fn unavailable(presets: &Presets, env: &EnvSnapshot, step: &WorkflowStep) -> Option<String> {
    match presets.resolve(step.kind, &step.name, env) {
        Ok(ResolvedPreset {
            generator: Some(generator),
            ..
        }) if step.kind == PresetKind::Configure && !options::is_generator(&generator) => {
            Some(format!("generator \"{generator}\" is not available"))
        }
        Ok(_) => None,
        Err(PresetsError { message }) if message.starts_with("Cannot use hidden ") => {
            Some("hidden".into())
        }
        Err(PresetsError { message }) if message.starts_with("Cannot use disabled ") => {
            Some(presets.preset_condition(step.kind, &step.name).map_or_else(
                || "condition evaluated to false".into(),
                |condition| {
                    format!(
                        "condition evaluated to false: {}",
                        rcmake_json::write_compact(condition)
                    )
                },
            ))
        }
        Err(_) => Some("unsupported macro expansion".into()),
    }
}

fn run_step(
    step: &WorkflowStep,
    options: &Options,
    host: &Arc<dyn Host>,
    env: &EnvSnapshot,
) -> i32 {
    let mut args = vec![BString::from("--preset"), step.name.clone()];
    if let Some(file) = &options.file {
        args.push("--presets-file".into());
        args.push(rcmake_host::path::from_native(file));
    }
    match step.kind {
        PresetKind::Configure => {
            if options.fresh {
                args.push("--fresh".into());
            }
            args.insert(0, "cmake".into());
            let exit = crate::run(&args, Arc::clone(host));
            // Configure can change the environment through presets and set(ENV).
            // Later steps start with the caller's environment.
            restore_environment(host.as_ref(), &env.vars);
            exit
        }
        PresetKind::Build => {
            let mut argv = Vec::with_capacity(args.len() + 2);
            argv.push("cmake".into());
            argv.push("--build".into());
            argv.extend(args.iter().cloned());
            crate::build::run(&argv, &args, host.as_ref())
        }
        PresetKind::Test => {
            args.insert(0, "ctest".into());
            rcmake_ctest::main(&args, Arc::clone(host))
        }
        PresetKind::Package => {
            args.insert(0, "cpack".into());
            rcmake_cpack::main(&args, Arc::clone(host))
        }
        PresetKind::Workflow => 1,
    }
}

fn restore_environment(host: &dyn Host, original: &BTreeMap<BString, BString>) {
    let current: BTreeMap<_, _> = host.env_all().into_iter().collect();
    for name in current.keys().filter(|name| !original.contains_key(*name)) {
        host.env_set(name, None);
    }
    for (name, value) in original
        .iter()
        .filter(|(name, value)| current.get(*name) != Some(*value))
    {
        host.env_set(name, Some(value));
    }
}
