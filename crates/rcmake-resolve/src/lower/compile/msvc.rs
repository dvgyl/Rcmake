use indexmap::IndexSet;
use rcmake_base::{BString, list};
use rcmake_diag::MessageKind;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel, Target};

use crate::ResolveError;
use crate::lower::definition;
use crate::usage::{UsageResolver, handle};

pub(crate) fn targets_msvc_abi(model: &ProjectModel, dir: DirId, language: &[u8]) -> bool {
    let is_msvc = |lang: &[u8]| {
        [b"_COMPILER_ID".as_slice(), b"_SIMULATE_ID"]
            .into_iter()
            .any(|suffix| {
                definition(model, dir, &[b"CMAKE_", lang, suffix].concat())
                    .is_some_and(|id| id == b"MSVC")
            })
    };
    if language == b"RC"
        && definition(model, dir, b"CMAKE_RC_COMPILER")
            .is_some_and(|compiler| compiler.windows(7).any(|part| part == b"llvm-rc"))
    {
        if definition(model, dir, b"CMAKE_C_COMPILER_ID").is_some_and(|id| !id.is_empty()) {
            return is_msvc(b"C");
        }
        return is_msvc(b"CXX");
    }
    is_msvc(language)
}

fn eval_msvc_property(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    property: &[u8],
    default: &BString,
    usage: Option<&mut UsageResolver<'_>>,
) -> Result<Option<BString>, ResolveError> {
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Ok(None);
    };
    let value = target.props.get(property).unwrap_or(default);
    let context = EvalContext {
        config: config.clone(),
        language: Some(language.into()),
        head: Some(handle(id)),
        current: Some(handle(id)),
        ..Default::default()
    };
    let evaluated = match usage {
        Some(usage) => usage.eval_ide_property(id, property, value, &context)?,
        None => UsageResolver::new(model).eval_ide_property(id, property, value, &context)?,
    };
    Ok(Some(evaluated))
}

fn report_msvc_error(
    model: &ProjectModel,
    target: &Target,
    usage: Option<&UsageResolver<'_>>,
    body: BString,
) -> ResolveError {
    if let Some(paths) = usage.and_then(UsageResolver::source_paths) {
        paths.emit(
            model,
            MessageKind::Error,
            crate::cxx_modules::generation_location(model, target),
            body.clone(),
        );
        ResolveError::Reported(body)
    } else {
        ResolveError::Message(body)
    }
}

pub(crate) fn debug_options(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    mut usage: Option<&mut UsageResolver<'_>>,
) -> Result<Vec<BString>, ResolveError> {
    let Some(default) = definition(
        model,
        target.dir,
        b"CMAKE_MSVC_DEBUG_INFORMATION_FORMAT_DEFAULT",
    )
    .filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let Some(format) = eval_msvc_property(
        model,
        target,
        config,
        language,
        b"MSVC_DEBUG_INFORMATION_FORMAT",
        default,
        usage.as_deref_mut(),
    )?
    else {
        return Ok(Vec::new());
    };
    if format.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(options) = definition(
        model,
        target.dir,
        &[
            b"CMAKE_",
            language,
            b"_COMPILE_OPTIONS_MSVC_DEBUG_INFORMATION_FORMAT_",
            &format,
        ]
        .concat(),
    ) {
        return Ok(list::split(options, list::Empty::Drop));
    }
    if targets_msvc_abi(model, target.dir, language) {
        let language = BString::from(language);
        let body: BString = format!(
            "MSVC_DEBUG_INFORMATION_FORMAT value '{format}' not known for this {language} compiler."
        )
        .into();
        return Err(report_msvc_error(model, target, usage.as_deref(), body));
    }
    Ok(Vec::new())
}

pub(crate) fn runtime_checks(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    mut usage: Option<&mut UsageResolver<'_>>,
) -> Result<Vec<BString>, ResolveError> {
    let Some(default) = definition(model, target.dir, b"CMAKE_MSVC_RUNTIME_CHECKS_DEFAULT")
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let Some(evaluated) = eval_msvc_property(
        model,
        target,
        config,
        language,
        b"MSVC_RUNTIME_CHECKS",
        default,
        usage.as_deref_mut(),
    )?
    else {
        return Ok(Vec::new());
    };
    let mut checks: IndexSet<BString> = list::split(&evaluated, list::Empty::Drop)
        .into_iter()
        .collect();
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    const STACK_FRAME: &[u8] = b"StackFrameErrorCheck";
    const UNINIT: &[u8] = b"UninitializedVariable";
    const RTC_SU: &[u8] = b"RTCsu";
    if (checks.contains(STACK_FRAME) && checks.contains(UNINIT)) || checks.contains(RTC_SU) {
        checks.shift_remove(STACK_FRAME);
        checks.shift_remove(UNINIT);
        checks.shift_remove(RTC_SU);
        checks.insert(RTC_SU.into());
    }
    let msvc_abi = targets_msvc_abi(model, target.dir, language);
    let mut result = Vec::new();
    for check in checks {
        if let Some(options) = definition(
            model,
            target.dir,
            &[
                b"CMAKE_",
                language,
                b"_COMPILE_OPTIONS_MSVC_RUNTIME_CHECKS_",
                &check,
            ]
            .concat(),
        ) {
            result.extend(list::split(options, list::Empty::Drop));
        } else if msvc_abi {
            let language = BString::from(language);
            let body: BString = format!(
                "MSVC_RUNTIME_CHECKS value '{check}' not known for this {language} compiler."
            )
            .into();
            return Err(report_msvc_error(model, target, usage.as_deref(), body));
        }
    }
    Ok(result)
}

pub(crate) fn runtime_library(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
    mut usage: Option<&mut UsageResolver<'_>>,
) -> Result<Vec<BString>, ResolveError> {
    let Some(default) = definition(model, target.dir, b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT")
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let Some(runtime) = eval_msvc_property(
        model,
        target,
        config,
        language,
        b"MSVC_RUNTIME_LIBRARY",
        default,
        usage.as_deref_mut(),
    )?
    else {
        return Ok(Vec::new());
    };
    if runtime.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(options) = definition(
        model,
        target.dir,
        &[
            b"CMAKE_",
            language,
            b"_COMPILE_OPTIONS_MSVC_RUNTIME_LIBRARY_",
            &runtime,
        ]
        .concat(),
    ) {
        return Ok(list::split(options, list::Empty::Drop));
    }
    if targets_msvc_abi(model, target.dir, language) {
        let language = BString::from(language);
        let body: BString = format!(
            "MSVC_RUNTIME_LIBRARY value '{runtime}' not known for this {language} compiler."
        )
        .into();
        return Err(report_msvc_error(model, target, usage.as_deref(), body));
    }
    Ok(Vec::new())
}
