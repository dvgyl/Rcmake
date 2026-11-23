//! Add the CTest instrumentation launcher to build commands.

use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, ProjectModel, Target};

use super::{definition, shell::direct};
use crate::{Invocation, ResolveError};

/// Return the launcher prefix when the project has an instrumentation query.
///
/// # Errors
///
/// This function does not return errors.
pub fn instrumentation_launcher(
    model: &ProjectModel,
) -> Result<Option<Vec<BString>>, ResolveError> {
    Ok(prefix(model))
}

fn prefix(model: &ProjectModel) -> Option<Vec<BString>> {
    if model.global.instrumentation.is_empty()
        && model
            .global
            .props
            .get(b"_RCMAKE_INSTRUMENTATION_QUERIES")
            .is_none()
    {
        return None;
    }
    let ctest = definition(model, DirId::ROOT, b"CMAKE_CTEST_COMMAND")
        .cloned()
        .unwrap_or_else(|| "ctest".into());
    Some(vec![ctest, "--instrument".into()])
}

pub(super) struct Command<'a> {
    pub role: &'static str,
    pub config: &'a ConfigId,
    pub output: &'a BString,
    pub source: Option<&'a BString>,
    pub language: &'a BString,
}

impl Command<'_> {
    pub(super) fn wrap(
        self,
        model: &ProjectModel,
        target: &Target,
        invocation: Invocation,
    ) -> Invocation {
        let Some(mut args) = prefix(model) else {
            return invocation;
        };
        args.extend([
            "--command-type".into(),
            self.role.into(),
            "--target-name".into(),
            target.name.clone(),
            "--config".into(),
            self.config.0.clone(),
            "--build-dir".into(),
            model.dir(DirId::ROOT).binary_dir.clone(),
            "--output".into(),
            self.output.clone(),
        ]);
        if let Some(source) = self.source {
            args.extend(["--source".into(), source.clone()]);
        }
        args.extend(["--language".into(), self.language.clone(), "--".into()]);
        args.extend(invocation.argv);
        direct(args, invocation.working_dir)
    }
}
