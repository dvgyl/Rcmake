use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target};

use crate::ResolveError;
use crate::lower::definition;
use crate::usage::{UsageResolver, handle};

pub(crate) fn debug_options(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
) -> Result<Vec<BString>, ResolveError> {
    let Some(default) = definition(
        model,
        target.dir,
        b"CMAKE_MSVC_DEBUG_INFORMATION_FORMAT_DEFAULT",
    )
    .filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Ok(Vec::new());
    };
    let property = b"MSVC_DEBUG_INFORMATION_FORMAT";
    let value = target.props.get(property).unwrap_or(default);
    let context = EvalContext {
        config: config.clone(),
        language: Some(language.into()),
        head: Some(handle(id)),
        current: Some(handle(id)),
        ..Default::default()
    };
    let mut usage = UsageResolver::new(model);
    let format = usage.eval_ide_property(id, property, value, &context)?;
    let options = definition(
        model,
        target.dir,
        &[
            b"CMAKE_",
            language,
            b"_COMPILE_OPTIONS_MSVC_DEBUG_INFORMATION_FORMAT_",
            &format,
        ]
        .concat(),
    );
    Ok(options.map_or_else(Vec::new, |options| list::split(options, list::Empty::Drop)))
}
