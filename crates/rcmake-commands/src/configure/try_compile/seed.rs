use std::collections::BTreeMap;

use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CacheDefine, Cx};
use rcmake_model::{CacheType, GeneratorKind};

use super::args::{Options, STANDARD_OPTIONS};
use super::split_byte;

const PROBE_LANGUAGES: &[&str] = &[
    "C", "CXX", "CUDA", "HIP", "OBJC", "OBJCXX", "SYCL", "Fortran", "ASM", "ISPC", "Swift",
];
const CMP0056: PolicyId = PolicyId::new(56);
const CMP0066: PolicyId = PolicyId::new(66);
const CMP0067: PolicyId = PolicyId::new(67);
const CMP0137: PolicyId = PolicyId::new(137);
const CMP0210: PolicyId = PolicyId::new(210);

const PLATFORM_VARIABLES: &[&[u8]] = &[
    b"CMAKE_APPLE_ARCH_SYSROOTS",
    b"CMAKE_CUDA_ARCHITECTURES",
    b"CMAKE_CUDA_RUNTIME_LIBRARY",
    b"CMAKE_CXX_SCAN_FOR_MODULES",
    b"CMAKE_ENABLE_EXPORTS",
    b"CMAKE_EXECUTABLE_ENABLE_EXPORTS",
    b"CMAKE_SHARED_LIBRARY_ENABLE_EXPORTS",
    b"CMAKE_HIP_ARCHITECTURES",
    b"CMAKE_HIP_PLATFORM",
    b"CMAKE_HIP_RUNTIME_LIBRARY",
    b"CMAKE_ISPC_INSTRUCTION_SETS",
    b"CMAKE_ISPC_HEADER_SUFFIX",
    b"CMAKE_LINK_SEARCH_END_STATIC",
    b"CMAKE_LINK_SEARCH_START_STATIC",
    b"CMAKE_LINKER_TYPE",
    b"CMAKE_MSVC_DEBUG_INFORMATION_FORMAT",
    b"CMAKE_MSVC_RUNTIME_CHECKS",
    b"CMAKE_MSVC_RUNTIME_LIBRARY",
    b"CMAKE_OSX_ARCHITECTURES",
    b"CMAKE_OSX_DEPLOYMENT_TARGET",
    b"CMAKE_OSX_SYSROOT",
    b"CMAKE_POSITION_INDEPENDENT_CODE",
    b"CMAKE_SYCL_DEVICE_TARGETS",
    b"CMAKE_SYSROOT",
    b"CMAKE_SYSROOT_COMPILE",
    b"CMAKE_SYSROOT_LINK",
    b"CMAKE_VS_USE_DEBUG_LIBRARIES",
    b"CMAKE_WATCOM_RUNTIME_LIBRARY",
];

pub(super) struct TryCompileSeed {
    pub(super) defines: Vec<CacheDefine>,
    pub(super) enabled_languages: BString,
    pub(super) probe_languages: Vec<BString>,
    /// Caller's explicit `CMAKE_TRY_COMPILE_CONFIGURATION`, if set and non-empty.
    pub(super) explicit_config: Option<BString>,
    /// Effective caller configuration for caller-side `CMP0066` flags and imported-target lookup.
    pub(super) caller_config: BString,
}

/// Return the caller's explicit `CMAKE_TRY_COMPILE_CONFIGURATION` if non-empty.
pub(super) fn explicit_configuration(cx: &mut Cx<'_>) -> Option<BString> {
    let explicit = cx.var_or_empty(b"CMAKE_TRY_COMPILE_CONFIGURATION");
    (!explicit.is_empty()).then_some(explicit)
}

/// Choose the effective caller configuration for caller-side flag seeding (`CMP0066`) and
/// imported-target property evaluation.
///
/// For multi-config generators, an explicit `CMAKE_TRY_COMPILE_CONFIGURATION` takes
/// precedence over the caller's generator default (`Debug` for Xcode/Visual Studio, or
/// `CMAKE_DEFAULT_BUILD_TYPE` / first `CMAKE_CONFIGURATION_TYPES` / `Debug` for Ninja
/// Multi-Config). Single-config generators use an explicit `CMAKE_TRY_COMPILE_CONFIGURATION`
/// if set, otherwise defer to `CMAKE_BUILD_TYPE`.
pub(super) fn caller_effective_configuration(cx: &mut Cx<'_>) -> BString {
    if let Some(explicit) = explicit_configuration(cx) {
        return explicit;
    }
    match cx.project().generator {
        Some(GeneratorKind::NinjaMultiConfig) => {
            let default_type = cx.var_or_empty(b"CMAKE_DEFAULT_BUILD_TYPE");
            if !default_type.is_empty() {
                default_type
            } else {
                list::split(
                    &cx.var_or_empty(b"CMAKE_CONFIGURATION_TYPES"),
                    list::Empty::Drop,
                )
                .into_iter()
                .next()
                .unwrap_or_else(|| "Debug".into())
            }
        }
        Some(generator) if generator.multi_config() => "Debug".into(),
        _ => BString::default(),
    }
}

