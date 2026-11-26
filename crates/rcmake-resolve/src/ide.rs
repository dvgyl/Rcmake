//! IDE facts: structured, unescaped per-target values for the IDE generators
//! (Xcode and Visual Studio). This is an extension of layer 2 (ADR-009, ADR-014,
//! ADR-015).
//!
//! The Ninja and Makefile generators use the lowered, shell-escaped
//! [`crate::BuildGraph`]. The IDE generators need raw values instead: raw flags, raw
//! link items, bundle data, and per-target attributes. Each IDE generator escapes the
//! values for its own file format.
//!
//! The resolver fills [`crate::ResolvedTarget::ide`] only when
//! [`crate::ResolveOptions::ide`] is set.

use indexmap::{IndexMap, IndexSet};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::Host;
use rcmake_model::{
    BT, DirId, Directory, FileSetKind, GeneratorKind, ProjectModel, Target, TargetId,
};

use crate::{
    CompileGroup, InterfaceInclude, ResolveError, ResolvedTarget,
    cxx_modules::module_error,
    lower,
    sources::{
        SourcePaths, TargetInputs, absolute_path, append_compile_flags, external_object, properties,
    },
    target::FileSetMember,
    usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver, handle},
};

/// The IDE file format that the facts are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdeFlavor {
    /// Xcode (`project.pbxproj`).
    Xcode,
    /// Visual Studio (`.vcxproj`, `.sln`, `.slnx`).
    VisualStudio,
}

/// IDE facts of one target and one configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdeFacts {
    /// Output directory and file name parts.
    pub naming: OutputNaming,
    /// Apple bundle form of the target.
    pub bundle: BundleKind,
    /// Absolute path of the generated `Info.plist`, if the target has one.
    pub info_plist: Option<BString>,
    /// Every source, header, resource, and object file of the target.
    pub sources: Vec<IdeSource>,
    /// Compile settings, one entry per language.
    pub compile: Vec<IdeCompileSettings>,
    /// Link items in link order.
    pub link_items: Vec<IdeLinkItem>,
    /// XCFramework wrappers and their selected library paths.
    pub xcframeworks: IndexMap<BString, BString>,
    /// Raw link options (not shell-escaped).
    pub link_options: Vec<BT<BString>>,
    /// Link directories.
    pub link_directories: Vec<BString>,
    /// Run-time search paths of the build tree.
    pub build_rpaths: Vec<BString>,
    /// `INSTALL_NAME_DIR` of the build tree (Apple).
    pub install_name_dir: Option<BString>,
    /// Custom commands and build events of the target.
    pub custom: Vec<IdeCustomStep>,
    /// Target properties for this configuration, with their original prefixes.
    /// Xcode attribute keys keep their `[sdk=...]` and `[arch=...]` filters.
    pub attributes: IndexMap<BString, BString>,
    /// Error body for an Xcode embed property that depends on the configuration.
    pub xcode_config_sensitive_embed: Option<BString>,
    /// Precompiled headers, one entry per language.
    pub pch: Vec<IdePch>,
}

/// Output directory and file name parts of a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputNaming {
    /// Absolute output directory.
    pub dir: BString,
    /// File name prefix (for example `lib`).
    pub prefix: BString,
    /// Base name (`OUTPUT_NAME` or the target name).
    pub base_name: BString,
    /// File name suffix (for example `.dylib`).
    pub suffix: BString,
    /// `VERSION` property.
    pub version: Option<BString>,
    /// `SOVERSION` property.
    pub soversion: Option<BString>,
}

pub use crate::lower::framework::{BundleKind, bundle_kind, platform_bundle_kind};

/// The role of one file in an IDE project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdeSourceKind {
    /// A source file that is compiled.
    Compile {
        /// Source language (for example `CXX`).
        language: BString,
    },
    /// A header file.
    Header,
    /// A resource (copied into a bundle).
    Resource,
    /// An object file made by another target (`$<TARGET_OBJECTS>`).
    Object,
    /// An object file from outside the project (`EXTERNAL_OBJECT`).
    ExternalObject,
    /// A `CMakeLists.txt` or other listfile.
    ListFile,
    /// Any other file.
    Other,
}

/// One file of an IDE project.
#[derive(Clone, Debug, PartialEq)]
pub struct IdeSource {
    /// Absolute path.
    pub path: BString,
    /// Role of the file.
    pub kind: IdeSourceKind,
    /// `source_group()` path, with `\` separators (empty for the default group).
    pub group: BString,
    /// The file is the output of a custom command.
    pub generated: bool,
    /// Visual Studio: configurations that do not build the file (`ExcludedFromBuild`).
    pub excluded_configs: Vec<BString>,
    /// Name of the file set that contains the source, if any.
    pub file_set: Option<BString>,
    /// Bundle subdirectory from `MACOSX_PACKAGE_LOCATION`.
    pub package_location: Option<BString>,
    /// Compile the source without the target precompiled header.
    pub skip_precompile_headers: bool,
    /// Compile settings that apply to this source only.
    pub compile: Option<IdeCompileSettings>,
}

