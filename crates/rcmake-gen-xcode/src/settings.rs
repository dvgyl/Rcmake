//! Project and target build settings for each configuration.

use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};

use rcmake_gen_common::GenError;

use rcmake_base::{BString, list as cmake_list, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_host::path::from_native_cmake;
use rcmake_model::{DirId, ParsedToolset, ProjectModel, Target, TargetKind};
use rcmake_resolve::{
    ResolvedTarget,
    ide::{BundleKind, IdeCompileSettings, IdeFacts, IdeLinkItem},
};

use crate::{
    object::{Attrs, Value, attrs, list},
    project::{
        XcodeIndices,
        link::{PhaseLink, apple_feature_flag, library_basename, object_input},
    },
    text,
};

pub(crate) const CONFIG_DIR: &str = "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)";
pub(crate) mod compile;

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
            dir = target_temp_dir(bin, &raw.name);
        } else if let Some(resolved_dir) = target
            .and_then(|target| target.artifacts.file.as_ref())
            .filter(|path| rcmake_genex::find_genex(path).is_none())
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
            bundle: rcmake_resolve::ide::bundle_kind(raw),
            kind,
            version: version(b"VERSION"),
            soversion: version(b"SOVERSION"),
            no_soname,
        }
    }

    pub(crate) fn file(&self) -> String {
        self.bundle.extension().map_or_else(
            || format!("{}{}{}", self.prefix, self.name, self.suffix),
            |extension| format!("{}.{}", self.name, text(extension)),
        )
    }

    pub(crate) fn types(&self) -> (&'static str, &'static str) {
        match self.bundle {
            BundleKind::App { .. } => ("application", "wrapper.application"),
            BundleKind::Framework { .. } => ("framework", "wrapper.framework"),
            BundleKind::Bundle { .. } => ("bundle", "wrapper.cfbundle"),
            BundleKind::None => match self.kind {
                TargetKind::Executable => ("tool", "compiled.mach-o.executable"),
                TargetKind::StaticLibrary | TargetKind::ObjectLibrary => {
                    ("library.static", "archive.ar")
                }
                // CMake builds a plain module as a tool linked with `-bundle`.
                TargetKind::ModuleLibrary => ("tool", "compiled.mach-o.executable"),
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

    pub(crate) fn version_links(&self) -> Vec<(String, String)> {
        let Some(version) = self.library_version() else {
            return Vec::new();
        };
        let actual = self.versioned_file(version);
        let soname = self.versioned_file(self.soversion.as_deref().unwrap_or(version));
        let mut links = Vec::new();
        if actual != soname {
            links.push((soname.clone(), actual));
        }
        links.push((self.file(), soname));
        links
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

fn target_temp_dir(bin: &Path, name: &[u8]) -> PathBuf {
    bin.join(format!("build/{}.build/{CONFIG_DIR}", text(name)))
}

pub(crate) fn project(
    model: &ProjectModel,
    root: DirId,
    bin: &Path,
    config: &str,
) -> Result<Attrs, GenError> {
    let mut settings = project_defaults(model, root, bin, config);
    let attributes = rcmake_resolve::ide::xcode_project_attributes(model, root, config.as_bytes())
        .map_err(|error| {
            let message = match error {
                rcmake_resolve::ResolveError::Genex(error) => error.without_location().to_string(),
                error => error.to_string(),
            };
            GenError::Invalid(message.into())
        })?;
    settings.extend(
        attributes
            .into_iter()
            .map(|(key, value)| (text(&key), attribute_value(&value))),
    );
    Ok(settings)
}

fn project_defaults(model: &ProjectModel, root: DirId, bin: &Path, config: &str) -> Attrs {
    let value = |key: &[u8]| {
        model
            .definition(root, key)
            .map_or_else(String::new, |v| text(v))
    };
    let archs = [
        value(b"CMAKE_OSX_ARCHITECTURES"),
        value(b"_CMAKE_APPLE_ARCHS_DEFAULT"),
    ]
    .into_iter()
    .find(|value| !value.is_empty())
    .unwrap_or_default();
    let sdk = value(b"CMAKE_OSX_SYSROOT");
    let system = value(b"CMAKE_SYSTEM_NAME");
    let mut settings = attrs([
        ("SDKROOT", sdk.clone().into()),
        (
            "SYMROOT",
            bin.join("build").to_string_lossy().into_owned().into(),
        ),
        (
            "ONLY_ACTIVE_ARCH",
            if archs.is_empty() { "YES" } else { "NO" }.into(),
        ),
    ]);
    if !archs.is_empty() {
        settings.insert("ARCHS".into(), archs.replace(';', " ").into());
    } else if system == "Darwin" && (sdk.is_empty() || sdk.to_ascii_lowercase().contains("macos")) {
        settings.insert("ARCHS".into(), "$(NATIVE_ARCH_ACTUAL)".into());
    }
    let deployment = value(b"CMAKE_OSX_DEPLOYMENT_TARGET");
    if !deployment.is_empty() {
        let platform = match value(b"CMAKE_SYSTEM_NAME").as_str() {
            "iOS" => "IPHONEOS",
            "tvOS" => "TVOS",
            "watchOS" => "WATCHOS",
            "visionOS" => "XROS",
            _ => "MACOSX",
        };
        settings.insert(format!("{platform}_DEPLOYMENT_TARGET"), deployment.into());
    }
    if config != "Debug"
        && model
            .definition(root, b"CMAKE_Swift_COMPILATION_MODE_DEFAULT")
            .is_none_or(|value| value.is_empty())
    {
        settings.insert("SWIFT_COMPILATION_MODE".into(), "wholemodule".into());
    }
    if model
        .global
        .props
        .get(b"ENABLED_LANGUAGES")
        .is_some_and(|langs| {
            cmake_list::split(langs, cmake_list::Empty::Drop)
                .iter()
                .any(|lang| lang == b"Swift")
        })
    {
        let swift_version = match model.definition(root, b"CMAKE_Swift_LANGUAGE_VERSION") {
            Some(version) => text(version),
            None => default_swift_version(model, root).into(),
        };
        settings.insert("SWIFT_VERSION".into(), swift_version.into());
    }
    if let ParsedToolset::Xcode {
        toolset: Some(toolset),
        ..
    } = &model.generator_spec.parsed
    {
        settings.insert("GCC_VERSION".into(), text(toolset).into());
    }
    settings
}

fn default_swift_version(model: &ProjectModel, root: DirId) -> &'static str {
    let Some(xcode) = model
        .definition(root, b"XCODE_VERSION")
        .filter(|value| !value.is_empty())
    else {
        return "4.0";
    };
    if rcmake_base::version::compare(xcode, b"10.2") != std::cmp::Ordering::Less {
        "4.0"
    } else if rcmake_base::version::compare(xcode, b"8.3") != std::cmp::Ordering::Less {
        "3.0"
    } else {
        "2.3"
    }
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
    out.insert(name, attribute_value(value));
}

fn attribute_value(value: &[u8]) -> Value {
    if value.starts_with(b"/") {
        Value::QuotedText(text(value))
    } else {
        text(value).into()
    }
}

pub(crate) struct TargetSettings<'a> {
    pub(crate) model: &'a ProjectModel,
    pub(crate) host: &'a dyn rcmake_host::Host,
    pub(crate) indices: &'a XcodeIndices<'a>,
    pub(crate) bin: &'a Path,
    pub(crate) config: &'a str,
    pub(crate) raw: &'a Target,
    pub(crate) target: Option<&'a ResolvedTarget>,
    pub(crate) xcframeworks: &'a BTreeSet<BString>,
    pub(crate) per_language_flags: bool,
    pub(crate) phase_links: &'a [PhaseLink],
}

impl TargetSettings<'_> {
    pub(crate) fn build(&self, product: &Product, plist: Option<&Path>) -> Result<Attrs, GenError> {
        let mut settings = project_defaults(self.model, self.raw.dir, self.bin, self.config);
        let ide = self.target.and_then(|target| target.ide.as_deref());
        let arch_key = format!("OSX_ARCHITECTURES_{}", self.config.to_ascii_uppercase());
        if let Some(arches) = self
            .raw
            .props
            .get(arch_key.as_bytes())
            .or_else(|| self.raw.props.get(b"OSX_ARCHITECTURES"))
            .filter(|arches| !arches.is_empty())
        {
            settings.insert("ARCHS".into(), text(arches).replace(';', " ").into());
            settings.insert("ONLY_ACTIVE_ARCH".into(), "NO".into());
        }
        self.add_product_settings(&mut settings, product, plist);
        self.add_compile_settings(&mut settings, ide)?;
        if let Some(target) = self.target
            && let Some(fortran) = &target.fortran
        {
            let groups = target
                .compile_groups
                .iter()
                .filter(|group| group.language == "Fortran");
            let flags = ide
                .into_iter()
                .flat_map(|ide| &ide.compile)
                .filter(|group| group.language == "Fortran")
                .flat_map(|group| group.flags.iter().map(|flag| flag_text(&flag.value)))
                .collect::<Vec<_>>()
                .join(" ");
            let includes = fortran
                .module_dir
                .iter()
                .cloned()
                .chain(groups.flat_map(|group| {
                    group
                        .includes
                        .iter()
                        .map(|include| include.path.value.clone())
                }))
                .collect::<Vec<_>>();
            crate::fortran_asm::add_fortran_settings(
                &mut settings,
                &includes,
                &flags,
                self.raw
                    .props
                    .get(b"Fortran_FORMAT")
                    .map(|format| format.as_slice()),
            );
        }
        self.add_link_settings(&mut settings, product, ide);
        if let Some(ide) = ide {
            for (key, value) in &ide.attributes {
                attribute(&mut settings, key, value, "XCODE_ATTRIBUTE_", self.config);
            }
        }
        Ok(settings)
    }

    fn add_product_settings(&self, settings: &mut Attrs, product: &Product, plist: Option<&Path>) {
        let Self {
            bin, config, raw, ..
        } = *self;
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
            ("GCC_OPTIMIZATION_LEVEL", "0".into()),
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
                target_temp_dir(bin, &raw.name)
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            ),
            ("USE_HEADERMAP", "NO".into()),
            ("WARNING_CFLAGS", list([INHERITED])),
            ("SKIP_INSTALL", "YES".into()),
        ]));
        if !matches!(
            product.bundle,
            BundleKind::App { .. } | BundleKind::Framework { .. }
        ) {
            settings.extend(attrs([
                ("EXECUTABLE_PREFIX", product.prefix.clone().into()),
                ("EXECUTABLE_SUFFIX", product.suffix.clone().into()),
            ]));
        }
        if let Some(plist) = plist {
            settings.insert(
                "INFOPLIST_FILE".into(),
                from_native_cmake(plist).to_string().into(),
            );
            settings.insert("CODE_SIGNING_ALLOWED".into(), "NO".into());
        }
        if let Some(extension) = product.bundle.extension() {
            settings.insert("WRAPPER_EXTENSION".into(), text(extension).into());
        }
        match &product.bundle {
            BundleKind::Framework { version, .. } => {
                settings.insert("FRAMEWORK_VERSION".into(), text(version).into());
                if product.kind == TargetKind::StaticLibrary {
                    settings.insert("MACH_O_TYPE".into(), "staticlib".into());
                }
            }
            BundleKind::Bundle { .. } => {
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
                } else if product.kind == TargetKind::ModuleLibrary {
                    "BUNDLE"
                } else {
                    "DYNAMIC"
                }
                .into(),
            );
        }
        if product.kind == TargetKind::ModuleLibrary {
            settings.insert("MACH_O_TYPE".into(), "mh_bundle".into());
        }
    }

    fn add_link_settings(&self, settings: &mut Attrs, product: &Product, ide: Option<&IdeFacts>) {
        let Self {
            model,
            indices,
            bin,
            config,
            raw,
            target,
            ..
        } = *self;
        let mut ldflags = Vec::new();
        let mut libraries = UniqueList::default();
        if let Some(ide) = ide {
            let use_config_directory =
                raw.policies.policies.get(PolicyId::new(142)) != PolicyStatus::New;
            for directory in &ide.link_directories {
                let directory = text(directory);
                if use_config_directory {
                    libraries.push_raw(format!("{}/{CONFIG_DIR}", directory.trim_end_matches('/')));
                }
                libraries.push_raw(directory);
            }
            if raw.kind != TargetKind::StaticLibrary {
                ldflags.extend(
                    ide.link_options
                        .iter()
                        .map(|option| flag_text(&option.value)),
                );
            }
            let rendered = self.rendered_link_flags(ide);
            let use_legacy = rendered.is_none();
            ldflags.extend(rendered.into_iter().flatten());
            for item in ide.link_items.iter().filter(|_| use_legacy) {
                if self.phase_links.iter().any(|link| &link.item == item) {
                    continue;
                }
                match item {
                    IdeLinkItem::Target(id) => {
                        if let Some(dep) = model.target(*id) {
                            let resolved = indices.resolved.get(&(*id, config.as_bytes())).copied();
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
                                let product = Product::new(model, dep, resolved, config, bin);
                                let path = product.link_file();
                                ldflags.push(
                                    apple_feature_flag(target, &path)
                                        .unwrap_or_else(|| xcode_flag(&path)),
                                );
                            }
                        }
                    }
                    IdeLinkItem::Framework(name) => {
                        let name = text(name);
                        ldflags.push(apple_feature_flag(target, &name).unwrap_or_else(|| {
                            format!(
                                "-framework {}",
                                xcode_flag(library_basename(&name).unwrap_or(&name))
                            )
                        }));
                    }
                    IdeLinkItem::Library(name) => {
                        let name = text(name);
                        ldflags.push(
                            apple_feature_flag(target, &name)
                                .unwrap_or_else(|| xcode_flag(&format!("-l{name}"))),
                        );
                    }
                    IdeLinkItem::File(path) => {
                        let in_frameworks_phase = self
                            .xcframeworks
                            .iter()
                            .any(|root| ide.xcframeworks.get(root) == Some(path));
                        if !in_frameworks_phase
                            && !ide.sources.iter().any(|source| {
                                source.path == *path
                                    && matches!(
                                        source.kind,
                                        rcmake_resolve::ide::IdeSourceKind::Object
                                            | rcmake_resolve::ide::IdeSourceKind::ExternalObject
                                    )
                            })
                        {
                            let path = text(path);
                            ldflags.push(
                                apple_feature_flag(target, &path)
                                    .unwrap_or_else(|| link_file(&path)),
                            );
                        }
                    }
                    IdeLinkItem::Flag(flag) => ldflags.push(flag_text(flag)),
                    IdeLinkItem::XcodeFeature(_) | IdeLinkItem::VisualStudioFeature { .. } => {}
                }
            }
            if let Some(dir) = &ide.install_name_dir {
                settings.insert("DYLIB_INSTALL_NAME_BASE".into(), text(dir).into());
            }
            ldflags.extend(ide.build_rpaths.iter().map(|path| rpath(&text(path))));
        }
        let source_objects = ide
            .into_iter()
            .flat_map(|ide| &ide.sources)
            .filter(|source| source.kind == rcmake_resolve::ide::IdeSourceKind::Object)
            .map(|source| object_input(&text(&source.path), self.config));
        let objects = source_objects.chain(self.link_objects());
        if matches!(
            raw.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) {
            let mut flags = ide
                .filter(|_| raw.kind == TargetKind::StaticLibrary)
                .into_iter()
                .flat_map(|ide| &ide.link_options)
                .map(|option| flag_text(&option.value))
                .chain(objects.map(|path| xcode_escape_path(&path)))
                .map(Value::from)
                .collect::<Vec<_>>();
            if flags.is_empty() {
                flags.push("".into());
            }
            settings.insert("OTHER_LIBTOOLFLAGS".into(), Value::List(flags));
        } else {
            ldflags.splice(0..0, objects.map(|path| xcode_flag(&path)));
        }
        // Xcode passes OTHER_LDFLAGS to libtool, which cannot read link libraries.
        if !matches!(
            raw.kind,
            TargetKind::StaticLibrary | TargetKind::ObjectLibrary
        ) {
            settings.insert("OTHER_LDFLAGS".into(), flags_with_inherited(&ldflags));
        }
        if raw.kind != TargetKind::StaticLibrary {
            for dir in self.phase_search_paths(false) {
                libraries.push_unique(dir);
            }
        }
        let libraries = libraries.into_vec();
        if !libraries.is_empty() {
            settings.insert("LIBRARY_SEARCH_PATHS".into(), inherited_paths(libraries));
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
    }
}

