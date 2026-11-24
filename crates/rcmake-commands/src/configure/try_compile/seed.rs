use std::collections::BTreeMap;

use rcmake_base::{BString, list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CacheDefine, Cx};
use rcmake_model::{CacheType, DirId, Target, TargetKind};

use super::args::{Options, STANDARD_OPTIONS};
use super::{probe_error, split_byte};

const PROBE_LANGUAGES: &[&str] = &[
    "C", "CXX", "CUDA", "HIP", "OBJC", "OBJCXX", "SYCL", "Fortran", "ASM", "ISPC", "Swift",
];
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
                | b"CMAKE_LINKER"
                | b"CMAKE_MT"
                | b"CMAKE_LINKER_TYPE"
                | b"CMAKE_RANLIB"
                | b"CMAKE_MAKE_PROGRAM"
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
        let config = cx.var_or_empty(b"CMAKE_TRY_COMPILE_CONFIGURATION");
        let honor_link_flags = !source_signature || cx.policy(CMP0210) == PolicyStatus::New;
        let honor_config_flags = cx.policy(CMP0066) == PolicyStatus::New;
        let forward_platform = (source_signature || cx.policy(CMP0137) == PolicyStatus::New)
            && !cx.var_is_on(b"CMAKE_TRY_COMPILE_NO_PLATFORM_VARIABLES");
        let selected_config = if config.is_empty() {
            b"DEBUG".to_vec()
        } else {
            config.to_ascii_uppercase()
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
        if !config.is_empty() {
            defines.push(CacheDefine {
                name: "CMAKE_BUILD_TYPE".into(),
                ty: Some(CacheType::String),
                value: config,
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

pub(super) fn probe_imports(
    cx: &mut Cx<'_>,
    options: &Options<'_>,
) -> Result<Vec<(Target, Vec<BString>)>, rcmake_eval::CmdError> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let libraries = options.many(b"LINK_LIBRARIES");
    for &name in libraries {
        let Some(target) = model.find_target(dir, name).and_then(|id| model.target(id)) else {
            continue;
        };
        let valid = match target.kind {
            TargetKind::StaticLibrary
            | TargetKind::SharedLibrary
            | TargetKind::InterfaceLibrary
            | TargetKind::UnknownLibrary => true,
            TargetKind::Executable => target
                .props
                .get(b"ENABLE_EXPORTS")
                .is_some_and(|value| rcmake_base::truth::is_on(value)),
            _ => false,
        };
        if !valid {
            let kind = target.kind.as_str();
            return Err(probe_error(
                cx,
                format!(
                    "Only libraries may be used as try_compile or try_run IMPORTED LINK_LIBRARIES.  Got {} of type {kind}.",
                    String::from_utf8_lossy(name)
                ),
            ));
        }
    }
    let mut targets: Vec<(Target, Vec<BString>)> = Vec::new();
    let mut imported = BTreeMap::new();
    let mut pending: Vec<BString> = libraries.iter().map(|&name| name.into()).collect();
    while let Some(name) = pending.pop() {
        let Some(id) = model.find_target(dir, &name) else {
            continue;
        };
        let Some(target) = model.target(id).filter(|target| target.is_imported()) else {
            continue;
        };
        let idx = *imported.entry(id.index()).or_insert_with(|| {
            let mut target = target.clone();
            target.dir = DirId::ROOT;
            for item in target.usage.interface_link_libraries.iter_mut().flatten() {
                item.lookup_dir = DirId::ROOT;
                pending.push(item.text.value.clone());
            }
            targets.push((target, Vec::new()));
            targets.len() - 1
        });
        if name != target.name
            && let Some((_, aliases)) = targets.get_mut(idx)
            && !aliases.contains(&name)
        {
            aliases.push(name);
        }
    }
    Ok(targets)
}
