//! MSBuild C++ project output.

use std::collections::{BTreeMap, HashMap};

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_gen_common::ide::UtilityKind;
use rcmake_model::{BT, DirId, ProjectModel, Target, TargetKind};
use rcmake_resolve::{
    BuildStep, LinkStep, ResolvedTarget,
    ide::{IdeCompileSettings, IdeCustomPhase, IdeSourceKind},
    usage::UsageResolver,
};

use crate::{
    GenError, Project, Settings, absolute, flags_table, guid, join, msbuild, msbuild_value,
    plan::VsIndices,
    rules::{self, Command, Rule},
    string, windows,
};

mod cl;
mod custom;
mod extensions;
mod fallback;
mod link;
mod pch;
mod render;
mod settings;
mod unity;

pub(crate) use cl::ClFacts;
pub(crate) use extensions::CudaConfig;
use extensions::gen_error;
pub(crate) use fallback::source_language;
use link::targets_files;
pub(crate) use render::nuget;
pub(crate) use render::{render, write_props_import};

pub(crate) struct Context<'a> {
    pub(crate) project: &'a Project,
    pub(crate) projects: &'a [Project],
    pub(crate) settings: &'a Settings,
    pub(crate) facts: Option<&'a BTreeMap<String, &'a ResolvedTarget>>,
    pub(crate) indices: &'a VsIndices<'a>,
    pub(crate) model: &'a ProjectModel,
    pub(crate) inputs: &'a [String],
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Compile,
    Cuda,
    Masm,
    Nasm,
    Header,
    Resource,
    Object,
    Other,
    Custom,
}

impl Kind {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Compile => "ClCompile",
            Self::Cuda => "CudaCompile",
            Self::Masm => "MASM",
            Self::Nasm => "NASM",
            Self::Header => "ClInclude",
            Self::Resource => "ResourceCompile",
            Self::Object => "Object",
            Self::Other => "None",
            Self::Custom => "CustomBuild",
        }
    }

    pub(crate) fn default_group(self) -> &'static str {
        match self {
            Self::Compile | Self::Cuda | Self::Masm | Self::Nasm | Self::Object => "Source Files",
            Self::Header => "Header Files",
            Self::Resource => "Resource Files",
            Self::Custom => "CMake Rules",
            Self::Other => "",
        }
    }

    pub(crate) fn compiles(self) -> bool {
        matches!(self, Self::Compile | Self::Cuda | Self::Masm | Self::Nasm)
    }

    fn from_language(language: &[u8], fallback: Self) -> Self {
        match crate::cuda::Assembler::from_language(language) {
            Some(crate::cuda::Assembler::Masm) => Self::Masm,
            Some(crate::cuda::Assembler::Nasm) => Self::Nasm,
            None if language == b"CUDA" => Self::Cuda,
            None => fallback,
        }
    }
}

pub(crate) struct Item {
    pub(crate) kind: Kind,
    pub(crate) path: String,
    pub(crate) group: String,
    pub(crate) synthetic: bool,
    pub(crate) configs: BTreeMap<String, SourceConfig>,
    /// The MSBuild item type when it is not the one of [`Kind::tag`], such
    /// as `FXCompile` for a shader or a `VS_TOOL_OVERRIDE` value.
    pub(crate) tool: Option<String>,
    /// `VS_SETTINGS` and `VS_SOURCE_SETTINGS_<tool>` metadata, see
    /// [`settings::Setting`].
    pub(crate) settings: Vec<settings::Setting>,
    /// The rule writes UTF-8 output (`UseUtf8Encoding`), as the rules that
    /// run CMake do.
    pub(crate) utf8: bool,
    /// The native MSBuild unity build metadata of a source, see
    /// [`unity::UnitySource`].
    pub(crate) unity: Option<unity::UnitySource>,
}

impl Item {
    /// The MSBuild item type of the item.
    pub(crate) fn tag(&self) -> &str {
        self.tool.as_deref().unwrap_or_else(|| self.kind.tag())
    }
}

#[derive(Default)]
pub(crate) struct SourceConfig {
    pub(crate) excluded: bool,
    pub(crate) language: String,
    pub(crate) object: String,
    /// `CompileAsC` or `CompileAsCpp` when the source language is not the
    /// language of its file extension.
    pub(crate) compile_as: Option<&'static str>,
    pub(crate) compile: Option<flags_table::Settings>,
    pub(crate) tool_compile: Option<IdeCompileSettings>,
    pub(crate) rule: Option<Rule>,
}

#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) items: BTreeMap<(Kind, String), Item>,
    pub(crate) configs: BTreeMap<String, Config>,
    /// Errors that do not stop the generation of the other files.
    pub(crate) errors: Vec<String>,
}

