//! CMake 4.5.1 built-in compile-feature properties.

#![forbid(unsafe_code)]

const C90: &str = "c_function_prototypes";
const C99: &str = "c_restrict;c_variadic_macros";
const C11: &str = "c_static_assert";

const CXX98: &str = "cxx_template_template_parameters";
const CXX11: &str = "cxx_alias_templates;cxx_alignas;cxx_alignof;cxx_attributes;cxx_auto_type;\
    cxx_constexpr;cxx_decltype;cxx_decltype_incomplete_return_types;\
    cxx_default_function_template_args;cxx_defaulted_functions;\
    cxx_defaulted_move_initializers;cxx_delegating_constructors;\
    cxx_deleted_functions;cxx_enum_forward_declarations;cxx_explicit_conversions;\
    cxx_extended_friend_declarations;cxx_extern_templates;cxx_final;\
    cxx_func_identifier;cxx_generalized_initializers;cxx_inheriting_constructors;\
    cxx_inline_namespaces;cxx_lambdas;cxx_local_type_template_args;\
    cxx_long_long_type;cxx_noexcept;cxx_nonstatic_member_init;cxx_nullptr;\
    cxx_override;cxx_range_for;cxx_raw_string_literals;\
    cxx_reference_qualified_functions;cxx_right_angle_brackets;cxx_rvalue_references;\
    cxx_sizeof_member;cxx_static_assert;cxx_strong_enums;cxx_thread_local;\
    cxx_trailing_return_types;cxx_unicode_literals;cxx_uniform_initialization;\
    cxx_unrestricted_unions;cxx_user_literals;cxx_variadic_macros;\
    cxx_variadic_templates";
const CXX14: &str = "cxx_aggregate_default_initializers;cxx_attribute_deprecated;cxx_binary_literals;\
    cxx_contextual_conversions;cxx_decltype_auto;cxx_digit_separators;\
    cxx_generic_lambdas;cxx_lambda_init_captures;cxx_relaxed_constexpr;\
    cxx_return_type_deduction;cxx_variable_templates";

const CUDA: &str =
    "cuda_std_03;cuda_std_11;cuda_std_14;cuda_std_17;cuda_std_20;cuda_std_23;cuda_std_26";
const HIP: &str = "hip_std_98;hip_std_11;hip_std_14;hip_std_17;hip_std_20;hip_std_23;hip_std_26";

pub(crate) const STANDARD_FEATURES: &[(&str, &str)] = &[
    ("CMAKE_C90_KNOWN_FEATURES", C90),
    ("CMAKE_C99_KNOWN_FEATURES", C99),
    ("CMAKE_C11_KNOWN_FEATURES", C11),
    ("CMAKE_CXX98_KNOWN_FEATURES", CXX98),
    ("CMAKE_CXX11_KNOWN_FEATURES", CXX11),
    ("CMAKE_CXX14_KNOWN_FEATURES", CXX14),
];

pub(crate) const LANGUAGES: &[&str] = &[
    "CMAKE_C_KNOWN_FEATURES",
    "CMAKE_CXX_KNOWN_FEATURES",
    "CMAKE_CUDA_KNOWN_FEATURES",
    "CMAKE_HIP_KNOWN_FEATURES",
    "CMAKE_SYCL_KNOWN_FEATURES",
];

pub(crate) fn known_features(name: &[u8]) -> Option<String> {
    if !LANGUAGES.iter().any(|key| key.as_bytes() == name) {
        return STANDARD_FEATURES
            .iter()
            .find(|(key, _)| key.as_bytes() == name)
            .map(|(_, value)| (*value).to_owned());
    }
    Some(match name {
        b"CMAKE_C_KNOWN_FEATURES" => {
            format!("c_std_90;c_std_99;c_std_11;c_std_17;c_std_23;{C90};{C99};{C11}")
        }
        b"CMAKE_CXX_KNOWN_FEATURES" => {
            format!(
                "cxx_std_98;cxx_std_11;cxx_std_14;cxx_std_17;cxx_std_20;cxx_std_23;cxx_std_26;{CXX98};{CXX11};{CXX14}"
            )
        }
        b"CMAKE_CUDA_KNOWN_FEATURES" => CUDA.to_owned(),
        b"CMAKE_HIP_KNOWN_FEATURES" => HIP.to_owned(),
        b"CMAKE_SYCL_KNOWN_FEATURES" => format!(
            "sycl_cxx_std_98;sycl_cxx_std_11;sycl_cxx_std_14;sycl_cxx_std_17;sycl_cxx_std_20;sycl_cxx_std_23;sycl_cxx_std_26;{CXX98};{CXX11};{CXX14}"
        ),
        _ => return None,
    })
}
