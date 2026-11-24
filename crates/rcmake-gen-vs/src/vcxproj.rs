//! MSBuild C++ project output.

use std::collections::BTreeMap;

use rcmake_base::{list, path_lex::Lexical, truth};
use rcmake_gen_common::ide::UtilityKind;
use rcmake_model::{ProjectModel, Target, TargetKind, UsageField};
use rcmake_resolve::{
    BuildStep, CustomStep, GenerationResult, LinkFragmentRole, LinkStep, ResolvedTarget,
    ide::{IdeCustomPhase, IdeLinkItem, IdeSourceKind},
};

use crate::{
    Project, Settings, absolute, flags_table, guid, join, msbuild,
    rules::{self, Command, Rule},
    string, windows,
};

mod fallback;
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
            Self::Header => "ClInclude",
            Self::Resource => "ResourceCompile",
            Self::Object => "Object",
            Self::Other => "None",
            Self::Custom => "CustomBuild",
        }
    }

    pub(crate) fn default_group(self) -> &'static str {
        match self {
            Self::Compile | Self::Object => "Source Files",
            Self::Header => "Header Files",
            Self::Resource => "Resource Files",
            Self::Custom => "CMake Rules",
            Self::Other => "",
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
    link: flags_table::Settings,
    events: [Vec<Rule>; 3],
    dir: String,
    name: String,
    extension: String,
    gui: bool,
    link_incremental: Option<String>,
    generate_manifest: Option<String>,
    properties: Vec<(String, String)>,
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

impl Context<'_> {
    pub(crate) fn plan(&self) -> Plan {
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
            if let Some(target) = target {
                config.gui = target
                    .props
                    .get(b"WIN32_EXECUTABLE")
                    .is_some_and(|value| truth::is_on(value));
            }

            if let Some(ide) = fact.and_then(|fact| fact.ide.as_deref()) {
                let compile = |language: &str| {
                    self.compile_settings(
                        target,
                        name,
                        language,
                        ide.compile
                            .iter()
                            .find(|settings| settings.language.as_slice() == language.as_bytes())
                            .map(|settings| {
                                let mut out = flags_table::compile(
                                    settings
                                        .flags
                                        .iter()
                                        .flat_map(|flag| ide_flags(&string(&flag.value))),
                                );
                                for define in &settings.defines {
                                    out.list(
                                        "PreprocessorDefinitions",
                                        msbuild(&string(&define.value)),
                                    );
                                }
                                for include in &settings.includes {
                                    out.list(
                                        "AdditionalIncludeDirectories",
                                        msbuild(&windows(&string(&include.path))),
                                    );
                                }
                                out
                            }),
                    )
                };
                if let Some(settings) = ide.compile.first().filter(|_| ide.compile.len() == 1) {
                    config.compile = compile(&string(&settings.language));
                }
                for source in &ide.sources {
                    let path = self.source_path(&string(&source.path));
                    let (kind, language) = match &source.kind {
                        IdeSourceKind::Compile { language } => {
                            (source_kind(&path, Kind::Compile), string(language))
                        }
                        IdeSourceKind::Header => (Kind::Header, String::new()),
                        IdeSourceKind::Resource => (source_kind(&path, Kind::Other), String::new()),
                        IdeSourceKind::Object | IdeSourceKind::ExternalObject => {
                            (Kind::Object, String::new())
                        }
                        IdeSourceKind::ListFile | IdeSourceKind::Other => {
                            (Kind::Other, String::new())
                        }
                    };
                    let item = plan.source(path.clone(), kind, &string(&source.group), name);
                    item.excluded = source
                        .excluded_configs
                        .iter()
                        .any(|value| value.as_slice() == name.as_bytes());
                    if kind == Kind::Compile {
                        item.object = self.object_path(&path, name, fact);
                        if ide.compile.len() > 1 {
                            item.compile = Some(compile(&language));
                        }
                        item.language = language;
                    }
                }
                config.link = self.ide_link(ide, name);
                for (index, step) in ide.custom.iter().enumerate() {
                    let rule = Rule::from(step);
                    match step.phase {
                        IdeCustomPhase::Rule => self.custom(&mut plan, name, index, None, rule),
                        IdeCustomPhase::PreBuild => config.events[0].push(rule),
                        IdeCustomPhase::PreLink => config.events[1].push(rule),
                        IdeCustomPhase::PostBuild => config.events[2].push(rule),
                    }
                }
                for (key, value) in &ide.attributes {
                    let key = string(key);
                    if let Some(property) =
                        key.strip_prefix("VS_GLOBAL_").filter(|key| xml_name(key))
                    {
                        config.properties.push((property.into(), string(value)));
                    }
                }
            } else {
                self.fallback(&mut plan, &mut config, fact, &steps, link, name);
            }

            if let Some(utility) = self.project.utility {
                self.utility(&mut plan, &mut config, name, utility);
            }
            config.link_incremental = config.link.take("LinkIncremental");
            config.generate_manifest = config.link.take("GenerateManifest");
            plan.configs.insert(name.clone(), config);
        }
        plan
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

    fn compile_settings(
        &self,
        target: Option<&Target>,
        config: &str,
        language: &str,
        resolved: Option<flags_table::Settings>,
    ) -> flags_table::Settings {
        if let Some(resolved) = resolved {
            return resolved;
        }
        let Some(target) = target else {
            return flags_table::Settings::default();
        };
        let flag_key = format!("CMAKE_{language}_FLAGS");
        let config_key = format!("{flag_key}_{}", config.to_ascii_uppercase());
        let dir = self.model.dir(target.dir);
        let flags = [flag_key, config_key]
            .into_iter()
            .filter_map(|key| self.model.definition(target.dir, key.as_bytes()))
            .flat_map(|value| flags_table::split(&string(value)))
            .chain(
                [&dir.usage, &target.usage]
                    .into_iter()
                    .flat_map(|usage| usage.entries(UsageField::CompileOptions))
                    .flat_map(|entry| ide_flags(&string(&entry.value))),
            );
        let mut out = flags_table::compile(flags);
        for usage in [&dir.usage, &target.usage] {
            for entry in usage.entries(UsageField::CompileDefinitions) {
                out.list("PreprocessorDefinitions", msbuild(&string(&entry.value)));
            }
            for entry in usage.entries(UsageField::IncludeDirectories) {
                out.list(
                    "AdditionalIncludeDirectories",
                    msbuild(&windows(&self.source_path(&string(&entry.value)))),
                );
            }
        }
        if let Some(standard) = target
            .props
            .get(b"CXX_STANDARD")
            .filter(|_| language == "CXX")
        {
            let value = match standard.as_slice() {
                b"14" => "stdcpp14",
                b"17" => "stdcpp17",
                b"20" => "stdcpp20",
                b"23" => "stdcpp23",
                _ => "stdcpplatest",
            };
            out.set("LanguageStandard", value);
        }
        out
    }

    fn ide_link(&self, ide: &rcmake_resolve::ide::IdeFacts, config: &str) -> flags_table::Settings {
        let archive = self.project.kind == TargetKind::StaticLibrary;
        let mut flags: Vec<_> = ide
            .link_options
            .iter()
            .flat_map(|flag| ide_flags(&string(&flag.value)))
            .collect();
        let mut libraries = Vec::new();
        for item in &ide.link_items {
            match item {
                IdeLinkItem::Target(id) => {
                    if !archive && let Some(file) = self.target_library(*id, config) {
                        libraries.push(file);
                    }
                    libraries.extend(self.target_objects(*id, config));
                }
                IdeLinkItem::File(file)
                    if !archive || source_kind(&string(file), Kind::Other) == Kind::Object =>
                {
                    libraries.push(windows(&string(file)));
                }
                IdeLinkItem::Library(name) if !archive => libraries.push(library(&string(name))),
                IdeLinkItem::Flag(flag) => flags.push(string(flag)),
                _ => {}
            }
        }
        let mut out = flags_table::link(flags, archive);
        for library in libraries {
            out.list("AdditionalDependencies", msbuild(&library));
        }
        for dir in &ide.link_directories {
            out.list(
                "AdditionalLibraryDirectories",
                msbuild(&windows(&string(dir))),
            );
        }
        out
    }

    fn target_library(&self, id: rcmake_model::TargetId, config: &str) -> Option<String> {
        let target = self.model.target(id)?;
        if !matches!(
            target.kind,
            TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::UnknownLibrary
        ) {
            return None;
        }
        let resolved = self
            .result
            .targets
            .iter()
            .find(|fact| fact.id == id && fact.config.0.as_slice() == config.as_bytes());
        if let Some(file) = resolved.and_then(|fact| {
            fact.artifacts
                .linker_file
                .as_ref()
                .or(fact.artifacts.import_file.as_ref())
                .or(fact.artifacts.file.as_ref())
        }) {
            return Some(windows(&absolute(&self.settings.bin, &string(file))));
        }
        if target.is_imported() {
            let config = config.to_ascii_uppercase();
            return [
                format!("IMPORTED_IMPLIB_{config}"),
                "IMPORTED_IMPLIB".into(),
                format!("IMPORTED_LOCATION_{config}"),
                "IMPORTED_LOCATION".into(),
            ]
            .into_iter()
            .find_map(|key| target.props.get(key.as_bytes()))
            .map(|file| windows(&string(file)));
        }
        let project = self
            .projects
            .iter()
            .find(|project| project.target == Some(id))?;
        Some(windows(&join(
            &join(&project.dir, config),
            &format!("{}.lib", project.name),
        )))
    }

    fn target_objects(&self, id: rcmake_model::TargetId, config: &str) -> Vec<String> {
        let Some(project) = self.projects.iter().find(|project| {
            project.target == Some(id) && project.kind == TargetKind::ObjectLibrary
        }) else {
            return Vec::new();
        };
        let fact = self
            .result
            .targets
            .iter()
            .find(|fact| fact.id == id && fact.config.0.as_slice() == config.as_bytes());
        let sources: Vec<_> = if let Some(ide) = fact.and_then(|fact| fact.ide.as_deref()) {
            ide.sources
                .iter()
                .filter(|source| {
                    matches!(source.kind, IdeSourceKind::Compile { .. })
                        && !source
                            .excluded_configs
                            .iter()
                            .any(|value| value.as_slice() == config.as_bytes())
                })
                .map(|source| string(&source.path))
                .collect()
        } else if let Some(fact) = fact.filter(|fact| !fact.compile_units.is_empty()) {
            fact.compile_units
                .iter()
                .map(|unit| string(&unit.source.value))
                .collect()
        } else if let Some(graph) = self
            .result
            .graphs
            .iter()
            .find(|graph| graph.config.0.as_slice() == config.as_bytes())
            && let Some(entry) = graph
                .targets
                .iter()
                .find(|entry| entry.name.as_slice() == project.name.as_bytes())
        {
            return entry
                .steps
                .iter()
                .filter_map(|&index| match graph.steps.get(index) {
                    Some(BuildStep::Compile { object, source, .. }) => {
                        let source = absolute(&project.source, &string(source));
                        if source_kind(&source, Kind::Compile) != Kind::Compile {
                            return None;
                        }
                        let object = string(object);
                        let path = if object.to_ascii_lowercase().ends_with(".obj") {
                            absolute(&self.settings.bin, &object)
                        } else {
                            object_path(project, self.settings, self.model, &source, config, fact)
                        };
                        Some(windows(&path))
                    }
                    _ => None,
                })
                .collect();
        } else {
            self.model
                .target(id)
                .into_iter()
                .flat_map(|target| &target.sources)
                .flat_map(|entry| list::split(&entry.value, list::Empty::Drop))
                .map(|path| string(&path))
                .filter(|path| source_kind(path, Kind::Other) == Kind::Compile)
                .collect()
        };
        sources
            .into_iter()
            .map(|source| {
                let source = absolute(&project.source, &source);
                windows(&object_path(
                    project,
                    self.settings,
                    self.model,
                    &source,
                    config,
                    fact,
                ))
            })
            .collect()
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
            UtilityKind::RunTests => vec![self.settings.ctest.clone(), "-C".into(), name.into()],
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
    join(&intermediate(project, config), &format!("{id}.obj"))
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
        "h" | "hh" | "hpp" | "hxx" | "inl" => Kind::Header,
        "rc" => Kind::Resource,
        "obj" | "o" => Kind::Object,
        _ => otherwise,
    }
}

fn xml_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
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

fn library(value: &str) -> String {
    let value = value.strip_prefix("-l").unwrap_or(value);
    if value.contains(['/', '\\', '.']) {
        windows(value)
    } else {
        format!("{value}.lib")
    }
}

fn invocation_rule(step: &CustomStep) -> Rule {
    Rule {
        commands: step
            .invocations
            .iter()
            .map(|cmd| Command {
                argv: cmd.argv.iter().map(|arg| string(arg)).collect(),
                dir: cmd
                    .working_dir
                    .as_ref()
                    .map_or_else(String::new, |path| string(path)),
            })
            .collect(),
        inputs: step.depends.iter().map(|path| string(path)).collect(),
        outputs: step
            .outputs
            .iter()
            .chain(&step.byproducts)
            .map(|path| string(path))
            .collect(),
        message: step
            .comment
            .as_ref()
            .map_or_else(String::new, |value| string(value)),
        always: step.outputs.is_empty(),
    }
}