#[derive(Default)]
pub(crate) struct Config {
    pub(crate) compile: flags_table::Settings,
    pub(crate) resource: flags_table::Settings,
    pub(crate) link: flags_table::Settings,
    pub(crate) link_output_dirs: Vec<String>,
    pub(crate) events: [Vec<Rule>; 3],
    pub(crate) dir: String,
    pub(crate) int_dir: String,
    pub(crate) name: String,
    pub(crate) extension: String,
    pub(crate) gui: bool,
    pub(crate) link_incremental: Option<String>,
    pub(crate) generate_manifest: Option<String>,
    pub(crate) debugger: Vec<(&'static str, String)>,
    pub(crate) dpi_aware: Option<&'static str>,
    pub(crate) cuda: Option<extensions::CudaConfig>,
    pub(crate) cl: ClFacts,
    /// The `.targets` files that the configuration links.
    pub(crate) targets_files: Vec<String>,
}

impl Plan {
    fn source(&mut self, path: String, kind: Kind, group: &str, config: &str) -> &mut SourceConfig {
        let item = self
            .items
            .entry((kind, path.clone()))
            .or_insert_with(|| Item {
                kind,
                path,
                group: if group.is_empty() {
                    kind.default_group().into()
                } else {
                    windows(group)
                },
                synthetic: false,
                configs: BTreeMap::new(),
                tool: None,
                settings: Vec::new(),
                utf8: false,
                unity: None,
            });
        item.configs.entry(config.into()).or_default()
    }

    /// Attach a custom rule to the item `path`. As in CMake, the rule
    /// replaces another item for the same file, such as a source listed in
    /// the target, and keeps its group.
    fn rule(&mut self, path: String, synthetic: bool, rule: Rule, config: &str) {
        let replaced: Vec<_> = self
            .items
            .keys()
            .filter(|(kind, other)| *kind != Kind::Custom && *other == path)
            .cloned()
            .collect();
        let group = replaced
            .iter()
            .filter_map(|key| self.items.remove(key))
            .map(|item| item.group)
            .last();
        self.source(path.clone(), Kind::Custom, "", config).rule = Some(rule);
        if let Some(item) = self.items.get_mut(&(Kind::Custom, path)) {
            item.synthetic = synthetic;
            if let Some(group) = group {
                item.group = group;
            }
        }
    }
}

const DEBUGGER_PROPERTIES: [(&[u8], &str); 4] = [
    (b"VS_DEBUGGER_COMMAND", "LocalDebuggerCommand"),
    (
        b"VS_DEBUGGER_COMMAND_ARGUMENTS",
        "LocalDebuggerCommandArguments",
    ),
    (b"VS_DEBUGGER_ENVIRONMENT", "LocalDebuggerEnvironment"),
    (
        b"VS_DEBUGGER_WORKING_DIRECTORY",
        "LocalDebuggerWorkingDirectory",
    ),
];

impl<'a> Context<'a> {
    /// Get the `CMAKE_MSVCIDE_RUN_PATH` value of the directory that owns the
    /// project. A cache entry, also one from the environment, is the fallback.
    pub(crate) fn run_path(&self) -> Option<String> {
        let dir = self
            .project
            .target
            .and_then(|id| self.model.target(id))
            .map_or(DirId::ROOT, |target| target.dir);
        self.model
            .definition(dir, b"CMAKE_MSVCIDE_RUN_PATH")
            .map(|value| string(value))
    }

    /// Get a target property for one configuration. A `VS_*` property is
    /// evaluated by the resolver. The raw value is the fallback.
    pub(crate) fn property(&self, config: &str, key: &[u8]) -> Option<&'a BString> {
        let target = self.project.target.and_then(|id| self.model.target(id))?;
        let raw = target.props.get(key)?;
        Some(
            self.facts
                .and_then(|facts| facts.get(config))
                .and_then(|fact| fact.ide.as_deref())
                .and_then(|ide| ide.attributes.get(key))
                .unwrap_or(raw),
        )
    }

