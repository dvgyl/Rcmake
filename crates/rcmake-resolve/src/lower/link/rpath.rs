//! Runtime paths for link rules and installed executables and libraries.

use std::collections::BTreeMap;

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel, Target, TargetId};

use super::{frameworks, linked_libraries, shared_library, simple_args};
use crate::{
    LinkFragmentRole, LinkKind, LinkStep, ResolveError, ResolvedTarget,
    lower::{
        definition,
        install_rpath::{ElfRpathEdit, MachORpathEdit, RuntimeEdit},
        is_darwin, join, target_property,
    },
    usage::UsageResolver,
};

pub(super) fn link_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    darwin: bool,
    libraries: &[BString],
    languages: &IndexSet<BString>,
) -> Result<Vec<BString>, ResolveError> {
    let prefix = runtime_setting(model, target, step, b"_FLAG").unwrap_or(b"-Wl,-rpath,");
    if prefix.is_empty() {
        return Ok(Vec::new());
    }
    let paths = RuntimePaths::new(model, target, config, step, darwin, libraries, languages)?;
    if !darwin
        && matches!(
            model.generator,
            Some(
                rcmake_model::GeneratorKind::Ninja | rcmake_model::GeneratorKind::NinjaMultiConfig
            )
        )
        && on(model, target, b"CMAKE_NO_BUILTIN_CHRPATH")
        && !build_with_install_rpath(model, target)
        && paths.build != paths.install
        && crate::lower::install::has_runtime_install(model, target, config)
    {
        return Err(ResolveError::Message(format!(
            "The install of the {} target requires changing an RPATH from the build tree, but this is not supported with the Ninja generator unless on an ELF-based or XCOFF-based platform.  The CMAKE_BUILD_WITH_INSTALL_RPATH variable may be set to avoid this relinking step.",
            target.name
        ).into()));
    }
    let build = if build_with_install_rpath(model, target) {
        &paths.install
    } else {
        &paths.build
    };
    let separator = runtime_setting(model, target, step, b"_FLAG_SEP").unwrap_or(if darwin {
        b""
    } else {
        b":"
    });
    let format = |path: &[u8]| [prefix, quote(path).as_slice()].concat().into();
    if separator.is_empty() {
        return Ok(build.iter().map(|path| format(path)).collect());
    }
    if elf_edit_enabled(model, target, config, step)
        && let Some(edit) = paths.elf_edit(model, target, step)
    {
        return Ok(vec![format(&edit.old)]);
    }
    Ok((!build.is_empty())
        .then(|| format(&join_paths(build, separator)))
        .into_iter()
        .collect())
}

struct RuntimePaths {
    build: IndexSet<BString>,
    install: IndexSet<BString>,
}

impl RuntimePaths {
    fn new(
        model: &ProjectModel,
        target: &Target,
        config: &ConfigId,
        step: &LinkStep,
        darwin: bool,
        libraries: &[BString],
        languages: &IndexSet<BString>,
    ) -> Result<Self, ResolveError> {
        let paths = |installs, skip| {
            if on(model, target, b"CMAKE_SKIP_RPATH") || skip {
                return Ok(IndexSet::new());
            }
            let property = if installs {
                b"INSTALL_RPATH".as_slice()
            } else {
                b"BUILD_RPATH"
            };
            let value = rpath_value(model, target, config, step, property)?;
            Ok::<_, ResolveError>(rpath_entries(
                model, target, step, darwin, libraries, installs, &value,
            ))
        };
        let mut build = paths(false, property_on(model, target, b"SKIP_BUILD_RPATH"))?;
        let mut install = paths(true, on(model, target, b"CMAKE_SKIP_INSTALL_RPATH"))?;
        for language in languages {
            let prefix = [b"CMAKE_", language.as_slice()].concat();
            if on(
                model,
                target,
                &[
                    prefix.as_slice(),
                    b"_USE_IMPLICIT_LINK_DIRECTORIES_IN_RUNTIME_PATH",
                ]
                .concat(),
            ) && let Some(directories) = definition(
                model,
                target.dir,
                &[prefix.as_slice(), b"_IMPLICIT_LINK_DIRECTORIES"].concat(),
            ) {
                let directories = list::split(directories, list::Empty::Drop);
                build.extend(directories.iter().cloned());
                if !property_on(model, target, b"INSTALL_REMOVE_ENVIRONMENT_RPATH") {
                    install.extend(directories);
                }
            }
        }
        if let Some(required) =
            definition(model, target.dir, b"CMAKE_PLATFORM_REQUIRED_RUNTIME_PATH")
        {
            let required = list::split(required, list::Empty::Drop);
            build.extend(required.iter().cloned());
            install.extend(required);
        }
        Ok(Self { build, install })
    }