/// Compile settings of one language.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdeCompileSettings {
    /// Language (for example `C` or `CXX`).
    pub language: BString,
    /// Raw compile flags (not shell-escaped).
    pub flags: Vec<BT<BString>>,
    /// Preprocessor definitions (`NAME` or `NAME=value`).
    pub defines: Vec<BT<BString>>,
    /// Include directories.
    pub includes: Vec<IdeInclude>,
}

/// One include directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeInclude {
    /// Absolute path.
    pub path: BString,
    /// The directory is a system include directory.
    pub system: bool,
    /// The directory is a framework search path (Apple `-F`).
    pub framework: bool,
}

/// One link item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdeLinkItem {
    /// A target of this project.
    Target(TargetId),
    /// A library file, by absolute path.
    File(BString),
    /// An Apple framework, by name or path.
    Framework(BString),
    /// A library name that the linker finds (`-l<name>`).
    Library(BString),
    /// A raw linker flag.
    Flag(BString),
    /// The resolved `LINK_LIBRARY` feature of the next Xcode link item.
    XcodeFeature(BString),
    /// Visual Studio link item with its resolved link-library feature format.
    VisualStudioFeature {
        /// Library or project reference.
        item: Box<IdeLinkItem>,
        /// Format with a library placeholder.
        template: BString,
    },
}

/// When a custom step runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdeCustomPhase {
    /// A custom command that makes output files (`add_custom_command(OUTPUT)`).
    Rule,
    /// `PRE_BUILD` build event.
    PreBuild,
    /// `PRE_LINK` build event.
    PreLink,
    /// `POST_BUILD` build event.
    PostBuild,
}

/// One custom command or build event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeCustomStep {
    /// When the step runs.
    pub phase: IdeCustomPhase,
    /// Raw argument vectors, one per `COMMAND`.
    pub commands: Vec<Vec<BString>>,
    /// Absolute working directory.
    pub working_dir: BString,
    /// Absolute output paths.
    pub outputs: Vec<BString>,
    /// Absolute byproduct paths.
    pub byproducts: Vec<BString>,
    /// Absolute dependency paths.
    pub depends: Vec<BString>,
    /// `COMMENT` text.
    pub comment: Option<BString>,
    /// The step must run on every build.
    pub always_run: bool,
}

/// A precompiled header of one language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdePch {
    /// Language.
    pub language: BString,
    /// Absolute path of the generated header that the sources include.
    pub header: BString,
    /// Absolute path of the source that makes the precompiled header.
    pub source: BString,
    /// Absolute path of the precompiled header output.
    pub artifact: BString,
    /// Absolute path of the companion object, when the compiler creates one.
    pub companion: Option<BString>,
    /// Absolute path of the compiler program database, if configured.
    pub compile_pdb: Option<BString>,
    /// Target whose precompiled header this target uses (`REUSE_FROM`).
    pub reuse_from: Option<TargetId>,
}

impl IdeFlavor {
    pub(crate) fn from_model(model: &ProjectModel) -> Option<Self> {
        match model.generator {
            Some(GeneratorKind::Xcode) => Some(Self::Xcode),
            Some(GeneratorKind::VisualStudio(_)) => Some(Self::VisualStudio),
            _ => None,
        }
    }
}

pub(crate) fn object_directory(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    flavor: IdeFlavor,
) -> BString {
    let binary = &model.dir(target.dir).binary_dir;
    match flavor {
        IdeFlavor::Xcode => {
            let build = absolute_path(
                &model.dir(DirId::ROOT).binary_dir,
                &[b"build/", target.name.as_slice(), b".build"].concat(),
            );
            let arch = xcode_object_arch(model, target);
            let config = absolute_path(&build, &config.0);
            absolute_path(&config, &[b"Objects-normal/", arch].concat())
        }
        IdeFlavor::VisualStudio => {
            if lower::short_object_paths(model) {
                let root = &model.dir(DirId::ROOT).binary_dir;
                let support =
                    absolute_path(root, &lower::artifacts::short_support_dir(model, target));
                absolute_path(&support, &config.0)
            } else {
                let dir = absolute_path(binary, &[target.name.as_slice(), b".dir"].concat());
                absolute_path(&dir, &config.0)
            }
        }
    }
}

