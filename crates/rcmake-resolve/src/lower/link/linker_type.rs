//! Select linker flags from the `LINKER_TYPE` target property.

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target};

use crate::{
    LinkKind, ResolveError, cxx_modules::generation_location, lower::definition,
    usage::UsageResolver,
};

#[derive(Default)]
pub(super) struct LinkerSelection {
    pub(super) flags: Vec<BString>,
    pub(super) executable: Option<BString>,
}

pub(super) fn select(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    kind: LinkKind,
    usage: &mut UsageResolver<'_>,
) -> Result<LinkerSelection, ResolveError> {
    if !matches!(
        kind,
        LinkKind::Executable | LinkKind::SharedLibrary | LinkKind::ModuleLibrary
    ) {
        return Ok(LinkerSelection::default());
    }
    let mode_key = [b"CMAKE_", language, b"_LINK_MODE"].concat();
    let direct = definition(model, target.dir, &mode_key).is_some_and(|mode| mode == "LINKER");
    let linker_type = if let Some(value) = target.props.get(b"LINKER_TYPE")
        && let Some(id) = model.find_target(target.dir, &target.name)
    {
        usage.eval_ide_property(
            id,
            b"LINKER_TYPE",
            value,
            &EvalContext {
                config: config.clone(),
                head: Some(crate::usage::handle(id)),
                link_language: Some(language.into()),
                traversal: rcmake_genex::UsageTraversal::Link,
                ..EvalContext::default()
            },
        )?
    } else {
        target
            .props
            .get(b"LINKER_TYPE")
            .cloned()
            .unwrap_or_default()
    };
    let linker_type = if linker_type.is_empty() {
        BString::from("DEFAULT")
    } else {
        linker_type
    };
    let variable: BString = [
        b"CMAKE_",
        language,
        b"_USING_LINKER_",
        linker_type.as_slice(),
    ]
    .concat()
    .into();
    if let Some(flags) = definition(model, target.dir, &variable) {
        return Ok(if direct {
            LinkerSelection {
                executable: (!flags.is_empty()).then(|| flags.clone()),
                ..LinkerSelection::default()
            }
        } else {
            LinkerSelection {
                flags: list::split(flags, list::Empty::Drop),
                ..LinkerSelection::default()
            }
        });
    }
    if linker_type == "DEFAULT" {
        return Ok(LinkerSelection::default());
    }
    let body: BString = if linker_type.iter().all(u8::is_ascii_uppercase) {
        format!("LINKER_TYPE '{linker_type}' is unknown or not supported by this toolchain.")
    } else {
        format!(
            "LINKER_TYPE '{linker_type}' is unknown.  Did you forget to define the '{variable}' variable?"
        )
    }
    .into();
    if let Some(paths) = usage.source_paths() {
        paths.emit(
            model,
            rcmake_diag::MessageKind::Error,
            generation_location(model, target),
            body.clone(),
        );
    }
    Err(ResolveError::Reported(body))
}
