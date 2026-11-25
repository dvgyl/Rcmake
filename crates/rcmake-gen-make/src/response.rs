use rcmake_gen_common::link_response::{self, POSIX_BUDGET};
use rcmake_resolve::ResponseRole;

use super::{BuildStep, GenError, Host, Path, Target, invalid_graph, shell_invocation};

const OBJECT_BUDGET: usize = 60 * 1024;

pub(super) fn prepare(
    targets: &mut [Target<'_>],
    config: &[u8],
    build: &Path,
    host: &dyn Host,
) -> Result<(), GenError> {
    for target in targets {
        for step in &mut target.rules {
            let invocations = match step.action {
                BuildStep::Link { invocations, .. } => invocations,
                BuildStep::Custom(custom)
                    if custom
                        .invocations
                        .iter()
                        .any(|invocation| invocation.link_response.is_some()) =>
                {
                    &custom.invocations
                }
                _ => continue,
            };
            let mut revised = None;
            for (index, invocation) in invocations.iter().enumerate() {
                let exceeds = shell_invocation(invocation)
                    .is_some_and(|command| command.len() >= POSIX_BUDGET);
                let rewritten = link_response::rewrite(
                    invocation,
                    (&step.goal, config, index),
                    build,
                    |role, length, setting| {
                        setting.unwrap_or(match role {
                            ResponseRole::Objects => exceeds || length >= OBJECT_BUDGET,
                            ResponseRole::Libraries => exceeds,
                        })
                    },
                );
                let effective = if let Some((invocation, files)) = rewritten {
                    for file in files {
                        file.write(host, build)?;
                        step.inputs.push(file.path);
                    }
                    let revised = revised.get_or_insert_with(|| invocations.to_vec());
                    let Some(effective) = revised.get_mut(index) else {
                        continue;
                    };
                    *effective = invocation;
                    effective
                } else {
                    invocation
                };
                if shell_invocation(effective).is_some_and(|command| command.len() >= POSIX_BUDGET)
                {
                    return Err(invalid_graph(format!(
                        "The link command for {} exceeds the shell argument limit. Check CMAKE_<LANG>_RESPONSE_FILE_LINK_FLAG and CMAKE_<LANG>_USE_RESPONSE_FILE_FOR_OBJECTS.",
                        step.goal,
                    )));
                }
            }
            step.revised_invocations = revised;
        }
    }
    Ok(())
}
