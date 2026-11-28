//! Runtime paths for link rules and installed executables and libraries.

use std::{
    cell::RefCell,
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap, HashMap},
};

use rcmake_host::{Follow, Host, path};

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_diag::{Location, MessageKind};
use rcmake_genex::{ArtifactKind, ArtifactPart, ConfigId, EvalContext};
use rcmake_model::{DirId, ProjectModel, Target, TargetId, TargetKind, UsageField};

use super::{frameworks, linked_libraries, shared_library};
use crate::{
    LinkFragmentRole, LinkKind, LinkStep, ResolveError, ResolvedTarget,
    lower::{
        definition,
        install_rpath::{ElfRpathEdit, MachORpathEdit, RuntimeEdit},
        is_darwin, join, target_property,
    },
    usage::UsageResolver,
};

/// How a link step treats a shared library that the link reaches only as a
/// dependency of another shared library (`LinkEntry::SharedDep`,
/// `cmComputeLinkInformation::SharedDependencyMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedDependencyMode {
    /// Drop the dependency.
    None,
    /// Link to the dependency (`CMAKE_LINK_DEPENDENT_LIBRARY_FILES`).
    Link,
    /// Add the directory of the dependency to the linker search path
    /// (`CMAKE_LINK_DEPENDENT_LIBRARY_DIRS`).
    LibDir,
    /// Add the directory of the dependency to the `-rpath-link` path
    /// (`CMAKE_SHARED_LIBRARY_RPATH_LINK_<LANG>_FLAG`).
    Dir,
}

/// Select the shared dependency mode of a target in `dir` that links with
/// `language`. The variables have this precedence: dependent library files,
/// dependent library directories, then the `-rpath-link` flag.
pub(crate) fn shared_dependency_mode(
    model: &ProjectModel,
    dir: DirId,
    language: Option<&[u8]>,
) -> SharedDependencyMode {
    let on = |name: &[u8]| definition(model, dir, name).is_some_and(|value| truth::is_on(value));
    if on(b"CMAKE_LINK_DEPENDENT_LIBRARY_FILES") {
        SharedDependencyMode::Link
    } else if on(b"CMAKE_LINK_DEPENDENT_LIBRARY_DIRS") {
        SharedDependencyMode::LibDir
    } else if language.is_some_and(|language| rpath_link_variable(model, dir, language).is_some()) {
        SharedDependencyMode::Dir
    } else {
        SharedDependencyMode::None
    }
}

/// Return `true` when the link line keeps `item`, an entry that only a shared
/// library dependency added, in the [`SharedDependencyMode::Link`] mode
/// (`AddSharedDepItem`): a shared library target, or a full path to a file
/// with a shared library name.
pub(crate) fn links_shared_dependency(
    model: &ProjectModel,
    item: &crate::usage::ResolvedLinkItem,
) -> bool {
    match item.kind {
        crate::usage::ResolvedLinkKind::Library(id) => model
            .target(id)
            .is_some_and(|target| target.kind == TargetKind::SharedLibrary),
        crate::usage::ResolvedLinkKind::Raw => {
            let path = Lexical::new(&item.text.value);
            path.absolute() && shared_library(path.filename())
        }
        _ => false,
    }
}

/// Runtime path options of a link line.
#[derive(Default)]
pub(super) struct RuntimeFlags {
    /// `-rpath` options, which precede the link libraries.
    pub(super) rpath: Vec<BString>,
    /// The `-rpath-link` option, which follows the link libraries.
    pub(super) rpath_link: Option<BString>,
}

