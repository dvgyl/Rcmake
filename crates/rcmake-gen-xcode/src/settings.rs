//! Project and target build settings for each configuration.

use std::{collections::BTreeSet, path::Path};

use rcmake_base::{list as cmake_list, truth};
use rcmake_model::{DirId, ParsedToolset, ProjectModel, Target, TargetKind};
use rcmake_resolve::{
    BuildStep, GenerationResult, LinkFragmentRole, ResolvedTarget,
    ide::{BundleKind, IdeLinkItem},
};

use crate::{
    object::{Attrs, attrs, list},
    phases::quote,
    text,
};

pub(crate) const CONFIG_DIR: &str = "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)";
const INHERITED: &str = "$(inherited)";

pub(crate) struct Product {
    pub(crate) name: String,
    pub(crate) dir: String,
    pub(crate) prefix: String,
    pub(crate) suffix: String,
    pub(crate) bundle: BundleKind,
    pub(crate) kind: TargetKind,
    version: Option<String>,
    soversion: Option<String>,
    no_soname: bool,
}

impl Product {
    pub(crate) fn new(
        model: &ProjectModel,
        raw: &Target,
        target: Option<&ResolvedTarget>,
        config: &str,
        bin: &Path,
    ) -> Self {
        let version = |key| {
            raw.props
                .get(key)
                .filter(|value| !value.is_empty())
                .map(|value| text(value))
        };
        let no_soname = on(raw, b"NO_SONAME");
        if let Some(ide) = target.and_then(|t| t.ide.as_deref())
            && !ide.naming.base_name.is_empty()
        {
            return Self {
                name: text(&ide.naming.base_name),
                dir: text(&ide.naming.dir),
                prefix: text(&ide.naming.prefix),
                suffix: text(&ide.naming.suffix),
                bundle: ide.bundle.clone(),
                kind: raw.kind,
                version: ide
                    .naming
                    .version
                    .as_ref()
                    .map(|value| text(value))
                    .or_else(|| version(b"VERSION")),
                soversion: ide
                    .naming
                    .soversion
                    .as_ref()
                    .map(|value| text(value))
                    .or_else(|| version(b"SOVERSION")),
                no_soname,
            };
        }
        let kind = raw.kind;
        let (class, platform, prefix, suffix) = match kind {
            TargetKind::Executable => ("RUNTIME", "EXECUTABLE", "", ""),
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary => {
                ("ARCHIVE", "STATIC_LIBRARY", "lib", ".a")
            }
            TargetKind::ModuleLibrary => ("LIBRARY", "SHARED_MODULE", "lib", ".so"),
            _ => ("LIBRARY", "SHARED_LIBRARY", "lib", ".dylib"),
        };
        let upper = config.to_ascii_uppercase();
        let configured = |name: &str| {
            raw.props
                .get(format!("{name}_{upper}").as_bytes())
                .or_else(|| raw.props.get(name.as_bytes()))
        };
        let name = configured(&format!("{class}_OUTPUT_NAME"))
            .or_else(|| configured("OUTPUT_NAME"))
            .map_or_else(|| text(&raw.name), |v| text(v));
        let postfix = raw
            .props
            .get(format!("{upper}_POSTFIX").as_bytes())
            .map_or_else(String::new, |v| text(v));
        let out_key = format!("{class}_OUTPUT_DIRECTORY");
        let configured_dir = raw.props.get(format!("{out_key}_{upper}").as_bytes());
        let mut dir = bin.join(text(&model.dir(raw.dir).binary_dir));
        if let Some(value) = configured_dir.or_else(|| raw.props.get(out_key.as_bytes())) {
            dir = dir.join(text(value));
        }
        if kind == TargetKind::ObjectLibrary {
            dir = bin.join(format!("build/{}.build/{CONFIG_DIR}", text(&raw.name)));
        } else if let Some(resolved_dir) = target
            .and_then(|target| target.artifacts.file.as_ref())
            .filter(|path| !path.windows(2).any(|pair| pair == b"$<"))
            .and_then(|path| {
                rcmake_host::path::to_native(path)
                    .parent()
                    .map(Path::to_owned)
            })
            && model.generator == Some(rcmake_model::GeneratorKind::Xcode)
        {
            dir = resolved_dir;
        } else if configured_dir.is_none() && !config.is_empty() {
            dir = dir.join(config);
        }
        let affix = |key: &str, default: &str| {
            raw.props
                .get(key.as_bytes())
                .or_else(|| model.definition(raw.dir, format!("CMAKE_{platform}_{key}").as_bytes()))
                .map_or_else(|| default.into(), |v| text(v))
        };
        Self {
            name: format!("{name}{postfix}"),
            dir: dir.to_string_lossy().into_owned(),
            prefix: affix("PREFIX", prefix),
            suffix: affix("SUFFIX", suffix),
            bundle: if on(raw, b"MACOSX_BUNDLE") {
                BundleKind::App
            } else if on(raw, b"FRAMEWORK") {
                BundleKind::Framework {
                    version: raw
                        .props
                        .get(b"FRAMEWORK_VERSION")
                        .cloned()
                        .unwrap_or_else(|| "A".into()),
                }
            } else if on(raw, b"BUNDLE") {
                BundleKind::Bundle {
                    extension: raw
                        .props
                        .get(b"BUNDLE_EXTENSION")
                        .cloned()
                        .unwrap_or_else(|| "bundle".into()),
                }
            } else {
                BundleKind::None
            },
            kind,
            version: version(b"VERSION"),
            soversion: version(b"SOVERSION"),
            no_soname,
        }
    }