fn xcode_object_arch<'a>(model: &'a ProjectModel, target: &'a Target) -> &'a [u8] {
    const CURRENT: &[u8] = b"$(CURRENT_ARCH)";
    if let Some(architectures) = target.props.get(b"OSX_ARCHITECTURES") {
        let mut archs = architectures
            .split(|&byte| byte == b';')
            .filter(|arch| !arch.is_empty());
        if let Some(arch) = archs.next() {
            return if archs.next().is_some() {
                CURRENT
            } else {
                arch
            };
        }
    }
    let definition = |key| lower::definition(model, DirId::ROOT, key);
    let embedded_sdk = || {
        definition(b"CMAKE_OSX_SYSROOT").is_some_and(|sdk| {
            let sdk = sdk.to_ascii_lowercase();
            sdk.split(|&byte| byte == b'/').any(|part| {
                [
                    b"appletvos".as_slice(),
                    b"appletvsimulator",
                    b"iphoneos",
                    b"iphonesimulator",
                    b"watchos",
                    b"watchsimulator",
                    b"xros",
                    b"xrsimulator",
                ]
                .iter()
                .any(|prefix| part.starts_with(prefix))
            })
        })
    };
    if model
        .global
        .props
        .get(b"XCODE_EMIT_EFFECTIVE_PLATFORM_NAME")
        .map_or_else(embedded_sdk, |value| truth::is_on(value))
    {
        return CURRENT;
    }
    for key in [
        b"CMAKE_OSX_ARCHITECTURES".as_slice(),
        b"_CMAKE_APPLE_ARCHS_DEFAULT",
    ] {
        if let Some(value) = definition(key) {
            let mut archs = value
                .split(|&byte| byte == b';')
                .filter(|arch| !arch.is_empty());
            if let Some(arch) = archs.next() {
                return if archs.next().is_some() {
                    CURRENT
                } else {
                    arch
                };
            }
        }
    }
    definition(b"CMAKE_XCODE_ARCHS")
        .and_then(|value| value.split(|&byte| byte == b';').next())
        .filter(|arch| !arch.is_empty())
        .unwrap_or(CURRENT)
}

pub(crate) fn object_path(
    model: &ProjectModel,
    target: &Target,
    config: &ConfigId,
    source: &[u8],
    flavor: IdeFlavor,
) -> BString {
    let (name, _) = Lexical::new(source).stem_extension(true);
    let suffix = match flavor {
        IdeFlavor::Xcode => b".o".as_slice(),
        IdeFlavor::VisualStudio => b".obj",
    };
    absolute_path(
        &object_directory(model, target, config, flavor),
        &[name, suffix].concat(),
    )
}

impl IdeFacts {
    pub(crate) fn collect_properties(
        &mut self,
        usage: &mut UsageResolver<'_>,
        target: &Target,
        id: TargetId,
        ctx: &EvalContext,
        flavor: IdeFlavor,
    ) -> Result<(), ResolveError> {
        let ctx = EvalContext {
            head: Some(handle(id)),
            ..ctx.clone()
        };
        let mut selected = IndexMap::new();
        for property in target
            .props
            .names()
            .into_iter()
            .filter(|name| is_ide_property(name, flavor))
        {
            let (key, specific) = if property.starts_with(b"XCODE_ATTRIBUTE_") {
                let Some(filter) = xcode_attribute_name(&property, &ctx.config.0) else {
                    continue;
                };
                filter
            } else {
                (property.clone(), false)
            };
            if specific || !selected.contains_key(&key) {
                selected.insert(key, property);
            }
        }
        for (key, property) in selected {
            if let Some(value) = target.props.get(&property) {
                let (evaluated, flags) =
                    usage.eval_ide_property_with_flags(id, &property, value, &ctx)?;
                if flavor == IdeFlavor::Xcode
                    && flags.context_sensitive
                    && matches!(
                        property.as_slice(),
                        b"XCODE_EMBED_FRAMEWORKS"
                            | b"XCODE_EMBED_PLUGINS"
                            | b"XCODE_EMBED_APP_EXTENSIONS"
                            | b"XCODE_EMBED_EXTENSIONKIT_EXTENSIONS"
                            | b"XCODE_EMBED_XPC_SERVICES"
                            | b"XCODE_EMBED_RESOURCES"
                    )
                {
                    let name = &target.name;
                    self.xcode_config_sensitive_embed.get_or_insert_with(|| {
                        format!(
                            "Xcode does not support per-config {property}:\n  {value}\nspecified for target:\n  {name}"
                        ).into()
                    });
                }
                self.attributes.insert(key, evaluated);
            }
        }
        Ok(())
    }
}

fn is_ide_property(name: &[u8], flavor: IdeFlavor) -> bool {
    matches!(
        name,
        b"FOLDER" | b"PUBLIC_HEADER" | b"PRIVATE_HEADER" | b"RESOURCE"
    ) || match flavor {
        IdeFlavor::Xcode => {
            name.starts_with(b"XCODE_")
                || name.starts_with(b"MACOSX_BUNDLE")
                || matches!(
                    name,
                    b"FRAMEWORK"
                        | b"FRAMEWORK_VERSION"
                        | b"BUNDLE"
                        | b"BUNDLE_EXTENSION"
                        | b"INFO_PLIST"
                        | b"MACOSX_FRAMEWORK_INFO_PLIST"
                )
        }
        IdeFlavor::VisualStudio => name.starts_with(b"VS_"),
    }
}

