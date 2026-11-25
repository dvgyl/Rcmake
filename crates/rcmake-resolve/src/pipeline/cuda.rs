use indexmap::IndexSet;
use rcmake_diag::{MessageKind, RenderOptions};
use rcmake_host::Stream;
use rcmake_model::ProjectModel;

use crate::{
    ResolveError, ResolvedTarget,
    lower::{
        cuda::{self, arch},
        definition,
    },
    sources::SourcePaths,
};

pub(super) fn validate<'a>(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    targets: impl IntoIterator<Item = &'a ResolvedTarget>,
) -> Result<(), ResolveError> {
    let mut checked_dirs = IndexSet::new();
    let mut checked_targets = IndexSet::new();
    for facts in targets {
        if !facts
            .compile_groups
            .iter()
            .any(|group| group.language == "CUDA")
        {
            continue;
        }
        let Some(target) = model.target(facts.id) else {
            continue;
        };
        if !cuda::in_try_compile(model)
            && let Some(value) =
                definition(model, target.dir, b"CMAKE_CUDA_ARCHITECTURES").filter(|value| {
                    !value.is_empty() && target.props.get(b"CUDA_ARCHITECTURES") == Some(*value)
                })
            && checked_dirs.insert(target.dir)
        {
            arch::validate_architecture_variable(value)
                .map_err(|body| paths.target_error(model, target, body))?;
        }
        if !checked_targets.insert(facts.id) {
            continue;
        }
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
    Ok(())
}