    pub(crate) fn file(&self) -> String {
        match &self.bundle {
            BundleKind::App => format!("{}.app", self.name),
            BundleKind::Framework { .. } => format!("{}.framework", self.name),
            BundleKind::Bundle { extension } => format!("{}.{}", self.name, text(extension)),
            BundleKind::None => format!("{}{}{}", self.prefix, self.name, self.suffix),
        }
    }

    pub(crate) fn types(&self) -> (&'static str, &'static str) {
        match self.bundle {
            BundleKind::App => ("application", "wrapper.application"),
            BundleKind::Framework { .. } => ("framework", "wrapper.framework"),
            BundleKind::Bundle { .. } => ("bundle", "wrapper.cfbundle"),
            BundleKind::None => match self.kind {
                TargetKind::Executable => ("tool", "compiled.mach-o.executable"),
                TargetKind::StaticLibrary | TargetKind::ObjectLibrary => {
                    ("library.static", "archive.ar")
                }
                TargetKind::ModuleLibrary => ("library.dynamic", "compiled.mach-o.dylib"),
                _ => ("library.dynamic", "compiled.mach-o.dylib"),
            },
        }
    }

    fn library_version(&self) -> Option<&str> {
        (self.kind == TargetKind::SharedLibrary
            && self.bundle == BundleKind::None
            && !self.no_soname)
            .then(|| self.version.as_deref().or(self.soversion.as_deref()))
            .flatten()
    }

    fn versioned_file(&self, version: &str) -> String {
        format!("{}{}.{version}{}", self.prefix, self.name, self.suffix)
    }

    pub(crate) fn version_links(&self) -> Option<(String, Vec<String>)> {
        let version = self.library_version()?;
        let actual = self.versioned_file(version);
        let soname = self.versioned_file(self.soversion.as_deref().unwrap_or(version));
        let mut aliases = vec![self.file()];
        if actual != soname {
            aliases.push(soname);
        }
        Some((actual, aliases))
    }

    fn link_file(&self) -> String {
        let file = self
            .library_version()
            .map_or_else(|| self.file(), |version| self.versioned_file(version));
        match &self.bundle {
            BundleKind::Framework { .. } => format!("{}/{file}/{}", self.dir, self.name),
            _ => format!("{}/{file}", self.dir),
        }
    }
}

pub(crate) fn on(target: &Target, key: &[u8]) -> bool {
    target.props.get(key).is_some_and(|v| truth::is_on(v))
}

