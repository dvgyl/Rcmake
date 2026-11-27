//! Device-link selection, arguments, and graph inputs.

use std::collections::BTreeSet;

use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, ProjectModel, Target, TargetKind};

mod rule;

use super::arch::{expand_architectures, ipo_enabled, target_architectures};
use crate::lower::shell::{direct, join_words};
use crate::lower::{compute_artifacts, definition, join, object_dir};
use crate::usage::link_options::{Wrapper, shell_arguments};
use crate::usage::{ResolvedLinkItem, ResolvedLinkKind};
use crate::{CubinLink, DeviceLinkMode, DeviceLinkResponseFile, ResolveError};

/// Action that writes the device object for a host link or archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceLinkPlan {
    /// Language that supplies the device linker.
    pub(crate) lang: BString,
    /// Device object path, relative to the build root.
    pub(crate) output: BString,
    /// File dependencies of the device link.
    pub(crate) inputs: Vec<BString>,
    /// Process invocations and their outputs.
    pub(crate) mode: DeviceLinkMode,
}

/// Return whether any device-link language is enabled and supports a device-link phase for `target`.
pub(crate) fn device_link_supported(model: &ProjectModel, target: &Target) -> bool {
    if !matches!(
        target.kind,
        TargetKind::Executable
            | TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::ModuleLibrary
    ) {
        return false;
    }
    let Some(enabled) = model.global.props.get(b"ENABLED_LANGUAGES") else {
        return false;
    };
    let enabled = list::split(enabled, list::Empty::Drop);
    [b"CUDA".as_slice(), b"SYCL"].into_iter().any(|lang| {
        enabled.iter().any(|language| language.as_slice() == lang)
            && lang_definition(model, target, lang, b"_COMPILER_HAS_DEVICE_LINK_PHASE")
                .is_some_and(|value| truth::is_on(value))
    })
}

/// Return the first language that requires a device link.
pub(crate) fn device_link_language(
    model: &ProjectModel,
    target: &Target,
    closure_langs: &[BString],
    link_items: &[ResolvedLinkItem],
) -> Option<&'static [u8]> {
    [b"CUDA".as_slice(), b"SYCL"]
        .into_iter()
        .find(|lang| require_device_linking(model, target, closure_langs, link_items, lang))
}

/// Apply the device-link predicate in policy order.
pub(crate) fn require_device_linking(
    model: &ProjectModel,
    target: &Target,
    closure_langs: &[BString],
    link_items: &[ResolvedLinkItem],
    lang: &[u8],
) -> bool {
    let enabled = model
        .global
        .props
        .get(b"ENABLED_LANGUAGES")
        .is_some_and(|value| {
            list::split(value, list::Empty::Drop)
                .iter()
                .any(|language| language.as_slice() == lang)
        });
    if !enabled
        || !matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::StaticLibrary
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
        )
    {
        return false;
    }
    if !lang_definition(model, target, lang, b"_COMPILER_HAS_DEVICE_LINK_PHASE")
        .is_some_and(|value| truth::is_on(value))
    {
        return false;
    }
    let resolves = [lang, b"_RESOLVE_DEVICE_SYMBOLS"].concat();
    if let Some(value) = target.props.get(&resolves) {
        return truth::is_on(value);
    }
    if target.kind == TargetKind::StaticLibrary {
        return false;
    }
    if !closure_langs
        .iter()
        .any(|language| language.as_slice() == lang)
    {
        return false;
    }
    let separable = [lang, b"_SEPARABLE_COMPILATION"].concat();
    if target
        .props
        .get(&separable)
        .is_some_and(|value| truth::is_on(value))
    {
        return matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary | TargetKind::Executable
        );
    }
    unresolved_static_libraries(model, link_items, lang)
        .next()
        .is_some()
}

