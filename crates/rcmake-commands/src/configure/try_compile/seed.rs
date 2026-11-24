use std::collections::BTreeMap;

use rcmake_base::{BString, list};
use rcmake_eval::{CacheDefine, Cx};
use rcmake_model::{CacheType, DirId, Target};

use super::args::Options;
use super::split_byte;

pub(super) struct TryCompileSeed {
    pub(super) defines: Vec<CacheDefine>,
    pub(super) enabled_languages: BString,
}

pub(super) fn toolchain_variable(name: &[u8]) -> bool {
    [
        b"CMAKE_C_".as_slice(),
        b"CMAKE_CXX_",
        b"CMAKE_OSX_",
        b"CMAKE_SYSROOT",
        b"CMAKE_SYSTEM_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name.starts_with(b"CMAKE_")
            && name
                .windows(b"_LINKER_FLAGS".len())
                .any(|part| part == b"_LINKER_FLAGS")
        || matches!(
            name,
            b"CMAKE_SYSTEM"
                | b"CMAKE_AR"
                | b"CMAKE_RANLIB"
                | b"CMAKE_MAKE_PROGRAM"
                | b"_CMAKE_OSX_SYSROOT_PATH"
        )
}

impl TryCompileSeed {
    pub(super) fn new(cx: &mut Cx<'_>, options: &Options<'_>) -> Self {
        let config = cx.var_or_empty(b"CMAKE_TRY_COMPILE_CONFIGURATION");
        let mut languages = cx
            .global()
            .props
            .get(b"ENABLED_LANGUAGES")
            .map(|value| list::split(value, list::Empty::Drop))
            .unwrap_or_default();
        let mut defines = Vec::new();
        for name in cx
            .variable_names()
            .iter()
            .filter(|name| toolchain_variable(name))
        {
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
        if cx.var_or_empty(b"CMAKE_SYSTEM_NAME") == b"Darwin" {
            let sysroot = cx.var_or_empty(b"CMAKE_OSX_SYSROOT");
            let resolved = cx.var_or_empty(b"_CMAKE_OSX_SYSROOT_PATH");
            if !sysroot.contains(&b'/') && !resolved.is_empty() {
                defines.push(CacheDefine {
                    name: "CMAKE_OSX_SYSROOT".into(),
                    ty: Some(CacheType::Path),
                    value: resolved,
                });
            }
        }
        for language in ["C", "CXX"] {
            let compiler = format!("CMAKE_{language}_COMPILER");
            if !cx.var_or_empty(compiler.as_bytes()).is_empty()
                && !languages.iter().any(|loaded| loaded == language)
            {
                languages.push(language.into());
            }
            defines.push(CacheDefine {
                name: format!("CMAKE_{language}_COMPILER_WORKS").into(),
                ty: Some(CacheType::Internal),
                value: "1".into(),
            });
        }
        for name in list::split(
            &cx.var_or_empty(b"CMAKE_TRY_COMPILE_PLATFORM_VARIABLES"),
            list::Empty::Drop,
        ) {
            seed_var(cx, &mut defines, &name);
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
        for name in [
            "C_STANDARD",
            "C_STANDARD_REQUIRED",
            "C_EXTENSIONS",
            "CXX_STANDARD",
            "CXX_STANDARD_REQUIRED",
            "CXX_EXTENSIONS",
        ] {
            if let Some(value) = options.one(name.as_bytes()) {
                defines.push(CacheDefine {
                    name: format!("CMAKE_{name}").into(),
                    ty: Some(CacheType::String),
                    value: value.into(),
                });
            }
        }
        Self {
            defines,
            enabled_languages: list::join(languages),
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

pub(super) fn probe_imports(cx: &mut Cx<'_>, options: &Options<'_>) -> Vec<(Target, Vec<BString>)> {
    let dir = cx.current_dir_id();
    let model = cx.project();
    let libraries = options.many(b"LINK_LIBRARIES");
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
    targets
}
