//! Add instrumentation launchers to lowered custom commands.

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{FileKind, Follow, Host, path};
use rcmake_model::{CustomCommandKind, ProjectModel, TargetEvent, TargetKind};
use rcmake_resolve::{BuildStep, GenerationResult, Invocation};

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
    let build = path::from_native(bin);
    let utility_outputs: BTreeMap<_, _> = model
        .targets()
        .filter(|(_, target)| target.kind == TargetKind::Utility)
        .map(|(_, target)| {
            let dir = path::to_native(&model.dir(target.dir).binary_dir);
            let output = dir.join("CMakeFiles").join(path::to_native(&target.name));
            (
                &target.name,
                path::from_native(output.strip_prefix(bin).unwrap_or(&output)),
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
    for graph in &mut result.graphs {
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
            for output in outputs.iter().chain(byproducts).chain(utility) {
                prefix.extend(["--output-path".into(), output.clone()]);
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
                    wrap(invocation, ctest, &prefix, &shell_prefix);
                }
            }
        }
    }
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