/// Configuration-file paths for one Xcode build configuration.
pub struct XcodeConfigFiles<'a>(SourcePaths<'a>);

impl<'a> XcodeConfigFiles<'a> {
    /// Index generated files for the specified configuration.
    ///
    /// # Errors
    /// Returns an error if a generated output path cannot be evaluated.
    pub fn new(
        model: &ProjectModel,
        host: &'a dyn Host,
        config: &[u8],
    ) -> Result<Self, ResolveError> {
        let mut paths = SourcePaths::new(host);
        paths.set_ide(Some(IdeFlavor::Xcode));
        paths.index_generated(model, &ConfigId(config.into()))?;
        Ok(Self(paths))
    }

    /// Find an existing or generated configuration file in the source or build directory.
    #[must_use]
    pub fn find(&self, directory: &Directory, path: &[u8]) -> Option<BString> {
        self.0.find(directory, path)
    }
}

/// Evaluate the project-wide Xcode build settings for one configuration.
///
/// # Errors
/// Returns an error if a build setting contains an invalid generator expression.
pub fn xcode_project_attributes(
    model: &ProjectModel,
    dir: DirId,
    config: &[u8],
) -> Result<IndexMap<BString, BString>, ResolveError> {
    const PREFIX: &[u8] = b"CMAKE_XCODE_ATTRIBUTE_";
    let mut definitions = IndexMap::new();
    definitions.extend(model.cache.iter().map(|(key, entry)| (key, &entry.value)));
    definitions.extend(model.dir(dir).vars.iter());
    let mut selected = IndexMap::new();
    for (key, value) in definitions {
        let Some(name) = key.strip_prefix(PREFIX) else {
            continue;
        };
        if matches!(name, b"knownRegions" | b"developmentRegion") {
            continue;
        }
        if let Some((name, specific)) = xcode_attribute_name(name, config)
            && (specific || !selected.contains_key(&name))
        {
            selected.insert(name, value);
        }
    }
    let ctx = EvalContext {
        config: ConfigId(BString::from(config)),
        ..EvalContext::default()
    };
    let mut usage = UsageResolver::new(model).with_ide(Some(IdeFlavor::Xcode));
    let mut host = usage.host(dir);
    selected
        .into_iter()
        .map(|(name, value)| {
            let ast = rcmake_genex::parse(value)?;
            let (value, _) = rcmake_genex::evaluate(&ast, &ctx, &mut host)?;
            Ok((name, value))
        })
        .collect()
}

fn xcode_attribute_name(name: &[u8], config: &[u8]) -> Option<(BString, bool)> {
    let mut sections = name.split_inclusive(|&byte| byte == b']');
    let mut key = BString::default();
    let mut specific = false;
    for section in &mut sections {
        let Some(open) = section.iter().position(|&byte| byte == b'[') else {
            key.extend_from_slice(section);
            continue;
        };
        let (prefix, filter) = section.split_at(open);
        key.extend_from_slice(prefix);
        if let Some(variant) = filter
            .strip_prefix(b"[variant=")
            .and_then(|v| v.strip_suffix(b"]"))
        {
            if variant != config {
                return None;
            }
            specific = true;
        } else {
            key.extend_from_slice(filter);
        }
    }
    Some((key, specific))
}

/// Collects IDE compile settings by language and compile group.
pub(crate) struct IdeCompile {
    flavor: IdeFlavor,
    base: IndexMap<BString, IdeCompileSettings>,
    overrides: IndexMap<usize, IdeCompileSettings>,
}

impl IdeCompile {
    pub(crate) fn new(flavor: IdeFlavor) -> Self {
        Self {
            flavor,
            base: IndexMap::new(),
            overrides: IndexMap::new(),
        }
    }

    pub(crate) fn settings(&self, group: &CompileGroup) -> IdeCompileSettings {
        compile_settings(group, self.flavor)
    }

    pub(crate) fn record_group(
        &mut self,
        group: usize,
        base: IdeCompileSettings,
        compiled: &CompileGroup,
    ) {
        let mut overrides = self.settings(compiled);
        if overrides.flags.starts_with(&base.flags) {
            overrides.flags.drain(..base.flags.len());
        }
        if overrides.defines.starts_with(&base.defines) {
            overrides.defines.drain(..base.defines.len());
        }
        overrides
            .includes
            .retain(|include| !base.includes.contains(include));
        if !overrides.flags.is_empty()
            || !overrides.defines.is_empty()
            || !overrides.includes.is_empty()
        {
            self.overrides.insert(group, overrides);
        }
        self.base.entry(base.language.clone()).or_insert(base);
    }

