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

/// Cap the dense reachability bitset at 2 MiB (`4096 * 64 * 8` bytes).
const MAX_BITSET_TARGETS: usize = 4096;

pub(super) fn counts_all(targets: &[Target<'_>]) -> Vec<usize> {
    counts_all_with_limit(targets, MAX_BITSET_TARGETS)
}

fn counts_all_with_limit(targets: &[Target<'_>], max_bitset_targets: usize) -> Vec<usize> {
    let n = targets.len();
    if n == 0 {
        return Vec::new();
    }
    if n > max_bitset_targets {
        return (0..n).map(|index| count(targets, [index])).collect();
    }
    let own: Vec<usize> = targets
        .iter()
        .map(|target| {
            target
                .rules
                .iter()
                .filter(|step| step.progress.is_some())
                .count()
        })
        .collect();
    let words = n.div_ceil(64);
    let mut reach = vec![0_u64; n * words];
    for i in 0..n {
        if let Some(word) = reach.get_mut(i * words + (i / 64)) {
            *word |= 1_u64 << (i % 64);
        }
    }
    let mut state = vec![0_u8; n];
    let mut order = Vec::with_capacity(n);
    let mut stack = Vec::new();
    for start in 0..n {
        if state.get(start).copied() != Some(0) {
            continue;
        }
        stack.push((start, false));
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                if let Some(slot) = state.get_mut(node) {
                    *slot = 2;
                }
                order.push(node);
                continue;
            }
            if state.get(node).copied() != Some(0) {
                continue;
            }
            if let Some(slot) = state.get_mut(node) {
                *slot = 1;
            }
            stack.push((node, true));
            if let Some(target) = targets.get(node) {
                for &dep in &target.dependencies {
                    if state.get(dep).copied() == Some(0) {
                        stack.push((dep, false));
                    }
                }
            }
        }
    }
    for &node in &order {
        let Some(target) = targets.get(node) else {
            continue;
        };
        for &dep in &target.dependencies {
            if dep >= n || dep == node {
                continue;
            }
            let src_base = dep * words;
            let dst_base = node * words;
            for w in 0..words {
                let bits = reach.get(src_base + w).copied().unwrap_or(0);
                if let Some(dst) = reach.get_mut(dst_base + w) {
                    *dst |= bits;
                }
            }
        }
    }
    let mut totals = vec![0_usize; n];
    for i in 0..n {
        let mut total = 0_usize;
        let base = i * words;
        for w in 0..words {
            let mut bits = reach.get(base + w).copied().unwrap_or(0);
            while bits != 0 {
                let tz = bits.trailing_zeros() as usize;
                total += own.get(w * 64 + tz).copied().unwrap_or(0);
                bits &= bits - 1;
            }
        }
        if let Some(slot) = totals.get_mut(i) {
            *slot = total;
        }
    }
    totals
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rcmake_base::BString;
    use rcmake_resolve::BuildStep;

    use super::*;
    use crate::Step;

    fn make_step(action: &BuildStep, progress: Option<usize>) -> Step<'_> {
        Step {
            action,
            goal: BString::default(),
            outputs: Vec::new(),
            symbolic_outputs: Vec::new(),
            revised_invocations: None,
            inputs: Vec::new(),
            depfile: None,
            always: false,
            progress,
            jobserver_aware: false,
            default_comment: None,
        }
    }

    #[test]
    fn counts_all_matches_single_root_count_on_dag() {
        let dummy = BuildStep::Custom(rcmake_resolve::CustomStep::default());
        let make_target = |name: &'static [u8], deps: &[usize], reported: usize| Target {
            name,
            file: BString::default(),
            goal: BString::default(),
            clean_goal: BString::default(),
            dependencies: deps.iter().copied().collect::<BTreeSet<_>>(),
            rules: (0..reported)
                .map(|i| make_step(&dummy, Some(i + 1)))
                .chain(std::iter::once(make_step(&dummy, None)))
                .collect(),
            products: Vec::new(),
            fortran: None,
        };
        // Diamond + chain + disconnected target:
        // 0 -> [1, 2], 1 -> [3], 2 -> [3], 3 -> [], 4 -> [0], 5 -> []
        let targets = vec![
            make_target(b"t0", &[1, 2], 2),
            make_target(b"t1", &[3], 3),
            make_target(b"t2", &[3], 5),
            make_target(b"t3", &[], 7),
            make_target(b"t4", &[0], 11),
            make_target(b"t5", &[], 13),
        ];
        let all = counts_all(&targets);
        let fallback = counts_all_with_limit(&targets, 2);
        let expected: Vec<usize> = (0..targets.len()).map(|i| count(&targets, [i])).collect();
        assert_eq!(all, expected);
        assert_eq!(fallback, expected);
        assert_eq!(all, vec![17, 10, 12, 7, 28, 13]);
        assert!(counts_all(&[]).is_empty());
    }
}