    fn elf_edit(
        &self,
        model: &ProjectModel,
        target: &Target,
        step: &LinkStep,
    ) -> Option<ElfRpathEdit> {
        let separator = runtime_setting(model, target, step, b"_FLAG_SEP").unwrap_or(b":");
        let mut old = join_paths(&self.build, separator);
        let new = join_paths(&self.install, separator);
        if old.is_empty() && new.is_empty() || separator.is_empty() {
            return None;
        }
        if !old.is_empty() {
            old.extend_from_slice(separator);
        }
        while old.len() < new.len() {
            old.extend_from_slice(separator);
        }
        Some(ElfRpathEdit {
            old,
            new,
            strip: property_on(model, target, b"INSTALL_REMOVE_ENVIRONMENT_RPATH"),
            escape_new: target.policies.policies.get(rcmake_diag::PolicyId::new(95))
                == rcmake_diag::PolicyStatus::New,
        })
    }
}

fn join_paths(paths: &IndexSet<BString>, separator: &[u8]) -> BString {
    let mut joined = BString::default();
    for path in paths {
        if !joined.is_empty() {
            joined.extend_from_slice(separator);
        }
        joined.extend_from_slice(path);
    }
    joined
}

fn elf_edit_enabled(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
) -> bool {
    definition(model, target.dir, b"CMAKE_EXECUTABLE_FORMAT").is_some_and(|format| format == "ELF")
        && definition(
            model,
            target.dir,
            &[
                b"CMAKE_SHARED_LIBRARY_RUNTIME_".as_slice(),
                &step.language,
                b"_FLAG_SEP",
            ]
            .concat(),
        )
        .is_some_and(|separator| !separator.is_empty())
        && runtime_setting(model, target, step, b"_FLAG") != Some(b"")
        && !on(model, target, b"CMAKE_SKIP_RPATH")
        && !on(model, target, b"CMAKE_NO_BUILTIN_CHRPATH")
        && !build_with_install_rpath(model, target)
        && crate::lower::install::has_runtime_install(model, target, config)
}

fn build_with_install_rpath(model: &ProjectModel, target: &Target) -> bool {
    property_on(model, target, b"BUILD_WITH_INSTALL_RPATH")
}

fn on(model: &ProjectModel, target: &Target, key: &[u8]) -> bool {
    definition(model, target.dir, key).is_some_and(|value| truth::is_on(value))
}

fn property_on(model: &ProjectModel, target: &Target, key: &[u8]) -> bool {
    target_property(model, target, key).is_some_and(|value| truth::is_on(value))
}

fn rpath_value(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    link: &LinkStep,
    property: &[u8],
) -> Result<BString, ResolveError> {
    let Some(value) = target_property(model, target, property) else {
        return Ok(BString::default());
    };
    if let Some(id) = model.find_target(target.dir, &target.name)
        && value.windows(2).any(|part| part == b"$<")
    {
        let ctx = EvalContext {
            config: config.clone(),
            link_language: Some(link.language.clone()),
            ..EvalContext::default()
        };
        return UsageResolver::new(model).eval_ide_property(id, property, value, &ctx);
    }
    Ok(value.clone())
}

pub(super) fn runtime_libraries(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    libraries: &[BString],
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    if libraries.is_empty() {
        return Ok(Vec::new());
    }
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Ok(libraries.to_vec());
    };
    let ctx = EvalContext {
        config: config.clone(),
        link_language: Some(step.language.clone()),
        ..EvalContext::default()
    };
    let build = &model.dir(DirId::ROOT).binary_dir;
    let absolute = |path: &[u8]| Lexical::new(&join(build, path)).normalize();
    let mut runtimes = BTreeMap::new();
    for id in usage.link_targets(id, &ctx)? {
        let Some(dependency) = model.target(id) else {
            continue;
        };
        let imported = dependency.is_imported()
            && !crate::lower::artifacts::is_dll_platform(model, target.dir);
        let stub = if imported {
            crate::usage::artifacts::imported_property(dependency, b"IMPORTED_IMPLIB", config)
                .map(|(path, _)| path)
        } else {
            usage
                .artifact(id, ArtifactKind::ImportFile, ArtifactPart::Full, config)
                .filter(|stub| stub.ends_with(b".tbd"))
        };
        if let Some(stub) = stub {
            let location = usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config);
            runtimes.insert(absolute(&stub), location);
        }
    }
    Ok(libraries
        .iter()
        .filter_map(|file| {
            runtimes
                .get(&absolute(file))
                .cloned()
                .unwrap_or_else(|| Some(file.clone()))
        })
        .collect())
}