    /// Select `UseDebugLibraries` for one configuration, as CMake does.
    ///
    /// 1. A `VS_USE_DEBUG_LIBRARIES` value, evaluated for the configuration,
    ///    is used as a boolean. An empty value writes no setting. The
    ///    projects that CMake adds, such as `ALL_BUILD`, use the last
    ///    top-level `CMAKE_VS_USE_DEBUG_LIBRARIES` value.
    /// 2. Otherwise, with policy `CMP0162` set to `NEW`, `runtime` is used.
    ///    It tells if the MSVC runtime library of the configuration is a
    ///    debug library. When it is not known, the configuration name
    ///    `Debug` selects the debug libraries.
    /// 3. Otherwise, no setting is written.
    pub(crate) fn debug_libraries(&self, config: &str, runtime: Option<bool>) -> Option<bool> {
        let target = self.project.target.and_then(|id| self.model.target(id));
        let value = match target {
            Some(target) => target.props.get(b"VS_USE_DEBUG_LIBRARIES").map(|raw| {
                self.facts
                    .and_then(|facts| facts.get(config))
                    .and_then(|fact| fact.ide.as_deref())
                    .and_then(|ide| ide.attributes.get(b"VS_USE_DEBUG_LIBRARIES".as_slice()))
                    .unwrap_or(raw)
            }),
            None => self
                .model
                .definition(DirId::ROOT, b"CMAKE_VS_USE_DEBUG_LIBRARIES"),
        };
        if let Some(value) = value {
            // A value that the resolver did not evaluate cannot be used.
            if value.is_empty() || value.windows(2).any(|pair| pair == b"$<") {
                return None;
            }
            return Some(truth::is_on(value));
        }
        let policies = match target {
            Some(target) => &target.policies.policies,
            None => self.model.dir(DirId::ROOT).final_policies.as_ref()?,
        };
        (policies.get(PolicyId::new(162)) == PolicyStatus::New)
            .then(|| runtime.unwrap_or_else(|| config.eq_ignore_ascii_case("Debug")))
    }

