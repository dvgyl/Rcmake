//! Compile flags and compiler rule expansion.

use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_genex::ConfigId;
use rcmake_model::{ProjectModel, Target, TargetKind};

use super::artifacts::is_darwin;
use super::shell::{command, join_words, quote, rule_invocations};
use super::{definition, expand_rule, join, object_path, target_property};
use crate::{ArtifactSet, BuildStep, CompileGroup, CompileUnit, ResolveError};

pub(super) fn language_flags(
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
) -> Vec<BString> {
    if !is_darwin(model, target.dir) {
        return Vec::new();
    }
    let architectures = [&b"OSX_ARCHITECTURES_"[..], &config.0.to_ascii_uppercase()].concat();
    let mut flags = Vec::new();
    if let Some(architectures) = target
        .props
        .get(&architectures)
        .or_else(|| target_property(model, target, b"OSX_ARCHITECTURES"))
    {
        for arch in list::split(architectures, list::Empty::Drop) {
            flags.push("-arch".into());
            flags.push(quote(&arch));
        }
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

fn standard_flag(model: &ProjectModel, target: &Target, language: &[u8]) -> Option<BString> {
    let standard = target_property(model, target, &[language, b"_STANDARD"].concat())?;
    if standard.is_empty() || !standard.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let extensions = target_property(model, target, &[language, b"_EXTENSIONS"].concat())
        .or_else(|| {
            definition(
                model,
                target.dir,
                &[b"CMAKE_", language, b"_EXTENSIONS_DEFAULT"].concat(),
            )
        })
        .is_none_or(|v| truth::is_on(v));
    let option: &[u8] = if extensions {
        b"_EXTENSION_COMPILE_OPTION"
    } else {
        b"_STANDARD_COMPILE_OPTION"
    };
    let key = [b"CMAKE_", language, b"_", standard, option].concat();
    if let Some(flag) = definition(model, target.dir, &key) {
        return Some(join_words(
            list::split(flag, list::Empty::Drop)
                .into_iter()
                .map(|flag| quote(&flag)),
        ));
    }
    let dialect: &[u8] = match (language, extensions) {
        (b"CXX" | b"OBJCXX", true) => b"gnu++",
        (b"CXX" | b"OBJCXX", false) => b"c++",
        (b"C" | b"OBJC", true) => b"gnu",
        (b"C" | b"OBJC", false) => b"c",
        _ => return None,
    };
    Some([b"-std=", dialect, standard].concat().into())
}

fn compile_flags(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    group: &CompileGroup,
) -> Vec<BString> {
    let language = group.language.as_slice();
    let mut flags = language_flags(model, target, config, language);
    flags.extend(standard_flag(model, target, language));
    let pic = target_property(model, target, b"POSITION_INDEPENDENT_CODE").map_or(
        matches!(
            target.kind,
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary
        ),
        |v| truth::is_on(v),
    );
    if pic {
        let key = [b"CMAKE_", language, b"_COMPILE_OPTIONS_PIC"].concat();
        match definition(model, target.dir, &key) {
            Some(options) => flags.extend(
                list::split(options, list::Empty::Drop)
                    .into_iter()
                    .map(|v| quote(&v)),
            ),
            None => flags.push("-fPIC".into()),
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
    flags.extend(apple_flags(model, target, config));
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
    join_words(group.includes.iter().map(|include| {
        let path = include.path.value.as_slice();
        if darwin
            && let Some(end) = path
                .split(|&b| b == b'/')
                .find(|part| part.ends_with(b".framework"))
        {
            let framework = path.windows(end.len()).position(|part| part == end);
            if let Some(index) = framework {
                let dir = path.get(..index).unwrap_or_default();
                return quote(
                    &[b"-F".as_slice(), if dir.is_empty() { b"." } else { dir }].concat(),
                );
            }
        }
        if include.system {
            join_words(["-isystem".into(), quote(path)])
        } else {
            quote(&[b"-I", path].concat())
        }
    }))
}

pub(super) fn compile(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    artifacts: &ArtifactSet,
    unit: &CompileUnit,
    group: &CompileGroup,
) -> Result<BuildStep, ResolveError> {
    let language = group.language.as_slice();
    let mut flags = compile_flags(model, target, config, group);
    let defines = join_words(
        group
            .defines
            .iter()
            .map(|define| quote(&[b"-D", define.value.as_slice()].concat())),
    );
    let includes = includes(model, target, group);
    let source = join(&model.dir(target.dir).source_dir, &unit.source.value);
    let object = if unit.object.is_empty() {
        object_path(model, target, config, &source)
    } else {
        unit.object.clone()
    };

    if model
        .dir(target.dir)
        .source_props
        .get(&Lexical::new(&source).normalize())
        .and_then(|props| props.get(b"LANGUAGE"))
        .is_some()
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
    let depfile: BString = [object.as_slice(), b".d"].concat().into();
    let key = [b"CMAKE_", language, b"_COMPILE_OBJECT"].concat();
    let fallback = [
        b"<CMAKE_",
        language,
        b"_COMPILER> <DEFINES> <INCLUDES> <FLAGS> -o <OBJECT> -c <SOURCE>",
    ]
    .concat();
    let template = definition(model, target.dir, &key).filter(|v| !v.is_empty());
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
    let writes_depfile = dep_flags.is_some_and(|f| has_placeholder(f, b"<DEP_FILE>"))
        || has_placeholder(template_text, b"<DEP_FILE>");
    Ok(BuildStep::Compile {
        object,
        source,
        invocation,
        depfile: writes_depfile.then_some(depfile),
    })
}

fn has_placeholder(text: &[u8], key: &[u8]) -> bool {
    text.windows(key.len()).any(|part| part == key)
}