fn unresolved_static_libraries<'a>(
    model: &'a ProjectModel,
    items: &'a [ResolvedLinkItem],
    lang: &[u8],
) -> impl Iterator<Item = &'a Target> + 'a {
    let separable = [lang, b"_SEPARABLE_COMPILATION"].concat();
    let resolves = [lang, b"_RESOLVE_DEVICE_SYMBOLS"].concat();
    items
        .iter()
        .flat_map(ResolvedLinkItem::targets)
        .filter_map(move |id| model.target(id))
        .filter(move |dep| {
            dep.kind == TargetKind::StaticLibrary
                && dep
                    .props
                    .get(&separable)
                    .is_some_and(|value| truth::is_on(value))
                && !dep
                    .props
                    .get(&resolves)
                    .is_some_and(|value| truth::is_on(value))
        })
}

/// The argument list and file dependencies of a device link.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeviceLibraries {
    /// Filtered linker arguments in link order.
    pub(crate) args: Vec<BString>,
    /// Objects and archives that activate the rule when they change.
    pub(crate) files: Vec<BString>,
    /// Whether an unresolved separable static library uses interprocedural optimization.
    pub(crate) requires_ipo: bool,
}

/// Select the libraries and file prerequisites that the device linker can consume.
pub(crate) fn device_libraries(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    items: &[ResolvedLinkItem],
    lang: &[u8],
) -> DeviceLibraries {
    let mut libraries = DeviceLibraries {
        requires_ipo: unresolved_static_libraries(model, items, lang)
            .any(|dep| ipo_enabled(model, dep, config, lang)),
        ..DeviceLibraries::default()
    };
    let mut seen = BTreeSet::new();
    let mut iter = items.iter().flat_map(|item| match &item.kind {
        ResolvedLinkKind::Group { members, .. } => members.as_slice(),
        _ => std::slice::from_ref(item),
    });
    let resolves = [lang, b"_RESOLVE_DEVICE_SYMBOLS"].concat();
    while let Some(item) = iter.next() {
        let (arg, file) = match item.kind {
            ResolvedLinkKind::Library(id) => {
                let Some(library) = model.target(id) else {
                    continue;
                };
                if matches!(
                    library.kind,
                    TargetKind::SharedLibrary | TargetKind::ModuleLibrary
                ) || library
                    .props
                    .get(&resolves)
                    .is_some_and(|value| truth::is_on(value))
                {
                    continue;
                }
                let Some(path) = compute_artifacts(model, library, config).linker_file else {
                    continue;
                };
                if !device_file(&path) {
                    continue;
                }
                (path, true)
            }
            ResolvedLinkKind::ObjectFile => (item.text.value.clone(), true),
            ResolvedLinkKind::Raw => {
                let value = &item.text.value;
                if value == b"-framework" {
                    iter.next();
                    continue;
                }
                let flag = value.starts_with(b"-");
                if flag && !matches_library_flag(value) {
                    continue;
                }
                let file = !flag && (Lexical::new(value).absolute() || value.contains(&b'/'));
                if file && !device_file(value) {
                    continue;
                }
                (value.clone(), file)
            }
            _ => continue,
        };
        if !arg.is_empty() && seen.insert(arg.clone()) {
            if file {
                libraries.files.push(arg.clone());
            }
            libraries.args.push(arg);
        }
    }

    if let Some(standard) = lang_definition(model, target, lang, b"_STANDARD_LIBRARIES") {
        libraries.args.extend(shell_arguments(standard));
    }
    if !libraries.args.is_empty()
        && let Some(whole) =
            lang_definition(model, target, lang, b"_DEVICE_LINK_OPTIONS_WHOLE_ARCHIVE")
                .filter(|whole| !whole.is_empty())
    {
        libraries
            .args
            .splice(..0, list::split(whole, list::Empty::Drop));
        if let Some(end) = lang_definition(
            model,
            target,
            lang,
            b"_DEVICE_LINK_OPTIONS_NO_WHOLE_ARCHIVE",
        ) {
            libraries.args.extend(list::split(end, list::Empty::Drop));
        }
    }
    libraries
}

