use std::collections::HashSet;

use rcmake_base::ByteVec;
use rcmake_gen_common::GenError;
use rcmake_resolve::BuildStep;

use rcmake_base::shell::quote_always;

use super::{MAKEFILE2, Target, make_recipe_text, recursive_make};

pub(super) fn assign(targets: &mut [Target<'_>]) {
    let mut next = 1;
    for step in targets.iter_mut().flat_map(|target| &mut target.rules) {
        let reports = match step.action {
            BuildStep::Compile { .. } | BuildStep::Link { .. } => true,
            BuildStep::Custom(custom) => {
                custom.comment.as_ref().is_some_and(|text| !text.is_empty())
                    || step.default_comment.is_some()
            }
            _ => false,
        };
        if reports {
            step.progress = Some(next);
            next += 1;
        }
    }
}

pub(super) fn count(targets: &[Target<'_>], roots: impl IntoIterator<Item = usize>) -> usize {
    let mut pending: Vec<_> = roots.into_iter().collect();
    let mut seen = HashSet::new();
    let mut count = 0;
    while let Some(index) = pending.pop() {
        if seen.insert(index)
            && let Some(target) = targets.get(index)
        {
            pending.extend(&target.dependencies);
            count += target
                .rules
                .iter()
                .filter(|step| step.progress.is_some())
                .count();
        }
    }
    count
}

pub(super) fn build(out: &mut Vec<u8>, goal: &[u8], count: Option<usize>) {
    if let Some(count) = count {
        out.push_str(
            format!("\t$(SILENT)$(CMAKE_COMMAND) -E cmake_progress_start CMakeFiles {count}\n")
                .as_bytes(),
        );
    }
    recursive_make(out, MAKEFILE2, goal);
    if count.is_some() {
        out.push_str(b"\t$(SILENT)$(CMAKE_COMMAND) -E cmake_progress_start CMakeFiles 0\n");
    }
}

pub(super) fn report(
    out: &mut Vec<u8>,
    message: &[u8],
    number: Option<usize>,
) -> Result<(), GenError> {
    out.push_str(b"\t@");
    if let Some(number) = number {
        echo(out, number.to_string().as_bytes());
    } else {
        out.push_str(b"printf '%s\\n'");
    }
    arguments(out, message)
}

pub(super) fn complete(out: &mut Vec<u8>, target: &Target<'_>) -> Result<(), GenError> {
    let numbers = target
        .rules
        .iter()
        .filter_map(|step| step.progress)
        .map(|number| number.to_string())
        .collect::<Vec<_>>()
        .join(",");
    out.push_str(b"\t@");
    echo(out, numbers.as_bytes());
    arguments(out, &[b"Built target ".as_slice(), target.name].concat())
}

fn echo(out: &mut Vec<u8>, numbers: &[u8]) {
    out.push_str(b"$(CMAKE_COMMAND) -E cmake_echo_color --progress-dir=CMakeFiles --progress-num=");
    out.push_str(numbers);
}

fn arguments(out: &mut Vec<u8>, message: &[u8]) -> Result<(), GenError> {
    for line in message.split(|byte| *byte == b'\n') {
        out.push(b' ');
        make_recipe_text(out, &quote_always(line))?;
    }
    out.push(b'\n');
    Ok(())
}