pub(super) fn toolchain_variable(name: &[u8]) -> bool {
    if name.ends_with(b"_LOADED") || name.starts_with(b"CMAKE_COMPILER_IS_") {
        return false;
    }
    [
        b"CMAKE_C_".as_slice(),
        b"CMAKE_CUDA_",
        b"_CMAKE_CUDA_",
        b"CMAKE_Fortran_",
        b"CMAKE_ASM",
        b"CMAKE_HIP_",
        b"CMAKE_ISPC_",
        b"CMAKE_Swift_",
        b"CMAKE_SYCL_",
        b"CMAKE_RC_",
        b"CMAKE_DEPFILE_FLAGS_",
        b"CMAKE_MSVC_",
        b"CMAKE_CXX_",
        b"CMAKE_OBJC_",
        b"CMAKE_OBJCXX_",
        b"CMAKE_OSX_",
        b"CMAKE_SYSROOT",
        b"CMAKE_SYSTEM_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name.starts_with(b"CMAKE_")
            && [b"_LINKER_FLAGS".as_slice(), b"_USING_LINKER_"]
                .iter()
                .any(|part| name.windows(part.len()).any(|window| window == *part))
        || matches!(
            name,
            b"CMAKE_SYSTEM"
                | b"CMAKE_AR"
                | b"CMAKE_LIBRARY_PATH_FLAG"
                | b"CMAKE_LINKER"
                | b"CMAKE_LINK_LIBRARY_FLAG"
                | b"CMAKE_LINK_LIBRARY_SUFFIX"
                | b"CMAKE_MT"
                | b"CMAKE_LINKER_TYPE"
                | b"CMAKE_RANLIB"
                | b"CMAKE_MAKE_PROGRAM"
                | b"CMAKE_VS_USE_DEBUG_LIBRARIES"
                | b"_CMAKE_OSX_SYSROOT_PATH"
                | b"_CMAKE_APPLE_ARCHS_DEFAULT"
        )
}

pub(super) fn enabled_languages(cx: &mut Cx<'_>) -> Vec<BString> {
    let mut languages = cx
        .global()
        .props
        .get(b"ENABLED_LANGUAGES")
        .map(|value| list::split(value, list::Empty::Drop))
        .unwrap_or_default();
    for &language in PROBE_LANGUAGES {
        let compiler = format!("CMAKE_{language}_COMPILER");
        if !cx.var_or_empty(compiler.as_bytes()).is_empty()
            && !languages.iter().any(|loaded| loaded == language)
        {
            languages.push(language.into());
        }
    }
    languages
}

impl TryCompileSeed {
    pub(super) fn new(cx: &mut Cx<'_>, options: &Options<'_>, source_signature: bool) -> Self {
        let generator = cx.project().generator;
        let multi_config = generator.is_some_and(GeneratorKind::multi_config);
        let explicit_config = explicit_configuration(cx);
        let caller_config = caller_effective_configuration(cx);
        let honor_link_flags = !source_signature || cx.policy(CMP0210) == PolicyStatus::New;
        let honor_config_flags = cx.policy(CMP0066) == PolicyStatus::New;
        if source_signature
            && cx.policy(CMP0066) == PolicyStatus::Warn
            && cx.var_is_on(b"CMAKE_POLICY_WARNING_CMP0066")
        {
            cx.issue_policy_warning(
                CMP0066,
                b"",
                b"For compatibility with older versions of CMake, try_compile is not honoring caller config-specific compiler flags (e.g. CMAKE_C_FLAGS_DEBUG) in the test project.",
            );
        }
        let forward_platform = (source_signature || cx.policy(CMP0137) == PolicyStatus::New)
            && !cx.var_is_on(b"CMAKE_TRY_COMPILE_NO_PLATFORM_VARIABLES");
        let selected_config = if caller_config.is_empty() {
            b"DEBUG".to_vec()
        } else {
            caller_config.to_ascii_uppercase()
        };
        let mut defines = Vec::new();
        for name in cx
            .variable_names()
            .iter()
            .filter(|name| toolchain_variable(name))
        {
            if is_platform_variable(name) && !forward_platform {
                continue;
            }
            if PROBE_LANGUAGES.iter().any(|lang| {
                let Some(suffix) = name.strip_prefix(format!("CMAKE_{lang}_").as_bytes()) else {
                    return false;
                };
                if !source_signature {
                    return suffix == b"FLAGS"
                        || suffix.strip_prefix(b"FLAGS_").is_some_and(|flag_config| {
                            flag_config != b"INIT" && !flag_config.ends_with(b"_INIT")
                        });
                }
                matches!(suffix, b"STANDARD" | b"STANDARD_REQUIRED" | b"EXTENSIONS")
                    || suffix.strip_prefix(b"FLAGS_").is_some_and(|flag_config| {
                        flag_config != b"INIT"
                            && !flag_config.ends_with(b"_INIT")
                            && (!honor_config_flags || flag_config != selected_config.as_slice())
                    })
                    || (suffix == b"LINK_FLAGS" || suffix.starts_with(b"LINK_FLAGS_"))
                        && !honor_link_flags
            }) {
                continue;
            }
            seed_var(cx, &mut defines, name);
        }
        for name in [
            "CMAKE_PLATFORM_INFO_DIR",
            "CMAKE_TRY_COMPILE_PLATFORM_VARIABLES",
            "CMAKE_TRY_COMPILE_CONFIGURATION",
            "CMAKE_DEBUG_TRY_COMPILE",
            "CMAKE_GENERATOR",
            "CMAKE_EXPERIMENTAL_SYCL",
            "CMAKE_TRY_COMPILE_NO_PLATFORM_VARIABLES",
        ] {
            seed_var(cx, &mut defines, name.as_bytes());
        }
        if forward_platform && let Some(archs) = cx.var(b"CMAKE_TRY_COMPILE_OSX_ARCHITECTURES") {
            defines.push(CacheDefine {
                name: "CMAKE_OSX_ARCHITECTURES".into(),
                ty: Some(CacheType::String),
                value: archs,
            });
        }
        for language in PROBE_LANGUAGES {
            defines.push(CacheDefine {
                name: format!("CMAKE_{language}_COMPILER_WORKS").into(),
                ty: Some(CacheType::Internal),
                value: "1".into(),
            });
        }
        if forward_platform {
            for &name in PLATFORM_VARIABLES {
                if !toolchain_variable(name) {
                    seed_var(cx, &mut defines, name);
                }
            }
            for name in list::split(
                &cx.var_or_empty(b"CMAKE_TRY_COMPILE_PLATFORM_VARIABLES"),
                list::Empty::Drop,
            ) {
                seed_var(cx, &mut defines, &name);
            }
        }
        if !source_signature
            && cx
                .global()
                .props
                .get(b"PROPAGATE_TOP_LEVEL_INCLUDES_TO_TRY_COMPILE")
                .is_some_and(|value| truth::is_on(value))
        {
            seed_var(cx, &mut defines, b"CMAKE_PROJECT_TOP_LEVEL_INCLUDES");
        }
        if source_signature && generator == Some(GeneratorKind::NinjaMultiConfig) {
            seed_var(cx, &mut defines, b"CMAKE_CONFIGURATION_TYPES");
            seed_var(cx, &mut defines, b"CMAKE_DEFAULT_BUILD_TYPE");
            seed_var(cx, &mut defines, b"CMAKE_CROSS_CONFIGS");
            seed_var(cx, &mut defines, b"CMAKE_DEFAULT_CONFIGS");
        }
        if !multi_config && let Some(explicit) = &explicit_config {
            defines.push(CacheDefine {
                name: "CMAKE_BUILD_TYPE".into(),
                ty: Some(CacheType::String),
                value: explicit.clone(),
            });
        }
        let mut flags = options.many(b"CMAKE_FLAGS").iter().copied();
        while let Some(flag) = flags.next() {
            let Some(define) = (if flag == b"-D" {
                flags.next()
            } else {
                flag.strip_prefix(b"-D")
            }) else {
                continue;
            };
            if let Some((key, value)) = split_byte(define, b'=') {
                let (name, ty) = split_byte(key, b':')
                    .map_or((key, None), |(name, ty)| (name, CacheType::parse(ty)));
                defines.push(CacheDefine {
                    name: name.into(),
                    ty,
                    value: value.into(),
                });
            }
        }
        Self {
            defines,
            enabled_languages: list::join(enabled_languages(cx)),
            probe_languages: Vec::new(),
            explicit_config,
            caller_config,
        }
    }
}

fn is_platform_variable(name: &[u8]) -> bool {
    PLATFORM_VARIABLES.contains(&name)
        || name.ends_with(b"_COMPILER_TARGET")
        || name.ends_with(b"_COMPILER_EXTERNAL_TOOLCHAIN")
        || name
            .windows(b"_USING_LINKER_".len())
            .any(|word| word == b"_USING_LINKER_")
}

/// Variables that the configure log records as passed to the test project, sorted by
/// name. `languages` is `Some` for the source file signature. `CMAKE_FLAGS` entries are
/// not recorded.
pub(super) fn logged_variables(
    cx: &mut Cx<'_>,
    languages: Option<&[BString]>,
) -> Vec<(BString, BString)> {
    let mut variables = BTreeMap::new();
    if let Some(languages) = languages {
        let config = caller_effective_configuration(cx);
        let config = if config.is_empty() {
            b"DEBUG".to_vec()
        } else {
            config.to_ascii_uppercase()
        };
        let honor_config_flags = cx.policy(CMP0066) == PolicyStatus::New;
        for lang in languages {
            let flags = [b"CMAKE_", lang.as_slice(), b"_FLAGS"].concat();
            if honor_config_flags {
                let name = [flags.as_slice(), b"_", &config].concat();
                let value = cx.var_or_empty(&name);
                variables.insert(BString::from(name), value);
            }
            let value = cx.var_or_empty(&flags);
            variables.insert(BString::from(flags), value);
        }
        if cx.policy(CMP0056) == PolicyStatus::New {
            let name = b"CMAKE_EXE_LINKER_FLAGS";
            variables.insert(BString::from(name.as_slice()), cx.var_or_empty(name));
        }
        if !cx.var_is_on(b"CMAKE_TRY_COMPILE_NO_PLATFORM_VARIABLES") {
            let extra = list::split(
                &cx.var_or_empty(b"CMAKE_TRY_COMPILE_PLATFORM_VARIABLES"),
                list::Empty::Drop,
            );
            for name in PLATFORM_VARIABLES
                .iter()
                .map(|&name| BString::from(name))
                .chain(extra)
            {
                if let Some(value) = cx.var(&name) {
                    variables.insert(name, value);
                }
            }
        }
    }
    let top_level = b"CMAKE_PROJECT_TOP_LEVEL_INCLUDES";
    if cx
        .global()
        .props
        .get(b"PROPAGATE_TOP_LEVEL_INCLUDES_TO_TRY_COMPILE")
        .is_some_and(|value| truth::is_on(value))
        && let Some(value) = cx.var(top_level)
    {
        variables.insert(BString::from(top_level.as_slice()), value);
    }
    variables.into_iter().collect()
}

/// `CMP0067` `WARN` with `CMAKE_POLICY_WARNING_CMP0067`: name the language standard
/// variables that the test project does not honor.
pub(super) fn warn_ignored_standards(cx: &mut Cx<'_>, options: &Options<'_>, languages: &[&[u8]]) {
    if cx.policy(CMP0067) != PolicyStatus::Warn
        || !cx.var_is_on(b"CMAKE_POLICY_WARNING_CMP0067")
        || STANDARD_OPTIONS.iter().any(|name| options.has(name))
    {
        return;
    }
    let mut ignored = Vec::new();
    for &lang in languages {
        for suffix in [
            b"_STANDARD".as_slice(),
            b"_STANDARD_REQUIRED",
            b"_EXTENSIONS",
        ] {
            let name = [b"CMAKE_".as_slice(), lang, suffix].concat();
            if !cx.var_or_empty(&name).is_empty() {
                ignored.push(name);
            }
        }
    }
    if ignored.is_empty() {
        return;
    }
    let mut text = b"For compatibility with older versions of CMake, try_compile is not honoring language standard variables in the test project:\n".to_vec();
    for name in ignored {
        text.extend_from_slice(b"  ");
        text.extend_from_slice(&name);
        text.push(b'\n');
    }
    cx.issue_policy_warning(CMP0067, b"", &text);
}

pub(super) fn language_properties(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
    languages: &[&[u8]],
) -> Vec<(BString, BString)> {
    let honor = cx.policy(CMP0067) == PolicyStatus::New
        || STANDARD_OPTIONS.iter().any(|name| options.has(name));
    let mut properties = Vec::new();
    for &lang in languages {
        for suffix in [
            b"_STANDARD".as_slice(),
            b"_STANDARD_REQUIRED",
            b"_EXTENSIONS",
        ] {
            let lang = if lang == b"SYCL" && suffix != b"_EXTENSIONS" {
                b"SYCL_CXX"
            } else {
                lang
            };
            let key = [lang, suffix].concat();
            let value = options
                .one(&key)
                .map(BString::from)
                .or_else(|| honor.then(|| cx.var_or_empty(&[b"CMAKE_", key.as_slice()].concat())));
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                properties.push((key.into(), value));
            }
        }
    }
    properties
}

fn seed_var(cx: &Cx<'_>, defines: &mut Vec<CacheDefine>, name: &[u8]) {
    if cx.var_defined(name) {
        defines.push(CacheDefine {
            name: name.into(),
            ty: None,
            value: cx.var_or_empty(name),
        });
    }
}