    pub(crate) fn finish(
        self,
        resolved: &mut ResolvedTarget,
        model: &ProjectModel,
        target: &Target,
        paths: &SourcePaths<'_>,
        inputs: &TargetInputs,
        members: &IndexMap<BString, FileSetMember<'_>>,
    ) -> Result<(), ResolveError> {
        if let Some(facts) = &mut resolved.ide {
            for mut base in self.base.into_values() {
                let mut flags = Vec::new();
                for value in lower::language_flags(model, target, &resolved.config, &base.language)
                {
                    append_compile_flags(&mut flags, &value, Backtrace::default());
                }
                if let Some(standard) = lower::checked_standard_flag(
                    model,
                    target,
                    &base.language,
                    inputs.standard_requirements.get(&base.language).copied(),
                )? {
                    append_compile_flags(&mut flags, &standard, Backtrace::default());
                }
                if self.flavor == IdeFlavor::VisualStudio {
                    flags.extend(
                        lower::msvc_debug_options(model, target, &resolved.config, &base.language)?
                            .into_iter()
                            .map(|option| BT::new(option, Backtrace::default())),
                    );
                }
                flags.append(&mut base.flags);
                base.flags = flags;
                facts.compile.push(base);
            }
            let compiled = resolved
                .compile_units
                .iter()
                .filter_map(|unit| {
                    let group = resolved.compile_groups.get(unit.group)?;
                    Some((
                        unit.source.value.clone(),
                        (
                            group.language.clone(),
                            self.overrides.get(&unit.group).cloned(),
                        ),
                    ))
                })
                .collect();
            collect_sources(
                facts,
                model.dir(target.dir),
                paths,
                inputs,
                members,
                compiled,
            );
        }
        Ok(())
    }
}

fn compile_settings(group: &CompileGroup, flavor: IdeFlavor) -> IdeCompileSettings {
    IdeCompileSettings {
        language: group.language.clone(),
        flags: group.flags.clone(),
        defines: group.defines.clone(),
        includes: group
            .includes
            .iter()
            .map(|entry| include(entry, flavor))
            .collect(),
    }
}

pub(crate) fn include(entry: &InterfaceInclude, flavor: IdeFlavor) -> IdeInclude {
    let path = &entry.path.value;
    let wrapper = Lexical::new(lower::without_trailing_slashes(path));
    let framework = (flavor == IdeFlavor::Xcode && wrapper.filename().ends_with(b".framework"))
        .then(|| BString::from(wrapper.parent()));
    IdeInclude {
        framework: framework.is_some(),
        path: framework.unwrap_or_else(|| path.clone()),
        system: entry.system,
    }
}

fn collect_sources(
    facts: &mut IdeFacts,
    dir: &rcmake_model::Directory,
    paths: &SourcePaths<'_>,
    inputs: &TargetInputs,
    members: &IndexMap<BString, FileSetMember<'_>>,
    mut compiled: IndexMap<BString, (BString, Option<IdeCompileSettings>)>,
) {
    let property_files = |name: &[u8]| -> IndexSet<BString> {
        facts
            .attributes
            .get(name)
            .into_iter()
            .flat_map(|value| list::split(value, list::Empty::Drop))
            .map(|path| {
                paths
                    .find(dir, &path)
                    .unwrap_or_else(|| absolute_path(&dir.source_dir, &path))
            })
            .collect()
    };
    let public_headers = property_files(b"PUBLIC_HEADER");
    let private_headers = property_files(b"PRIVATE_HEADER");
    let resources = property_files(b"RESOURCE");
    let sources: IndexSet<_> = inputs
        .sources
        .iter()
        .chain(&public_headers)
        .chain(&private_headers)
        .chain(&resources)
        .cloned()
        .collect();
    facts.sources = sources
        .into_iter()
        .map(|path| {
            let props = properties(dir, &path);
            let member = members.get(&path).map(|entry| entry.set);
            let package_location = props.get(b"MACOSX_PACKAGE_LOCATION").cloned();
            let file = Lexical::new(&path);
            let generated = paths.is_generated(dir, &path)
                || props
                    .get(b"GENERATED")
                    .is_some_and(|value| truth::is_on(value))
                || paths.object_owner(&path).is_some();
            let (kind, compile) = if resources.contains(&path) || package_location.is_some() {
                (IdeSourceKind::Resource, None)
            } else if public_headers.contains(&path)
                || private_headers.contains(&path)
                || member.is_some_and(|set| set.kind == FileSetKind::Headers)
                || props
                    .get(b"HEADER_FILE_ONLY")
                    .is_some_and(|value| truth::is_on(value))
            {
                (IdeSourceKind::Header, None)
            } else if let Some((language, compile)) = compiled.swap_remove(&path) {
                (IdeSourceKind::Compile { language }, compile)
            } else if paths.object_owner(&path).is_some() {
                (IdeSourceKind::Object, None)
            } else if external_object(dir, &path) {
                (IdeSourceKind::ExternalObject, None)
            } else if matches!(
                file.stem_extension(true).1,
                b".h" | b".hh" | b".hpp" | b".hxx" | b".inl"
            ) {
                (IdeSourceKind::Header, None)
            } else if file.filename() == b"CMakeLists.txt"
                || file.stem_extension(true).1 == b".cmake"
            {
                (IdeSourceKind::ListFile, None)
            } else {
                (IdeSourceKind::Other, None)
            };
            let skip_precompile_headers = inputs.sources_without_pch.contains(&path)
                || props
                    .get(b"SKIP_PRECOMPILE_HEADERS")
                    .is_some_and(|value| truth::is_on(value));
            IdeSource {
                skip_precompile_headers,
                group: props.get(b"SOURCE_GROUP").cloned().unwrap_or_default(),
                generated,
                file_set: member.map(|set| set.name.clone()),
                package_location,
                path,
                kind,
                excluded_configs: Vec::new(),
                compile,
            }
        })
        .collect();
}

