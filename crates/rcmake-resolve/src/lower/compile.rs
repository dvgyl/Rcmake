//! Compile flags and compiler rule expansion.

mod lint;

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_genex::{ConfigId, EvalContext, evaluate, parse};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target};

use super::artifacts::{compile_pdb, is_darwin};
use super::launcher;
use super::pch::PchFlags;
use super::policy_flags::Flags;
use super::shell::{command, join_words, raw_flags, rule_invocations};
use super::standard::{checked_standard_flag, standard_flag};
use super::{
    definition, expand_rule, join, object_path, target_property, without_trailing_slashes,
};
use crate::{
    ArtifactSet, BuildStep, CompileGroup, CompileUnit, ResolveError, usage::UsageResolver,
};

pub(crate) fn language_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
) -> Vec<BString> {
    let base = [b"CMAKE_", language, b"_FLAGS"].concat();
    let configured = [base.as_slice(), b"_", &config.0.to_ascii_uppercase()].concat();
    [base.as_slice(), configured.as_slice()]
        .into_iter()
        .filter_map(|key| {
            definition(model, target.dir, key).map(|value| raw_flags(model, target.dir, value))
        })
        .collect()
}

pub(super) fn apple_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    language: &[u8],
) -> Vec<BString> {
    if !is_darwin(model, target.dir) {
        return Vec::new();
    }
    let key = [&b"OSX_ARCHITECTURES_"[..], &config.0.to_ascii_uppercase()].concat();
    let mut architectures = (!config.0.is_empty())
        .then(|| target.props.get(&key))
        .flatten()
        .or_else(|| target_property(model, target, b"OSX_ARCHITECTURES"))
        .map_or_else(Vec::new, |value| list::split(value, list::Empty::Drop));
    let compiler_target = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_COMPILER_TARGET"].concat(),
    )
    .filter(|value| !value.is_empty());
    if architectures.is_empty()
        && matches!(
            language,
            b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM" | b"SYCL"
        )
        && compiler_target.is_none()
        && let Some(default) = definition(model, target.dir, b"_CMAKE_APPLE_ARCHS_DEFAULT")
    {
        architectures = list::split(default, list::Empty::Drop);
    }
    let system = super::apple::system(model, target.dir);
    let variant = super::apple::variant(model, target.dir);
    let mut flags = Vec::new();
    let version = definition(model, target.dir, b"CMAKE_OSX_DEPLOYMENT_TARGET")
        .map_or(b"".as_slice(), |value| value.as_slice());
    let triple = match (system, variant) {
        (b"iOS", Some(super::apple::Variant::Catalyst)) => {
            Some((b"ios".as_slice(), b"-macabi".as_slice()))
        }
        (b"visionOS", Some(super::apple::Variant::Simulator)) => {
            Some((b"xros".as_slice(), b"-simulator".as_slice()))
        }
        (b"visionOS", _) => Some((b"xros".as_slice(), b"".as_slice())),
        _ => None,
    };
    for arch in &architectures {
        flags.push("-arch".into());
        flags.push(quote(arch));
    }
    if let Some((platform, suffix)) = triple
        && compiler_target.is_none()
        && !version.is_empty()
    {
        let arch = architectures
            .iter()
            .find(|arch| *arch == "arm64")
            .or_else(|| architectures.first())
            .map_or(b"arm64".as_slice(), |arch| arch.as_slice());
        flags.push(quote(
            &[b"--target=", arch, b"-apple-", platform, version, suffix].concat(),
        ));
    }
    let sysroot = definition(model, target.dir, b"CMAKE_OSX_SYSROOT")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            definition(
                model,
                target.dir,
                &[b"CMAKE_", language, b"_COMPILER_APPLE_SYSROOT_REQUIRED"].concat(),
            )
            .filter(|value| truth::is_on(value))
            .and_then(|_| definition(model, target.dir, b"_CMAKE_OSX_SYSROOT_PATH"))
        });
    let sysroot_flag = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_SYSROOT_FLAG"].concat(),
    )
    .map_or(b"-isysroot".as_slice(), |value| value.as_slice());
    if let Some(sdk) = sysroot.filter(|value| !value.is_empty() && value.as_slice() != b"/")
        && !sysroot_flag.is_empty()
    {
        flags.push(sysroot_flag.into());
        flags.push(quote(sdk));
    }
    if !version.is_empty() && triple.is_none() {
        let prefix: &[u8] = match (system, variant) {
            (b"iOS", Some(super::apple::Variant::Simulator)) => b"-mios-simulator-version-min=",
            (b"iOS", _) => b"-miphoneos-version-min=",
            (b"tvOS", Some(super::apple::Variant::Simulator)) => b"-mtvos-simulator-version-min=",
            (b"tvOS", _) => b"-mtvos-version-min=",
            (b"watchOS", Some(super::apple::Variant::Simulator)) => {
                b"-mwatchos-simulator-version-min="
            }
            (b"watchOS", _) => b"-mwatchos-version-min=",
            _ => b"-mmacosx-version-min=",
        };
        flags.push(quote(&[prefix, version].concat()));
    }
    flags
}

