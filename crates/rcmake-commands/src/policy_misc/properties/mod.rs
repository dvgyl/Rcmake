use rcmake_base::BString;
use rcmake_eval::Cx;
use rcmake_model::PropScope;

use super::{quoted, values};

pub(super) use define::define;
pub(super) use get::{get, get_cmake, get_directory, get_source};
pub(super) use print::print_value;
pub(super) use set::{set, set_directory, set_sources};

mod computed;
mod define;
mod file_set;
mod get;
mod print;
mod scope;
mod set;

// These CMake 4.5.1 lists are available before the language bootstrap runs.
const KNOWN_FEATURES: [(&[u8], &str); 4] = [
    (
        b"CMAKE_C_KNOWN_FEATURES",
        "c_std_90;c_std_99;c_std_11;c_std_17;c_std_23;c_function_prototypes;c_restrict;\
         c_variadic_macros;c_static_assert",
    ),
    (
        b"CMAKE_CXX_KNOWN_FEATURES",
        "cxx_std_98;cxx_std_11;cxx_std_14;cxx_std_17;cxx_std_20;cxx_std_23;cxx_std_26;\
         cxx_template_template_parameters;cxx_alias_templates;cxx_alignas;cxx_alignof;\
         cxx_attributes;cxx_auto_type;cxx_constexpr;cxx_decltype;\
         cxx_decltype_incomplete_return_types;cxx_default_function_template_args;\
         cxx_defaulted_functions;cxx_defaulted_move_initializers;cxx_delegating_constructors;\
         cxx_deleted_functions;cxx_enum_forward_declarations;cxx_explicit_conversions;\
         cxx_extended_friend_declarations;cxx_extern_templates;cxx_final;\
         cxx_func_identifier;cxx_generalized_initializers;cxx_inheriting_constructors;\
         cxx_inline_namespaces;cxx_lambdas;cxx_local_type_template_args;\
         cxx_long_long_type;cxx_noexcept;cxx_nonstatic_member_init;cxx_nullptr;\
         cxx_override;cxx_range_for;cxx_raw_string_literals;\
         cxx_reference_qualified_functions;cxx_right_angle_brackets;\
         cxx_rvalue_references;cxx_sizeof_member;cxx_static_assert;cxx_strong_enums;\
         cxx_thread_local;cxx_trailing_return_types;cxx_unicode_literals;\
         cxx_uniform_initialization;cxx_unrestricted_unions;cxx_user_literals;\
         cxx_variadic_macros;cxx_variadic_templates;cxx_aggregate_default_initializers;\
         cxx_attribute_deprecated;cxx_binary_literals;cxx_contextual_conversions;\
         cxx_decltype_auto;cxx_digit_separators;cxx_generic_lambdas;\
         cxx_lambda_init_captures;cxx_relaxed_constexpr;cxx_return_type_deduction;\
         cxx_variable_templates",
    ),
    (
        b"CMAKE_CUDA_KNOWN_FEATURES",
        "cuda_std_03;cuda_std_11;cuda_std_14;cuda_std_17;cuda_std_20;cuda_std_23;cuda_std_26",
    ),
    (
        b"CMAKE_HIP_KNOWN_FEATURES",
        "hip_std_98;hip_std_11;hip_std_14;hip_std_17;hip_std_20;hip_std_23;hip_std_26",
    ),
];

fn known_features(name: &[u8]) -> Option<&'static str> {
    KNOWN_FEATURES
        .into_iter()
        .find_map(|(property, features)| (name == property).then_some(features))
}

fn is_inherited(cx: &mut Cx<'_>, scope: &[u8], name: &[u8]) -> bool {
    cx.global()
        .defined_props
        .iter()
        .any(|entry| entry.scope == scope && entry.name == name && entry.inherited)
}

fn model_value(
    cx: &mut Cx<'_>,
    scope: &PropScope,
    name: &[u8],
    inherited: bool,
) -> Option<BString> {
    if inherited {
        cx.project().get_prop_inherited(scope, name)
    } else {
        cx.project().get_prop(scope, name)
    }
}
