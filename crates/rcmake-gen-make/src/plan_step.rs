use std::path::Path;

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_resolve::{BuildStep, CustomStep, GenerationResult};

use super::{Step, build_path};

pub(super) fn plan_step<'a>(
    action: &'a BuildStep,
    step_index: usize,
    result: &GenerationResult,
    build_dir: &Path,
) -> Result<Step<'a>, GenError> {
    let make_path = |value: &BString| build_path(value, build_dir);
    let step = match action {
        BuildStep::Compile {
            object,
            byproducts,
            source,
            compile_input,
            depfile,
            ..
        } => Step {
            action,
            revised_invocations: None,
            symbolic_outputs: Vec::new(),
            goal: make_path(object),
            outputs: std::iter::once(object)
                .chain(byproducts)
                .map(make_path)
                .collect(),
            inputs: std::iter::once(compile_input.as_ref().unwrap_or(source))
                .chain(result.compile_inputs.get(object).into_iter().flatten())
                .map(make_path)
                .collect(),
            depfile: depfile.as_ref().map(make_path),
            always: false,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
            default_comment: None,
        },
        BuildStep::Link {
            step,
            inputs,
            implicit_inputs,
            ..
        } => Step {
            action,
            revised_invocations: None,
            symbolic_outputs: Vec::new(),
            goal: make_path(&step.output),
            outputs: vec![make_path(&step.output)],
            inputs: inputs
                .iter()
                .chain(implicit_inputs)
                .map(make_path)
                .collect(),
            depfile: None,
            always: false,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
            default_comment: None,
        },
        BuildStep::Custom(custom) => Step {
            action,
            revised_invocations: None,
            symbolic_outputs: custom.symbolic_outputs.iter().map(make_path).collect(),
            goal: custom.outputs.first().map_or_else(
                || format!("CMakeFiles/rcmake-step-{step_index}").into(),
                make_path,
            ),
            outputs: custom
                .outputs
                .iter()
                .chain(&custom.byproducts)
                .map(make_path)
                .collect(),
            inputs: custom.depends.clone(),
            depfile: custom.depfile.as_ref().map(make_path),
            always: custom.outputs.is_empty() || custom.symbolic,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
            default_comment: default_comment(custom, build_dir),
        },
        BuildStep::DeviceLink { output, inputs, .. } => Step {
            action,
            revised_invocations: None,
            symbolic_outputs: Vec::new(),
            goal: make_path(output),
            outputs: super::cuda::clean_artifacts(action, build_dir),
            inputs: inputs.iter().map(make_path).collect(),
            depfile: None,
            always: false,
            progress: None,
            jobserver_aware: false,
            default_comment: None,
        },
        BuildStep::Scan { .. } | BuildStep::Collate { .. } => {
            return Err(GenError::Unimplemented(
                "C++ module build steps for Unix Makefiles",
            ));
        }
    };
    Ok(step)
}

/// Build the comment of a custom command that has outputs and no `COMMENT`,
/// as CMake does: `Generating <output>, <output>`.
pub(super) fn default_comment(custom: &CustomStep, build_dir: &Path) -> Option<BString> {
    if custom.comment.is_some() || custom.symbolic || custom.outputs.is_empty() {
        return None;
    }
    let mut comment = BString::from("Generating ");
    for (index, output) in custom.outputs.iter().enumerate() {
        if index > 0 {
            comment.extend_from_slice(b", ");
        }
        comment.extend_from_slice(&build_path(output, build_dir));
    }
    Some(comment)
}