fn device_file(value: &[u8]) -> bool {
    [b".o".as_slice(), b".obj", b".a", b".lib"]
        .iter()
        .any(|suffix| value.ends_with(suffix))
}

fn matches_library_flag(value: &[u8]) -> bool {
    [b"-l".as_slice(), b"-L", b"--library"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

/// Filter evaluated link options and expand the linker wrapper for the selected phase.
///
/// Supply raw evaluated `LINK_OPTIONS` and interface options in the selected phase.
/// For a static library's device link, use `LINK_OPTIONS`. Use `STATIC_LIBRARY_OPTIONS`
/// for the archive command. Keep phase markers and `SHELL:` and `LINKER:` groups
/// intact until this function returns the individual arguments.
pub(crate) fn link_options(
    model: &ProjectModel,
    target: &Target,
    lang: &[u8],
    options: &[BString],
    device: bool,
) -> Result<Vec<BString>, ResolveError> {
    if device && target.policies.policies.get(PolicyId::new(105)) != PolicyStatus::New {
        return Ok(Vec::new());
    }
    let prefix = if device {
        b"DEVICE_LINKER".as_slice()
    } else {
        b"LINKER"
    };
    let wrapper = Wrapper::new(model, target, lang, prefix);
    let mut selected = Vec::new();
    let mut unique = BTreeSet::new();
    let mut in_device = false;
    let mut in_host = false;
    for option in options {
        match option.as_slice() {
            b"<DEVICE_LINK>" => in_device = true,
            b"</DEVICE_LINK>" => in_device = false,
            b"<HOST_LINK>" => in_host = true,
            b"</HOST_LINK>" => in_host = false,
            _ if (in_device && !device) || (in_host && device) => {}
            _ if unique.insert(option) => {
                let args = if device
                    && !in_device
                    && !option.starts_with(b"LINKER:")
                    && !option.starts_with(b"DEVICE_LINKER:")
                {
                    let value = option.strip_prefix(b"SHELL:").unwrap_or(option);
                    host_options(model, target, lang, shell_arguments(value))?
                } else {
                    option
                        .strip_prefix(b"SHELL:")
                        .map_or_else(|| vec![option.clone()], shell_arguments)
                };
                for arg in args {
                    selected.extend(expand_linker(&wrapper, prefix, &arg, device)?);
                }
            }
            _ => {}
        }
    }
    Ok(selected)
}

fn expand_linker(
    wrapper: &Wrapper<'_>,
    prefix: &[u8],
    arg: &[u8],
    device: bool,
) -> Result<Vec<BString>, ResolveError> {
    if device && let Some(payload) = arg.strip_prefix(b"LINKER:") {
        return wrapper.expand(&[prefix, b":", payload].concat());
    }
    wrapper.expand(arg)
}

fn host_options(
    model: &ProjectModel,
    target: &Target,
    lang: &[u8],
    args: Vec<BString>,
) -> Result<Vec<BString>, ResolveError> {
    if lang_definition(model, target, lang, b"_DEVICE_COMPILER_WRAPPER_FLAG")
        .is_none_or(|value| value.is_empty())
    {
        return Ok(args);
    }
    let wrapper = Wrapper::new(model, target, lang, b"DEVICE_COMPILER");
    let mut wrapped = Vec::new();
    let mut ordinary = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let linker_arg = if arg.starts_with(b"LINKER:") || arg.starts_with(b"DEVICE_LINKER:") {
            Some(arg.clone())
        } else if let Some(payload) = arg
            .strip_prefix(b"-Wl,")
            .or_else(|| arg.strip_prefix(b"-Xlinker="))
        {
            Some(prefixed(b"LINKER:", payload))
        } else if arg == b"-Xlinker" {
            Some(
                args.next()
                    .map_or_else(|| arg.clone(), |payload| prefixed(b"LINKER:", &payload)),
            )
        } else {
            None
        };
        if let Some(linker_arg) = linker_arg {
            append_host_options(&wrapper, &mut wrapped, &mut ordinary)?;
            wrapped.push(linker_arg);
        } else {
            ordinary.push(arg);
        }
    }
    append_host_options(&wrapper, &mut wrapped, &mut ordinary)?;
    Ok(wrapped)
}

fn append_host_options(
    wrapper: &Wrapper<'_>,
    wrapped: &mut Vec<BString>,
    ordinary: &mut Vec<BString>,
) -> Result<(), ResolveError> {
    if !ordinary.is_empty() {
        wrapped.extend(wrapper.expand(&prefixed(b"DEVICE_COMPILER:SHELL:", &words(ordinary)))?);
        ordinary.clear();
    }
    Ok(())
}

/// Configured inputs used by the device linker and stub compiler.
pub(crate) struct DeviceLinkInputs<'a> {
    /// Target objects in compile order, including directly linked object libraries.
    pub(crate) objects: &'a [BString],
    /// Filtered libraries and file dependencies.
    pub(crate) libraries: &'a DeviceLibraries,
    /// Device link directories, expressed as compiler arguments.
    pub(crate) directories: &'a [BString],
    /// Device link options, including architecture flags.
    pub(crate) link_flags: &'a [BString],
    /// Device compiler flags, excluding the flags already in link_flags.
    pub(crate) compile_flags: &'a [BString],
}