pub(crate) fn collect_link_items<'a>(
    usage: &UsageResolver<'_>,
    model: &ProjectModel,
    config: &ConfigId,
    items: impl Iterator<Item = &'a ResolvedLinkItem>,
    vs_template: Option<&BString>,
) -> Vec<IdeLinkItem> {
    use rcmake_genex::{ArtifactKind, ArtifactPart};
    items
        .filter_map(|item| {
            let classify = |path: BString| {
                if Lexical::new(&path)
                    .components()
                    .any(|part| part.ends_with(b".framework"))
                {
                    IdeLinkItem::Framework(path)
                } else {
                    IdeLinkItem::File(path)
                }
            };
            let value = &item.text.value;
            let link = match item.kind {
                ResolvedLinkKind::Library(id) | ResolvedLinkKind::Objects(id) => {
                    if model.target(id)?.is_imported() {
                        usage
                            .artifact(id, ArtifactKind::LinkerFile, ArtifactPart::Full, config)
                            .or_else(|| {
                                usage.artifact(id, ArtifactKind::File, ArtifactPart::Full, config)
                            })
                            .map(classify)
                    } else {
                        Some(IdeLinkItem::Target(id))
                    }
                }
                ResolvedLinkKind::Raw => {
                    Some(if let Some(name) = value.strip_prefix(b"-framework ") {
                        IdeLinkItem::Framework(name.into())
                    } else if value.starts_with(b"-") || value.starts_with(b"/DEFAULTLIB:") {
                        IdeLinkItem::Flag(value.clone())
                    } else if Lexical::new(value).absolute()
                        || (usage.ide() == Some(IdeFlavor::Xcode) && value.starts_with(b"/"))
                    {
                        classify(value.clone())
                    } else {
                        IdeLinkItem::Library(value.clone())
                    })
                }
                ResolvedLinkKind::ObjectFile => Some(IdeLinkItem::File(value.clone())),
                _ => None,
            }?;
            let link = if let Some(template) = vs_template
                && item
                    .feature
                    .as_ref()
                    .is_some_and(|name| name == b"WHOLE_ARCHIVE")
                && matches!(link, IdeLinkItem::File(_) | IdeLinkItem::Target(_))
            {
                IdeLinkItem::VisualStudioFeature {
                    item: Box::new(link),
                    template: template.clone(),
                }
            } else {
                link
            };
            let feature = item
                .feature
                .as_ref()
                .filter(|name| {
                    name.as_slice() != b"DEFAULT" && usage.ide() == Some(IdeFlavor::Xcode)
                })
                .cloned()
                .map(IdeLinkItem::XcodeFeature);
            Some([feature, Some(link)])
        })
        .flatten()
        .flatten()
        .collect()
}

pub(crate) fn add_pch_sources(facts: &mut IdeFacts) {
    facts.sources.extend(
        facts
            .pch
            .iter()
            .filter(|pch| pch.reuse_from.is_none() && pch.companion.is_some())
            .map(|pch| IdeSource {
                path: pch.source.clone(),
                kind: IdeSourceKind::Compile {
                    language: pch.language.clone(),
                },
                group: BString::default(),
                generated: true,
                excluded_configs: Vec::new(),
                file_set: None,
                package_location: None,
                skip_precompile_headers: false,
                compile: None,
            }),
    );
}