pub(crate) fn project(
    model: &ProjectModel,
    root: DirId,
    bin: &Path,
    arch: &str,
    config: &str,
) -> Attrs {
    let value = |key: &[u8]| {
        model
            .definition(root, key)
            .map_or_else(String::new, |v| text(v))
    };
    let archs = [
        value(b"CMAKE_OSX_ARCHITECTURES"),
        value(b"CMAKE_XCODE_ARCHS"),
        arch.into(),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or_default();
    let mut settings = attrs([
        ("ARCHS", archs.replace(';', " ").into()),
        ("SDKROOT", value(b"CMAKE_OSX_SYSROOT").into()),
        (
            "SYMROOT",
            bin.join("build").to_string_lossy().into_owned().into(),
        ),
        ("ONLY_ACTIVE_ARCH", "NO".into()),
    ]);
    let deployment = value(b"CMAKE_OSX_DEPLOYMENT_TARGET");
    if !deployment.is_empty() {
        settings.insert("MACOSX_DEPLOYMENT_TARGET".into(), deployment.into());
    }
    if config != "Debug" {
        settings.insert("SWIFT_COMPILATION_MODE".into(), "wholemodule".into());
    }
    if let ParsedToolset::Xcode {
        toolset: Some(toolset),
        ..
    } = &model.generator_spec.parsed
    {
        settings.insert("GCC_VERSION".into(), text(toolset).into());
    }
    for (key, value) in &model.dir(root).vars {
        attribute(&mut settings, key, value, "CMAKE_XCODE_ATTRIBUTE_", config);
    }
    for (key, entry) in model.cache.iter() {
        if !model.dir(root).vars.contains_key(key.as_slice()) {
            attribute(
                &mut settings,
                key,
                &entry.value,
                "CMAKE_XCODE_ATTRIBUTE_",
                config,
            );
        }
    }
    settings
}

fn attribute(out: &mut Attrs, key: &[u8], value: &[u8], prefix: &str, config: &str) {
    let Ok(key) = std::str::from_utf8(key) else {
        return;
    };
    let Some(key) = key.strip_prefix(prefix) else {
        return;
    };
    let name = if let Some((head, tail)) = key.split_once("[variant=") {
        let Some((variant, rest)) = tail.split_once(']') else {
            return;
        };
        if variant != config {
            return;
        }
        format!("{head}{rest}")
    } else {
        key.into()
    };
    out.insert(name, text(value).into());
}

pub(crate) struct TargetSettings<'a> {
    pub(crate) model: &'a ProjectModel,
    pub(crate) result: &'a GenerationResult,
    pub(crate) bin: &'a Path,
    pub(crate) config: &'a str,
    pub(crate) arch: &'a str,
    pub(crate) raw: &'a Target,
    pub(crate) target: Option<&'a ResolvedTarget>,
}

