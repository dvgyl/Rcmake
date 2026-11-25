//! Language standards and compile feature requirements.

use rcmake_base::{BString, list, shell::quote, truth};
use rcmake_diag::{
    Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus, RenderOptions, render,
};
use rcmake_model::{BT, DirId, ProjectModel, Target};

use super::shell::join_words;
use super::{definition, target_property};
use crate::ResolveError;

pub(crate) fn required_standard(
    language: &[u8],
    features: &[BT<BString>],
) -> Option<&'static [u8]> {
    features
        .iter()
        .filter_map(|entry| feature_standard(language, &entry.value))
        .max_by_key(|standard| rank(language, standard))
}

fn rank(language: &[u8], standard: &[u8]) -> Option<usize> {
    let levels: &[&[u8]] = match language {
        b"C" | b"OBJC" => &[b"90", b"99", b"11", b"17", b"23"],
        b"CXX" | b"OBJCXX" => &[b"98", b"11", b"14", b"17", b"20", b"23", b"26"],
        b"CUDA" => CUDA_LEVELS,
        _ => return None,
    };
    let standard = if language == b"CUDA" && standard == b"98" {
        b"03".as_slice()
    } else {
        standard
    };
    levels.iter().position(|level| *level == standard)
}

const CUDA_LEVELS: &[&[u8]] = &[b"03", b"11", b"14", b"17", b"20", b"23", b"26"];

fn feature_standard(language: &[u8], feature: &[u8]) -> Option<&'static [u8]> {
    if language == b"CUDA" {
        return feature
            .strip_prefix(b"cuda_std_")
            .and_then(|standard| rank(language, standard))
            .and_then(|index| CUDA_LEVELS.get(index).copied());
    }
    match (language, feature) {
        (b"C", b"c_std_90" | b"c_function_prototypes") => Some(b"90"),
        (b"C", b"c_std_99" | b"c_restrict" | b"c_variadic_macros") => Some(b"99"),
        (b"C", b"c_std_11" | b"c_static_assert") => Some(b"11"),
        (b"C", b"c_std_17") => Some(b"17"),
        (b"C", b"c_std_23") => Some(b"23"),
        (b"CXX", b"cxx_std_98" | b"cxx_template_template_parameters") => Some(b"98"),
        (
            b"CXX",
            b"cxx_std_11"
            | b"cxx_alias_templates"
            | b"cxx_alignas"
            | b"cxx_alignof"
            | b"cxx_attributes"
            | b"cxx_auto_type"
            | b"cxx_constexpr"
            | b"cxx_decltype"
            | b"cxx_decltype_incomplete_return_types"
            | b"cxx_default_function_template_args"
            | b"cxx_defaulted_functions"
            | b"cxx_defaulted_move_initializers"
            | b"cxx_delegating_constructors"
            | b"cxx_deleted_functions"
            | b"cxx_enum_forward_declarations"
            | b"cxx_explicit_conversions"
            | b"cxx_extended_friend_declarations"
            | b"cxx_extern_templates"
            | b"cxx_final"
            | b"cxx_func_identifier"
            | b"cxx_generalized_initializers"
            | b"cxx_inheriting_constructors"
            | b"cxx_inline_namespaces"
            | b"cxx_lambdas"
            | b"cxx_local_type_template_args"
            | b"cxx_long_long_type"
            | b"cxx_noexcept"
            | b"cxx_nonstatic_member_init"
            | b"cxx_nullptr"
            | b"cxx_override"
            | b"cxx_range_for"
            | b"cxx_raw_string_literals"
            | b"cxx_reference_qualified_functions"
            | b"cxx_right_angle_brackets"
            | b"cxx_rvalue_references"
            | b"cxx_sizeof_member"
            | b"cxx_static_assert"
            | b"cxx_strong_enums"
            | b"cxx_thread_local"
            | b"cxx_trailing_return_types"
            | b"cxx_unicode_literals"
            | b"cxx_uniform_initialization"
            | b"cxx_unrestricted_unions"
            | b"cxx_user_literals"
            | b"cxx_variadic_macros"
            | b"cxx_variadic_templates",
        ) => Some(b"11"),
        (
            b"CXX",
            b"cxx_std_14"
            | b"cxx_aggregate_default_initializers"
            | b"cxx_attribute_deprecated"
            | b"cxx_binary_literals"
            | b"cxx_contextual_conversions"
            | b"cxx_decltype_auto"
            | b"cxx_digit_separators"
            | b"cxx_generic_lambdas"
            | b"cxx_lambda_init_captures"
            | b"cxx_relaxed_constexpr"
            | b"cxx_return_type_deduction"
            | b"cxx_variable_templates",
        ) => Some(b"14"),
        (b"CXX", b"cxx_std_17") => Some(b"17"),
        (b"CXX", b"cxx_std_20") => Some(b"20"),
        (b"CXX", b"cxx_std_23") => Some(b"23"),
        (b"CXX", b"cxx_std_26") => Some(b"26"),
        _ => None,
    }
}

