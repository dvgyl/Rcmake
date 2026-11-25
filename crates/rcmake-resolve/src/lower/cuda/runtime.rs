use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target};

use crate::{LinkKind, LinkStep, ResolveError, lower::definition, usage::UsageResolver};

pub(crate) fn host_args(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    if step.kind == LinkKind::StaticArchive {
        return Ok(Vec::new());
    }
    let mut args = IndexSet::new();
    for lang in [b"CUDA".as_slice(), b"Fortran"] {
        if step.language != lang && !languages.contains(lang) {
            continue;
        }
        let implicit = values(model, target, lang, b"_IMPLICIT_LINK_LIBRARIES");
        let driver_libs = values(model, target, &step.language, b"_IMPLICIT_LINK_LIBRARIES");
        if lang == b"CUDA" {
            let selected = selected_runtime(model, target, config, &step.language, usage)?;
            let suffix = [b"_RUNTIME_LIBRARY_LINK_OPTIONS_", &selected[..]].concat();
            args.extend(
                values(model, target, lang, &suffix)
                    .into_iter()
                    .filter(|lib| !driver_libs.contains(lib))
                    .map(|lib| library_argument(model, target, &step.language, lib)),
            );
        }
        if step.language != lang {
            args.extend(
                values(model, target, lang, b"_IMPLICIT_LINK_DIRECTORIES")
                    .into_iter()
                    .map(|dir| {
                        let flag = definition(model, target.dir, b"CMAKE_LIBRARY_PATH_FLAG")
                            .map_or(b"-L".as_slice(), |value| value.as_slice());
                        [flag, dir.as_slice()].concat().into()
                    }),
            );
            args.extend(
                implicit
                    .into_iter()
                    .filter(|lib| !driver_libs.contains(lib))
                    .map(|lib| library_argument(model, target, &step.language, lib)),
            );
        }
    }
    Ok(args.into_iter().collect())
}

pub(crate) fn selected_runtime(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    link_language: &[u8],
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<u8>, ResolveError> {
    let Some(default) = definition(model, target.dir, b"CMAKE_CUDA_RUNTIME_LIBRARY_DEFAULT")
        .filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let Some(raw) = target.props.get(b"CUDA_RUNTIME_LIBRARY") else {
        return Ok(default.to_ascii_uppercase());
    };
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Ok(raw.to_ascii_uppercase());
    };
    let context = EvalContext {
        config: config.clone(),
        link_language: Some(link_language.into()),
        ..Default::default()
    };
    Ok(usage
        .eval_ide_property(id, b"CUDA_RUNTIME_LIBRARY", raw, &context)?
        .to_ascii_uppercase())
}

fn values(model: &ProjectModel, target: &Target, language: &[u8], suffix: &[u8]) -> Vec<BString> {
    definition(model, target.dir, &[b"CMAKE_", language, suffix].concat())
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop))
}

fn library_argument(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    lib: BString,
) -> BString {
    if Lexical::new(&lib).absolute() || lib.starts_with(b"-") {
        return lib;
    }
    let flag = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_LINK_LIBRARY_FLAG"].concat(),
    )
    .or_else(|| definition(model, target.dir, b"CMAKE_LINK_LIBRARY_FLAG"))
    .map_or(b"-l".as_slice(), |value| value.as_slice());
    let suffix = definition(model, target.dir, b"CMAKE_LINK_LIBRARY_SUFFIX")
        .map_or(b"".as_slice(), |value| value.as_slice());
    [flag, lib.as_slice(), suffix].concat().into()
}