fn compile_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    group: &CompileGroup,
    required_standard: Option<&[u8]>,
    position_independent: Option<bool>,
) -> Result<Vec<BString>, ResolveError> {
    let language = group.language.as_slice();
    let mut flags = language_flags(model, target, config, language);
    let standard = if language == b"CUDA" {
        checked_standard_flag(model, target, language, required_standard)?
    } else {
        standard_flag(model, target, language, required_standard)
    };
    flags.extend(standard);
    flags.extend(apple_flags(model, target, config, language));
    let policy = Flags::new(model, target, language);
    flags.extend(policy.compile(policy.ipo(config), position_independent));

    if let Some(default) = definition(model, target.dir, b"CMAKE_MSVC_RUNTIME_LIBRARY_DEFAULT")
        .filter(|value| !value.is_empty())
    {
        let raw = target.props.get(b"MSVC_RUNTIME_LIBRARY").unwrap_or(default);
        let target_id = model
            .find_target(target.dir, &target.name)
            .map(crate::usage::handle);
        let context = EvalContext {
            config: config.clone(),
            head: target_id,
            current: target_id,
            language: Some(group.language.clone()),
            ..EvalContext::default()
        };
        let mut usage = crate::usage::UsageResolver::new(model);
        let (runtime, _) = evaluate(&parse(raw)?, &context, &mut usage.host(target.dir))?;
        if !runtime.is_empty()
            && let Some(options) = definition(
                model,
                target.dir,
                &[
                    b"CMAKE_",
                    language,
                    b"_COMPILE_OPTIONS_MSVC_RUNTIME_LIBRARY_",
                    &runtime,
                ]
                .concat(),
            )
        {
            flags.extend(
                list::split(options, list::Empty::Drop)
                    .iter()
                    .map(|option| quote(option)),
            );
        }
    }

    let preset_key = [language, b"_VISIBILITY_PRESET"].concat();
    if let Some(preset) = target_property(model, target, &preset_key).filter(|p| !p.is_empty()) {
        flags.push(quote(&[b"-fvisibility=", preset.as_slice()].concat()));
    }
    if matches!(language, b"CXX" | b"OBJCXX")
        && target_property(model, target, b"VISIBILITY_INLINES_HIDDEN")
            .is_some_and(|v| truth::is_on(v))
    {
        flags.push("-fvisibility-inlines-hidden".into());
    }
    policy.append_color(&mut flags);
    flags.extend(
        group
            .flags
            .iter()
            .filter(|flag| !flag.value.is_empty())
            .map(|flag| {
                flag.value
                    .strip_prefix(b"SHELL:")
                    .map_or_else(|| quote(&flag.value), BString::from)
            }),
    );
    Ok(flags)
}

fn includes(model: &ProjectModel, target: &Target, group: &CompileGroup) -> BString {
    let darwin = is_darwin(model, target.dir);
    let key = [
        b"CMAKE_",
        group.language.as_slice(),
        b"_IMPLICIT_INCLUDE_DIRECTORIES",
    ]
    .concat();
    let implicit: IndexSet<_> = definition(model, target.dir, &key)
        .into_iter()
        .flat_map(|value| list::split(value, list::Empty::Drop))
        .map(|path| Lexical::new(&path).normalize())
        .map(|path| BString::from(without_trailing_slashes(&path)))
        .collect();
    let mut paths = IndexSet::new();
    let mut frameworks = implicit_frameworks(model, target);
    join_words(group.includes.iter().filter_map(|include| {
        let path = clean_include_path(&include.path.value);
        let identity = Lexical::new(&path).normalize();
        let identity = without_trailing_slashes(&identity);
        if implicit.contains(identity) || !paths.insert(path.clone()) {
            return None;
        }
        if darwin && let Some(directory) = framework_directory(&path) {
            let identity = Lexical::new(directory).normalize();
            return frameworks.insert(identity).then(|| {
                framework_flag(model, target, &group.language, directory, include.system)
            });
        }
        Some(if include.system {
            join_words(["-isystem".into(), quote(&path)])
        } else {
            quote(&[b"-I".as_slice(), &path].concat())
        })
    }))
}

