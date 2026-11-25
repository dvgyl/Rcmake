//! CUDA architecture flags and device-link plans.
#![forbid(unsafe_code)]

pub(crate) mod arch;
pub(crate) mod device_link;
pub(crate) mod runtime;

use rcmake_base::{BString, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{ProjectModel, Target};

use super::definition;

pub(crate) fn in_try_compile(model: &ProjectModel) -> bool {
    model
        .global
        .props
        .get(b"IN_TRY_COMPILE")
        .is_some_and(|value| truth::is_on(value))
}

pub(crate) fn compile_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
) -> Vec<BString> {
    let (mut flags, _) = arch::architecture_flags(
        model,
        target,
        arch::ArchPhase::Compile,
        config,
        in_try_compile(model),
    );
    if target
        .props
        .get(b"CUDA_SEPARABLE_COMPILATION")
        .is_some_and(|value| truth::is_on(value))
        && let Some(flag) = definition(model, target.dir, b"_CMAKE_CUDA_RDC_FLAG")
    {
        flags.extend(crate::usage::link_options::shell_arguments(flag));
    }
    flags
}

pub(crate) fn compile_mode(model: &ProjectModel, target: &Target) -> BString {
    let key = [
        (
            b"CUDA_PTX_COMPILATION".as_slice(),
            b"_CMAKE_CUDA_PTX_FLAG".as_slice(),
        ),
        (b"CUDA_CUBIN_COMPILATION", b"_CMAKE_CUDA_CUBIN_FLAG"),
        (b"CUDA_FATBIN_COMPILATION", b"_CMAKE_CUDA_FATBIN_FLAG"),
        (b"CUDA_OPTIX_COMPILATION", b"_CMAKE_CUDA_OPTIX_FLAG"),
    ]
    .into_iter()
    .find_map(|(property, flag)| {
        target
            .props
            .get(property)
            .is_some_and(|value| truth::is_on(value))
            .then_some(flag)
    })
    .unwrap_or(b"_CMAKE_CUDA_WHOLE_FLAG");
    definition(model, target.dir, key)
        .cloned()
        .unwrap_or_else(|| "-c".into())
}

pub(crate) fn output_extension(target: &Target) -> Option<&'static [u8]> {
    [
        (b"CUDA_PTX_COMPILATION".as_slice(), b".ptx".as_slice()),
        (b"CUDA_CUBIN_COMPILATION", b".cubin"),
        (b"CUDA_FATBIN_COMPILATION", b".fatbin"),
        (b"CUDA_OPTIX_COMPILATION", b".optixir"),
    ]
    .into_iter()
    .find_map(|(property, extension)| {
        target
            .props
            .get(property)
            .is_some_and(|value| truth::is_on(value))
            .then_some(extension)
    })
}

/// Evaluate CUDA architecture and device-link options for an IDE target.
///
/// # Errors
///
/// Returns an error if the architecture or a link option is invalid.
pub fn ide_flags(
    model: &ProjectModel,
    id: rcmake_model::TargetId,
    config: &[u8],
    architecture: Option<&BString>,
    device: bool,
) -> Result<Vec<BString>, crate::ResolveError> {
    let config = ConfigId(config.into());
    let Some(original) = model.target(id) else {
        return Ok(Vec::new());
    };
    let mut target = original.clone();
    if let Some(architecture) = architecture {
        target
            .props
            .set("CUDA_ARCHITECTURES".into(), architecture.clone());
    }
    let phase = if device {
        arch::ArchPhase::Link
    } else {
        arch::ArchPhase::Compile
    };
    let (mut flags, diagnostics) =
        arch::architecture_flags(model, &target, phase, &config, in_try_compile(model));
    if let Some(diagnostic) = diagnostics
        .into_iter()
        .find(|diagnostic| diagnostic.kind == rcmake_diag::MessageKind::Error)
    {
        return Err(crate::ResolveError::Message(diagnostic.body));
    }
    if device {
        let context = rcmake_genex::EvalContext {
            config: config.clone(),
            link_language: Some("CUDA".into()),
            traversal: rcmake_genex::UsageTraversal::Link,
            device_link: true,
            ..Default::default()
        };
        let mut usage = crate::usage::UsageResolver::new(model);
        let options = usage
            .collect(id, rcmake_model::UsageField::LinkOptions, &context)?
            .entries
            .into_iter()
            .map(|entry| entry.value)
            .collect::<Vec<_>>();
        flags.extend(device_link::link_options(
            model, &target, b"CUDA", &options, true,
        )?);
        for key in [
            b"CMAKE_CUDA_DEVICE_LINK_OPTIONS_IPO".as_slice(),
            b"CMAKE_CUDA_EXTRA_DEVICE_LINK_FLAGS",
        ] {
            if (key != b"CMAKE_CUDA_DEVICE_LINK_OPTIONS_IPO"
                || arch::ipo_enabled(model, &target, &config, b"CUDA"))
                && let Some(value) = definition(model, target.dir, key)
            {
                flags.extend(crate::usage::link_options::shell_arguments(value));
            }
        }
    }
    Ok(flags)
}