pub(super) fn standard_flag(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    required: Option<&[u8]>,
) -> Option<BString> {
    checked_standard_flag(model, target, language, required)
        .ok()
        .flatten()
}

/// Select the standard flag and report invalid standard settings.
pub(super) fn checked_standard_flag(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    required: Option<&[u8]>,
) -> Result<Option<BString>, ResolveError> {
    let new_policy = target.policies.policies.get(PolicyId::new(128)) == PolicyStatus::New;
    if language == b"CUDA" {
        return cuda_standard_flag(model, target, required, new_policy);
    }
    validate_standard(model, target, language)?;
    Ok(other_standard_flag(
        model, target, language, required, new_policy,
    ))
}

/// Reject a `<LANG>_STANDARD` value that is not a standard level.
fn validate_standard(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
) -> Result<(), ResolveError> {
    let name = [language, b"_STANDARD"].concat();
    let Some(value) = target_property(model, target, &name) else {
        return Ok(());
    };
    if value.is_empty() || value.iter().all(u8::is_ascii_digit) {
        return Ok(());
    }
    let diagnostic = Diagnostic {
        annotations: None,
        kind: MessageKind::Error,
        effect: Effect::None,
        category: None,
        policy: None,
        location: Location::Backtrace(super::listed_backtrace(&target.backtrace)),
        body: format!("{} is set to invalid value '{value}'", BString::from(name)).into(),
    };
    let text = render(
        &diagnostic,
        &RenderOptions {
            relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
            ..RenderOptions::default()
        },
    );
    Err(ResolveError::Message(text))
}