fn clean_include_path(path: &[u8]) -> BString {
    let path = without_trailing_slashes(path);
    let lex = Lexical::new(path);
    let root_end = path.len() - lex.relative_part().len();
    let mut result = BString::from(path.get(..root_end).unwrap_or_default());
    let mut parts = lex.components();
    if let Some(first) = parts.next() {
        result.extend_from_slice(first);
    }
    for part in parts {
        result.push(b'/');
        result.extend_from_slice(part);
    }
    result
}

fn framework_directory(path: &[u8]) -> Option<&[u8]> {
    let path = Lexical::new(path);
    path.filename().ends_with(b".framework").then(|| {
        let parent = path.parent();
        if parent.is_empty() {
            b".".as_slice()
        } else {
            parent
        }
    })
}

fn framework_flag(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    directory: &[u8],
    system: bool,
) -> BString {
    let flag = |suffix| definition(model, target.dir, &[b"CMAKE_", language, suffix].concat());
    let prefix = system
        .then(|| flag(b"_SYSTEM_FRAMEWORK_SEARCH_FLAG"))
        .flatten()
        .filter(|value| !value.is_empty())
        .or_else(|| flag(b"_FRAMEWORK_SEARCH_FLAG"))
        .map_or(b"-F".as_slice(), |value| value.as_slice());
    if prefix.ends_with(b" ") {
        [prefix, &quote(directory)].concat().into()
    } else {
        quote(&[prefix, directory].concat())
    }
}

fn implicit_frameworks(model: &ProjectModel, target: &Target) -> IndexSet<BString> {
    definition(
        model,
        target.dir,
        b"CMAKE_PLATFORM_IMPLICIT_LINK_FRAMEWORK_DIRECTORIES",
    )
    .into_iter()
    .flat_map(|value| list::split(value, list::Empty::Drop))
    .map(|value| Lexical::new(&value).normalize())
    .collect()
}

fn linked_framework_includes(
    model: &ProjectModel,
    target: &Target,
    group: &CompileGroup,
    directories: &[(BString, bool)],
) -> BString {
    if directories.is_empty() {
        return BString::default();
    }
    let mut emitted = implicit_frameworks(model, target);
    emitted.extend(group.includes.iter().filter_map(|include| {
        let path = clean_include_path(&include.path.value);
        framework_directory(&path).map(|path| Lexical::new(path).normalize())
    }));
    join_words(directories.iter().filter_map(|(directory, system)| {
        let path = clean_include_path(directory);
        emitted.insert(Lexical::new(&path).normalize()).then(|| {
            let system = *system
                || group.includes.iter().any(|include| {
                    include.system && Lexical::new(&path).equal(Lexical::new(&include.path.value))
                });
            framework_flag(model, target, &group.language, &path, system)
        })
    }))
}