pub(crate) fn check_xcode_sources(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    baseline: &mut IndexMap<TargetId, (ConfigId, Vec<BString>)>,
    inputs: &IndexMap<TargetId, TargetInputs>,
    config: &ConfigId,
) -> Result<(), ResolveError> {
    for (&id, input) in inputs {
        let sources = input
            .sources
            .iter()
            .filter(|path| paths.object_owner(path).is_none())
            .cloned()
            .collect::<Vec<_>>();
        let entry = baseline.entry(id);
        match entry {
            indexmap::map::Entry::Vacant(entry) => {
                entry.insert((config.clone(), sources));
            }
            indexmap::map::Entry::Occupied(entry) => {
                let (first_config, first_sources) = entry.get();
                if first_sources != &sources
                    && let Some(target) = model.target(id)
                {
                    let block = |config: &ConfigId, sources: &[BString]| {
                        let mut text = format!("Config \"{}\":", config.0);
                        if !sources.is_empty() {
                            let files = sources.iter().map(ToString::to_string).collect::<Vec<_>>();
                            text.push_str("\n  ");
                            text.push_str(&files.join("\n  "));
                        }
                        text
                    };
                    let name = &target.name;
                    let first = block(first_config, first_sources);
                    let different = block(config, &sources);
                    let body = format!(
                        "Target \"{name}\" has source files which vary by configuration. This is not supported by the \"Xcode\" generator.\n{first}\n{different}"
                    );
                    return Err(module_error(model, paths, target, body.into()));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn finish_vs_sources(targets: &mut [ResolvedTarget]) {
    let mut by_id = IndexMap::<_, Vec<_>>::new();
    for target in targets {
        if target.ide.is_some() {
            by_id.entry(target.id).or_default().push(target);
        }
    }
    for configs in by_id.into_values() {
        let mut sources = IndexMap::<_, (IdeSource, IndexSet<BString>)>::new();
        for target in &configs {
            for source in target.ide.iter().flat_map(|facts| &facts.sources) {
                sources
                    .entry(source.path.clone())
                    .or_insert_with(|| (source.clone(), IndexSet::new()))
                    .1
                    .insert(target.config.0.clone());
            }
        }
        let all_configs: Vec<_> = configs
            .iter()
            .map(|target| target.config.0.clone())
            .collect();
        for target in configs {
            let Some(facts) = &mut target.ide else {
                continue;
            };
            let mut local: IndexMap<_, _> = std::mem::take(&mut facts.sources)
                .into_iter()
                .map(|source| (source.path.clone(), source))
                .collect();
            facts.sources = sources
                .iter()
                .map(|(path, (source, included))| {
                    let mut source = local.swap_remove(path).unwrap_or_else(|| {
                        let mut source = source.clone();
                        source.compile = None;
                        source
                    });
                    source.excluded_configs = all_configs
                        .iter()
                        .filter(|config| !included.contains(*config))
                        .cloned()
                        .collect();
                    source
                })
                .collect();
        }
    }
}

/// The source groups that every directory starts with, in creation order
/// (`cmMakefile::cmMakefile`).
#[must_use]
pub fn default_source_groups() -> Vec<rcmake_model::SourceGroupDef> {
    const DEFAULTS: [(&[u8], &[u8]); 8] = [
        (b"", b"^.*$"),
        (
            b"Source Files",
            b"\\.(C|F|M|c|c\\+\\+|cc|cpp|mpp|cxx|ixx|cppm|ccm|cxxm|c\\+\\+m|cu|f|f90|for|fpp|ftn|m|mm|rc|def|r|odl|idl|hpj|bat)$",
        ),
        (b"Header Files", b"\\.(h|hh|h\\+\\+|hm|hpp|hxx|in|txx|inl)$"),
        (b"Precompile Header File", b"cmake_pch(_[^.]+)?\\.(h|hxx)$"),
        (b"CMake Rules", b"\\.rule$"),
        (b"Resources", b"\\.(pdf|plist|png|jpeg|jpg|storyboard|xcassets)$"),
        (b"Object Files", b"\\.(lo|o|obj)$"),
        (b"Object Libraries", b"^MATCH_NO_SOURCES$"),
    ];
    DEFAULTS
        .iter()
        .map(|(name, regex)| rcmake_model::SourceGroupDef {
            name: (*name).into(),
            regex: Some((*regex).into()),
            ..rcmake_model::SourceGroupDef::default()
        })
        .collect()
}

/// Return the full name (`a\b`) of the source group of `path`
/// (`cmSourceGroup::FindSourceGroup`). The search goes through the groups of
/// `dir` in reverse creation order: first for a group that lists the file, then
/// for a group whose regular expression matches. A directory without
/// `source_group()` calls uses [`default_source_groups`]. `matches(pattern, path)`
/// evaluates a CMake regular expression.
pub fn find_source_group(
    dir: &rcmake_model::Directory,
    path: &[u8],
    matches: &mut dyn FnMut(&[u8], &[u8]) -> bool,
) -> BString {
    let defaults;
    let groups = if dir.source_groups.is_empty() {
        defaults = default_source_groups();
        &defaults
    } else {
        &dir.source_groups
    };
    groups
        .iter()
        .rev()
        .find_map(|group| search_group(group, Order::Pre, &mut |group| group.files.contains(path)))
        .or_else(|| {
            groups.iter().rev().find_map(|group| {
                search_group(group, Order::Post, &mut |group| {
                    group
                        .regex
                        .as_ref()
                        .is_some_and(|pattern| matches(pattern, path))
                })
            })
        })
        .or_else(|| groups.first().map(|group| group.name.clone()))
        .unwrap_or_default()
}

/// When [`search_group`] tests a group relative to its children.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    /// The group, then its children (`cmSourceGroup::MatchChildrenFiles`).
    Pre,
    /// The children, then the group (`cmSourceGroup::MatchChildrenRegex`).
    Post,
}

/// Return the full name (`a\b`) of the first group in the tree of `root` that
/// `test` accepts. The walk uses an explicit stack, so the nesting depth of
/// the groups does not use the call stack. The name is built only for the
/// group that matches.
fn search_group(
    root: &rcmake_model::SourceGroupDef,
    order: Order,
    test: &mut dyn FnMut(&rcmake_model::SourceGroupDef) -> bool,
) -> Option<BString> {
    // Each frame is a group and the position of its next child.
    let mut frames = vec![(root, 0usize)];
    if order == Order::Pre && test(root) {
        return Some(full_name(&frames));
    }
    while let Some(&mut (group, ref mut next)) = frames.last_mut() {
        if let Some(child) = group.children.get(*next) {
            *next += 1;
            frames.push((child, 0));
            if order == Order::Pre && test(child) {
                return Some(full_name(&frames));
            }
            continue;
        }
        if order == Order::Post && test(group) {
            return Some(full_name(&frames));
        }
        frames.pop();
    }
    None
}

/// Join the names of the groups on the walk stack with `\`. An empty prefix
/// adds no separator.
fn full_name(frames: &[(&rcmake_model::SourceGroupDef, usize)]) -> BString {
    let mut name = Vec::new();
    for (group, _) in frames {
        if !name.is_empty() {
            name.push(b'\\');
        }
        name.extend_from_slice(&group.name);
    }
    name.into()
}

#[cfg(test)]
mod tests {
    use rcmake_model::{Directory, SourceGroupDef};

    use super::find_source_group;

    fn group(name: &str, regex: Option<&str>, files: &[&str]) -> SourceGroupDef {
        SourceGroupDef {
            name: name.into(),
            regex: regex.map(Into::into),
            files: files.iter().map(|file| (*file).into()).collect(),
            children: Vec::new(),
        }
    }

    fn regex(pattern: &[u8], path: &[u8]) -> bool {
        pattern
            .strip_prefix(b"*")
            .is_some_and(|suffix| path.ends_with(suffix))
    }

    /// Drop a group tree without recursion.
    fn dismantle(groups: Vec<SourceGroupDef>) {
        let mut pending = groups;
        while let Some(mut group) = pending.pop() {
            pending.append(&mut group.children);
        }
    }

    #[test]
    fn explicit_files_win_over_regex_and_children_regex_win_over_parent() {
        let mut parent = group("p", Some("*.c"), &[]);
        parent.children.push(group("c", Some("*.c"), &[]));
        parent.children.push(group("other", Some("*.c"), &[]));
        let mut listed = group("listed", None, &[]);
        listed.children.push(group("x", None, &["/s/a.c"]));
        let dir = Directory {
            source_groups: vec![group("", Some("*"), &[]), listed, parent],
            ..Directory::default()
        };
        let mut matches = regex;
        // The explicit file wins, even though the later group matches by regex.
        assert_eq!(
            find_source_group(&dir, b"/s/a.c", &mut matches),
            "listed\\x"
        );
        // The first child regex wins over its parent and later siblings.
        assert_eq!(find_source_group(&dir, b"/s/b.c", &mut matches), "p\\c");
        // The first group is the fallback.
        assert_eq!(find_source_group(&dir, b"/s/b.h", &mut matches), "");
    }

    #[test]
    fn very_deep_group_tree_is_searched_without_recursion() {
        const DEPTH: usize = 100_000;
        let mut deep = group("g", Some("*.c"), &["/s/leaf.c"]);
        for _ in 1..DEPTH {
            let mut parent = group("g", None, &[]);
            parent.children.push(deep);
            deep = parent;
        }
        let mut dir = Directory {
            source_groups: vec![group("", None, &[]), deep],
            ..Directory::default()
        };
        let expected = vec!["g"; DEPTH].join("\\");
        let mut matches = regex;
        assert_eq!(
            find_source_group(&dir, b"/s/leaf.c", &mut matches),
            expected.as_str()
        );
        assert_eq!(
            find_source_group(&dir, b"/s/other.c", &mut matches),
            expected.as_str()
        );
        assert_eq!(find_source_group(&dir, b"/s/other.h", &mut matches), "");
        dismantle(std::mem::take(&mut dir.source_groups));
    }
}
