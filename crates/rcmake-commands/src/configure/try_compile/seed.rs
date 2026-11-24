use std::collections::BTreeMap;

use rcmake_base::{BString, list};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{CacheDefine, Cx};
use rcmake_model::{CacheType, DirId, Target, TargetKind};

use super::args::{Options, STANDARD_OPTIONS};
use super::{probe_error, split_byte};

const PROBE_LANGUAGES: &[&str] = &["C", "CXX", "OBJC", "OBJCXX"];
const CMP0137: PolicyId = PolicyId::new(137);
const CMP0210: PolicyId = PolicyId::new(210);

pub(super) struct TryCompileSeed {
    pub(super) defines: Vec<CacheDefine>,
    pub(super) enabled_languages: BString,
}

pub(super) fn toolchain_variable(name: &[u8]) -> bool {
    [
        b"CMAKE_C_".as_slice(),
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
        let mut defines = Vec::new();
        for name in cx
            .variable_names()
            .iter()
            .filter(|name| toolchain_variable(name))
        {
            if !honor_link_flags
                && PROBE_LANGUAGES.iter().any(|lang| {
                    let key = format!("CMAKE_{lang}_LINK_FLAGS");
                    name.strip_prefix(key.as_bytes())
                        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(b"_"))
                })
            {
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
        ] {
            seed_var(cx, &mut defines, name.as_bytes());
        }
        if let Some(archs) = cx.var(b"CMAKE_TRY_COMPILE_OSX_ARCHITECTURES") {
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
        if (source_signature || cx.policy(CMP0137) == PolicyStatus::New)
            && !cx.var_is_on(b"CMAKE_TRY_COMPILE_NO_PLATFORM_VARIABLES")
        {
            for name in [
                b"CMAKE_ENABLE_EXPORTS".as_slice(),
                b"CMAKE_CUDA_ARCHITECTURES",
            ] {
                seed_var(cx, &mut defines, name);
            }
            for name in list::split(
                &cx.var_or_empty(b"CMAKE_TRY_COMPILE_PLATFORM_VARIABLES"),
                list::Empty::Drop,
            ) {
                seed_var(cx, &mut defines, &name);
            }
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
        for name in STANDARD_OPTIONS {
            if let Some(value) = options.one(name) {
                defines.push(CacheDefine {
                    name: [b"CMAKE_".as_slice(), name].concat().into(),
                    ty: Some(CacheType::String),
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
