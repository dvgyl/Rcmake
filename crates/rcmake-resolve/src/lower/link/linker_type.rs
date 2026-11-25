//! Select linker flags from the `LINKER_TYPE` target property.

use rcmake_base::{BString, list};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target};

use crate::{
    LinkKind, ResolveError, cxx_modules::generation_location, lower::definition,
    usage::UsageResolver,
};

/// Return the `CMAKE_<LANG>_USING_LINKER_<TYPE>` flags of `target`.
///
/// CMake applies these flags only when the compiler driver links
/// (`CMAKE_<LANG>_LINK_MODE` is unset or `DRIVER`). A type without flags is an
/// error unless it is `DEFAULT`.
///
/// # Errors
///
/// Reports an unknown linker type at the directory of the target.
pub(super) fn linker_type_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    kind: LinkKind,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    if !matches!(
        kind,
        LinkKind::Executable | LinkKind::SharedLibrary | LinkKind::ModuleLibrary
    ) {
        return Ok(Vec::new());
    }
    let mode_key = [b"CMAKE_", language, b"_LINK_MODE"].concat();
    if definition(model, target.dir, &mode_key).is_some_and(|mode| mode != "DRIVER") {
        return Ok(Vec::new());
    }
    let linker_type = match target
        .props
        .get(b"LINKER_TYPE")
        .filter(|value| !value.is_empty())
    {
        Some(value) => evaluate(model, target, config, language, value, usage),
        None => BString::default(),
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
        return Ok(list::split(flags, list::Empty::Drop));
    }
    if linker_type == "DEFAULT" {
        return Ok(Vec::new());
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

fn evaluate(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    value: &[u8],
    usage: &mut UsageResolver<'_>,
) -> BString {
    if !value.windows(2).any(|pair| pair == b"$<") {
        return value.into();
    }
    let handle = model
        .find_target(target.dir, &target.name)
        .map(crate::usage::handle);
    let ctx = EvalContext {
        config: config.clone(),
        head: handle,
        current: handle,
        language: Some(language.into()),
        ..EvalContext::default()
    };
    rcmake_genex::parse(value)
        .and_then(|ast| rcmake_genex::evaluate(&ast, &ctx, &mut usage.host(target.dir)))
        .map(|(value, _)| value)
        .unwrap_or_default()
}