fn runtime_setting<'a>(
    model: &'a ProjectModel,
    target: &Target,
    step: &LinkStep,
    suffix: &[u8],
) -> Option<&'a [u8]> {
    let kind = if step.kind == LinkKind::Executable {
        b"CMAKE_EXECUTABLE_RUNTIME_".as_slice()
    } else {
        b"CMAKE_SHARED_LIBRARY_RUNTIME_"
    };
    definition(model, target.dir, &[kind, &step.language, suffix].concat())
        .map(|value| value.as_slice())
}

fn open_link_paths(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
) -> Result<Vec<BString>, ResolveError> {
    let mut libraries = linked_libraries(step);
    if is_darwin(model, target.dir) {
        libraries.extend(frameworks::named_framework_files(
            model, target, config, step, &libraries,
        ));
    }
    runtime_libraries(
        model,
        target,
        config,
        step,
        &libraries,
        &mut UsageResolver::new(model),
    )
}

pub(in crate::lower) fn install_rpath(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    languages: &IndexSet<BString>,
    targets: &IndexMap<(TargetId, &ConfigId), &ResolvedTarget>,
) -> Result<Option<RuntimeEdit>, ResolveError> {
    let Some(link) = resolved
        .link
        .as_ref()
        .filter(|link| link.kind != LinkKind::StaticArchive)
    else {
        return Ok(None);
    };
    let darwin = is_darwin(model, target.dir);
    if !darwin && !elf_edit_enabled(model, target, &resolved.config, link) {
        return Ok(None);
    }
    let libraries = open_link_paths(model, target, &resolved.config, link)?;
    let paths = RuntimePaths::new(
        model,
        target,
        &resolved.config,
        link,
        darwin,
        &libraries,
        languages,
    )?;
    if darwin {
        let old_name =
            super::install_name_prefix(model, target, &resolved.artifacts, &resolved.config, None)?;
        let installed_name = super::install_name_prefix(
            model,
            target,
            &resolved.artifacts,
            &resolved.config,
            Some(b"${CMAKE_INSTALL_PREFIX}"),
        )?;
        let name = if link.kind == LinkKind::SharedLibrary
            && old_name != installed_name
            && let Some(soname) = &resolved.artifacts.soname_file
        {
            let mut name = installed_name;
            name.extend_from_slice(Lexical::new(soname).filename());
            Some(name)
        } else {
            None
        };
        let changes = linked_install_names(model, resolved, link, targets)?;
        let update_paths = !build_with_install_rpath(model, target)
            && !on(model, target, b"CMAKE_NO_BUILTIN_CHRPATH")
            && runtime_setting(model, target, link, b"_FLAG") != Some(b"")
            && paths.build != paths.install;
        if name.is_none() && changes.is_empty() && !update_paths {
            return Ok(None);
        }
        let tool = definition(model, target.dir, b"CMAKE_INSTALL_NAME_TOOL")
            .filter(|tool| !truth::is_off(tool.as_slice()))
            .cloned()
            .unwrap_or_else(|| "install_name_tool".into());
        return Ok(Some(RuntimeEdit::MachO(MachORpathEdit {
            tool,
            install_name: name,
            changes,
            old: if update_paths {
                paths.build.into_iter().collect()
            } else {
                Vec::new()
            },
            new: if update_paths {
                paths.install.into_iter().collect()
            } else {
                Vec::new()
            },
        })));
    }
    Ok(paths.elf_edit(model, target, link).map(RuntimeEdit::Elf))
}

