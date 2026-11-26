//! MSBuild C++ project output.

use std::collections::BTreeMap;

use rcmake_base::{path_lex::Lexical, truth};
use rcmake_gen_common::ide::UtilityKind;
use rcmake_model::{ProjectModel, Target, TargetKind};
use rcmake_resolve::{
    BuildStep, GenerationResult, LinkStep, ResolvedTarget,
    ide::{IdeCompileSettings, IdeCustomPhase, IdeSourceKind},
};

use crate::{
    GenError, Project, Settings, absolute, flags_table, guid, join, msbuild, msbuild_value,
    rules::{self, Command, Rule},
    string, windows,
};

mod extensions;
mod fallback;
mod link;
mod pch;
mod render;

use fallback::source_language;
pub(crate) use render::render;

pub(crate) struct Context<'a> {
    pub(crate) project: &'a Project,
    pub(crate) projects: &'a [Project],
    pub(crate) settings: &'a Settings,
    pub(crate) facts: Option<&'a BTreeMap<String, &'a ResolvedTarget>>,
    pub(crate) result: &'a GenerationResult,
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

    fn compiles(self) -> bool {
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
    configs: BTreeMap<String, SourceConfig>,
}

#[derive(Default)]
struct SourceConfig {
    excluded: bool,
    language: String,
    object: String,
    compile: Option<flags_table::Settings>,
    tool_compile: Option<IdeCompileSettings>,
    rule: Option<Rule>,
}

#[derive(Default)]
pub(crate) struct Plan {
    pub(crate) items: BTreeMap<(Kind, String), Item>,
    configs: BTreeMap<String, Config>,
}

#[derive(Default)]
struct Config {
    compile: flags_table::Settings,
    resource: flags_table::Settings,
    link: flags_table::Settings,
    link_output_dirs: Vec<String>,
    events: [Vec<Rule>; 3],
    dir: String,
    int_dir: String,
    name: String,
    extension: String,
    gui: bool,
    link_incremental: Option<String>,
    generate_manifest: Option<String>,
    debugger: Vec<(&'static str, String)>,
    dpi_aware: Option<&'static str>,
    cuda: Option<extensions::CudaConfig>,
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
            });
        item.configs.entry(config.into()).or_default()
    }

    fn rule(&mut self, path: String, synthetic: bool, rule: Rule, config: &str) {
        self.source(path.clone(), Kind::Custom, "", config).rule = Some(rule);
        if let Some(item) = self.items.get_mut(&(Kind::Custom, path)) {
            item.synthetic = synthetic;
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

impl Context<'_> {
    pub(crate) fn plan(&self) -> Result<Plan, GenError> {
        let mut plan = Plan::default();
        let target = self.project.target.and_then(|id| self.model.target(id));
        for name in &self.settings.configs {
            let mut config = Config::default();
            let fact = self.facts.and_then(|facts| facts.get(name)).copied();
            let graph = self
                .result
                .graphs
                .iter()
                .find(|graph| graph.config.0.as_slice() == name.as_bytes());
            let steps = graph
                .into_iter()
                .flat_map(|graph| {
                    graph
                        .targets
                        .iter()
                        .find(|target| target.name.as_slice() == self.project.name.as_bytes())
                        .into_iter()
                        .flat_map(|target| {
                            target
                                .steps
                                .iter()
                                .filter_map(|&index| graph.steps.get(index))
                        })
                })
                .collect::<Vec<_>>();

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
                let use_common = languages.clone().count() == 1;
                if use_common && let Some(settings) = languages.clone().next() {
                    config.compile = target_compile(settings);
                }
                if languages.clone().next().is_some() {
                    config.compile.compiler_defaults(&self.settings.toolset);
                    config.compile.list(
                        "PreprocessorDefinitions",
                        r#"CMAKE_INTDIR="$(Configuration)""#,
                    );
                }
                for source in &ide.sources {
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
                        item.object = self.object_path(&path, name, fact);
                        let base = (!use_common)
                            .then(|| {
                                ide.compile.iter().find(|settings| {
                                    settings.language.as_slice() == language.as_bytes()
                                })
                            })
                            .flatten();
                        item.compile = match (base, &source.compile) {
                            (Some(settings), overrides) => {
                                let mut compiled = target_compile(settings);
                                if let Some(overrides) = overrides {
                                    compiled.append(visual_studio_compile(
                                        overrides,
                                        &self.settings.toolset,
                                    ));
                                }
                                Some(compiled)
                            }
                            (None, overrides) => overrides.as_ref().map(|settings| {
                                visual_studio_compile(settings, &self.settings.toolset)
                            }),
                        };
                        pch::configure_source(item, source, ide, language.as_bytes());
                        item.language = language;
                    } else if kind == Kind::Resource {
                        item.compile = source.compile.as_ref().map(|settings| {
                            visual_studio_compile(settings, &self.settings.toolset)
                        });
                    } else if kind.compiles() {
                        item.object = self.object_path(&path, name, fact);
                        item.tool_compile.clone_from(&source.compile);
                    }
                }
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
                self.fallback(&mut plan, &mut config, fact, &steps, link, name);
            }
            self.windows_link(&plan, &mut config, fact, name);
            self.toolchain_settings(&plan, &mut config, fact, name)?;
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
                    return Err(GenError::Invalid(
                        format!("Bad parameter for VS_DPI_AWARE: {}", string(value)).into(),
                    ));
                }
            };

            if let Some(utility) = self.project.utility {
                self.utility(&mut plan, &mut config, name, utility);
            }
            config.link_incremental = config.link.take("LinkIncremental");
            config.generate_manifest = config.link.take("GenerateManifest");
            plan.configs.insert(name.clone(), config);
        }
        self.apply_source_groups(&mut plan, target);
        Ok(plan)
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

    fn object_path(&self, source: &str, config: &str, fact: Option<&ResolvedTarget>) -> String {
        object_path(
            self.project,
            self.settings,
            self.model,
            source,
            config,
            fact,
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
                let list = join(&self.settings.source, "CMakeLists.txt");
                plan.rule(
                    list,
                    false,
                    rules::regeneration(self.settings, self.project, self.inputs),
                    name,
                );
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

fn object_path(
    project: &Project,
    settings: &Settings,
    model: &ProjectModel,
    source: &str,
    config: &str,
    fact: Option<&ResolvedTarget>,
) -> String {
    if let Some(unit) = fact.and_then(|fact| {
        fact.compile_units
            .iter()
            .find(|unit| unit.source.value.as_slice() == source.as_bytes())
    }) {
        let file = absolute(&settings.bin, &string(&unit.object));
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

fn ide_flags(text: &str) -> Vec<String> {
    if let Some(shell) = text.strip_prefix("SHELL:") {
        flags_table::split(shell)
    } else if text.is_empty() {
        Vec::new()
    } else {
        vec![text.into()]
    }
}
