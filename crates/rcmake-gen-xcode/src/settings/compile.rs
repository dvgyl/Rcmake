use super::*;

impl TargetSettings<'_> {
    pub(super) fn add_compile_settings(&self, settings: &mut Attrs, ide: Option<&IdeFacts>) {
        let preferred = self.target.and_then(|target| preferred_language(target));
        let pch = self.target.and_then(prefix_header);
        if let Some(pch) = pch {
            settings.insert("GCC_PREFIX_HEADER".into(), text(&pch.header).into());
            settings.insert("GCC_PRECOMPILE_PREFIX_HEADER".into(), "YES".into());
        }
        let mut headers = Vec::new();
        let mut frameworks = Vec::new();
        let mut defines = vec![format!("CMAKE_INTDIR=\"{CONFIG_DIR}\"")];
        if let Some(group) = ide.filter(|_| !self.per_language_flags).and_then(|ide| {
            ide.compile
                .iter()
                .find(|group| Some(group.language.as_slice()) == preferred)
        }) {
            for define in &group.defines {
                unique(&mut defines, text(&define.value));
            }
            for include in group.includes.iter().filter(|include| !include.system) {
                let paths = if include.framework {
                    &mut frameworks
                } else {
                    &mut headers
                };
                unique(paths, text(&include.path));
            }
        }
        for (languages, key) in FAMILIES {
            let mut common = None;
            if let Some(target) = self.target {
                for &language in languages {
                    let Some(group) = compile_group(target, language) else {
                        continue;
                    };
                    let flags = baseline_flags(target, group, self.per_language_flags);
                    if self.per_language_flags {
                        settings.insert(setting(language), flags.join(" ").into());
                    } else {
                        common.get_or_insert(flags);
                    }
                }
            }
            settings.insert(
                key.into(),
                flags_with_inherited(&common.unwrap_or_default()),
            );
        }
        if let Some(ide) = ide {
            for item in &ide.link_items {
                if let IdeLinkItem::File(path) | IdeLinkItem::Framework(path) = item
                    && let Some(parent) = framework_search_path(path)
                {
                    unique(&mut frameworks, parent);
                }
                if let IdeLinkItem::Target(id) = item
                    && let Some(product) = self.linked_product(*id)
                    && matches!(product.bundle, BundleKind::Framework { .. })
                {
                    unique(&mut frameworks, product.dir);
                }
            }
        }
        for dir in self.phase_search_paths(true) {
            unique(&mut frameworks, dir);
        }
        settings.insert(
            "GCC_PREPROCESSOR_DEFINITIONS".into(),
            inherited_values(
                defines
                    .into_iter()
                    .map(|value| xcode_flag(&value))
                    .collect(),
            ),
        );
        for (key, values) in [
            ("HEADER_SEARCH_PATHS", headers),
            ("FRAMEWORK_SEARCH_PATHS", frameworks),
        ] {
            if !values.is_empty() {
                settings.insert(key.into(), inherited_paths(values));
            }
        }
    }
}

const FAMILIES: [(&[&[u8]], &str); 2] = [
    (&[b"C", b"OBJC"], "OTHER_CFLAGS"),
    (&[b"CXX", b"OBJCXX"], "OTHER_CPLUSPLUSFLAGS"),
];

fn compile_group<'a>(
    target: &'a ResolvedTarget,
    language: &[u8],
) -> Option<&'a IdeCompileSettings> {
    target
        .ide
        .as_ref()?
        .compile
        .iter()
        .find(|group| group.language == language)
}

fn preferred_language(target: &ResolvedTarget) -> Option<&[u8]> {
    target
        .link
        .as_ref()
        .map(|link| link.language.as_slice())
        .into_iter()
        .chain([b"CXX".as_slice(), b"C", b"OBJCXX", b"OBJC"])
        .find(|language| {
            FAMILIES.iter().any(|(family, _)| family.contains(language))
                && compile_group(target, language).is_some()
        })
}