/// Construct the compiler-driver or explicit device-link actions for one configuration.
pub(crate) fn plan(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    lang: &[u8],
    input: &DeviceLinkInputs<'_>,
) -> Result<DeviceLinkPlan, ResolveError> {
    let dir = object_dir(model, target, config);
    let get = |suffix: &[u8]| lang_definition(model, target, lang, suffix);
    let extension = get(b"_OUTPUT_EXTENSION").map_or(b".o".as_slice(), |value| value.as_slice());
    let output = join(&dir, &[b"cmake_device_link".as_slice(), extension].concat());
    let mut seen = BTreeSet::new();
    let inputs: Vec<BString> = input
        .objects
        .iter()
        .chain(&input.libraries.files)
        .filter(|value| seen.insert(*value))
        .cloned()
        .collect();
    let explicit = lang == b"CUDA" && get(b"_COMPILER_ID").is_some_and(|id| id == b"Clang");

    let mut link_flags = Vec::new();
    if (input.libraries.requires_ipo || ipo_enabled(model, target, config, lang))
        && let Some(flags) = get(b"_DEVICE_LINK_OPTIONS_IPO")
    {
        for flag in list::iter(flags, list::Empty::Drop).flat_map(|value| shell_arguments(&value)) {
            if !input.link_flags.contains(&flag) && !link_flags.contains(&flag) {
                link_flags.push(flag);
            }
        }
    }
    link_flags.extend_from_slice(input.link_flags);
    let rule = rule::Rule {
        model,
        target,
        config,
        lang,
        dir: &dir,
        output: &output,
        input,
        link_flags: &link_flags,
    };
    let mode = if explicit {
        explicit_plan(&rule, &inputs)?
    } else {
        let suffix: &[u8] = match target.kind {
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
                b"_DEVICE_LINK_LIBRARY"
            }
            _ => b"_DEVICE_LINK_EXECUTABLE",
        };
        let template = get(suffix).ok_or_else(|| missing(lang, suffix))?;
        let invocation = rule.expand(template, None)?;
        let response_file = get(b"_RESPONSE_FILE_DEVICE_LINK_FLAG")
            .filter(|flag| !flag.is_empty())
            .map(|flag| DeviceLinkResponseFile {
                flag: flag.clone(),
                objects: input.objects.to_vec(),
                libraries: input.libraries.args.clone(),
            });
        DeviceLinkMode::Driver {
            invocation,
            response_file,
        }
    };
    Ok(DeviceLinkPlan {
        lang: lang.into(),
        output,
        inputs,
        mode,
    })
}

