use indexmap::IndexSet;
use rcmake_diag::{MessageKind, RenderOptions};
use rcmake_host::Stream;
use rcmake_model::ProjectModel;

use crate::{
    ResolveError, ResolvedTarget,
    lower::{
        cuda::{self, arch},
        definition, hip,
    },
    sources::SourcePaths,
};

pub(super) fn validate<'a>(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    targets: impl IntoIterator<Item = &'a ResolvedTarget>,
) -> Result<(), ResolveError> {
    let mut checked_dirs = IndexSet::new();
    let mut checked_cuda_targets = IndexSet::new();
    let mut checked_hip_targets = IndexSet::new();
    for facts in targets {
        let uses_cuda = facts
            .compile_groups
            .iter()
            .any(|group| group.language == "CUDA")
            || facts
                .link
                .as_ref()
                .is_some_and(|link| link.language == "CUDA");
        let uses_hip = facts
            .compile_groups
            .iter()
            .any(|group| group.language == "HIP")
            || facts
                .link
                .as_ref()
                .is_some_and(|link| link.language == "HIP");
        if !uses_cuda && !uses_hip {
            continue;
        }
        let Some(target) = model.target(facts.id) else {
            continue;
        };
        if uses_cuda {
            if !cuda::in_try_compile(model)
                && let Some(value) = definition(model, target.dir, b"CMAKE_CUDA_ARCHITECTURES")
                    .filter(|value| {
                        !value.is_empty() && target.props.get(b"CUDA_ARCHITECTURES") == Some(*value)
                    })
                && checked_dirs.insert(target.dir)
            {
                arch::validate_architecture_variable(value)
                    .map_err(|body| paths.target_error(model, target, body))?;
            }
            if checked_cuda_targets.insert(facts.id) {
                let (_, diagnostics) = arch::architecture_flags(
                    model,
                    target,
                    arch::ArchPhase::Compile,
                    &facts.config,
                    cuda::in_try_compile(model),
                );
                for diagnostic in diagnostics {
                    if matches!(diagnostic.kind, MessageKind::Error) {
                        return Err(paths.target_error(model, target, diagnostic.body));
                    }
                    paths.host.write_out(
                        Stream::Stderr,
                        &rcmake_diag::render(&diagnostic, &RenderOptions::default()),
                    );
                }
            }
        }
        if uses_hip && checked_hip_targets.insert(facts.id) {
            let (_, diagnostics) = hip::architecture_flags(
                model,
                target,
                arch::ArchPhase::Compile,
                &facts.config,
                cuda::in_try_compile(model),
            );
            for diagnostic in diagnostics {
                if matches!(diagnostic.kind, MessageKind::Error) {
                    return Err(paths.target_error(model, target, diagnostic.body));
                }
                paths.host.write_out(
                    Stream::Stderr,
                    &rcmake_diag::render(&diagnostic, &RenderOptions::default()),
                );
            }
        }
    }
    Ok(())
}