pub(crate) fn prefix_header(target: &ResolvedTarget) -> Option<&rcmake_resolve::ide::IdePch> {
    let preferred = preferred_language(target);
    let pch = &target.ide.as_ref()?.pch;
    pch.iter()
        .find(|pch| Some(pch.language.as_slice()) == preferred)
        .or_else(|| {
            [b"CXX".as_slice(), b"C", b"OBJCXX", b"OBJC"]
                .iter()
                .find_map(|language| pch.iter().find(|pch| pch.language == *language))
        })
}

pub(crate) fn setting(language: &[u8]) -> String {
    format!("RCMAKE_{}_COMPILE_FLAGS", text(language))
}

pub(crate) fn needs_per_language_flags(target: &ResolvedTarget) -> bool {
    let groups: Vec<_> = FAMILIES
        .iter()
        .flat_map(|(family, _)| *family)
        .filter_map(|language| compile_group(target, language))
        .collect();
    if let Some(first) = groups.first()
        && groups.iter().skip(1).any(|group| {
            group.includes != first.includes
                || !group
                    .defines
                    .iter()
                    .map(|def| &def.value)
                    .eq(first.defines.iter().map(|def| &def.value))
        })
    {
        return true;
    }
    FAMILIES.iter().any(|(family, _)| {
        let mut flags = family
            .iter()
            .filter_map(|language| compile_group(target, language))
            .map(|group| baseline_flags(target, group, false));
        flags
            .next()
            .is_some_and(|first| flags.any(|other| other != first))
    })
}

pub(crate) fn supported(language: &[u8]) -> bool {
    FAMILIES
        .iter()
        .any(|(family, _)| family.contains(&language))
}

fn baseline_flags(
    target: &ResolvedTarget,
    group: &IdeCompileSettings,
    complete: bool,
) -> Vec<String> {
    let mut flags = group
        .flags
        .iter()
        .map(|flag| flag_text(&flag.value))
        .collect::<Vec<_>>();
    if complete {
        flags.extend(
            group
                .defines
                .iter()
                .map(|define| xcode_flag(&format!("-D{}", text(&define.value)))),
        );
    }
    for include in &group.includes {
        if include.system {
            let prefix = if include.framework {
                "-iframework"
            } else {
                "-isystem"
            };
            flags.push(format!("{prefix} {}", xcode_flag(&text(&include.path))));
        } else if complete {
            let prefix = if include.framework { "-F" } else { "-I" };
            flags.push(xcode_flag(&format!("{prefix}{}", text(&include.path))));
        }
    }
    if let Some(prefix) = prefix_header(target) {
        if let Some(alternate) = target.ide.as_ref().and_then(|ide| {
            ide.pch
                .iter()
                .find(|pch| pch.language == group.language && pch.header != prefix.header)
        }) {
            flags.push(format!("-include {}", xcode_flag(&text(&alternate.header))));
        }
        if flags.iter().any(|flag| flag.contains("-include"))
            && let Some(flags) = hide_force_includes(&flags.join(" "))
        {
            return vec![flags];
        }
    }
    flags
}

pub(crate) fn hide_force_includes(flags: &str) -> Option<String> {
    let args = rcmake_resolve::lower::rule_arguments(flags.as_bytes())?;
    let mut args = args.iter().peekable();
    let mut out = Vec::new();
    while let Some(arg) = args.next() {
        if arg == b"-Xclang" {
            out.push("-Xclang".into());
            if let Some(next) = args.next() {
                out.push(xcode_flag(&text(next)));
            }
        } else if arg == b"-include" {
            if let Some(next) = args.next() {
                out.push("-Xclang".into());
                out.push(xcode_flag(&format!("-include{}", text(next))));
            } else {
                out.push("-include".into());
            }
        } else {
            if arg.starts_with(b"-include") && !arg.starts_with(b"-include-pch") {
                out.push("-Xclang".into());
            }
            out.push(xcode_flag(&text(arg)));
        }
    }
    Some(out.join(" "))
}