pub(super) fn link_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    usage: &mut UsageResolver<'_>,
    libraries: &RuntimeLibraries,
    languages: &IndexSet<BString>,
) -> Result<RuntimeFlags, ResolveError> {
    let darwin = is_darwin(model, target.dir);
    let installed = crate::lower::install::has_target_install(model, target)
        || model
            .find_target(target.dir, &target.name)
            .is_some_and(|id| usage.runtime_install_targets.contains(&id));
    let chrpath = installed
        && !on(model, target, b"CMAKE_SKIP_RPATH")
        && !build_with_install_rpath(model, target)
        && !on(model, target, b"CMAKE_NO_BUILTIN_CHRPATH")
        && (darwin || elf_chrpath_supported(model, target, step));
    if installed
        && !on(model, target, b"CMAKE_SKIP_RPATH")
        && !build_with_install_rpath(model, target)
        && !chrpath
        && model.generator.is_some_and(|generator| {
            matches!(
                generator,
                rcmake_model::GeneratorKind::Ninja | rcmake_model::GeneratorKind::NinjaMultiConfig
            )
        })
        && shared_runtime_setting(model, target, step, b"_FLAG").is_some_and(|value| {
            !value.is_empty() && !value.ends_with(b"-NOTFOUND") && value != b"NOTFOUND"
        })
        && needs_tree_rpath(model, target, config, step, usage)?
    {
        let message: BString = format!(
            "The install of the {} target requires changing an RPATH from the build tree, but this is not supported with the Ninja generator unless on an ELF-based or XCOFF-based platform.  The CMAKE_BUILD_WITH_INSTALL_RPATH variable may be set to avoid this relinking step.",
            target.name
        ).into();
        if let Some(paths) = usage.source_paths() {
            paths.emit(
                model,
                MessageKind::Error,
                Location::Backtrace(target.backtrace.clone()),
                message.clone(),
            );
            return Err(ResolveError::Reported(message));
        }
        return Err(ResolveError::Message(message));
    }
    let prefix = runtime_setting(model, target, step, b"_FLAG").unwrap_or(b"-Wl,-rpath,");
    let paths = RuntimePaths::new(
        model,
        target,
        config,
        step,
        darwin,
        languages,
        (libraries, usage),
    )?;
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
    let windows = crate::lower::shell::windows_host(model, target.dir);
    // `cmOutputConverter::EscapeForShell`: POSIX double quotes, `$` escaped.
    let escape = |path: &[u8]| {
        if windows {
            quote(path)
        } else {
            crate::lower::shell::cmake_posix_quote(path)
        }
    };
    let format = |path: &[u8]| [prefix, escape(path).as_slice()].concat().into();
    let mut flags = Vec::new();
    if !prefix.is_empty() {
        if separator.is_empty() {
            flags.extend(build.iter().map(|path| format(path)));
        } else if !darwin
            && chrpath
            && let Some(edit) = paths.elf_edit(model, target, step)
        {
            flags.push(format(&edit.old));
        } else if !build.is_empty() {
            flags.push(format(&join_paths(build, separator)));
        }
    }
    let rpath_link = rpath_link_flag(model, target, step)
        .filter(|_| !libraries.dependent_dirs.is_empty())
        .map(|prefix| {
            [
                prefix,
                &escape(&join_paths(&libraries.dependent_dirs, b":")),
            ]
            .concat()
            .into()
        });
    Ok(RuntimeFlags {
        rpath: flags,
        rpath_link,
    })
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
        languages: &IndexSet<BString>,
        (libraries, usage): (&RuntimeLibraries, &mut UsageResolver<'_>),
    ) -> Result<Self, ResolveError> {
        let host = usage.source_paths().map(|paths| paths.host);
        let implicit = implicit_directories(model, target, &step.language, host);
        let mut paths = |installs, skip| {
            if on(model, target, b"CMAKE_SKIP_RPATH") || skip {
                return Ok(IndexSet::new());
            }
            let property = if installs {
                b"INSTALL_RPATH".as_slice()
            } else {
                b"BUILD_RPATH"
            };
            let value = rpath_value(model, target, config, step, property, usage)?;
            Ok::<_, ResolveError>(rpath_entries(
                model, target, step, darwin, libraries, installs, &value, &implicit, host,
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

fn elf_chrpath_supported(model: &ProjectModel, target: &Target, step: &LinkStep) -> bool {
    definition(model, target.dir, b"CMAKE_EXECUTABLE_FORMAT").is_some_and(|format| format == "ELF")
        && shared_runtime_setting(model, target, step, b"_FLAG_SEP")
            .is_some_and(|separator| !separator.is_empty())
}

fn elf_edit_enabled(model: &ProjectModel, target: &Target, step: &LinkStep) -> bool {
    elf_chrpath_supported(model, target, step)
        && runtime_setting(model, target, step, b"_FLAG") != Some(b"")
        && !on(model, target, b"CMAKE_SKIP_RPATH")
        && !on(model, target, b"CMAKE_NO_BUILTIN_CHRPATH")
        && !build_with_install_rpath(model, target)
}

fn needs_tree_rpath(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    usage: &mut UsageResolver<'_>,
) -> Result<bool, ResolveError> {
    let build = !property_on(model, target, b"SKIP_BUILD_RPATH")
        && (target_property(model, target, b"BUILD_RPATH").is_some()
            || step
                .fragments
                .iter()
                .any(|part| part.role == LinkFragmentRole::Libraries));
    Ok(build
        || (!on(model, target, b"CMAKE_SKIP_INSTALL_RPATH")
            && !rpath_value(model, target, config, step, b"INSTALL_RPATH", usage)?.is_empty()))
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
    usage: &mut UsageResolver<'_>,
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
        return usage.eval_ide_property(id, property, value, &ctx);
    }
    Ok(value.clone())
}

pub(super) struct RuntimeLibraries {
    files: Vec<BString>,
    user_dirs: IndexSet<BString>,
    dependent_dirs: IndexSet<BString>,
    pub(super) link_stubs: IndexSet<BString>,
}

pub(super) fn runtime_libraries(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    libraries: &[BString],
    usage: &mut UsageResolver<'_>,
) -> Result<RuntimeLibraries, ResolveError> {
    let mut result = RuntimeLibraries {
        files: libraries.to_vec(),
        user_dirs: IndexSet::new(),
        dependent_dirs: IndexSet::new(),
        link_stubs: IndexSet::new(),
    };
    let Some(id) = model.find_target(target.dir, &target.name) else {
        return Ok(result);
    };
    let ctx = EvalContext {
        config: config.clone(),
        link_language: Some(step.language.clone()),
        ..EvalContext::default()
    };
    let implicit = implicit_directories(
        model,
        target,
        &step.language,
        usage.source_paths().map(|paths| paths.host),
    );
    let dll = crate::lower::artifacts::is_dll_platform(model, target.dir);
    if !dll || runtime_setting(model, target, step, b"_FLAG").is_some() {
        result.user_dirs.extend(
            usage
                .collect(id, UsageField::LinkDirectories, &ctx)?
                .entries
                .into_iter()
                .filter(|entry| !entry.value.is_empty())
                .map(|entry| Lexical::new(&entry.value).normalize())
                .filter(|dir| !implicit.contains(dir)),
        );
    }
    let build = &model.dir(DirId::ROOT).binary_dir;
    let absolute = |path: &[u8]| Lexical::new(&join(build, path)).normalize();
    let apple = on(model, target, b"CMAKE_PLATFORM_HAS_INSTALLNAME");
    // Only the `Dir` mode adds `-rpath-link` directories. The `Link` mode puts
    // the dependencies on the link line in the link order.
    let rpath_link = shared_dependency_mode(model, target.dir, Some(&step.language))
        == SharedDependencyMode::Dir;
    let mut runtimes = BTreeMap::new();
    let mut shared = IndexSet::new();
    for id in usage.link_targets(id, &ctx)? {
        let Some(dependency) = model.target(id) else {
            continue;
        };
        if !matches!(
            dependency.kind,
            TargetKind::SharedLibrary | TargetKind::UnknownLibrary
        ) {
            continue;
        }
        if dependency.kind == TargetKind::SharedLibrary {
            shared.insert(id);
        }
        let stub = if dependency.is_imported() && !dll {
            crate::usage::artifacts::imported_property(dependency, b"IMPORTED_IMPLIB", config)
                .map(|(path, _)| path)
        } else {
            usage
                .artifact(id, ArtifactKind::ImportFile, ArtifactPart::Full, config)
                .filter(|stub| stub.ends_with(b".tbd"))
        };
        let location = usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config);
        let eligible = if let Some(file) = &location
            && apple
        {
            apple_runtime_library(model, dependency, config, &absolute(file), usage)?
        } else {
            true
        };
        if let Some(stub) = stub {
            runtimes.insert(absolute(&stub), location.clone().filter(|_| eligible));
        }
        if let Some(file) = location {
            for kind in [ArtifactKind::LinkerFile, ArtifactKind::SonameFile] {
                if let Some(alias) = usage.artifact(id, kind, ArtifactPart::Full, config) {
                    runtimes.insert(absolute(&alias), eligible.then(|| file.clone()));
                }
            }
            runtimes.insert(absolute(&file), eligible.then_some(file));
        }
    }

    let mut direct = IndexSet::new();
    result.files.retain_mut(|file| {
        let path = absolute(file);
        direct.insert(path.clone());
        match runtimes.get(&path) {
            Some(Some(location)) => file.clone_from(location),
            Some(None) => return false,
            None if apple && !file_uses_rpath(&path, usage) => return false,
            None => {}
        }
        true
    });
    if rpath_link {
        let linker = [b"CMAKE_", step.language.as_slice(), b"_COMPILER_LINKER_ID"].concat();
        let link_stub = definition(model, target.dir, &linker)
            .is_some_and(|name| matches!(name.as_slice(), b"GNU" | b"Solaris"));
        let mut next = 0;
        while let Some(&id) = shared.get_index(next) {
            next += 1;
            let Some(dependency) = model.target(id) else {
                continue;
            };
            let mut files = Vec::new();
            if dependency.is_imported() {
                if let Some((items, _)) = crate::usage::artifacts::imported_property(
                    dependency,
                    b"IMPORTED_LINK_DEPENDENT_LIBRARIES",
                    config,
                ) {
                    for item in list::split(&items, list::Empty::Drop) {
                        if let Some(id) = model.find_target(dependency.dir, &item) {
                            add_shared_file(model, usage, config, id, &mut shared, &mut files);
                        } else {
                            files.push((item, false));
                        }
                    }
                }
            } else {
                for id in usage.link_targets(id, &ctx)? {
                    add_shared_file(model, usage, config, id, &mut shared, &mut files);
                }
            }
            for (file, stub) in files {
                let path = Lexical::new(&file);
                if !path.absolute()
                    || !shared_library(path.filename())
                    || !direct.insert(absolute(&file))
                {
                    continue;
                }
                if stub && link_stub {
                    result.link_stubs.insert(file);
                } else {
                    let directory = Lexical::new(path.parent()).normalize();
                    if !implicit.contains(&directory) {
                        result.dependent_dirs.insert(directory);
                        if !stub {
                            result.files.push(file);
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

fn add_shared_file(
    model: &ProjectModel,
    usage: &UsageResolver<'_>,
    config: &ConfigId,
    id: TargetId,
    shared: &mut IndexSet<TargetId>,
    files: &mut Vec<(BString, bool)>,
) {
    if model
        .target(id)
        .is_some_and(|target| target.kind == TargetKind::SharedLibrary)
    {
        shared.insert(id);
        if let Some(file) = usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config) {
            files.push((file, false));
        } else if let Some(file) =
            usage.artifact(id, ArtifactKind::ImportFile, ArtifactPart::Full, config)
        {
            files.push((file, true));
        }
    }
}

fn apple_runtime_library(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    file: &[u8],
    usage: &UsageResolver<'_>,
) -> Result<bool, ResolveError> {
    if target.is_imported() {
        use crate::usage::artifacts::{imported_for_config, imported_property};
        if let Some((_, selected)) = imported_property(target, b"IMPORTED_LOCATION", config) {
            let property = |key| {
                imported_for_config(target, key, &selected)
                    .or_else(|| imported_for_config(target, key, b""))
            };
            if !property(b"IMPORTED_NO_SONAME").is_some_and(|v| truth::is_on(&v))
                && let Some(name) = property(b"IMPORTED_SONAME").filter(|v| !v.is_empty())
            {
                return Ok(name.starts_with(b"@rpath/"));
            }
        }
        return Ok(file_uses_rpath(file, usage));
    }
    let artifacts = crate::ArtifactSet {
        file: Some(file.into()),
        ..Default::default()
    };
    Ok(super::install_name_dir(model, target, &artifacts, config, None)? == b"@rpath/")
}

fn file_uses_rpath(file: &[u8], usage: &UsageResolver<'_>) -> bool {
    usage.source_paths().is_some_and(|paths| {
        super::rpath_macho::uses_rpath(paths.host, &rcmake_host::path::to_native(file))
    })
}

fn rpath_link_flag<'a>(
    model: &'a ProjectModel,
    target: &Target,
    step: &LinkStep,
) -> Option<&'a [u8]> {
    rpath_link_variable(model, target.dir, &step.language)
}

fn rpath_link_variable<'a>(
    model: &'a ProjectModel,
    dir: DirId,
    language: &[u8],
) -> Option<&'a [u8]> {
    let key = [b"CMAKE_SHARED_LIBRARY_RPATH_LINK_", language, b"_FLAG"].concat();
    definition(model, dir, &key)
        .filter(|flag| !flag.is_empty())
        .map(|flag| flag.as_slice())
}

pub(in crate::lower) struct ImplicitDirs<'a> {
    host: Option<&'a dyn Host>,
    paths: IndexSet<BString>,
    canonical: IndexSet<BString>,
    cache: RefCell<BTreeMap<BString, BString>>,
}

impl ImplicitDirs<'_> {
    pub(in crate::lower) fn contains(&self, dir: &[u8]) -> bool {
        let dir = super::super::without_trailing_slashes(dir);
        self.paths.contains(dir)
            || self.host.is_some_and(|host| {
                let mut cache = self.cache.borrow_mut();
                let real = cache
                    .entry(dir.into())
                    .or_insert_with(|| canonical_dir(host, dir));
                self.canonical.contains(real.as_slice())
            })
    }
}

fn canonical_dir(host: &dyn Host, dir: &[u8]) -> BString {
    host.canonicalize(&path::to_native(dir)).map_or_else(
        |_| dir.into(),
        |real| super::super::without_trailing_slashes(&path::from_native_cmake(&real)).into(),
    )
}

fn implicit_directories<'a>(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    host: Option<&'a dyn Host>,
) -> ImplicitDirs<'a> {
    let platform = definition(
        model,
        target.dir,
        b"CMAKE_PLATFORM_IMPLICIT_LINK_DIRECTORIES",
    )
    .map_or(Vec::new(), |value| list::split(value, list::Empty::Drop));
    let language_key = [b"CMAKE_", language, b"_IMPLICIT_LINK_DIRECTORIES"].concat();
    let language = definition(model, target.dir, &language_key)
        .map_or(Vec::new(), |value| list::split(value, list::Empty::Drop));
    let arch = definition(model, target.dir, b"CMAKE_LIBRARY_ARCHITECTURE")
        .filter(|arch| !arch.is_empty());
    let multiarch = platform
        .iter()
        .filter_map(|dir| arch.map(|arch| join(dir, arch)));
    let paths: IndexSet<BString> = multiarch
        .chain(platform.iter().cloned())
        .chain(language)
        .map(|dir| super::super::without_trailing_slashes(&Lexical::new(&dir).normalize()).into())
        .collect();
    let canonical = host.map_or_else(IndexSet::new, |host| {
        paths.iter().map(|dir| canonical_dir(host, dir)).collect()
    });
    ImplicitDirs {
        host,
        paths,
        canonical,
        cache: RefCell::default(),
    }
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

fn shared_runtime_setting<'a>(
    model: &'a ProjectModel,
    target: &Target,
    step: &LinkStep,
    suffix: &[u8],
) -> Option<&'a [u8]> {
    definition(
        model,
        target.dir,
        &[
            b"CMAKE_SHARED_LIBRARY_RUNTIME_".as_slice(),
            &step.language,
            suffix,
        ]
        .concat(),
    )
    .map(|value| value.as_slice())
}