pub(super) fn compile_arguments(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    group: &CompileGroup,
    required_standard: Option<&[u8]>,
    source: &[u8],
    position_independent: Option<bool>,
) -> Result<(Vec<BString>, BString, BString), ResolveError> {
    let language = group.language.as_slice();
    let mut flags = compile_flags(
        model,
        target,
        config,
        group,
        required_standard,
        position_independent,
    )?;
    let boundary = group.source_define_start.unwrap_or(group.defines.len());
    let (target_defines, source_defines) = group
        .defines
        .split_at_checked(boundary)
        .unwrap_or((&group.defines, &[]));
    let defines = [target_defines, source_defines]
        .into_iter()
        .flat_map(|definitions| {
            let mut definitions: Vec<_> = definitions
                .iter()
                .map(|define| define.value.as_slice())
                .collect();
            definitions.sort_unstable();
            definitions.dedup();
            definitions
        });
    let intdir = (model.generator == Some(GeneratorKind::NinjaMultiConfig)).then(|| {
        let mut define = BString::from(b"-DCMAKE_INTDIR=\\\"".as_slice());
        define.extend_from_slice(&quote(&config.0));
        define.extend_from_slice(b"\\\"");
        define
    });
    let defines = join_words(
        intdir
            .into_iter()
            .chain(defines.map(|define| quote(&[b"-D", define].concat()))),
    );
    let includes = includes(model, target, group);
    if (model
        .dir(target.dir)
        .source_props
        .get(&Lexical::new(source).normalize())
        .and_then(|props| props.get(b"LANGUAGE"))
        .is_some()
        || super::source_language(model, target, source).is_none_or(|natural| natural != language))
        && let Some(options) = definition(
            model,
            target.dir,
            &[b"CMAKE_", language, b"_COMPILE_OPTIONS_EXPLICIT_LANGUAGE"].concat(),
        )
    {
        flags.extend(
            list::split(options, list::Empty::Drop)
                .into_iter()
                .map(|value| quote(&value)),
        );
    }
    Ok((flags, defines, includes))
}

#[derive(Default)]
pub(super) struct Settings<'a> {
    pub(super) standard: Option<&'a [u8]>,
    pub(super) pch: Option<PchFlags<'a>>,
    pub(super) position_independent: Option<bool>,
    pub(super) skip_file_set_linting: Option<bool>,
    pub(super) extra_framework_dirs: &'a [(BString, bool)],
}