    pub(crate) fn plan(&self, usage: &mut Option<UsageResolver<'a>>) -> Result<Plan, GenError> {
        let mut plan = Plan::default();
        let target = self.project.target.and_then(|id| self.model.target(id));
        for name in &self.settings.configs {
            let mut config = Config::default();
            let fact = self.facts.and_then(|facts| facts.get(name)).copied();
            let compile_units = build_compile_units(fact);
            let graph = self.indices.graph_by_config.get(name.as_bytes()).copied();
            let steps = graph
                .and_then(|graph| {
                    let target = self
                        .indices
                        .graph_target
                        .get(&(name.as_bytes(), self.project.name.as_bytes()))?;
                    Some(
                        target
                            .steps
                            .iter()
                            .filter_map(|&index| graph.steps.get(index))
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_default();

            let link = fact.and_then(|fact| fact.link.as_ref()).or_else(|| {
                steps.iter().find_map(|step| match step {
                    BuildStep::Link { step, .. } => Some(step),
                    _ => None,
                })
            });
            self.output(&mut config, fact, link, name);
            config.int_dir = fact
                .and_then(|fact| fact.artifacts.object_dir.as_ref())
                .map(|dir| absolute(&self.settings.bin, &string(dir)))
                .unwrap_or_else(|| intermediate(self.project, name));
            if let Some(target) = target {
                config.gui = target
                    .props
                    .get(b"WIN32_EXECUTABLE")
                    .is_some_and(|value| truth::is_on(value));
            }

            if let Some(ide) = fact.and_then(|fact| fact.ide.as_deref()) {
                let target_compile = |settings: &IdeCompileSettings| {
                    let mut compiled = visual_studio_compile(settings, &self.settings.toolset);
                    if matches!(settings.language.as_slice(), b"C" | b"CXX") {
                        compiled.take("CompileAs");
                    }
                    compiled
                };
                let languages = ide.compile.iter().filter(|settings| {
                    settings.language != b"RC"
                        && Kind::from_language(&settings.language, Kind::Compile) == Kind::Compile
                });
                let link_language = link.map(|link| link.language.as_slice());
                let base = cl_compile_language(link_language, languages.clone());
                if let Some(settings) = base {
                    let mut ordered = settings.clone();
                    if let Some(target) = target {
                        // CMake adds these flags after `CMAKE_<LANG>_FLAGS`
                        // and `CMAKE_<LANG>_FLAGS_<CONFIG>`, which the target
                        // settings list first, and before the target options.
                        // So `MSVC_RUNTIME_LIBRARY` wins over a `/MT` in
                        // `CMAKE_CXX_FLAGS`.
                        let extras =
                            self.language_extras(target, &settings.language, name, usage)?;
                        let at = language_flag_count(self.model, target, &settings.language, name)
                            .min(ordered.flags.len());
                        let options = ordered.flags.split_off(at);
                        ordered.flags.extend(
                            extras.iter().map(|flag| {
                                BT::new(BString::from(flag.as_str()), Default::default())
                            }),
                        );
                        ordered.flags.extend(options);
                    }
                    let mut compiled = target_compile(&ordered);
                    if let Some(flag) = target.and_then(cl::clr_flag) {
                        compiled.append(flags_table::compile([flag], &self.settings.toolset));
                    }
                    config.compile = compiled;
                    // MSBuild reads `LanguageStandard_C` only for C sources, so
                    // CMake adds the C standard to the C++ settings.
                    if settings.language == b"CXX"
                        && let Some(standard) = languages
                            .clone()
                            .find(|settings| settings.language == b"C")
                            .and_then(|c| target_compile(c).take("LanguageStandard_C"))
                    {
                        config.compile.set("LanguageStandard_C", standard);
                    }
                }
                if let Some(target) = target {
                    config.cl = self.finish_cl(target, &mut config.compile);
                }
                // The language of the target precompiled header settings.
                let pch_language = link_language
                    .filter(|language| matches!(*language, b"C" | b"CXX"))
                    .or_else(|| base.map(|base| base.language.as_slice()))
                    .unwrap_or_default()
                    .to_vec();
                if base.is_some() {
                    pch::configure_target(&mut config.compile, ide, &pch_language);
                }
                if languages.clone().next().is_some() {
                    config.compile.compiler_defaults(&self.settings.toolset);
                    config.compile.list(
                        "PreprocessorDefinitions",
                        r#"CMAKE_INTDIR="$(Configuration)""#,
                    );
                }
                let members = match target {
                    Some(target) => self.unity_members(ide, target)?,
                    None => Vec::new(),
                };
                for source in ide.sources.iter().chain(&members) {
                    let path = self.source_path(&string(&source.path));
                    let (kind, language) = match &source.kind {
                        IdeSourceKind::Compile { language } => (
                            Kind::from_language(
                                language,
                                self.fallback_source_kind(&path, Kind::Compile),
                            ),
                            string(language),
                        ),
                        IdeSourceKind::Header => (Kind::Header, String::new()),
                        IdeSourceKind::Resource => (source_kind(&path, Kind::Other), String::new()),
                        IdeSourceKind::Object | IdeSourceKind::ExternalObject => {
                            (Kind::Object, String::new())
                        }
                        IdeSourceKind::Other => {
                            let kind = match self.fallback_source_kind(&path, Kind::Other) {
                                Kind::Masm => Kind::Masm,
                                Kind::Nasm
                                    if self.definition(b"CMAKE_ASM_NASM_COMPILER").is_some() =>
                                {
                                    Kind::Nasm
                                }
                                _ => Kind::Other,
                            };
                            (kind, String::new())
                        }
                        IdeSourceKind::ListFile => (Kind::Other, String::new()),
                    };
                    let kind = if self.project.kind == TargetKind::InterfaceLibrary {
                        Kind::Other
                    } else {
                        kind
                    };
                    let item = plan.source(path.clone(), kind, &string(&source.group), name);
                    item.excluded = source
                        .excluded_configs
                        .iter()
                        .any(|value| value.as_slice() == name.as_bytes());
                    if kind == Kind::Compile {
                        item.object = self.object_path(&path, name, fact, compile_units.as_ref());
                        // As in CMake, a source has only its own settings. A
                        // source of another language than the target settings
                        // does not get the flags of its language.
                        item.compile = source.compile.as_ref().map(|settings| {
                            visual_studio_compile(settings, &self.settings.toolset)
                        });
                        // CMake compiles C sources without Windows Runtime
                        // extensions when the target settings use them.
                        if config.cl.winrt && language == "C" {
                            item.compile
                                .get_or_insert_with(Default::default)
                                .set("CompileAsWinRT", "false");
                        }
                        item.compile_as = compile_as(&language, &path);
                        pch::configure_source(
                            item,
                            source,
                            ide,
                            language.as_bytes(),
                            pch_language.as_slice(),
                        );
                        item.language = language;
                    } else if kind == Kind::Resource {
                        item.compile = source.compile.as_ref().map(|settings| {
                            visual_studio_compile(settings, &self.settings.toolset)
                        });
                        // CMake compiles C sources without Windows Runtime
                        // extensions when the target settings use them.
                        if config.cl.winrt && language == "C" {
                            item.compile
                                .get_or_insert_with(Default::default)
                                .set("CompileAsWinRT", "false");
                        }
                    } else if kind.compiles() {
                        item.object = self.object_path(&path, name, fact, compile_units.as_ref());
                        item.tool_compile.clone_from(&source.compile);
                    }
                }
                self.mark_unity_sources(&mut plan, (ide, fact), &members, name);
                if let Some(resource) = ide
                    .compile
                    .iter()
                    .find(|settings| settings.language == b"RC")
                {
                    config.resource = visual_studio_compile(resource, &self.settings.toolset);
                    for settings in languages {
                        for define in &settings.defines {
                            let value = msbuild_value(&string(&define.value));
                            config
                                .resource
                                .list_unique("PreprocessorDefinitions", value);
                        }
                    }
                } else if let Some(settings) = languages.clone().next() {
                    config.resource = include_definitions(settings);
                }
                config.link = self.ide_link(ide, name);
                config.targets_files = targets_files(ide);
                pch::link_reused(self.project.kind, ide, &mut config.link);
                for (index, step) in ide.custom.iter().enumerate() {
                    let rule = Rule::from(step);
                    match step.phase {
                        IdeCustomPhase::Rule => self.custom(&mut plan, name, index, None, rule),
                        IdeCustomPhase::PreBuild => config.events[0].push(rule),
                        IdeCustomPhase::PreLink => config.events[1].push(rule),
                        IdeCustomPhase::PostBuild => config.events[2].push(rule),
                    }
                }
            } else {
                self.fallback(
                    &mut plan,
                    &mut config,
                    (fact, compile_units.as_ref()),
                    &steps,
                    link,
                    name,
                );
            }
            self.windows_link(&plan, &mut config, fact, name);
            self.toolchain_settings(&plan, &mut config, fact, name, usage)?;
            let property = |key: &[u8]| {
                fact.and_then(|fact| fact.ide.as_ref())
                    .and_then(|ide| ide.attributes.get(key))
                    .or_else(|| target.and_then(|target| target.props.get(key)))
            };
            config.debugger = DEBUGGER_PROPERTIES
                .iter()
                .filter_map(|&(key, element)| {
                    let value = property(key).filter(|value| !value.is_empty())?;
                    Some((element, string(value)))
                })
                .collect();
            config.dpi_aware = match property(b"VS_DPI_AWARE").map(|value| value.as_slice()) {
                None | Some(b"") => None,
                Some(b"PerMonitor") => Some("PerMonitorHighDPIAware"),
                Some(value) if truth::is_on(value) => Some("true"),
                Some(value) if truth::is_off(value) => Some("false"),
                Some(value) => {
                    // As in CMake, a bad value is an error, and generation
                    // continues with the other projects.
                    let error = format!("Bad parameter for VS_DPI_AWARE: {}", string(value));
                    if !plan.errors.contains(&error) {
                        plan.errors.push(error);
                    }
                    None
                }
            };

            if let Some(utility) = self.project.utility {
                self.utility(&mut plan, &mut config, name, utility);
            }
            config.link_incremental = config.link.take("LinkIncremental");
            config.generate_manifest = config.link.take("GenerateManifest");
            plan.configs.insert(name.clone(), config);
        }
        // As in CMake, managed code cannot link with `/DEBUG:FASTLINK`, so a
        // managed target uses `/DEBUG` in each configuration.
        if plan.configs.values().any(|config| config.cl.managed) {
            for config in plan.configs.values_mut() {
                if config.link.get("GenerateDebugInformation") == Some("DebugFastLink") {
                    config.link.set("GenerateDebugInformation", "Debug");
                }
            }
        }
        self.listfile_rule(&mut plan);
        if let Some(target) = target {
            self.source_settings(&mut plan, target, usage)?;
            self.source_group_genex(&mut plan, target, usage)?;
        }
        self.apply_source_groups(&mut plan, target);
        Ok(plan)
    }

    /// Add the `CMakeLists.txt` file of the project directory as a custom
    /// build item, as CMake adds it to each project except `ZERO_CHECK` and
    /// the global targets such as `INSTALL`.
    ///
    /// As in CMake, the rule runs
    /// `cmake -S<source> -B<binary> --check-stamp-file <stamp>`, where the
    /// stamp is `CMakeFiles/generate.stamp` of the project directory. Its
    /// inputs are the listfiles of the directory. The group of the item is
    /// the source group of the file in its directory.
    fn listfile_rule(&self, plan: &mut Plan) {
        if !matches!(self.project.utility, None | Some(UtilityKind::AllBuild))
            || !self
                .projects
                .iter()
                .any(|project| project.utility == Some(UtilityKind::ZeroCheck))
        {
            return;
        }
        let dir = self
            .project
            .target
            .and_then(|id| self.model.target(id))
            .map(|target| target.dir)
            .or_else(|| {
                self.model
                    .dirs()
                    .find(|(_, dir)| {
                        absolute(&self.settings.source, &string(&dir.source_dir))
                            == self.project.source
                    })
                    .map(|(id, _)| id)
            })
            .unwrap_or(DirId::ROOT);
        let directory = self.model.dir(dir);
        let path = join(&self.project.source, "CMakeLists.txt");
        let mut inputs: Vec<String> = directory
            .listfiles
            .iter()
            .map(|file| absolute(&self.project.source, &string(file)))
            .collect();
        let glob_stamp = join(&self.settings.bin, rcmake_gen_common::glob::STAMP);
        if self.inputs.contains(&glob_stamp) {
            inputs.push(glob_stamp);
        }
        inputs.sort();
        inputs.dedup();
        inputs.retain(|input| *input != path);
        let stamp = join(&self.project.dir, "CMakeFiles/generate.stamp");
        let mut argv = vec![
            self.settings.cmake.clone(),
            format!("-S{}", self.settings.source),
            format!("-B{}", self.settings.bin),
            "--check-stamp-file".into(),
            stamp.clone(),
        ];
        argv.extend(self.warning_as_error_arguments());
        let rule = Rule {
            commands: vec![Command {
                argv,
                dir: self.project.dir.clone(),
            }],
            inputs,
            outputs: vec![stamp],
            message: format!("Building Custom Rule {path}"),
            ..Rule::default()
        };
        for config in &self.settings.configs {
            plan.rule(path.clone(), false, rule.clone(), config);
        }
        if let Some(item) = plan.items.get_mut(&(Kind::Custom, path)) {
            item.utf8 = true;
            item.group = self
                .indices
                .listfile_groups
                .get(&dir)
                .cloned()
                .unwrap_or_default();
        }
    }

    /// The arguments that keep the warning-as-error options of the command
    /// line when a build runs CMake again, as CMake adds them.
    fn warning_as_error_arguments(&self) -> Vec<String> {
        let cached = |key: &[u8]| {
            self.model
                .cache
                .get(key)
                .is_some_and(|entry| truth::is_on(&entry.value))
        };
        let mut arguments = Vec::new();
        if cached(b"_RCMAKE_NO_WARNING_AS_ERROR") {
            arguments.push("--compile-no-warning-as-error".into());
        }
        if cached(b"_RCMAKE_IGNORE_LINK_WARNING_AS_ERROR") {
            arguments.push("--link-no-warning-as-error".into());
        }
        arguments
    }

    /// The generation stamp of each directory, in directory order. The
    /// generator writes them with [`crate::write_stamps`].
    fn directory_stamps(&self) -> Vec<String> {
        self.model
            .dirs()
            .map(|(_, dir)| {
                join(
                    &absolute(&self.settings.bin, &string(&dir.binary_dir)),
                    "CMakeFiles/generate.stamp",
                )
            })
            .collect()
    }

    /// The top-level solution file, which `--vs-solution-file` names.
    fn top_solution(&self) -> String {
        let name = self
            .model
            .definition(DirId::ROOT, b"PROJECT_NAME")
            .or_else(|| self.model.dir(DirId::ROOT).props.get(b"PROJECT_NAME"))
            .map_or_else(|| "Project".to_owned(), |name| string(name));
        let extension = crate::solution_ext(self.settings.version);
        join(&self.settings.bin, &format!("{name}.{extension}"))
    }

    fn apply_source_groups(&self, plan: &mut Plan, target: Option<&Target>) {
        let Some(target) = target else { return };
        let dir = self.model.dir(target.dir);
        for item in plan.items.values_mut() {
            let path = Lexical::new(item.path.as_bytes());
            let binary = Lexical::new(&dir.binary_dir);
            let group = dir
                .source_props
                .get(item.path.as_bytes())
                .and_then(|props| props.get(b"SOURCE_GROUP"))
                .or_else(|| {
                    if !binary.prefix(path) {
                        return None;
                    }
                    let relative = path.relative(binary)?;
                    let source = Lexical::new(&dir.source_dir).append(&relative);
                    dir.source_props.get(&source)?.get(b"SOURCE_GROUP")
                });
            if let Some(group) = group {
                item.group = windows(&string(group));
            }
        }
    }

    fn output(
        &self,
        config: &mut Config,
        fact: Option<&ResolvedTarget>,
        link: Option<&LinkStep>,
        name: &str,
    ) {
        let naming = fact
            .and_then(|fact| fact.ide.as_ref())
            .map(|ide| &ide.naming);
        let filename = fact
            .and_then(|fact| fact.artifacts.file.as_ref())
            .map(|path| string(path))
            .or_else(|| {
                link.filter(|link| !link.output.is_empty())
                    .map(|link| string(&link.output))
            });
        config.dir = join(&self.project.dir, name);
        config.name.clone_from(&self.project.name);
        config.extension = match self.project.kind {
            TargetKind::Executable => ".exe",
            TargetKind::StaticLibrary => ".lib",
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary => ".dll",
            _ => "",
        }
        .into();
        if let Some(file) = filename {
            let file = absolute(&self.settings.bin, &file);
            let path = Lexical::new(file.as_bytes());
            let (stem, extension) = path.stem_extension(true);
            config.dir = string(path.parent());
            config.name = string(stem);
            config.extension = string(extension);
        }
        if let Some(naming) = naming {
            if !naming.dir.is_empty() {
                config.dir = string(&naming.dir);
            }
            if !naming.base_name.is_empty() {
                config.name = format!("{}{}", naming.prefix, naming.base_name);
                config.extension = string(&naming.suffix);
            }
        }
    }

    fn source_path(&self, value: &str) -> String {
        absolute(&self.project.source, value)
    }

    fn object_path(
        &self,
        source: &str,
        config: &str,
        fact: Option<&ResolvedTarget>,
        compile_units: Option<&HashMap<&[u8], &[u8]>>,
    ) -> String {
        object_path(
            self.project,
            self.settings,
            self.model,
            source,
            config,
            fact,
            compile_units,
        )
    }

    fn custom(
        &self,
        plan: &mut Plan,
        config: &str,
        index: usize,
        main: Option<String>,
        mut rule: Rule,
    ) {
        if rule.commands.is_empty() {
            return;
        }
        if main.is_none()
            && let Some((id, command)) = rule
                .outputs
                .first()
                .and_then(|output| self.rule_command(config, output))
        {
            let attachment = self.attach(id, command);
            rule.parallel = attachment.parallel;
            plan.rule(attachment.path, attachment.synthetic, rule, config);
            return;
        }
        let key = rule
            .outputs
            .first()
            .or(rule.inputs.first())
            .map_or_else(|| index.to_string(), Clone::clone);
        let guid = guid::bare(&guid::for_name(self.model, &self.settings.bin, &key));
        let synthetic = main.is_none();
        let path = main.unwrap_or_else(|| {
            join(
                &self.project.dir,
                &format!("CMakeFiles/{}.dir/{guid}-{index}.rule", self.project.name),
            )
        });
        if rule.outputs.is_empty() {
            rule.outputs.push(format!("{path}.always"));
        }
        plan.rule(path, synthetic, rule, config);
    }

    fn utility(&self, plan: &mut Plan, config: &mut Config, name: &str, utility: UtilityKind) {
        let command = match utility {
            UtilityKind::AllBuild => return,
            UtilityKind::ZeroCheck => {
                config.events[0].extend(rules::verify_globs(
                    self.settings,
                    self.project,
                    self.inputs,
                ));
                // As in CMake, the rule is attached to a `.rule` file named
                // after the first stamp, because the `CMakeLists.txt` file of
                // each directory has its own rule.
                let stamps = self.directory_stamps();
                let first = stamps
                    .first()
                    .cloned()
                    .unwrap_or_else(|| join(&self.settings.bin, "CMakeFiles/generate.stamp"));
                let path = custom::rule_file(&self.settings.bin, &self.settings.bin, &first, &[]);
                let extra = self.warning_as_error_arguments();
                let solution = self.top_solution();
                let rule = rules::regeneration(
                    self.settings,
                    self.project,
                    self.inputs,
                    (stamps.as_slice(), solution.as_str(), extra.as_slice()),
                );
                plan.rule(path.clone(), true, rule, name);
                if let Some(item) = plan.items.get_mut(&(Kind::Custom, path)) {
                    item.utf8 = true;
                }
                return;
            }
            UtilityKind::Install => vec![
                self.settings.cmake.clone(),
                "--install".into(),
                self.settings.bin.clone(),
                "--config".into(),
                name.into(),
            ],
            UtilityKind::RunTests => {
                let mut argv = vec![self.settings.ctest.clone()];
                argv.extend(self.settings.ctest_arguments.iter().cloned());
                argv.extend(["-C".into(), name.into()]);
                argv
            }
            UtilityKind::Package | UtilityKind::PackageSource => {
                let filename = if utility == UtilityKind::Package {
                    "CPackConfig.cmake"
                } else {
                    "CPackSourceConfig.cmake"
                };
                vec![
                    self.settings.cpack.clone(),
                    "--config".into(),
                    filename.into(),
                    "-C".into(),
                    name.into(),
                ]
            }
        };
        config.events[2].push(Rule {
            commands: vec![Command {
                argv: command,
                dir: self.project.dir.clone(),
            }],
            ..Rule::default()
        });
    }
}

/// Select the compile settings that the `ClCompile` item definitions use, as
/// CMake selects the language for them.
///
/// The linker language is used when it is C or C++. Otherwise C++, then C,
/// is used when the target compiles it.
fn cl_compile_language<'s>(
    link_language: Option<&[u8]>,
    mut languages: impl Iterator<Item = &'s IdeCompileSettings> + Clone,
) -> Option<&'s IdeCompileSettings> {
    if let Some(language) = link_language.filter(|language| matches!(*language, b"C" | b"CXX"))
        && let Some(settings) = languages
            .clone()
            .find(|settings| settings.language == language)
    {
        return Some(settings);
    }
    [b"CXX".as_slice(), b"C"]
        .into_iter()
        .find_map(|language| {
            languages
                .clone()
                .find(|settings| settings.language == language)
        })
        .or_else(|| languages.next())
}

/// Count the entries that the resolver writes first in the target settings
/// of `language`: one for each non-empty `CMAKE_<LANG>_FLAGS` and
/// `CMAKE_<LANG>_FLAGS_<CONFIG>` value.
fn language_flag_count(
    model: &ProjectModel,
    target: &Target,
    language: &[u8],
    config: &str,
) -> usize {
    let base = [b"CMAKE_".as_slice(), language, b"_FLAGS"].concat();
    let configured = [
        base.as_slice(),
        b"_",
        config.to_ascii_uppercase().as_bytes(),
    ]
    .concat();
    [base, configured]
        .iter()
        .filter(|key| {
            model
                .definition(target.dir, key.as_slice())
                .is_some_and(|value| value.iter().any(|byte| !byte.is_ascii_whitespace()))
        })
        .count()
}

/// Select the `CompileAs` value of a C or C++ source, as CMake does.
///
/// CMake forces the language only when the source language is not the
/// language of the file extension. MSBuild selects the language of the
/// other sources from their extension.
fn compile_as(language: &str, path: &str) -> Option<&'static str> {
    let force = language != source_language(path);
    match language {
        "C" if force => Some("CompileAsC"),
        "CXX" if force => Some("CompileAsCpp"),
        _ => None,
    }
}

