//! Compile flags and compiler rule expansion.

use indexmap::IndexSet;
use rcmake_base::{BString, list, path_lex::Lexical, shell::quote, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{ProjectModel, Target};

use super::artifacts::is_darwin;
use super::launcher;
use super::pch::PchFlags;
use super::policy_flags::Flags;
use super::shell::{command, join_words, rule_invocations};
use super::standard::standard_flag;
use super::{
    definition, expand_rule, join, object_path, target_property, without_trailing_slashes,
};
use crate::{ArtifactSet, BuildStep, CompileGroup, CompileUnit, ResolveError};

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
        .filter_map(|key| definition(model, target.dir, key).cloned())
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
    if architectures.is_empty()
        && matches!(
            language,
            b"C" | b"CXX" | b"OBJC" | b"OBJCXX" | b"ASM" | b"SYCL"
        )
        && definition(
            model,
            target.dir,
            &[b"CMAKE_", language, b"_COMPILER_TARGET"].concat(),
        )
        .is_none_or(|value| value.is_empty())
        && let Some(default) = definition(model, target.dir, b"_CMAKE_APPLE_ARCHS_DEFAULT")
    {
        architectures = list::split(default, list::Empty::Drop);
    }
    let mut flags = Vec::new();
    for arch in architectures {
        flags.push("-arch".into());
        flags.push(quote(&arch));
    }
    if let Some(sdk) = definition(model, target.dir, b"CMAKE_OSX_SYSROOT").filter(|v| !v.is_empty())
    {
        flags.push("-isysroot".into());
        flags.push(quote(sdk));
    }
    if let Some(version) =
        definition(model, target.dir, b"CMAKE_OSX_DEPLOYMENT_TARGET").filter(|v| !v.is_empty())
    {
        let prefix: &[u8] =
            match definition(model, target.dir, b"CMAKE_SYSTEM_NAME").map(|v| v.as_slice()) {
                Some(b"iOS") => b"-miphoneos-version-min=",
                Some(b"tvOS") => b"-mtvos-version-min=",
                Some(b"watchOS") => b"-mwatchos-version-min=",
                Some(b"visionOS") => b"-mxros-version-min=",
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
) -> Vec<BString> {
    let language = group.language.as_slice();
    let mut flags = language_flags(model, target, config, language);
    flags.extend(standard_flag(model, target, language, required_standard));
    flags.extend(apple_flags(model, target, config, language));
    let policy = Flags::new(model, target, language);
    flags.extend(policy.compile(policy.ipo(config)));

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
    flags
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
    join_words(group.includes.iter().filter_map(|include| {
        let path = without_trailing_slashes(&include.path.value);
        if !implicit.is_empty()
            && implicit.contains(without_trailing_slashes(&Lexical::new(path).normalize()))
        {
            return None;
        }
        if darwin
            && let Some(end) = path
                .split(|&b| b == b'/')
                .find(|part| part.ends_with(b".framework"))
        {
            let framework = path.windows(end.len()).position(|part| part == end);
            if let Some(index) = framework {
                let dir = path.get(..index).unwrap_or_default();
                return Some(quote(
                    &[b"-F".as_slice(), if dir.is_empty() { b"." } else { dir }].concat(),
                ));
            }
        }
        Some(if include.system {
            join_words(["-isystem".into(), quote(path)])
        } else {
            quote(&[b"-I", path].concat())
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
) -> (Vec<BString>, BString, BString) {
    let language = group.language.as_slice();
    let mut flags = compile_flags(model, target, config, group, required_standard);
    let mut defines: Vec<_> = group
        .defines
        .iter()
        .map(|define| define.value.as_slice())
        .collect();
    defines.sort_unstable();
    defines.dedup();
    let defines = join_words(
        defines
            .into_iter()
            .map(|define| quote(&[b"-D", define].concat())),
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
    (flags, defines, includes)
}

#[derive(Default)]
pub(super) struct Settings<'a> {
    pub(super) standard: Option<&'a [u8]>,
    pub(super) pch: Option<PchFlags<'a>>,
}

pub(super) fn compile(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    unit: &CompileUnit,
    group: &CompileGroup,
    settings: Settings<'_>,
) -> Result<BuildStep, ResolveError> {
    let language = group.language.as_slice();
    let source = join(&model.dir(target.dir).source_dir, &unit.source.value);
    let (mut flags, defines, includes) =
        compile_arguments(model, target, config, group, settings.standard, &source);
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
    let dep_flags = definition(
        model,
        target.dir,
        &[b"CMAKE_DEPFILE_FLAGS_", language].concat(),
    )
    .map(|v| v.as_slice())
    .or_else(|| {
        (template.is_none() && matches!(language, b"C" | b"CXX" | b"OBJC" | b"OBJCXX"))
            .then_some(b"-MD -MT <DEP_TARGET> -MF <DEP_FILE>")
    });
    let lookup_paths = |key: &[u8]| match key {
        b"SOURCE" => Some(quote(&source)),
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
        b"TARGET_PDB" | b"TARGET_COMPILE_PDB" => Some(
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
    for invocation in &mut invocations {
        *invocation = launch.launch_tool(
            model,
            target,
            b"_COMPILER_LAUNCHER",
            std::mem::take(invocation),
        )?;
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
    let writes_depfile = dep_flags.is_some_and(|f| has_placeholder(f, b"<DEP_FILE>"))
        || has_placeholder(template_text, b"<DEP_FILE>");
    Ok(BuildStep::Compile {
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
