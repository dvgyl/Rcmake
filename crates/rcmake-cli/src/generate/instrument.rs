//! Add instrumentation launchers to lowered custom commands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_host::{FileKind, Follow, Host, WriteMode, path};
use rcmake_model::{CustomCommandKind, ProjectModel, TargetEvent, TargetKind};
use rcmake_resolve::{BuildStep, GenerationResult, Invocation};

/// Spill custom-command output lists to a file once inline arguments exceed
/// this byte budget or item count so Windows `cmd.exe`/`CreateProcess` and
/// POSIX `ARG_MAX` limits are never exceeded.
const INLINE_OUTPUT_BYTE_LIMIT: usize = 2048;
const INLINE_OUTPUT_COUNT_LIMIT: usize = 32;

pub(super) fn wrap_custom_commands(
    result: &mut GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
) {
    let Some(ctest) = result.metadata.ctest_command.as_ref() else {
        return;
    };
    let launcher = if host
        .metadata(&path::to_native(ctest), Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
    {
        vec![ctest.clone(), "--instrument".into()]
    } else if let Some(cmake) = &result.metadata.cmake_command {
        vec![cmake.clone(), "-E".into(), "__instrument".into()]
    } else {
        return;
    };
    let build = path::from_native_cmake(bin);
    let utility_outputs: BTreeMap<_, _> = model
        .targets()
        .filter(|(_, target)| target.kind == TargetKind::Utility)
        .map(|(_, target)| {
            let dir = path::to_native(&model.dir(target.dir).binary_dir);
            let output = dir.join("CMakeFiles").join(path::to_native(&target.name));
            (
                &target.name,
                path::from_native_cmake(output.strip_prefix(bin).unwrap_or(&output)),
            )
        })
        .collect();
    let mut events = BTreeMap::<_, (usize, usize)>::new();
    for (_, command) in model.commands() {
        if let CustomCommandKind::TargetEvent { target, event } = command.kind
            && let Some(target) = model.target(target)
        {
            let (before, after) = events.entry(&target.name).or_default();
            let count = command
                .command_lines
                .iter()
                .filter(|line| !line.is_empty())
                .count();
            match event {
                TargetEvent::PreBuild | TargetEvent::PreLink => *before += count,
                TargetEvent::PostBuild => *after += count,
            }
        }
    }
    let mut written_output_files = BTreeSet::new();
    let mut existing_generated: BTreeSet<BString> =
        result.metadata.generated_files.iter().cloned().collect();
    let ctest = ctest.clone();
    let graphs = &mut result.graphs;
    let generated_files = &mut result.metadata.generated_files;
    for graph in graphs {
        let mut owners = vec![None; graph.steps.len()];
        for target in &graph.targets {
            for step in &target.steps {
                if let Some(owner) = owners.get_mut(*step) {
                    *owner = Some(&target.name);
                }
            }
        }
        for (step, owner) in graph.steps.iter_mut().zip(owners) {
            let (commands, outputs, byproducts, events_only) = match step {
                BuildStep::Custom(step) => (
                    &mut step.invocations,
                    step.outputs.as_slice(),
                    step.byproducts.as_slice(),
                    None,
                ),
                BuildStep::Link { invocations, .. } => {
                    let Some(counts) = owner.and_then(|target| events.get(target)).copied() else {
                        continue;
                    };
                    (invocations, &[][..], &[][..], Some(counts))
                }
                _ => continue,
            };
            let attached = owner.filter(|_| outputs.is_empty() && byproducts.is_empty());
            let utility = attached.and_then(|target| utility_outputs.get(target));
            let target = attached.filter(|_| utility.is_none());
            let mut prefix = launcher.clone();
            prefix.extend([
                "--command-type".into(),
                "custom".into(),
                "--build-dir".into(),
                build.clone(),
                "--config".into(),
                graph.config.0.clone(),
            ]);
            if let Some(target) = target {
                prefix.extend(["--target-name".into(), target.clone()]);
            }
            let all_outputs: Vec<&BString> =
                outputs.iter().chain(byproducts).chain(utility).collect();
            let inline_bytes: usize = all_outputs
                .iter()
                .map(|output| output.len().saturating_add(18))
                .sum();
            let spilled = if inline_bytes > INLINE_OUTPUT_BYTE_LIMIT
                || all_outputs.len() > INLINE_OUTPUT_COUNT_LIMIT
            {
                write_outputs_file(
                    host,
                    bin,
                    &all_outputs,
                    &mut written_output_files,
                    &mut existing_generated,
                    generated_files,
                )
            } else {
                None
            };
            if let Some(file) = spilled {
                prefix.extend(["--output-as-file-name".into(), file]);
            } else {
                for output in all_outputs {
                    prefix.extend(["--output-path".into(), output.clone()]);
                }
            }
            prefix.push("--".into());
            let mut shell_prefix = BString::default();
            for arg in &prefix {
                append_shell_argument(&mut shell_prefix, arg);
                shell_prefix.push(b' ');
            }
            let command_count = commands.len();
            for (index, invocation) in commands.iter_mut().enumerate() {
                if events_only.is_none_or(|(before, after)| {
                    index < before || index >= command_count.saturating_sub(after)
                }) {
                    wrap(invocation, &ctest, &prefix, &shell_prefix);
                }
            }
        }
    }
}

fn write_outputs_file(
    host: &dyn Host,
    bin: &Path,
    outputs: &[&BString],
    written: &mut BTreeSet<String>,
    existing_generated: &mut BTreeSet<BString>,
    generated_files: &mut Vec<BString>,
) -> Option<BString> {
    let needs_nul = outputs.iter().any(|output| {
        output
            .iter()
            .any(|&byte| matches!(byte, b',' | b'\n' | b'\r' | b'\0'))
    });
    let mut payload = Vec::new();
    let delimiter = if needs_nul { b'\0' } else { b',' };
    for (index, output) in outputs.iter().enumerate() {
        if index > 0 {
            payload.push(delimiter);
        }
        payload.extend_from_slice(output);
    }
    if needs_nul && outputs.len() == 1 {
        payload.push(b'\0');
    }
    let digest = HashAlgo::Sha256.hex_digest(&payload);
    let relative = BString::from(format!("CMakeFiles/c/{digest}"));
    let dir = bin.join("CMakeFiles").join("c");
    let full = dir.join(&digest);
    if !written.contains(&digest) {
        host.create_dir_all(&dir).ok()?;
        host.write(&full, &payload, WriteMode::Truncate).ok()?;
        written.insert(digest);
    }
    if existing_generated.insert(relative.clone()) {
        generated_files.push(relative);
    }
    Some(path::from_native_cmake(&full))
}

fn wrap(invocation: &mut Invocation, ctest: &BString, prefix: &[BString], shell_prefix: &[u8]) {
    if invocation.argv.is_empty()
        || invocation
            .argv
            .starts_with(&[ctest.clone(), "--instrument".into()])
    {
        return;
    }
    invocation.argv.splice(0..0, prefix.iter().cloned());
    invocation.shell = [shell_prefix, invocation.shell.as_slice()].concat().into();
}

fn append_shell_argument(shell: &mut BString, value: &[u8]) {
    if !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(byte))
    {
        shell.extend_from_slice(value);
        return;
    }
    shell.push(b'\'');
    for &byte in value {
        if byte == b'\'' {
            shell.extend_from_slice(b"'\\''");
        } else {
            shell.push(byte);
        }
    }
    shell.push(b'\'');
}