fn linked_install_names(
    model: &ProjectModel,
    resolved: &ResolvedTarget,
    link: &LinkStep,
    targets: &IndexMap<(TargetId, &ConfigId), &ResolvedTarget>,
) -> Result<IndexMap<BString, BString>, ResolveError> {
    let context = EvalContext {
        config: resolved.config.clone(),
        link_language: Some(link.language.clone()),
        ..Default::default()
    };
    let mut changes = IndexMap::new();
    let mut usage = UsageResolver::new(model);
    for id in usage.link_targets(resolved.id, &context)? {
        let Some(dependency) = model
            .target(id)
            .filter(|dependency| dependency.kind == rcmake_model::TargetKind::SharedLibrary)
        else {
            continue;
        };
        let Some(facts) = targets.get(&(id, &resolved.config)) else {
            continue;
        };
        let Some(soname) = &facts.artifacts.soname_file else {
            continue;
        };
        let mut old = super::install_name_prefix(
            model,
            dependency,
            &facts.artifacts,
            &resolved.config,
            None,
        )?;
        let mut new = super::install_name_prefix(
            model,
            dependency,
            &facts.artifacts,
            &resolved.config,
            Some(b"${CMAKE_INSTALL_PREFIX}"),
        )?;
        if old != new {
            let soname = Lexical::new(soname).filename();
            old.extend_from_slice(soname);
            new.extend_from_slice(soname);
            changes.insert(old, new);
        }
    }
    Ok(changes)
}

pub(crate) fn ide_rpaths(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    libraries: &[BString],
    languages: &IndexSet<BString>,
    usage: &mut UsageResolver<'_>,
) -> Result<Vec<BString>, ResolveError> {
    let libraries = runtime_libraries(model, target, config, step, libraries, usage)?;
    let paths = RuntimePaths::new(model, target, config, step, true, &libraries, languages)?;
    Ok(if build_with_install_rpath(model, target) {
        paths.install
    } else {
        paths.build
    }
    .into_iter()
    .collect())
}

fn rpath_entries(
    model: &ProjectModel,
    target: &Target,
    step: &LinkStep,
    darwin: bool,
    libraries: &[BString],
    installs: bool,
    value: &[u8],
) -> IndexSet<BString> {
    let mut paths: IndexSet<BString> = list::split(value, list::Empty::Drop).into_iter().collect();
    if !installs {
        if darwin
            && definition(model, target.dir, b"CMAKE_PLATFORM_HAS_INSTALLNAME")
                .is_some_and(|value| truth::is_on(value))
        {
            paths.extend(
                step.fragments
                    .iter()
                    .filter(|fragment| fragment.role == LinkFragmentRole::LibraryPath)
                    .flat_map(|fragment| simple_args(&fragment.text.value).into_iter().flatten())
                    .filter_map(|arg| {
                        arg.strip_prefix(b"-L")
                            .filter(|path| !path.is_empty())
                            .map(BString::from)
                    }),
            );
        }
        let build = &model.dir(DirId::ROOT).binary_dir;
        let build_path = Lexical::new(build);
        let output = join(build, &step.output);
        let output_dir = Lexical::new(Lexical::new(&output).parent());
        let origin = !darwin
            && target_property(model, target, b"BUILD_RPATH_USE_ORIGIN")
                .is_some_and(|value| truth::is_on(value));
        for library in libraries {
            let path = Lexical::new(&join(build, library)).normalize();
            let file = Lexical::new(&path);
            if darwin
                && let Some(parent) =
                    crate::lower::install_rpath::framework_runtime_directory(model, target, &path)
            {
                let directory =
                    crate::lower::install_rpath::translated_link_directory(model, target, parent);
                paths.insert(directory.unwrap_or_else(|| parent.into()));
            } else if shared_library(file.filename())
                && (build_path.prefix(file)
                    || crate::lower::install_rpath::translated_link_directory(
                        model,
                        target,
                        file.parent(),
                    )
                    .is_some())
            {
                let parent = Lexical::new(file.parent());
                let translated = crate::lower::install_rpath::translated_link_directory(
                    model,
                    target,
                    file.parent(),
                );
                let path = if let Some(directory) = translated {
                    directory
                } else if origin {
                    parent.relative(output_dir).map_or_else(
                        || file.parent().into(),
                        |rel| {
                            if rel == b"." {
                                "$ORIGIN".into()
                            } else {
                                join(b"$ORIGIN", &rel)
                            }
                        },
                    )
                } else {
                    file.parent().into()
                };
                paths.insert(path);
            }
        }
    } else if target_property(model, target, b"INSTALL_RPATH_USE_LINK_PATH")
        .is_some_and(|value| truth::is_on(value))
    {
        paths.extend(crate::lower::install_rpath::external_link_directories(
            model, target, libraries,
        ));
    }
    paths
}