fn open_link_paths(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    step: &LinkStep,
    usage: &mut UsageResolver<'_>,
) -> Result<RuntimeLibraries, ResolveError> {
    let mut libraries =
        linked_libraries(step, super::super::shell::windows_host(model, target.dir));
    if is_darwin(model, target.dir) {
        libraries.extend(frameworks::named_framework_files(
            model, target, config, step, &libraries,
        ));
    }
    runtime_libraries(model, target, config, step, &libraries, usage)
}

pub(in crate::lower) fn install_rpath(
    model: &ProjectModel,
    target: &Target,
    resolved: &ResolvedTarget,
    languages: &IndexSet<BString>,
    targets: &IndexMap<(TargetId, &ConfigId), &ResolvedTarget>,
    usage: &mut UsageResolver<'_>,
) -> Result<Option<RuntimeEdit>, ResolveError> {
    let Some(link) = resolved
        .link
        .as_ref()
        .filter(|link| link.kind != LinkKind::StaticArchive)
    else {
        return Ok(None);
    };
    let darwin = is_darwin(model, target.dir);
    if !darwin && !elf_edit_enabled(model, target, link) {
        return Ok(None);
    }
    let libraries = open_link_paths(model, target, &resolved.config, link, usage)?;
    let paths = RuntimePaths::new(
        model,
        target,
        &resolved.config,
        link,
        darwin,
        languages,
        (&libraries, usage),
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
        let changes = linked_install_names(model, resolved, link, targets, usage)?;
        let update_paths = !build_with_install_rpath(model, target)
            && !on(model, target, b"CMAKE_NO_BUILTIN_CHRPATH")
            && runtime_setting(model, target, link, b"_FLAG") != Some(b"")
            && paths.build != paths.install;
        if name.is_none() && changes.is_empty() && !update_paths {
            return Ok(None);
        }
        let tool = definition(model, target.dir, b"CMAKE_INSTALL_NAME_TOOL")
            .cloned()
            .unwrap_or_default();
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
    usage: &mut UsageResolver<'_>,
) -> Result<IndexMap<BString, BString>, ResolveError> {
    let context = EvalContext {
        config: resolved.config.clone(),
        link_language: Some(link.language.clone()),
        ..Default::default()
    };
    let mut changes = IndexMap::new();
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
    let paths = RuntimePaths::new(
        model,
        target,
        config,
        step,
        true,
        languages,
        (&libraries, usage),
    )?;
    Ok(if build_with_install_rpath(model, target) {
        paths.install
    } else {
        paths.build
    }
    .into_iter()
    .collect())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Compute runtime directories for one link configuration."
)]
fn rpath_entries(
    model: &ProjectModel,
    target: &Target,
    step: &LinkStep,
    darwin: bool,
    libraries: &RuntimeLibraries,
    installs: bool,
    value: &[u8],
    implicit: &ImplicitDirs<'_>,
    host: Option<&dyn Host>,
) -> IndexSet<BString> {
    let mut paths: IndexSet<BString> = list::split(value, list::Empty::Drop).into_iter().collect();
    if !installs {
        let build = &model.dir(DirId::ROOT).binary_dir;
        let build_path = Lexical::new(build);
        let output = join(build, &step.output);
        let output_dir = Lexical::new(Lexical::new(&output).parent());
        let origin = !darwin
            && target_property(model, target, b"BUILD_RPATH_USE_ORIGIN")
                .is_some_and(|value| truth::is_on(value));
        let runtime_directory = |directory: &[u8]| {
            if let Some(translated) =
                crate::lower::install_rpath::translated_link_directory(model, target, directory)
            {
                return translated;
            }
            let parent = Lexical::new(directory);
            if origin
                && build_path.prefix(parent)
                && let Some(relative) = parent.relative(output_dir)
            {
                return if relative == b"." {
                    "$ORIGIN".into()
                } else if relative
                    .split(|&byte| byte == b'/')
                    .all(|part| part == b"..")
                {
                    [b"$ORIGIN/".as_slice(), &relative, b"/"].concat().into()
                } else {
                    join(b"$ORIGIN", &relative)
                };
            }
            directory.into()
        };
        let mut raw_entries: IndexMap<BString, BString> =
            paths.into_iter().map(|dir| (dir.clone(), dir)).collect();
        for dir in &libraries.user_dirs {
            raw_entries
                .entry(dir.clone())
                .or_insert_with(|| runtime_directory(dir));
        }
        let mut conflict_items: Vec<(BString, BString)> = Vec::new();
        for library in &libraries.files {
            let path = Lexical::new(&join(build, library)).normalize();
            let file = Lexical::new(&path);
            let parent_norm = Lexical::new(file.parent()).normalize();
            if implicit.contains(&parent_norm) {
                continue;
            }
            if darwin
                && let Some(parent) =
                    crate::lower::install_rpath::framework_runtime_directory(model, target, &path)
            {
                let directory =
                    crate::lower::install_rpath::translated_link_directory(model, target, parent);
                raw_entries
                    .entry(parent.into())
                    .or_insert_with(|| directory.unwrap_or_else(|| parent.into()));
            } else if shared_library(file.filename()) && (path.starts_with(b"/") || file.absolute())
            {
                let raw_dir: BString = file.parent().into();
                raw_entries
                    .entry(raw_dir.clone())
                    .or_insert_with(|| runtime_directory(file.parent()));
                conflict_items.push((raw_dir, file.filename().into()));
            }
        }
        if raw_entries.len() > 1
            && !conflict_items.is_empty()
            && let Some(host) = host
        {
            let order = order_runtime_directories(host, &raw_entries, &conflict_items);
            return order
                .into_iter()
                .filter_map(|idx| raw_entries.get_index(idx).map(|(_, out)| out.clone()))
                .collect();
        }
        return raw_entries.into_values().collect();
    } else if target_property(model, target, b"INSTALL_RPATH_USE_LINK_PATH")
        .is_some_and(|value| truth::is_on(value))
    {
        let root = model.dir(DirId::ROOT);
        for directory in &libraries.user_dirs {
            let path = Lexical::new(directory);
            if (directory.starts_with(b"/") || path.absolute())
                && !Lexical::new(&root.source_dir).prefix(path)
                && !Lexical::new(&root.binary_dir).prefix(path)
            {
                paths.insert(
                    crate::lower::install_rpath::translated_link_directory(
                        model, target, directory,
                    )
                    .unwrap_or_else(|| directory.clone()),
                );
            }
        }
        paths.extend(crate::lower::install_rpath::external_link_directories(
            model,
            target,
            &libraries.files,
            implicit,
        ));
    }
    paths
}

