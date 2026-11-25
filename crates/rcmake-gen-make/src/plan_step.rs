use std::path::Path;

use rcmake_base::BString;
use rcmake_gen_common::GenError;
use rcmake_resolve::{BuildStep, GenerationResult};

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
            source,
            compile_input,
            depfile,
            ..
        } => Step {
            action,
            goal: make_path(object),
            outputs: vec![make_path(object)],
            inputs: std::iter::once(compile_input.as_ref().unwrap_or(source))
                .chain(result.compile_inputs.get(object).into_iter().flatten())
                .map(make_path)
                .collect(),
            depfile: depfile.as_ref().map(make_path),
            always: false,
            progress: None,
            jobserver_aware: result.metadata.jobserver_steps.contains(&step_index),
        },
        BuildStep::Link {
            step,
            inputs,
            implicit_inputs,
            ..
        } => Step {
            action,
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
        },
        BuildStep::Custom(custom) => Step {
            action,
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
        },
        BuildStep::DeviceLink { .. } => {
            return Err(GenError::Unimplemented("device linking for Unix Makefiles"));
        }
        BuildStep::Scan { .. } | BuildStep::Collate { .. } => {
            return Err(GenError::Unimplemented(
                "C++ module build steps for Unix Makefiles",
            ));
        }
    };
    Ok(step)
}
