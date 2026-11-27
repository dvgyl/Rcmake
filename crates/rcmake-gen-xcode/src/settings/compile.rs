use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, RenderOptions};
use rcmake_host::Stream;

use super::*;

impl TargetSettings<'_> {
    pub(super) fn add_compile_settings(
        &self,
        settings: &mut Attrs,
        ide: Option<&IdeFacts>,
    ) -> Result<(), GenError> {
        let include_lang = self.target.and_then(|target| {
            preferred_language(target)
                .or_else(|| compile_group(target, b"Swift").map(|_| b"Swift".as_slice()))
        });
        let pch = self.target.and_then(prefix_header);
        if let Some(pch) = pch {
            settings.insert("GCC_PREFIX_HEADER".into(), text(&pch.header).into());
            settings.insert("GCC_PRECOMPILE_PREFIX_HEADER".into(), "YES".into());
        }
        let mut headers = UniqueList::default();
        let mut sys_headers = UniqueList::default();
        let mut frameworks = UniqueList::default();
        let mut sys_frameworks = UniqueList::default();
        let mut defines = UniqueList::default();
        defines.push_unique(format!("CMAKE_INTDIR=\"{CONFIG_DIR}\""));
        if let Some(group) = ide.filter(|_| !self.per_language_flags).and_then(|ide| {
            ide.compile
                .iter()
                .find(|group| Some(group.language.as_slice()) == include_lang)
        }) {
            let implicit = implicit_includes(self.model, self.raw.dir, &group.language);
            for define in &group.defines {
                defines.push_unique(text(&define.value));
            }
            for include in &group.includes {
                if is_implicit_include(&include.path, &implicit) {
                    continue;
                }
                let paths = match (include.system, include.framework) {
                    (false, false) => &mut headers,
                    (true, false) => &mut sys_headers,
                    (false, true) => &mut frameworks,
                    (true, true) => &mut sys_frameworks,
                };
                paths.push_unique(text(&include.path));
            }
        }
        // Collect Swift include paths directly from the Swift compile group even when
        // C-family languages use per-language flag settings (which have no Swift family).
        let mut swift_headers = UniqueList::default();
        let mut swift_sys_headers = UniqueList::default();
        if let Some(swift_group) = self
            .target
            .and_then(|target| compile_group(target, b"Swift"))
        {
            let mut implicit = implicit_includes(self.model, self.raw.dir, b"Swift");
            if let Some(lang) = include_lang.filter(|&lang| lang != b"Swift") {
                implicit.extend(implicit_includes(self.model, self.raw.dir, lang));
            }
            for include in &swift_group.includes {
                if is_implicit_include(&include.path, &implicit) {
                    continue;
                }
                let paths = match (include.system, include.framework) {
                    (false, false) => &mut swift_headers,
                    (true, false) => &mut swift_sys_headers,
                    (false, true) => &mut frameworks,
                    (true, true) => &mut sys_frameworks,
                };
                paths.push_unique(text(&include.path));
            }
        }
        let mut opt_level = None;
        for (languages, key) in FAMILIES {
            let mut common = None;
            if let Some(target) = self.target {
                for &language in languages {
                    let Some(group) = compile_group(target, language) else {
                        continue;
                    };
                    let implicit = implicit_includes(self.model, self.raw.dir, &group.language);
                    let mut flags =
                        baseline_flags(target, group, self.per_language_flags, &implicit);
                    if self.per_language_flags {
                        settings.insert(setting(language), flags.join(" ").into());
                    } else {
                        if let Some(level) = extract_optimization_level(&mut flags) {
                            opt_level = Some(level);
                        }
                        common.get_or_insert(flags);
                    }
                }
            }
            settings.insert(
                key.into(),
                flags_with_inherited(&common.unwrap_or_default()),
            );
        }
        if !self.per_language_flags {
            settings.insert(
                "GCC_OPTIMIZATION_LEVEL".into(),
                opt_level.unwrap_or_else(|| "0".into()).into(),
            );
        }
        if let Some(ide) = ide {
            for item in &ide.link_items {
                if let IdeLinkItem::File(path) | IdeLinkItem::Framework(path) = item
                    && let Some(parent) = framework_search_path(path)
                {
                    frameworks.push_unique(parent);
                }
                if let IdeLinkItem::Target(id) = item
                    && let Some(product) = self.linked_product(*id)
                    && matches!(product.bundle, BundleKind::Framework { .. })
                {
                    frameworks.push_unique(product.dir);
                }
            }
        }
        for dir in self.phase_search_paths(true) {
            frameworks.push_unique(dir);
        }
        settings.insert(
            "GCC_PREPROCESSOR_DEFINITIONS".into(),
            inherited_values(
                defines
                    .into_vec()
                    .into_iter()
                    .map(|value| xcode_flag(&value))
                    .collect(),
            ),
        );
        let has_swift = ide.is_some_and(|ide| {
            ide.sources.iter().any(|source| {
                matches!(
                    &source.kind,
                    rcmake_resolve::ide::IdeSourceKind::Compile { language } if language == b"Swift"
                )
            })
        });
        for (key, values) in [
            ("HEADER_SEARCH_PATHS", headers),
            ("SYSTEM_HEADER_SEARCH_PATHS", sys_headers),
            ("FRAMEWORK_SEARCH_PATHS", frameworks),
            ("SYSTEM_FRAMEWORK_SEARCH_PATHS", sys_frameworks),
        ] {
            if !values.is_empty() {
                settings.insert(key.into(), inherited_paths(values.into_vec()));
            }
        }
        if has_swift && !swift_headers.is_empty() {
            settings.insert(
                "SWIFT_INCLUDE_PATHS".into(),
                inherited_paths(swift_headers.into_vec()),
            );
        }
        if has_swift && !swift_sys_headers.is_empty() {
            settings.insert(
                "SWIFT_SYSTEM_INCLUDE_PATHS".into(),
                inherited_paths(swift_sys_headers.into_vec()),
            );
        }
        if has_swift {
            self.add_swift_target_settings(settings)?;
        }
        Ok(())
    }

    fn add_swift_target_settings(&self, settings: &mut Attrs) -> Result<(), GenError> {
        if let Some(swift_group) = self
            .target
            .and_then(|target| compile_group(target, b"Swift"))
        {
            // cmGlobalXCodeGenerator.cxx:2902-2926: strip `=value` suffixes from Swift defines.
            let mut conditions = UniqueList::default();
            for define in &swift_group.defines {
                let raw = text(&define.value);
                let name = raw.split('=').next().unwrap_or(&raw);
                if !name.is_empty() {
                    conditions.push_unique(xcode_flag(name));
                }
            }
            if !conditions.is_empty() {
                settings.insert(
                    "SWIFT_ACTIVE_COMPILATION_CONDITIONS".into(),
                    inherited_values(conditions.into_vec()),
                );
            }
            let swift_flags: Vec<String> = swift_group
                .flags
                .iter()
                .filter(|flag| !flag.value.is_empty())
                .map(|flag| flag_text(&flag.value))
                .collect();
            if !swift_flags.is_empty() {
                settings.insert(
                    "OTHER_SWIFT_FLAGS".into(),
                    flags_with_inherited(&swift_flags),
                );
            }
        }
        // CMP0157 NEW per-target SWIFT_COMPILATION_MODE (`cmGlobalXCodeGenerator.cxx:2928-2945`).
        if let Some(evaluated) = rcmake_resolve::lower::swift_compilation_mode(
            self.model,
            self.raw,
            self.config.as_bytes(),
        )
        .map_err(|error| {
            let message = match error {
                rcmake_resolve::ResolveError::Genex(error) => error.without_location().to_string(),
                error => error.to_string(),
            };
            GenError::Invalid(message.into())
        })? {
            match evaluated.as_slice() {
                b"wholemodule" => {
                    settings.insert("SWIFT_COMPILATION_MODE".into(), "wholemodule".into());
                }
                b"incremental" | b"singlefile" => {}
                _ => {
                    let diagnostic = Diagnostic {
                        annotations: None,
                        kind: MessageKind::AuthorWarning,
                        effect: Effect::None,
                        category: None,
                        policy: None,
                        location: Location::Backtrace(self.raw.backtrace.clone()),
                        body: format!(
                            "Unknown Swift_COMPILATION_MODE on target '{}'",
                            self.raw.name
                        )
                        .into(),
                    };
                    self.host.write_out(
                        Stream::Stderr,
                        &rcmake_diag::render(
                            &diagnostic,
                            &RenderOptions {
                                relative_base: Some(self.model.dir(DirId::ROOT).source_dir.clone()),
                                ..RenderOptions::default()
                            },
                        ),
                    );
                }
            }
        }
        // Per-target SWIFT_VERSION override.
        if let Some(version) = self
            .raw
            .props
            .get(b"Swift_LANGUAGE_VERSION")
            .filter(|value| !value.is_empty())
        {
            settings.insert("SWIFT_VERSION".into(), text(version).into());
        }
        // CMP0226 NEW: PRODUCT_MODULE_NAME from Swift_MODULE_NAME (`cmGlobalXCodeGenerator.cxx:2948-2953`).
        if let Some(module_name) = self
            .raw
            .props
            .get(b"Swift_MODULE_NAME")
            .filter(|value| !value.is_empty())
            && self.raw.policies.policies.get(PolicyId::new(226)) == PolicyStatus::New
        {
            settings.insert("PRODUCT_MODULE_NAME".into(), text(module_name).into());
        }
        // SWIFT_PACKAGE_NAME on Xcode >= 15 (or default modern Xcode when version is unset)
        // honoring Swift_PACKAGE_NAME and CMP0216 (`cmGlobalXCodeGenerator.cxx:2956-2962`).
        let xcode_supports_package = self
            .model
            .definition(self.raw.dir, b"XCODE_VERSION")
            .or_else(|| self.model.definition(DirId::ROOT, b"XCODE_VERSION"))
            .filter(|value| !value.is_empty())
            .is_none_or(|ver| rcmake_base::version::compare(ver, b"15.0").is_ge());
        if xcode_supports_package {
            let package = rcmake_resolve::lower::swift_package_name(self.model, self.raw);
            if !package.is_empty() {
                settings.insert("SWIFT_PACKAGE_NAME".into(), text(&package).into());
            }
        }
        Ok(())
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
            .map(|group| baseline_flags(target, group, false, &BTreeSet::new()));
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

fn implicit_includes(model: &ProjectModel, dir: DirId, language: &[u8]) -> BTreeSet<BString> {
    let key = [b"CMAKE_", language, b"_IMPLICIT_INCLUDE_DIRECTORIES"].concat();
    let mut set: BTreeSet<BString> = model
        .definition(dir, &key)
        .into_iter()
        .flat_map(|value| cmake_list::split(value, cmake_list::Empty::Drop))
        .map(|path| normalize_include_key(&path))
        .collect();
    if let Some(sdk) = model
        .definition(dir, b"_CMAKE_OSX_SYSROOT_PATH")
        .or_else(|| model.definition(dir, b"CMAKE_OSX_SYSROOT"))
        .filter(|path| path.starts_with(b"/"))
    {
        set.insert(normalize_include_key(
            &[sdk.as_slice(), b"/usr/include"].concat(),
        ));
    }
    set
}

fn normalize_include_key(path: &[u8]) -> BString {
    let normalized = rcmake_base::path_lex::Lexical::new(path).normalize();
    let trimmed = normalized
        .as_slice()
        .strip_suffix(b"/")
        .unwrap_or(&normalized);
    if trimmed.is_empty() {
        normalized
    } else {
        trimmed.into()
    }
}

fn is_implicit_include(path: &[u8], implicit: &BTreeSet<BString>) -> bool {
    implicit.contains(&normalize_include_key(path))
}

fn optimization_level(token: &str) -> Option<String> {
    if token == "-Ofast" {
        Some("fast".into())
    } else if token == "-Os" {
        Some("s".into())
    } else if token == "-O" {
        Some("1".into())
    } else if let Some(digits) = token.strip_prefix("-O")
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        Some(digits.into())
    } else {
        None
    }
}

