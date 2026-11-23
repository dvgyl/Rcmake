//! Language standards and compile feature requirements.

use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_model::{BT, ProjectModel, Target};

use super::shell::{join_words, quote};
use super::{definition, target_property};

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
        _ => return None,
    };
    levels.iter().position(|level| *level == standard)
}

fn feature_standard(language: &[u8], feature: &[u8]) -> Option<&'static [u8]> {
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
    let explicit = target_property(model, target, &[language, b"_STANDARD"].concat())
        .map(|value| value.as_slice())
        .filter(|value| !value.is_empty() && value.iter().all(u8::is_ascii_digit));
    let default = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_STANDARD_DEFAULT"].concat(),
    )
    .map(|value| value.as_slice())
    .filter(|value| rank(language, value).is_some());
    let new_policy = target.policies.policies.get(PolicyId::new(128)) == PolicyStatus::New;
    let default_extensions = definition(
        model,
        target.dir,
        &[b"CMAKE_", language, b"_EXTENSIONS_DEFAULT"].concat(),
    )
    .is_none_or(|value| truth::is_on(value));
    let extensions = target_property(model, target, &[language, b"_EXTENSIONS"].concat())
        .map_or(default_extensions, |value| truth::is_on(value));
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
        target_property(model, target, &[language, b"_STANDARD_REQUIRED"].concat())
            .is_some_and(|value| truth::is_on(value));
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