/// Order build runtime search directories using CMake's `cmOrderDirectories`
/// conflict rule: if a linked shared library `item_dir/filename` also exists in
/// another candidate directory `other_dir`, then `item_dir` must precede
/// `other_dir`. Unconstrained pairs retain their original relative order.
fn order_runtime_directories(
    host: &dyn Host,
    dirs: &IndexMap<BString, BString>,
    items: &[(BString, BString)],
) -> Vec<usize> {
    let n = dirs.len();
    let mut adj: Vec<IndexSet<usize>> = vec![IndexSet::new(); n];
    let mut in_degree = vec![0usize; n];
    let mut exists_cache: HashMap<(usize, &[u8]), bool> = HashMap::new();
    for (item_dir, filename) in items {
        let Some(idx_a) = dirs.get_index_of(item_dir) else {
            continue;
        };
        for (idx_b, (dir_b, _)) in dirs.iter().enumerate() {
            if idx_b == idx_a || (!dir_b.starts_with(b"/") && !Lexical::new(dir_b).absolute()) {
                continue;
            }
            let conflict = *exists_cache
                .entry((idx_b, filename.as_slice()))
                .or_insert_with(|| {
                    let probe = join(dir_b, filename);
                    host.metadata(&path::to_native(&probe), Follow::Yes).is_ok()
                });
            if conflict
                && adj.get_mut(idx_a).is_some_and(|edges| edges.insert(idx_b))
                && let Some(deg) = in_degree.get_mut(idx_b)
            {
                *deg += 1;
            }
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = in_degree
        .iter()
        .enumerate()
        .filter_map(|(i, &deg)| (deg == 0).then_some(Reverse(i)))
        .collect();
    let mut visited = vec![false; n];
    let mut result = Vec::with_capacity(n);
    while result.len() < n {
        let u = if let Some(Reverse(u)) = ready.pop() {
            u
        } else if let Some(u) = visited.iter().position(|&v| !v) {
            u
        } else {
            break;
        };
        let Some(slot) = visited.get_mut(u) else {
            break;
        };
        if *slot {
            continue;
        }
        *slot = true;
        result.push(u);
        for &v in adj.get(u).into_iter().flatten() {
            if visited.get(v) == Some(&false)
                && let Some(deg) = in_degree.get_mut(v)
            {
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    ready.push(Reverse(v));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcmake_host::PureHost;

    use super::*;

    #[test]
    fn order_runtime_directories_places_build_dir_before_conflicting_external_dir() {
        let host = PureHost::default();
        host.seed_file(Path::new("/opt/ros/lib/librclcpp.so"), b"")
            .unwrap();
        host.seed_file(Path::new("/opt/ros/lib/libbenchmark.so"), b"")
            .unwrap();
        host.seed_file(Path::new("/build/rclcpp/librclcpp.so"), b"")
            .unwrap();

        let mut dirs = IndexMap::new();
        dirs.insert(BString::from("/opt/ros/lib"), BString::from("/opt/ros/lib"));
        dirs.insert(
            BString::from("/build/rclcpp"),
            BString::from("/build/rclcpp"),
        );
        let items = vec![
            (
                BString::from("/opt/ros/lib"),
                BString::from("libbenchmark.so"),
            ),
            (
                BString::from("/build/rclcpp"),
                BString::from("librclcpp.so"),
            ),
        ];
        let order = order_runtime_directories(&host, &dirs, &items);
        let ordered: Vec<_> = order
            .into_iter()
            .map(|idx| dirs.get_index(idx).unwrap().0.clone())
            .collect();
        assert_eq!(
            ordered,
            vec![
                BString::from("/build/rclcpp"),
                BString::from("/opt/ros/lib")
            ]
        );
    }
}
