use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical};
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_model::{ProjectModel, Target};

use crate::{
    LinkKind, LinkStep, ResolveError,
    lower::{definition, without_trailing_slashes},
    usage::UsageResolver,
};

pub(crate) fn host_args(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    Ok(
        host_arg_roles(model, target, config, step, languages, usage)?
            .into_iter()
            .map(|(_, value)| value)
            .collect(),
    )
}

/// Return runtime and cross-language implicit link fragments (`("libraryPath" | "libraries", text)`)
/// for File API `codemodel-v2`.
pub fn codemodel_runtime_link_fragments(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<(&'static str, BString)>, ResolveError> {
    host_arg_roles(model, target, config, step, languages, usage)
}

fn host_arg_roles(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<(&'static str, BString)>, ResolveError> {
    if step.kind == LinkKind::StaticArchive {
        return Ok(Vec::new());
    }
    let mut args: IndexSet<(&'static str, BString)> = IndexSet::new();
    let driver_libs = values(model, target, &step.language, b"_IMPLICIT_LINK_LIBRARIES");
    let driver_dirs = implicit_link_dirs(model, target, &step.language);
    let lang_order: &[&[u8]] = if matches!(step.language.as_slice(), b"CUDA" | b"HIP") {
        &[b"CUDA", b"HIP", b"Fortran", b"CXX"]
    } else {
        &[b"CUDA", b"HIP", b"Fortran"]
    };
    for &lang in lang_order {
        if step.language != lang && !languages.contains(lang) {
            continue;
        }
        let implicit = values(model, target, lang, b"_IMPLICIT_LINK_LIBRARIES");
        if matches!(lang, b"CUDA" | b"HIP") {
            let selected = selected_runtime(model, target, config, lang, &step.language, usage)?;
            if !selected.is_empty() {
                let suffix = [b"_RUNTIME_LIBRARY_LINK_OPTIONS_", &selected[..]].concat();
                args.extend(
                    values(model, target, lang, &suffix)
                        .into_iter()
                        .filter(|lib| !driver_libs.contains(lib))
                        .map(|lib| {
                            (
                                "libraries",
                                library_argument(model, target, &step.language, lib),
                            )
                        }),
                );
            }
        }
        if step.language != lang {
            args.extend(
                values(model, target, lang, b"_IMPLICIT_LINK_DIRECTORIES")
                    .into_iter()
                    .filter(|dir| !driver_dirs.contains(&normalize_dir(dir)))
                    .map(|dir| {
                        let flag = definition(model, target.dir, b"CMAKE_LIBRARY_PATH_FLAG")
                            .map_or(b"-L".as_slice(), |value| value.as_slice());
                        ("libraryPath", [flag, dir.as_slice()].concat().into())
                    }),
            );
            args.extend(
                implicit
                    .into_iter()
                    .filter(|lib| !driver_libs.contains(lib))
                    .map(|lib| {
                        (
                            "libraries",
                            library_argument(model, target, &step.language, lib),
                        )
                    }),
            );
        }
    }
    Ok(args.into_iter().collect())
}

pub(crate) fn selected_runtime(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    lang: &[u8],
    link_language: &[u8],
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<u8>, ResolveError> {
    let default_var = [b"CMAKE_", lang, b"_RUNTIME_LIBRARY_DEFAULT"].concat();
    let Some(default) =
        definition(model, target.dir, &default_var).filter(|value| !value.is_empty())
    else {
        return Ok(Vec::new());
    };
    let prop = [lang, b"_RUNTIME_LIBRARY"].concat();
    let Some(raw) = target.props.get(&prop) else {
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
        .eval_ide_property(id, &prop, raw, &context)?
        .to_ascii_uppercase())
}

fn implicit_link_dirs(model: &ProjectModel, target: &Target, language: &[u8]) -> IndexSet<BString> {
    let mut dirs = IndexSet::new();
    let platform = definition(
        model,
        target.dir,
        b"CMAKE_PLATFORM_IMPLICIT_LINK_DIRECTORIES",
    )
    .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    let library_arch =
        definition(model, target.dir, b"CMAKE_LIBRARY_ARCHITECTURE").filter(|v| !v.is_empty());
    for dir in platform {
        if let Some(arch) = library_arch {
            dirs.insert(normalize_dir(
                &[dir.as_slice(), b"/", arch.as_slice()].concat(),
            ));
        }
        dirs.insert(normalize_dir(&dir));
    }
    for key in [b"_CMAKE_OSX_SYSROOT_PATH".as_slice(), b"CMAKE_OSX_SYSROOT"] {
        if let Some(sysroot) =
            definition(model, target.dir, key).filter(|v| Lexical::new(v).absolute())
        {
            dirs.insert(normalize_dir(&[sysroot.as_slice(), b"/usr/lib"].concat()));
            dirs.insert(normalize_dir(
                &[sysroot.as_slice(), b"/usr/local/lib"].concat(),
            ));
        }
    }
    for dir in values(model, target, language, b"_IMPLICIT_LINK_DIRECTORIES") {
        dirs.insert(normalize_dir(&dir));
    }
    dirs
}

fn normalize_dir(path: &[u8]) -> BString {
    let normalized = Lexical::new(path).normalize();
    without_trailing_slashes(&normalized).into()
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