impl TargetSettings<'_> {
    pub(crate) fn build(&self, product: &Product, plist: Option<&Path>) -> Attrs {
        let Self {
            model,
            result,
            bin,
            config,
            arch,
            raw,
            target,
        } = *self;
        let name = text(&raw.name);
        let mut settings = project(model, raw.dir, bin, arch, config);
        settings.extend(attrs([
            ("ALWAYS_SEARCH_USER_PATHS", "NO".into()),
            ("COMBINE_HIDPI_IMAGES", "YES".into()),
            ("CONFIGURATION_BUILD_DIR", product.dir.clone().into()),
            (
                "GCC_GENERATE_DEBUGGING_SYMBOLS",
                if matches!(config, "Debug" | "RelWithDebInfo") {
                    "YES"
                } else {
                    "NO"
                }
                .into(),
            ),
            (
                "GCC_OPTIMIZATION_LEVEL",
                match config {
                    "Release" => "3",
                    "RelWithDebInfo" => "2",
                    "MinSizeRel" => "s",
                    _ => "0",
                }
                .into(),
            ),
            (
                "PRODUCT_NAME",
                product
                    .library_version()
                    .map_or_else(
                        || product.name.clone(),
                        |version| format!("{}.{version}", product.name),
                    )
                    .into(),
            ),
            (
                "TARGET_TEMP_DIR",
                format!("{}/build/{name}.build/{CONFIG_DIR}", bin.display()).into(),
            ),
            ("USE_HEADERMAP", "NO".into()),
            ("WARNING_CFLAGS", list([INHERITED])),
            ("SKIP_INSTALL", "YES".into()),
        ]));
        if !matches!(
            product.bundle,
            BundleKind::App | BundleKind::Framework { .. }
        ) {
            settings.extend(attrs([
                ("EXECUTABLE_PREFIX", product.prefix.clone().into()),
                ("EXECUTABLE_SUFFIX", product.suffix.clone().into()),
            ]));
        }
        if let Some(plist) = plist {
            settings.insert(
                "INFOPLIST_FILE".into(),
                plist.to_string_lossy().into_owned().into(),
            );
            settings.insert("CODE_SIGNING_ALLOWED".into(), "NO".into());
        }
        match &product.bundle {
            BundleKind::Framework { version } => {
                settings.insert("FRAMEWORK_VERSION".into(), text(version).into());
            }
            BundleKind::Bundle { extension } => {
                settings.insert("WRAPPER_EXTENSION".into(), text(extension).into());
                settings.insert("MACH_O_TYPE".into(), "mh_bundle".into());
            }
            _ => {}
        }
        if product.kind == TargetKind::SharedLibrary {
            settings.insert("INSTALL_PATH".into(), "@rpath".into());
            settings.insert("DYLIB_INSTALL_NAME_BASE".into(), "@rpath".into());
        }
        if product.kind.is_library() {
            settings.insert(
                "LIBRARY_STYLE".into(),
                if matches!(
                    product.kind,
                    TargetKind::StaticLibrary | TargetKind::ObjectLibrary
                ) {
                    "STATIC"
                } else {
                    "DYNAMIC"
                }
                .into(),
            );
        }
        if product.kind == TargetKind::ModuleLibrary {
            settings.insert("MACH_O_TYPE".into(), "mh_bundle".into());
        }
        let ide = target.and_then(|target| target.ide.as_deref());
        let mut headers = Vec::new();
        let mut frameworks = Vec::new();
        let mut defines = vec![format!("CMAKE_INTDIR=\"{CONFIG_DIR}\"")];
        let mut cflags = Vec::new();
        let mut cxxflags = Vec::new();

        for (language, key, flags) in [
            ("C", "OTHER_CFLAGS", &mut cflags),
            ("CXX", "OTHER_CPLUSPLUSFLAGS", &mut cxxflags),
        ] {
            let flag_key = format!("CMAKE_{language}_FLAGS");
            for var in [
                flag_key.clone(),
                format!("{flag_key}_{}", config.to_ascii_uppercase()),
            ] {
                if let Some(value) = model
                    .definition(raw.dir, var.as_bytes())
                    .filter(|v| !v.is_empty())
                {
                    flags.push(text(value));
                }
            }
            let mut include = |flags: &mut Vec<String>, path: &[u8], system, framework| {
                if framework {
                    unique(&mut frameworks, text(path));
                } else if system {
                    flags.push(format!("-isystem {}", quote(&text(path))));
                } else {
                    unique(&mut headers, text(path));
                }
            };
            if let Some(ide) = ide {
                for group in ide
                    .compile
                    .iter()
                    .filter(|group| group.language.as_slice() == language.as_bytes())
                {
                    for dir in &group.includes {
                        include(flags, &dir.path, dir.system, dir.framework);
                    }
                    for define in &group.defines {
                        unique(&mut defines, text(&define.value));
                    }
                    flags.extend(group.flags.iter().map(|flag| flag_text(&flag.value)));
                }
                if let Some(pch) = ide
                    .pch
                    .iter()
                    .find(|pch| pch.language.as_slice() == language.as_bytes())
                {
                    flags.push(format!("-include {}", quote(&text(&pch.header))));
                }
            } else if let Some(target) = target {
                for group in target
                    .compile_groups
                    .iter()
                    .filter(|group| group.language.as_slice() == language.as_bytes())
                {
                    for dir in &group.includes {
                        include(flags, &dir.path.value, dir.system, false);
                    }
                    for define in &group.defines {
                        unique(&mut defines, text(&define.value));
                    }
                }
                flags.extend(
                    common_flags(target, language.as_bytes())
                        .iter()
                        .map(|flag| flag_text(&flag.value)),
                );
            }
            flags.push(INHERITED.into());
            settings.insert(key.into(), flags.join(" ").into());
        }
        for (key, mut values) in [
            ("GCC_PREPROCESSOR_DEFINITIONS", defines),
            ("HEADER_SEARCH_PATHS", headers),
            ("FRAMEWORK_SEARCH_PATHS", frameworks),
        ] {
            let values = values.drain(..).map(|value| quote(&value));
            settings.insert(key.into(), list(values.chain([INHERITED.into()])));
        }
        let mut ldflags = Vec::new();
        let mut libraries = Vec::new();
        if let Some(ide) = ide {
            libraries.extend(ide.link_directories.iter().map(|dir| text(dir)));
            ldflags.extend(
                ide.link_options
                    .iter()
                    .map(|option| flag_text(&option.value)),
            );
            for item in &ide.link_items {
                match item {
                    IdeLinkItem::Target(id) => {
                        if let Some(dep) = model.target(*id) {
                            let resolved = result.targets.iter().find(|target| {
                                target.id == *id && target.config.0.as_slice() == config.as_bytes()
                            });
                            if dep.is_imported() {
                                if let Some(resolved) = target {
                                    let artifacts = rcmake_resolve::lower::compute_artifacts(
                                        model,
                                        dep,
                                        &resolved.config,
                                    );
                                    if let Some(path) = artifacts.linker_file {
                                        ldflags.push(link_file(&text(&path)));
                                    }
                                }
                            } else if dep.kind.is_library()
                                && !matches!(
                                    dep.kind,
                                    TargetKind::InterfaceLibrary | TargetKind::ObjectLibrary
                                )
                            {
                                ldflags.push(quote(
                                    &Product::new(model, dep, resolved, config, bin).link_file(),
                                ));
                            }
                        }
                    }
                    IdeLinkItem::Framework(name) => {
                        let name = text(name);
                        if name.contains('/') {
                            ldflags.push(link_file(&name));
                        } else {
                            ldflags.push(format!(
                                "-framework {}",
                                quote(name.strip_suffix(".framework").unwrap_or(&name))
                            ));
                        }
                    }
                    IdeLinkItem::Library(name) => ldflags.push(quote(&format!("-l{}", text(name)))),
                    IdeLinkItem::File(path) => {
                        if !ide.sources.iter().any(|source| {
                            source.path == *path
                                && matches!(
                                    source.kind,
                                    rcmake_resolve::ide::IdeSourceKind::Object
                                        | rcmake_resolve::ide::IdeSourceKind::ExternalObject
                                )
                        }) {
                            ldflags.push(link_file(&text(path)));
                        }
                    }
                    IdeLinkItem::Flag(flag) => ldflags.push(flag_text(flag)),
                }
            }
            if let Some(dir) = &ide.install_name_dir {
                settings.insert("DYLIB_INSTALL_NAME_BASE".into(), text(dir).into());
            }
            ldflags.extend(ide.build_rpaths.iter().map(|path| rpath(&text(path))));
        } else if let Some(target) = target.and_then(|target| target.link.as_ref()) {
            for fragment in &target.fragments {
                let fragment_text = text(&fragment.text.value);
                if fragment.role == LinkFragmentRole::LibraryPath
                    && let Some(path) = fragment_text.strip_prefix("-L")
                {
                    libraries.push(path.into());
                    continue;
                }
                ldflags.push(self.fallback_library(&fragment_text, fragment.role));
            }
        }
        // Shared libraries linked by the fallback resolver still use @rpath install names.
        if ide.is_none()
            && let Some(target) = target
        {
            let rpaths: BTreeSet<_> = target
                .dependencies
                .iter()
                .filter_map(|id| {
                    let dep = model.target(*id)?;
                    (dep.kind == TargetKind::SharedLibrary && !dep.is_imported())
                        .then(|| Product::new(model, dep, None, config, bin).dir)
                })
                .collect();
            ldflags.extend(rpaths.iter().map(|path| rpath(path)));
        }
        let objects = self
            .link_objects()
            .map(|(_, path)| quote(&path))
            .collect::<Vec<_>>();
        if matches!(
            raw.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) {
            if !objects.is_empty() {
                settings.insert(
                    "OTHER_LIBTOOLFLAGS".into(),
                    objects
                        .into_iter()
                        .chain([INHERITED.into()])
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                );
            }
        } else {
            ldflags.splice(0..0, objects);
        }
        if let Some(target) = target {
            for unit in &target.compile_units {
                if let Some(group) = target.compile_groups.get(unit.group) {
                    let common = if let Some(ide) = ide {
                        ide.compile
                            .iter()
                            .find(|item| item.language == group.language)
                            .map(|item| item.flags.iter().collect())
                            .unwrap_or_default()
                    } else {
                        common_flags(target, &group.language)
                    };
                    let flags = group
                        .flags
                        .iter()
                        .filter(|flag| !common.iter().any(|base| base.value == flag.value))
                        .map(|flag| flag_text(&flag.value))
                        .collect::<Vec<_>>();
                    if !flags.is_empty() {
                        settings.insert(
                            source_flag_setting(&text(&unit.source.value)),
                            flags.join(" ").into(),
                        );
                    }
                }
            }
        }
        ldflags.push(INHERITED.into());
        settings.insert("OTHER_LDFLAGS".into(), ldflags.join(" ").into());
        settings.insert(
            "LIBRARY_SEARCH_PATHS".into(),
            list(
                libraries
                    .into_iter()
                    .map(|path| quote(&path))
                    .chain([INHERITED.into()]),
            ),
        );
        if let Some(standard) = raw
            .props
            .get(b"CXX_STANDARD")
            .filter(|value| !value.is_empty())
        {
            let dialect = if raw
                .props
                .get(b"CXX_EXTENSIONS")
                .is_some_and(|v| !truth::is_on(v))
            {
                "c++"
            } else {
                "gnu++"
            };
            settings.insert(
                "CLANG_CXX_LANGUAGE_STANDARD".into(),
                format!("{dialect}{}", text(standard)).into(),
            );
        }
        if product.kind == TargetKind::SharedLibrary {
            if let Some(version) = product.version.as_ref().or(product.soversion.as_ref()) {
                settings.insert("DYLIB_CURRENT_VERSION".into(), version.clone().into());
            }
            if let Some(version) = product.soversion.as_ref().or(product.version.as_ref()) {
                settings.insert("DYLIB_COMPATIBILITY_VERSION".into(), version.clone().into());
            }
            if let Some(version) = product.library_version() {
                let base = ide
                    .and_then(|facts| facts.install_name_dir.as_ref())
                    .map_or_else(|| "@rpath".into(), |path| text(path));
                let name = product.versioned_file(product.soversion.as_deref().unwrap_or(version));
                settings.insert(
                    "LD_DYLIB_INSTALL_NAME".into(),
                    format!("{}/{name}", base.trim_end_matches('/')).into(),
                );
            }
        }
        for key in raw.props.names() {
            if let Some(value) = raw.props.get(&key) {
                attribute(&mut settings, &key, value, "XCODE_ATTRIBUTE_", config);
            }
        }
        if let Some(ide) = ide {
            for (key, value) in &ide.attributes {
                attribute(&mut settings, key, value, "XCODE_ATTRIBUTE_", config);
            }
        }
        settings
    }

    fn fallback_library(&self, fragment: &str, role: LinkFragmentRole) -> String {
        if role == LinkFragmentRole::Libraries
            && let Some(target) = self.target
        {
            for &id in &target.dependencies {
                let Some(raw) = self.model.target(id).filter(|raw| !raw.is_imported()) else {
                    continue;
                };
                let Some(dep) = self
                    .result
                    .targets
                    .iter()
                    .find(|dep| dep.id == id && dep.config == target.config)
                else {
                    continue;
                };
                if let Some(path) = dep
                    .artifacts
                    .linker_file
                    .as_ref()
                    .or(dep.artifacts.file.as_ref())
                    && quote(&text(path)) == fragment
                {
                    return quote(
                        &Product::new(self.model, raw, Some(dep), self.config, self.bin)
                            .link_file(),
                    );
                }
            }
        }
        fragment.into()
    }

    pub(crate) fn link_objects(&self) -> impl Iterator<Item = (String, String)> {
        let target = self.target;
        let graph = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config.0.as_slice() == self.config.as_bytes());
        let inputs = graph.and_then(|graph| {
            let entry = graph
                .targets
                .iter()
                .find(|entry| entry.name == self.raw.name)?;
            entry
                .steps
                .iter()
                .filter_map(|index| graph.steps.get(*index))
                .find_map(|step| {
                    if let BuildStep::Link { inputs, .. } = step {
                        Some(inputs)
                    } else {
                        None
                    }
                })
        });
        inputs.into_iter().flatten().filter_map(move |input| {
            let target = target?;
            for &id in &target.dependencies {
                let Some(dep) = self
                    .result
                    .targets
                    .iter()
                    .find(|dep| dep.id == id && dep.config == target.config)
                else {
                    continue;
                };
                if dep.metadata.kind != TargetKind::ObjectLibrary {
                    continue;
                }
                if target.ide.as_ref().is_some_and(|ide| {
                    ide.sources.iter().any(|file| {
                        file.kind == rcmake_resolve::ide::IdeSourceKind::Object
                            && (file.path == *input
                                || self.bin.join(text(input)) == Path::new(&text(&file.path)))
                    })
                }) {
                    return None;
                }
                let Some(unit) = dep.compile_units.iter().find(|unit| unit.object == *input) else {
                    continue;
                };
                let stem = Path::new(&text(&unit.source.value))
                    .file_stem()?
                    .to_str()?
                    .to_owned();
                let name = text(&dep.metadata.name);
                let path = self.bin.join(format!(
                    "build/{name}.build/{CONFIG_DIR}/Objects-normal/$(CURRENT_ARCH)/{stem}.o"
                ));
                return Some((
                    format!("{name}/{stem}"),
                    path.to_string_lossy().into_owned(),
                ));
            }
            None
        })
    }
}