pub(super) type CompileUnits<'a> = HashMap<&'a [u8], &'a [u8]>;

pub(super) fn build_compile_units(fact: Option<&ResolvedTarget>) -> Option<CompileUnits<'_>> {
    let units = &fact?.compile_units;
    if units.is_empty() {
        return None;
    }
    let mut map = HashMap::with_capacity(units.len());
    for unit in units {
        map.entry(unit.source.value.as_slice())
            .or_insert(unit.object.as_slice());
    }
    Some(map)
}

fn object_path(
    project: &Project,
    settings: &Settings,
    model: &ProjectModel,
    source: &str,
    config: &str,
    fact: Option<&ResolvedTarget>,
    compile_units: Option<&HashMap<&[u8], &[u8]>>,
) -> String {
    if let Some(&object) = compile_units.and_then(|units| units.get(source.as_bytes())) {
        let file = absolute(&settings.bin, &string(object));
        if file.to_ascii_lowercase().ends_with(".obj") {
            return file;
        }
    }
    let id = guid::bare(&guid::for_name(model, &settings.bin, source));
    let dir = fact
        .and_then(|fact| fact.artifacts.object_dir.as_ref())
        .map(|dir| absolute(&settings.bin, &string(dir)))
        .unwrap_or_else(|| intermediate(project, config));
    join(&dir, &format!("{id}.obj"))
}