fn explicit_plan(
    rule: &rule::Rule<'_>,
    inputs: &[BString],
) -> Result<DeviceLinkMode, ResolveError> {
    let spec = target_architectures(rule.target).map_err(ResolveError::Message)?;
    let archs =
        expand_architectures(rule.model, rule.target, &spec).map_err(ResolveError::Message)?;
    if archs.is_empty() {
        return Err(ResolveError::Message(
            "CUDA_SEPARABLE_COMPILATION on Clang requires CUDA_ARCHITECTURES to be set.".into(),
        ));
    }
    let get = |key: &[u8]| {
        definition(rule.model, rule.target.dir, key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ResolveError::Message(
                    format!(
                        "{} is required for CUDA device linking.",
                        BString::from(key)
                    )
                    .into(),
                )
            })
    };
    let linker = get(b"CMAKE_CUDA_DEVICE_LINKER")?;
    let register_header = join(rule.dir, b"cmake_cuda_register.h");
    let fatbin_header = join(rule.dir, b"cmake_cuda_fatbin.h");
    let mut cubins = Vec::new();
    let mut seen = BTreeSet::new();
    let mut fatbinary = vec![
        get(b"CMAKE_CUDA_FATBINARY")?.clone(),
        "-64".into(),
        "-cmdline=--compile-only".into(),
        "-compress-all".into(),
        "-link".into(),
        prefixed(b"--embedded-fatbin=", &fatbin_header),
    ];
    for arch in archs
        .into_iter()
        .filter(|arch| seen.insert(arch.name.clone()))
    {
        let profile = prefixed(b"sm_", &arch.name);
        let cubin = join(rule.dir, &[profile.as_slice(), b".cubin"].concat());
        let mut argv = vec![linker.clone(), prefixed(b"-arch=", &profile)];
        if cubins.is_empty() {
            argv.push(prefixed(b"--register-link-binaries=", &register_header));
        }
        argv.push(prefixed(b"-o=", &cubin));
        argv.extend_from_slice(inputs);
        fatbinary.push(
            [b"-im=profile=".as_slice(), &profile, b",file=", &cubin]
                .concat()
                .into(),
        );
        cubins.push(CubinLink {
            arch: arch.name,
            output: cubin,
            invocation: direct(argv, None),
        });
    }
    let build_root = &rule.model.dir(DirId::ROOT).binary_dir;
    let abs_fatbin = if build_root.is_empty() {
        fatbin_header.clone()
    } else {
        join(build_root, &fatbin_header)
    };
    let abs_register = if build_root.is_empty() {
        register_header.clone()
    } else {
        join(build_root, &register_header)
    };
    let compile = rule.expand(
        get(b"CMAKE_CUDA_DEVICE_LINK_COMPILE")?,
        Some((&abs_fatbin, &abs_register)),
    )?;
    Ok(DeviceLinkMode::Explicit {
        cubins,
        fatbinary: direct(fatbinary, None),
        fatbin_header,
        register_header,
        compile,
    })
}

fn lang_definition<'a>(
    model: &'a ProjectModel,
    target: &Target,
    lang: &[u8],
    suffix: &[u8],
) -> Option<&'a BString> {
    definition(model, target.dir, &[b"CMAKE_", lang, suffix].concat())
}

fn missing(lang: &[u8], suffix: &[u8]) -> ResolveError {
    let variable = BString::from([b"CMAKE_", lang, suffix].concat());
    ResolveError::Message(format!("{variable} is required for device linking.").into())
}

fn words(args: &[BString]) -> BString {
    join_words(args.iter().map(|arg| quote(arg)))
}

fn prefixed(prefix: &[u8], value: &[u8]) -> BString {
    [prefix, value].concat().into()
}

#[cfg(test)]
mod tests;