pub(crate) fn source_flags(compile: &IdeCompileSettings) -> String {
    compile
        .flags
        .iter()
        .map(|flag| flag_text(&flag.value))
        .chain(compile.defines.iter().map(|define| {
            let define = text(&define.value);
            xcode_flag(&format!(
                "-D{}",
                define.strip_prefix("-D").unwrap_or(&define)
            ))
        }))
        .chain(compile.includes.iter().map(|include| {
            let prefix = if include.framework { "-F" } else { "-I" };
            xcode_flag(&format!("{prefix}{}", text(&include.path)))
        }))
        .filter(|flag| !flag.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn xcode_flag(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "'\\''");
    if value.contains(|ch: char| "`~!@#$%^&*()+={}[]|:;\"'<>,.?".contains(ch) || ch.is_whitespace())
    {
        format!("'{escaped}'")
    } else {
        escaped
    }
}

pub(crate) fn flag_text(value: &[u8]) -> String {
    value
        .strip_prefix(b"SHELL:")
        .map_or_else(|| xcode_flag(&text(value)), text)
}

pub(crate) fn strings(value: &[u8]) -> Vec<String> {
    cmake_list::split(value, cmake_list::Empty::Drop)
        .iter()
        .map(|part| text(part))
        .collect()
}

fn rpath(path: &str) -> String {
    format!("-Xlinker -rpath -Xlinker {}", xcode_flag(path))
}

fn link_file(path: &str) -> String {
    if path.ends_with(".framework")
        && let Some(name) = Path::new(path).file_name()
    {
        return xcode_flag(&format!(
            "{path}/{}",
            name.to_string_lossy().trim_end_matches(".framework")
        ));
    }
    xcode_flag(path)
}

fn inherited_paths(values: Vec<String>) -> Value {
    inherited_values(
        values
            .into_iter()
            .map(|path| xcode_escape_path(&path))
            .collect(),
    )
}

fn xcode_escape_path(path: &str) -> String {
    if path.contains([' ', '[', ']']) {
        format!(r#""{path}""#)
    } else {
        path.into()
    }
}

fn inherited_values(values: Vec<String>) -> Value {
    list(
        values
            .into_iter()
            .chain([INHERITED.into()])
            .map(Value::QuotedText),
    )
}

fn flags_with_inherited(flags: &[String]) -> Value {
    list([
        Value::QuotedText(flags.join(" ")),
        Value::QuotedText(INHERITED.into()),
    ])
}

fn framework_search_path(path: &[u8]) -> Option<String> {
    let path = rcmake_base::path_lex::Lexical::new(path);
    if !path.absolute() {
        return None;
    }
    let mut candidate = path.relative_part();
    let mut wrapper = path;
    loop {
        if wrapper.filename().ends_with(b".framework") {
            return Some(text(wrapper.parent()));
        }
        let parent = wrapper.parent();
        if parent.is_empty() || parent == candidate {
            return None;
        }
        candidate = parent;
        wrapper = rcmake_base::path_lex::Lexical::new(parent);
    }
}

#[derive(Default)]
struct UniqueList {
    items: Vec<String>,
    seen: HashSet<String>,
}

impl UniqueList {
    fn push_raw(&mut self, value: String) {
        self.seen.insert(value.clone());
        self.items.push(value);
    }

    fn push_unique(&mut self, value: String) {
        if self.seen.insert(value.clone()) {
            self.items.push(value);
        }
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn into_vec(self) -> Vec<String> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::xcode_flag;

    #[test]
    fn escapes_xcode_flag_quotes_and_backslashes() {
        for (input, escaped) in [
            (r"a\b", r"a\\b"),
            (r"a\", r"a\\"),
            (r"C:\dir\", r"'C:\\dir\\'"),
            ("it's", r"'it'\''s'"),
            ("x=y", "'x=y'"),
            (r#"KEY="\""#, r#"'KEY="\\"'"#),
            ("", ""),
        ] {
            assert_eq!(xcode_flag(input), escaped);
        }
    }
}