#[expect(
    clippy::too_many_arguments,
    reason = "Compile lowering reuses the caller's usage resolution session."
)]
pub(super) fn compile(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    unit: &CompileUnit,
    group: &CompileGroup,
    settings: Settings<'_>,
    usage: &mut UsageResolver<'_>,
) -> Result<BuildStep, ResolveError> {
    let language = group.language.as_slice();
    let source = join(&model.dir(target.dir).source_dir, &unit.source.value);
    let (mut flags, defines, includes) = compile_arguments(
        model,
        target,
        config,
        group,
        settings.standard,
        &source,
        settings.position_independent,
    )?;
    let includes = join_words([
        includes,
        linked_framework_includes(model, target, group, settings.extra_framework_dirs),
    ]);
    if let Some(pch) = settings.pch {
        flags.extend(pch.options(model, target, language));
    }
    let object = if unit.object.is_empty() {
        object_path(model, target, config, &source)
    } else {
        unit.object.clone()
    };

    let depfile: BString = [object.as_slice(), b".d"].concat().into();
    let key = [b"CMAKE_", language, b"_COMPILE_OBJECT"].concat();
    let bmi_only = unit.scan && super::modules::is_std_bmi(model, target);
    let gcc = bmi_only
        && super::modules::module_map_format(model, target)?
            == rcmake_cxxmodules::ModmapFormat::Gcc;
    let template = if bmi_only && !gcc {
        definition(model, target.dir, b"CMAKE_CXX_COMPILE_BMI").filter(|v| !v.is_empty())
    } else {
        definition(model, target.dir, &key).filter(|v| !v.is_empty())
    };
    let action: &[u8] = if bmi_only && !gcc {
        b" --precompile"
    } else {
        b" -c"
    };
    let fallback = [
        b"<CMAKE_",
        language,
        b"_COMPILER> <DEFINES> <INCLUDES> <FLAGS> -o <OBJECT>",
        action,
        b" <SOURCE>",
    ]
    .concat();
    if bmi_only {
        if let Some(flag) = definition(model, target.dir, b"CMAKE_CXX_MODULE_BMI_ONLY_FLAG") {
            flags.push(flag.clone());
        } else if gcc {
            flags.push("-fmodule-only".into());
        }
    }
    let modmap = unit.scan.then(|| super::modules::modmap_path(&object));
    if let Some(path) = &modmap {
        flags.push(super::modules::module_map_flag(model, target, path));
    }
    let template_text = template.map_or(fallback.as_slice(), |v| v.as_slice());
    let compile_pdb = if has_placeholder(template_text, b"<TARGET_COMPILE_PDB>") {
        compile_pdb(model, target, config, usage)?
    } else {
        BString::default()
    };
    let compiler_depfile = !matches!(language, b"Fortran" | b"Swift");
    let dep_flags = if compiler_depfile {
        definition(
            model,
            target.dir,
            &[b"CMAKE_DEPFILE_FLAGS_", language].concat(),
        )
        .map(|v| v.as_slice())
        .or_else(|| {
            (template.is_none() && matches!(language, b"C" | b"CXX" | b"OBJC" | b"OBJCXX"))
                .then_some(b"-MD -MT <DEP_TARGET> -MF <DEP_FILE>")
        })
    } else {
        None
    };
    let lookup_paths = |key: &[u8]| match key {
        b"SOURCE" => Some(quote(&source)),
        b"TARGET_NAME" => Some(quote(&target.name)),
        b"LANGUAGE" => Some(quote(language)),
        b"CMAKE_CURRENT_BINARY_DIR" => Some(quote(&model.dir(target.dir).binary_dir)),
        b"CMAKE_BINARY_DIR" => Some(quote(&model.dir(DirId::ROOT).binary_dir)),
        b"CMAKE_CL_SHOWINCLUDES_PREFIX" => Some(quote(
            definition(model, target.dir, b"CMAKE_CL_SHOWINCLUDES_PREFIX")
                .map_or(b"".as_slice(), |value| value.as_slice()),
        )),
        b"TARGET_SUPPORT_DIR" => Some(quote(
            artifacts
                .object_dir
                .as_ref()
                .map_or(b"".as_slice(), |value| value.as_slice()),
        )),
        b"OBJECT" | b"DEP_TARGET" => Some(quote(&object)),
        b"DEP_FILE" => Some(quote(&depfile)),
        b"OBJECT_DIR" | b"OBJECT_FILE_DIR" => Some(quote(if key == b"OBJECT_DIR" {
            artifacts
                .object_dir
                .as_ref()
                .map_or(b"".as_slice(), |v| v.as_slice())
        } else {
            Lexical::new(&object).parent()
        })),
        b"TARGET_COMPILE_PDB" => Some(quote(&compile_pdb)),
        b"TARGET_PDB" => Some(
            artifacts
                .pdb_file
                .as_ref()
                .map_or_else(BString::default, |path| quote(path)),
        ),
        _ => None,
    };
    if let Some(dep_flags) = dep_flags {
        flags.push(expand_rule(dep_flags, lookup_paths));
    }
    let flags = join_words(flags);
    let mut invocations = rule_invocations(model, target.dir, template_text, |key| match key {
        b"FLAGS" => Some(flags.clone()),
        b"DEFINES" => Some(defines.clone()),
        b"INCLUDES" => Some(includes.clone()),
        _ => lookup_paths(key),
    });
    let launch = launcher::Command {
        role: "compile",
        config,
        output: &object,
        source: Some(&source),
        language: &group.language,
    };
    if let Some(invocation) = invocations.first_mut() {
        *invocation = lint::launch(
            model,
            target,
            &launch,
            std::mem::take(invocation),
            settings.skip_file_set_linting,
            usage,
        )?;
    }
    if let Some(prefix) = lint::rule_launcher(model, target, &launch, usage)? {
        let prefix = expand_rule(&prefix, lookup_paths);
        for invocation in &mut invocations {
            let working_dir = invocation.working_dir.take();
            let shell = join_words([prefix.clone(), std::mem::take(&mut invocation.shell)]);
            *invocation = command(shell);
            invocation.working_dir = working_dir;
        }
    }
    let invocation = match invocations.len() {
        0 => {
            return Err(ResolveError::Message(
                "The compile rule has no commands.".into(),
            ));
        }
        1 => invocations.pop().unwrap_or_default(),
        _ => {
            let mut line = BString::default();
            for invocation in invocations {
                if !line.is_empty() {
                    line.extend_from_slice(b" && ");
                }
                line.extend_from_slice(&invocation.shell);
            }
            command(line)
        }
    };
    let invocation = launch.wrap(model, target, invocation);
    let writes_depfile = compiler_depfile
        && (dep_flags.is_some_and(|f| has_placeholder(f, b"<DEP_FILE>"))
            || has_placeholder(template_text, b"<DEP_FILE>"));
    Ok(BuildStep::Compile {
        compile_input: None,
        object,
        source,
        invocation,
        depfile: writes_depfile.then_some(depfile),
        modmap,
        dyndep: None,
    })
}

fn has_placeholder(text: &[u8], key: &[u8]) -> bool {
    text.windows(key.len()).any(|part| part == key)
}