fn other_standard_flag(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    required: Option<&[u8]>,
    new_policy: bool,
) -> Option<BString> {
    let paired: Option<&[u8]> = match language {
        b"OBJC" => Some(b"C"),
        b"OBJCXX" => Some(b"CXX"),
        _ => None,
    };
    let property = |suffix: &[u8]| {
        target_property(model, target, &[language, suffix].concat()).or_else(|| {
            paired.and_then(|paired| target_property(model, target, &[paired, suffix].concat()))
        })
    };
    let explicit = property(b"_STANDARD")
        .or_else(|| {
            paired.and_then(|paired| {
                definition(
                    model,
                    target.dir,
                    &[b"CMAKE_", paired, b"_STANDARD_DEFAULT"].concat(),
                )
            })
        })
        .map(|value| value.as_slice())
        .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii_digit));
    let default = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_STANDARD_DEFAULT"].concat(),
    )
    .map(|value| value.as_slice())
    .filter(|value| rank(language, value).is_some());
    let default_extensions = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_EXTENSIONS_DEFAULT"].concat(),
    )
    .is_none_or(|value| truth::is_on(value));
    let extensions =
        property(b"_EXTENSIONS").map_or(default_extensions, |value| truth::is_on(value));
    let standard = match (explicit, required) {
        (Some(explicit), _)
            if rank(language, explicit) >= required.and_then(|s| rank(language, s)) =>
        {
            explicit
        }
        (Some(_), Some(required)) => required,
        (None, Some(required))
            if rank(language, required) > default.and_then(|s| rank(language, s)) =>
        {
            required
        }
        (None, _) if new_policy && extensions != default_extensions => default?,
        _ => return None,
    };
    let standard_required =
        property(b"_STANDARD_REQUIRED").is_some_and(|value| truth::is_on(value));
    if new_policy
        && !standard_required
        && Some(standard) == default
        && extensions == default_extensions
    {
        return None;
    }
    let option: &[u8] = if extensions {
        b"_EXTENSION_COMPILE_OPTION"
    } else {
        b"_STANDARD_COMPILE_OPTION"
    };
    let flag = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, standard, option].concat(),
    )
    .or_else(|| {
        definition(
            model,
            target.dir,
            &[b"CMAKE_", language, b"_", standard, option].concat(),
        )
    });
    if let Some(flag) = flag {
        return (!flag.is_empty()).then(|| standard_options(flag));
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

fn cuda_standard_flag(
    model: &ProjectModel,
    target: &Target,
    required: Option<&[u8]>,
    new_policy: bool,
) -> Result<Option<BString>, ResolveError> {
    let definition = |key: &[u8]| definition(model, target.dir, key);
    let Some(default) = definition(b"CMAKE_CUDA_STANDARD_DEFAULT").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let default_extensions =
        definition(b"CMAKE_CUDA_EXTENSIONS_DEFAULT").is_some_and(|value| truth::is_on(value));
    let extensions = target
        .props
        .get(b"CUDA_EXTENSIONS")
        .map_or(!new_policy || default_extensions, |value| {
            truth::is_on(value)
        });
    let option: &[u8] = if extensions {
        b"_EXTENSION_COMPILE_OPTION"
    } else {
        b"_STANDARD_COMPILE_OPTION"
    };
    let cuda_option = |standard: &[u8]| definition(&[b"CMAKE_CUDA", standard, option].concat());
    let explicit = target
        .props
        .get(b"CUDA_STANDARD")
        .map(|value| value.as_slice());
    let explicit_rank = explicit
        .map(|value| {
            rank(b"CUDA", value).ok_or_else(|| {
                ResolveError::Message(
                    format!(
                        "CUDA_STANDARD is set to invalid value '{}'",
                        BString::from(value)
                    )
                    .into(),
                )
            })
        })
        .transpose()?;
    let default_rank = rank(b"CUDA", default);
    let standard = required
        .filter(|value| rank(b"CUDA", value) > explicit_rank.or(default_rank))
        .or(explicit);
    let Some(standard) = standard else {
        let flag = if new_policy && extensions != default_extensions {
            cuda_option(default)
        } else if !new_policy && extensions {
            definition(b"CMAKE_CUDA_EXTENSION_COMPILE_OPTION")
        } else {
            None
        };
        return Ok(flag.map(|flag| standard_options(flag)));
    };
    if target
        .props
        .get(b"CUDA_STANDARD_REQUIRED")
        .is_some_and(|value| truth::is_on(value))
    {
        let flag = cuda_option(standard).ok_or_else(|| {
            let compiler = definition(b"CMAKE_CUDA_COMPILER_ID").map_or(b"".as_slice(), |id| id.as_slice());
            let extensions = if extensions { "(with compiler extensions)" } else { "" };
            ResolveError::Message(format!(
                "Target \"{}\" requires the language dialect \"CUDA{}\" {extensions}. But the current compiler \"{}\" does not support this, or CMake does not know the flags to enable it.",
                target.name, BString::from(standard), BString::from(compiler)
            ).into())
        })?;
        return Ok(Some(standard_options(flag)));
    }
    let (Some(requested), Some(default_rank)) = (rank(b"CUDA", standard), default_rank) else {
        return Err(ResolveError::Message(
            format!("CMAKE_CUDA_STANDARD_DEFAULT is set to invalid value '{default}'").into(),
        ));
    };
    let flag = if requested < default_rank
        || (new_policy && extensions != default_extensions)
        || (!new_policy && requested == default_rank)
    {
        CUDA_LEVELS
            .get(requested)
            .and_then(|level| cuda_option(level))
    } else {
        CUDA_LEVELS
            .iter()
            .take(requested + 1)
            .skip(default_rank + 1)
            .rev()
            .find_map(|level| cuda_option(level))
    };
    Ok(flag.map(|flag| standard_options(flag)))
}

fn standard_options(flag: &[u8]) -> BString {
    join_words(
        list::split(flag, list::Empty::Drop)
            .into_iter()
            .map(|flag| quote(&flag)),
    )
}