fn extract_optimization_level(flags: &mut Vec<String>) -> Option<String> {
    let mut level = None;
    let mut kept = Vec::with_capacity(flags.len());
    for flag in std::mem::take(flags) {
        if let Some(found) = optimization_level(flag.as_str()) {
            level = Some(found);
        } else if !flag.contains(['\'', '"'])
            && flag
                .split_whitespace()
                .any(|tok| optimization_level(tok).is_some())
        {
            let mut rem = Vec::new();
            for tok in flag.split_whitespace() {
                if let Some(found) = optimization_level(tok) {
                    level = Some(found);
                } else {
                    rem.push(tok);
                }
            }
            if !rem.is_empty() {
                kept.push(rem.join(" "));
            }
        } else {
            kept.push(flag);
        }
    }
    *flags = kept;
    level
}

fn baseline_flags(
    target: &ResolvedTarget,
    group: &IdeCompileSettings,
    complete: bool,
    implicit: &BTreeSet<BString>,
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
        for include in &group.includes {
            if is_implicit_include(&include.path, implicit) {
                continue;
            }
            if include.system {
                let prefix = if include.framework {
                    "-iframework"
                } else {
                    "-isystem"
                };
                flags.push(format!("{prefix} {}", xcode_flag(&text(&include.path))));
            } else {
                let prefix = if include.framework { "-F" } else { "-I" };
                flags.push(xcode_flag(&format!("{prefix}{}", text(&include.path))));
            }
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
