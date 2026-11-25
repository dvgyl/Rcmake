//! Policy `CMP0081`: relative paths in the `LINK_DIRECTORIES` target property.

use rcmake_base::BString;
use rcmake_diag::{MessageKind, PolicyId, PolicyStatus};
use rcmake_model::{BT, ProjectModel, Target};

use crate::{ResolveError, cxx_modules::generation_location, sources::SourcePaths};

/// Report the first relative link directory of `target` as `CMP0081` requires.
///
/// With `NEW`, the relative path is an error. Without a setting, it is a policy
/// warning. With `OLD`, the path stays as given.
pub(crate) fn check_relative(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    provider: &Target,
    directories: &[BT<BString>],
) -> Result<(), ResolveError> {
    let Some(relative) = directories.iter().find(|directory| {
        !directory.value.is_empty()
            && !directory.value.starts_with(b"$<")
            && !crate::usage::paths::usage_path_is_full(
                model,
                paths,
                provider.dir,
                &directory.value,
            )
    }) else {
        return Ok(());
    };
    let message = format!(
        "Found relative path while evaluating link directories of \"{}\":\n  \"{}\"\n",
        target.name, relative.value
    );
    if provider.name != target.name {
        let body = format!(
            "Target \"{}\" contains relative path in its INTERFACE_LINK_DIRECTORIES:\n  \"{}\"",
            provider.name, relative.value
        )
        .into();
        paths.emit(
            model,
            MessageKind::Error,
            generation_location(model, target),
            BString::clone(&body),
        );
        return Err(ResolveError::Reported(body));
    }
    let policy = PolicyId::new(81);
    match target.policies.policies.get(policy) {
        PolicyStatus::Old => Ok(()),
        PolicyStatus::Warn => {
            crate::pipeline::policy_warning(model, paths.host, policy, target, &message);
            Ok(())
        }
        PolicyStatus::New => {
            let body = BString::from(message);
            paths.emit(
                model,
                MessageKind::Error,
                generation_location(model, target),
                body.clone(),
            );
            Err(ResolveError::Reported(body))
        }
    }
}