fn intermediate(project: &Project, config: &str) -> String {
    join(&project.dir, &format!("{}.dir/{config}", project.name))
}

fn source_kind(path: &str, otherwise: Kind) -> Kind {
    match path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "c" | "cc" | "cpp" | "cxx" | "c++" | "ixx" | "cppm" => Kind::Compile,
        "cu" => Kind::Cuda,
        "nasm" => Kind::Nasm,
        "h" | "hh" | "hpp" | "hxx" | "inl" => Kind::Header,
        "rc" => Kind::Resource,
        "obj" | "o" => Kind::Object,
        _ => otherwise,
    }
}

fn visual_studio_compile(settings: &IdeCompileSettings, toolset: &str) -> flags_table::Settings {
    let mut out = flags_table::compile(
        settings
            .flags
            .iter()
            .flat_map(|flag| ide_flags(&string(&flag.value))),
        toolset,
    );
    out.append(include_definitions(settings));
    out
}

fn include_definitions(settings: &IdeCompileSettings) -> flags_table::Settings {
    let mut out = flags_table::Settings::default();
    for define in &settings.defines {
        out.list(
            "PreprocessorDefinitions",
            msbuild_value(&string(&define.value)),
        );
    }
    for include in &settings.includes {
        out.list(
            "AdditionalIncludeDirectories",
            msbuild_value(&windows(&string(&include.path))),
        );
    }
    out
}

pub(crate) fn ide_flags(text: &str) -> Vec<String> {
    if let Some(shell) = text.strip_prefix("SHELL:") {
        flags_table::split(shell)
    } else if text.is_empty() {
        Vec::new()
    } else {
        vec![text.into()]
    }
}