fn common_flags<'a>(
    target: &'a ResolvedTarget,
    language: &[u8],
) -> Vec<&'a rcmake_model::BT<rcmake_base::BString>> {
    let mut groups = target
        .compile_groups
        .iter()
        .filter(|group| group.language.as_slice() == language);
    let Some(group) = groups.next() else {
        return Vec::new();
    };
    group
        .flags
        .iter()
        .filter(|flag| {
            groups
                .clone()
                .all(|group| group.flags.iter().any(|other| other.value == flag.value))
        })
        .collect()
}

pub(crate) fn source_flag_setting(path: &str) -> String {
    format!(
        "RCMAKE_SOURCE_FLAGS_{}",
        crate::ids::generate(crate::object::Isa::PBXBuildFile, path)
    )
}

pub(crate) fn flag_text(value: &[u8]) -> String {
    value
        .strip_prefix(b"SHELL:")
        .map_or_else(|| quote(&text(value)), text)
}

pub(crate) fn strings(value: &[u8]) -> Vec<String> {
    cmake_list::split(value, cmake_list::Empty::Drop)
        .iter()
        .map(|part| text(part))
        .collect()
}

fn rpath(path: &str) -> String {
    format!("-Xlinker -rpath -Xlinker {}", quote(path))
}

fn link_file(path: &str) -> String {
    if path.ends_with(".framework")
        && let Some(name) = Path::new(path).file_name()
    {
        return quote(&format!(
            "{path}/{}",
            name.to_string_lossy().trim_end_matches(".framework")
        ));
    }
    quote(path)
}

fn unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}
