//! Convert resolved build facts to an Xcode project.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use rcmake_base::truth;
use rcmake_gen_common::{
    GenError,
    ide::{
        IdeProject, UtilityKind, UtilityTarget, project_roots, targets_for_root, utility_targets,
    },
};
use rcmake_host::{Host, ReadRange, path::to_native};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{
    GenerationResult, ResolvedTarget,
    ide::{BundleKind, IdeSourceKind},
};

use crate::{
    file_name, ids,
    object::{Attrs, Isa, Objects, Reference, Value, attrs, list},
    phases, plist, scheme,
    settings::{self, Product, TargetSettings},
    text, workspace, write_file,
};

struct Project<'a> {
    result: &'a GenerationResult,
    model: &'a ProjectModel,
    host: &'a dyn Host,
    bin: &'a Path,
    path: PathBuf,
    configs: Vec<String>,
    default_config: String,
    arch: String,
    upgrade: String,
    objects: Objects,
    resolved: BTreeMap<(TargetId, String), &'a ResolvedTarget>,
}

struct TargetEntry<'a> {
    id: TargetId,
    raw: &'a Target,
    reference: Reference,
    isa: Isa,
}

pub(crate) fn cmake(result: &GenerationResult, model: &ProjectModel) -> String {
    command(
        model,
        result.metadata.cmake_command.as_ref(),
        b"CMAKE_COMMAND",
        "rcmake",
    )
}

fn command(
    model: &ProjectModel,
    value: Option<&rcmake_base::BString>,
    key: &[u8],
    fallback: &str,
) -> String {
    value
        .or_else(|| model.definition(DirId::ROOT, key))
        .filter(|value| !value.is_empty())
        .map_or_else(|| fallback.into(), |value| text(value))
}

fn target_key(name: &str) -> String {
    format!("target/{name}")
}
fn target_cache_key(name: &str) -> String {
    format!("{name}_GUID_CMAKE")
}
fn project_key(name: &str) -> String {
    format!("project/{name}")
}
fn project_cache_key(name: &str) -> String {
    format!("PROJECT_{name}_GUID_CMAKE")
}

fn target_type(target: &Target, result: &GenerationResult, id: TargetId) -> Option<Isa> {
    match target.kind {
        TargetKind::UnknownLibrary => None,
        TargetKind::InterfaceLibrary
            if target.sources.is_empty()
                && target.file_sets.is_empty()
                && !result.targets.iter().any(|t| {
                    t.id == id && t.ide.as_ref().is_some_and(|f| !f.sources.is_empty())
                }) =>
        {
            None
        }
        TargetKind::Utility | TargetKind::GlobalTarget | TargetKind::InterfaceLibrary => {
            Some(Isa::PBXAggregateTarget)
        }
        _ => Some(Isa::PBXNativeTarget),
    }
}

pub(crate) fn populate_guid_cache(
    result: &GenerationResult,
    model: &mut ProjectModel,
    host: &dyn Host,
) {
    for project in project_roots(GeneratorKind::Xcode, model) {
        let name = text(&project.name);
        ids::store(
            &mut model.cache,
            project_cache_key(&name),
            Isa::PBXProject,
            &project_key(&name),
        );
    }
    let mut targets = model
        .targets()
        .filter_map(|(id, target)| {
            let isa = target_type(target, result, id).filter(|_| !target.is_imported())?;
            Some((text(&target.name), isa))
        })
        .collect::<Vec<_>>();
    targets.extend(
        utility_targets(GeneratorKind::Xcode, model, host)
            .iter()
            .map(|target| (text(&target.name), Isa::PBXAggregateTarget)),
    );
    for (name, isa) in targets {
        ids::store(
            &mut model.cache,
            target_cache_key(&name),
            isa,
            &target_key(&name),
        );
    }
}

pub(crate) fn generate(
    result: &GenerationResult,
    model: &ProjectModel,
    host: &dyn Host,
    bin: &Path,
    out: &Path,
    root: &IdeProject,
    utilities: &[UtilityTarget],
) -> Result<(), GenError> {
    let name = text(&root.name);
    let path = out.join(format!("{}.xcodeproj", file_name(&name)?));
    let configs = if result.configs.is_empty() {
        GeneratorKind::Xcode
            .default_configuration_types()
            .iter()
            .map(|s| (*s).into())
            .collect()
    } else {
        result
            .configs
            .iter()
            .map(|cfg| text(&cfg.0))
            .collect::<Vec<_>>()
    };
    let default_config = configs
        .iter()
        .find(|name| *name == "Debug")
        .or_else(|| configs.first())
        .cloned()
        .unwrap_or_default();
    let arch = text(&host.system().machine);
    let upgrade = model.definition(DirId::ROOT, b"XCODE_VERSION").map_or_else(
        || "1200".into(),
        |version| {
            let version = text(version);
            let mut parts = version.split('.');
            let major = parts
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .unwrap_or(12);
            let minor = parts
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .unwrap_or(0);
            let patch = parts
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .unwrap_or(0);
            format!(
                "{:04}",
                major
                    .saturating_mul(100)
                    .saturating_add(minor.saturating_mul(10))
                    .saturating_add(patch)
            )
        },
    );
    let mut project = Project {
        result,
        model,
        host,
        bin,
        path,
        configs,
        default_config,
        arch,
        upgrade,
        objects: Objects::default(),
        resolved: result
            .targets
            .iter()
            .map(|t| ((t.id, text(&t.config.0)), t))
            .collect(),
    };
    project.write(root, &name, out, utilities)
}

impl Project<'_> {
    fn write(
        &mut self,
        root: &IdeProject,
        name: &str,
        out: &Path,
        utilities: &[UtilityTarget],
    ) -> Result<(), GenError> {
        let key = project_key(name);
        let project_ref = self.objects.reserve(
            Isa::PBXProject,
            &key,
            "Project object",
            ids::cached(&self.model.cache, &project_cache_key(name)),
        )?;
        let mut targets = Vec::new();
        for id in targets_for_root(self.model, self.result, root.dir) {
            if let Some(raw) = self.model.target(id)
                && let Some(isa) = target_type(raw, self.result, id)
            {
                let reference = self.reserve_target(isa, &text(&raw.name))?;
                targets.push(TargetEntry {
                    id,
                    raw,
                    reference,
                    isa,
                });
            }
        }
        let mut utility_refs = Vec::new();
        for utility in utilities {
            utility_refs.push((
                utility.kind,
                self.reserve_target(Isa::PBXAggregateTarget, &text(&utility.name))?,
            ));
        }
        let all = utility_refs
            .iter()
            .find(|(kind, _)| *kind == UtilityKind::AllBuild)
            .map(|(_, r)| r);
        let zero = utility_refs
            .iter()
            .find(|(kind, _)| *kind == UtilityKind::ZeroCheck)
            .map(|(_, r)| r);
        let mut products = Vec::new();
        let mut tree = Group::default();
        for target in &targets {
            let (group, product) = self.target(target, &targets, &project_ref, zero)?;
            let folder = target
                .raw
                .props
                .get(b"FOLDER")
                .map_or_else(String::new, |s| text(s));
            tree.add(&folder, group)?;
            products.extend(product);
        }
        let products = group(&mut self.objects, "group/products", "Products", products)?;
        let listfile = to_native(&self.model.dir(root.dir).source_dir).join("CMakeLists.txt");
        let listfile = file_ref(
            &mut self.objects,
            "project/listfile",
            &listfile.to_string_lossy(),
            "sourcecode.text",
            "<absolute>",
        )?;
        tree.files.push(group(
            &mut self.objects,
            "group/rules",
            "CMake Rules",
            vec![listfile],
        )?);
        tree.files.push(products.clone());
        let main = tree.finish(&mut self.objects, "group/main", "")?;
        let cmake = cmake(self.result, self.model);
        let ctest = command(
            self.model,
            self.result.metadata.ctest_command.as_ref(),
            b"CMAKE_CTEST_COMMAND",
            "rcmake-ctest",
        );
        let cpack = command(self.model, None, b"CMAKE_CPACK_COMMAND", "rcmake-cpack");
        for (kind, reference) in &utility_refs {
            let key = target_key(&reference.comment);
            let mut dependencies = Vec::new();
            if *kind != UtilityKind::ZeroCheck {
                dependencies.extend(zero);
            }
            match kind {
                UtilityKind::AllBuild => dependencies.extend(
                    targets
                        .iter()
                        .filter(|target| self.in_all(target))
                        .map(|target| &target.reference),
                ),
                UtilityKind::Install | UtilityKind::Package => dependencies.extend(all),
                _ => {}
            }
            let deps = self.dependencies(&key, &project_ref, dependencies)?;
            let script = phases::utility(
                *kind,
                self.bin,
                if *kind == UtilityKind::ZeroCheck {
                    self.bin
                } else {
                    out
                },
                &self.configs,
                &cmake,
                &ctest,
                &cpack,
            );
            let phase = script.shell(
                &mut self.objects,
                &format!("{key}/phase/utility"),
                &format!("Generate CMakeFiles/{}", reference.comment),
                &self.configs,
            )?;
            let config_list = self.config_list(
                &key,
                &reference.comment,
                Isa::PBXAggregateTarget,
                self.configs.iter().map(|_| Attrs::new()).collect(),
            )?;
            self.objects.insert(
                Isa::PBXAggregateTarget,
                reference,
                target_fields(reference, config_list, phase, deps),
            );
        }
        let mut project_targets = targets
            .into_iter()
            .map(|target| target.reference)
            .chain(utility_refs.into_iter().map(|(_, reference)| reference))
            .collect::<Vec<_>>();
        project_targets.sort_by_cached_key(|r| (r.comment.to_ascii_lowercase(), r.comment.clone()));
        let config_list = self.config_list(
            &key,
            name,
            Isa::PBXProject,
            self.configs
                .iter()
                .map(|config| settings::project(self.model, root.dir, self.bin, &self.arch, config))
                .collect(),
        )?;
        self.objects.insert(
            Isa::PBXProject,
            &project_ref,
            attrs([
                (
                    "attributes",
                    Value::Dict(attrs([
                        ("BuildIndependentTargetsInParallel", "YES".into()),
                        ("LastUpgradeCheck", self.upgrade.clone().into()),
                    ])),
                ),
                ("buildConfigurationList", config_list.into()),
                ("compatibilityVersion", "Xcode 3.2".into()),
                ("developmentRegion", "English".into()),
                ("hasScannedForEncodings", "0".into()),
                ("knownRegions", list(["en", "Base"])),
                ("mainGroup", main.into()),
                ("productRefGroup", products.into()),
                (
                    "projectDirPath",
                    text(&self.model.dir(root.dir).source_dir).into(),
                ),
                ("projectRoot", "".into()),
                ("targets", list(project_targets)),
            ]),
        );
        write_file(
            self.host,
            &self.path.join("project.pbxproj"),
            &plist::encode(&self.objects, &project_ref),
        )?;
        workspace::write(self.host, &self.path)
    }

    fn reserve_target(&mut self, isa: Isa, name: &str) -> Result<Reference, GenError> {
        file_name(name)?;
        self.objects.reserve(
            isa,
            &target_key(name),
            name,
            ids::cached(&self.model.cache, &target_cache_key(name)),
        )
    }

    fn in_all(&self, target: &TargetEntry<'_>) -> bool {
        if let Some(in_all) = self
            .result
            .graphs
            .first()
            .and_then(|g| g.targets.iter().find(|t| t.name == target.raw.name))
            .map(|t| t.in_all)
        {
            return in_all;
        }
        if settings::on(target.raw, b"EXCLUDE_FROM_ALL") {
            return false;
        }
        let mut dir = target.raw.dir;
        loop {
            let directory = self.model.dir(dir);
            if directory.exclude_from_all {
                return false;
            }
            let Some(parent) = directory
                .parent
                .filter(|parent| parent.index() < dir.index())
            else {
                break;
            };
            dir = parent;
        }
        true
    }

    fn config_list(
        &mut self,
        key: &str,
        name: &str,
        isa: Isa,
        settings: Vec<Attrs>,
    ) -> Result<Reference, GenError> {
        let configs = self
            .configs
            .iter()
            .zip(settings)
            .map(|(config, settings)| {
                self.objects.add(
                    Isa::XCBuildConfiguration,
                    &format!("{key}/config/{config}"),
                    config,
                    attrs([
                        ("buildSettings", Value::Dict(settings)),
                        ("name", config.as_str().into()),
                    ]),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.objects.add(
            Isa::XCConfigurationList,
            &format!("{key}/configs"),
            &format!("Build configuration list for {} \"{name}\"", isa.name()),
            attrs([
                ("buildConfigurations", list(configs)),
                ("defaultConfigurationIsVisible", "0".into()),
                (
                    "defaultConfigurationName",
                    self.default_config.clone().into(),
                ),
            ]),
        )
    }

    fn dependencies<'a>(
        &mut self,
        key: &str,
        project: &Reference,
        refs: impl IntoIterator<Item = &'a Reference>,
    ) -> Result<Vec<Reference>, GenError> {
        let mut seen = BTreeSet::new();
        let mut dependencies = Vec::new();
        for target in refs {
            if !seen.insert(&target.id) {
                continue;
            }
            let dep_key = format!("{key}/dep/{}", target.comment);
            let proxy = self.objects.add(
                Isa::PBXContainerItemProxy,
                &format!("{dep_key}/proxy"),
                "PBXContainerItemProxy",
                attrs([
                    ("containerPortal", project.clone().into()),
                    ("proxyType", "1".into()),
                    ("remoteGlobalIDString", target.id.clone().into()),
                    ("remoteInfo", target.comment.clone().into()),
                ]),
            )?;
            dependencies.push(self.objects.add(
                Isa::PBXTargetDependency,
                &dep_key,
                "PBXTargetDependency",
                attrs([
                    ("target", target.clone().into()),
                    ("targetProxy", proxy.into()),
                ]),
            )?);
        }
        Ok(dependencies)
    }

    fn target(
        &mut self,
        target: &TargetEntry<'_>,
        all: &[TargetEntry<'_>],
        project: &Reference,
        zero: Option<&Reference>,
    ) -> Result<(Reference, Option<Reference>), GenError> {
        let TargetEntry {
            id,
            raw,
            reference,
            isa,
        } = target;
        let name = &reference.comment;
        let key = target_key(name);
        let mut custom = phases::Custom::default();
        let mut dependencies = BTreeSet::new();
        let mut target_settings = Vec::new();
        let mut sources: Option<(Vec<Source>, String)> = None;
        let native = matches!(isa, Isa::PBXNativeTarget);
        let first = self.configs.first().cloned().unwrap_or_default();
        let first_target = self.resolved.get(&(*id, first.clone())).copied();
        let product = Product::new(self.model, raw, first_target, &first, self.bin);
        let plist = if native {
            self.info_plist(raw, name, &product, first_target)?
        } else {
            None
        };
        for config in &self.configs {
            let facts = self.resolved.get(&(*id, config.clone())).copied();
            let configured_product = Product::new(self.model, raw, facts, config, self.bin);
            custom.post.version_links(config, &configured_product)?;
            let graph = self
                .result
                .graphs
                .iter()
                .find(|g| g.config.0.as_slice() == config.as_bytes());
            if let Some(facts) = facts {
                dependencies.extend(facts.dependencies.iter().copied());
                custom.collect(self.model, raw, facts, graph, self.bin);
                let items = source_files(self.model, raw, facts, self.bin);
                if let Some((original, original_config)) = &sources {
                    if native && !same_sources(original, &items) {
                        return Err(GenError::Invalid(format!(
                            "Target \"{name}\" has source files which vary by configuration. This is not supported by the \"Xcode\" generator.\n\n  Config \"{original_config}\":\n\n{}\n\n  Config \"{config}\":\n\n{}",
                            source_lines(original), source_lines(&items),
                        ).into()));
                    }
                } else {
                    sources = Some((items, config.clone()));
                }
            }
            if let Some(graph) = graph
                && let Some(item) = graph.targets.iter().find(|item| item.name == raw.name)
            {
                for index in &item.depends {
                    if let Some(dep) = graph.targets.get(*index)
                        && let Some(binding) = self.model.find_target(raw.dir, &dep.name)
                    {
                        dependencies.insert(binding);
                    }
                }
            }
            let settings = if native {
                TargetSettings {
                    model: self.model,
                    result: self.result,
                    bin: self.bin,
                    config,
                    arch: &self.arch,
                    raw,
                    target: facts,
                }
                .build(&configured_product, plist.as_deref())
            } else {
                Attrs::new()
            };
            target_settings.push(settings);
        }
        for dep in &raw.utility_deps {
            if let Some(binding) = self.model.find_target(raw.dir, &dep.value) {
                dependencies.insert(binding);
            }
        }
        let dependencies = self.dependencies(
            &key,
            project,
            zero.into_iter().chain(
                all.iter()
                    .filter(|dep| dep.id != *id && dependencies.contains(&dep.id))
                    .map(|dep| &dep.reference),
            ),
        )?;
        let source_settings = target_settings
            .iter()
            .flat_map(|settings| settings.keys())
            .filter(|key| key.starts_with("RCMAKE_SOURCE_FLAGS_"))
            .cloned()
            .collect::<BTreeSet<_>>();
        let config_list = self.config_list(&key, name, *isa, target_settings)?;
        let configs = self.configs.clone();
        let phase = |objects: &mut Objects, script: phases::Script, role: &str, label: &str| {
            script.shell(objects, &format!("{key}/phase/{role}"), label, &configs)
        };
        let mut build_phases = Vec::new();
        build_phases.extend(phase(
            &mut self.objects,
            custom.pre,
            "pre",
            "CMake PreBuild Rules",
        )?);
        let rule_name = if native {
            name.clone()
        } else {
            format!("Generate CMakeFiles/{name}")
        };
        build_phases.extend(phase(&mut self.objects, custom.rules, "rules", &rule_name)?);
        let mut tree = Group::default();
        let mut files = BuildFiles::default();
        for file in sources.into_iter().flat_map(|(items, _)| items) {
            let path = Path::new(&file.path);
            let source_dir = to_native(&self.model.dir(DirId::ROOT).source_dir);
            let file_key = path
                .strip_prefix(source_dir)
                .unwrap_or(path)
                .to_string_lossy();
            let build_path = path.strip_prefix(self.bin).ok();
            let file_key = build_path.map_or_else(
                || file_key.into_owned(),
                |p| format!("generated/{}", p.display()),
            );
            let file_key = format!("{key}/file/{file_key}");
            let file_type = file_type(&file);
            let file_ref = file_ref(
                &mut self.objects,
                &file_key,
                &file.path,
                file_type,
                "<absolute>",
            )?;
            let folder = if file.group.is_empty() {
                match file.kind {
                    IdeSourceKind::Header => "Header Files",
                    IdeSourceKind::Resource => "Resources",
                    IdeSourceKind::ListFile => "CMake Rules",
                    _ => "Source Files",
                }
            } else {
                &file.group
            };
            tree.add(folder, file_ref.clone())?;
            if native {
                let phase = match file.kind {
                    IdeSourceKind::Compile { .. } => Some((&mut files.sources, "Sources")),
                    IdeSourceKind::Header
                        if matches!(product.bundle, BundleKind::Framework { .. }) =>
                    {
                        Some((&mut files.headers, "Headers"))
                    }
                    IdeSourceKind::Resource if product.bundle != BundleKind::None => {
                        Some((&mut files.resources, "Resources"))
                    }
                    IdeSourceKind::Object | IdeSourceKind::ExternalObject => {
                        Some((&mut files.frameworks, "Frameworks"))
                    }
                    _ => None,
                };
                if let Some((items, label)) = phase {
                    let mut build_settings = Attrs::new();
                    if let Some(visibility) = file.visibility {
                        build_settings.insert("ATTRIBUTES".into(), list([visibility]));
                    }
                    if matches!(file.kind, IdeSourceKind::Compile { .. }) {
                        let setting = settings::source_flag_setting(&file.path);
                        if source_settings.contains(&setting) {
                            build_settings
                                .insert("COMPILER_FLAGS".into(), format!("$({setting})").into());
                        }
                    }
                    items.push(phases::build_file(
                        &mut self.objects,
                        &format!("{file_key}/build"),
                        label,
                        &file_ref,
                        build_settings,
                    )?);
                }
            }
        }
        let mut product_ref = None;
        if native {
            build_phases.push(phases::files(
                &mut self.objects,
                Isa::PBXSourcesBuildPhase,
                &format!("{key}/phase/sources"),
                "Sources",
                files.sources,
            )?);
            build_phases.extend(phase(
                &mut self.objects,
                custom.link,
                "prelink",
                "CMake PreLink Rules",
            )?);
            build_phases.push(phases::files(
                &mut self.objects,
                Isa::PBXFrameworksBuildPhase,
                &format!("{key}/phase/frameworks"),
                "Frameworks",
                files.frameworks,
            )?);
            if !files.headers.is_empty() {
                build_phases.push(phases::files(
                    &mut self.objects,
                    Isa::PBXHeadersBuildPhase,
                    &format!("{key}/phase/headers"),
                    "Headers",
                    files.headers,
                )?);
            }
            if !files.resources.is_empty() {
                build_phases.push(phases::files(
                    &mut self.objects,
                    Isa::PBXResourcesBuildPhase,
                    &format!("{key}/phase/resources"),
                    "Resources",
                    files.resources,
                )?);
            }
            product_ref = Some(file_ref(
                &mut self.objects,
                &format!("product/{name}"),
                &product.file(),
                product.types().1,
                "BUILT_PRODUCTS_DIR",
            )?);
            build_phases.extend(self.embeds(&key, raw)?);
        }
        build_phases.extend(phase(
            &mut self.objects,
            custom.post,
            "post",
            "CMake PostBuild Rules",
        )?);
        let mut fields = target_fields(reference, config_list, build_phases, dependencies);
        if let Some(product_ref) = &product_ref {
            fields.extend(attrs([
                ("buildRules", list(Vec::<Value>::new())),
                ("productReference", product_ref.clone().into()),
                (
                    "productType",
                    format!("com.apple.product-type.{}", product.types().0).into(),
                ),
            ]));
        }
        self.objects.insert(*isa, reference, fields);
        let enabled = raw.props.get(b"XCODE_GENERATE_SCHEME").or_else(|| {
            self.model
                .definition(raw.dir, b"CMAKE_XCODE_GENERATE_SCHEME")
        });
        if enabled.is_some_and(|value| truth::is_on(value)) {
            let filename = product_ref
                .as_ref()
                .map_or_else(|| name.clone(), |_| product.file());
            scheme::write(
                self.host,
                &self.path,
                raw,
                reference,
                &filename,
                &self.default_config,
                &self.upgrade,
            )?;
        }
        let group = tree.finish(&mut self.objects, &format!("{key}/group"), name)?;
        Ok((group, product_ref))
    }

    fn embeds(&mut self, key: &str, raw: &Target) -> Result<Vec<Reference>, GenError> {
        let mut phases = Vec::new();
        for (kind, destination, label) in [
            ("FRAMEWORKS", "10", "Embed Frameworks"),
            ("PLUGINS", "13", "Embed PlugIns"),
            ("APP_EXTENSIONS", "13", "Embed App Extensions"),
            ("XPC_SERVICES", "16", "Embed XPC Services"),
            ("RESOURCES", "7", "Embed Resources"),
        ] {
            let property = format!("XCODE_EMBED_{kind}");
            let Some(items) = raw.props.get(property.as_bytes()) else {
                continue;
            };
            let key = format!("{key}/embed/{kind}");
            let mut files = Vec::new();
            let mut attributes = Vec::new();
            for (suffix, name) in [
                ("CODE_SIGN_ON_COPY", "CodeSignOnCopy"),
                ("REMOVE_HEADERS_ON_COPY", "RemoveHeadersOnCopy"),
            ] {
                if settings::on(raw, format!("{property}_{suffix}").as_bytes()) {
                    attributes.push(name);
                }
            }
            for (index, item) in settings::strings(items).into_iter().enumerate() {
                let file_key = format!("{key}/{index}");
                let (item, source_tree) = self
                    .model
                    .find_target(raw.dir, item.as_bytes())
                    .and_then(|binding| self.model.target(binding))
                    .filter(|target| !target.is_imported())
                    .map_or((item, "<absolute>"), |target| {
                        let config = &self.default_config;
                        let resolved =
                            self.model
                                .find_target(raw.dir, &target.name)
                                .and_then(|binding| {
                                    self.resolved.get(&(binding, config.clone())).copied()
                                });
                        (
                            Product::new(self.model, target, resolved, config, self.bin).file(),
                            "BUILT_PRODUCTS_DIR",
                        )
                    });
                let file = file_ref(
                    &mut self.objects,
                    &file_key,
                    &item,
                    "wrapper.framework",
                    source_tree,
                )?;
                files.push(phases::build_file(
                    &mut self.objects,
                    &format!("{file_key}/build"),
                    label,
                    &file,
                    attrs([("ATTRIBUTES", list(attributes.iter().copied()))]),
                )?);
            }
            if files.is_empty() {
                continue;
            }
            let path = raw
                .props
                .get(format!("{property}_PATH").as_bytes())
                .map_or_else(String::new, |p| text(p));
            phases.push(self.objects.add(
                Isa::PBXCopyFilesBuildPhase,
                &key,
                label,
                attrs([
                    ("buildActionMask", "2147483647".into()),
                    ("dstPath", path.into()),
                    ("dstSubfolderSpec", destination.into()),
                    ("files", list(files)),
                    ("name", label.into()),
                    ("runOnlyForDeploymentPostprocessing", "0".into()),
                ]),
            )?);
        }
        Ok(phases)
    }

    fn info_plist(
        &self,
        raw: &Target,
        name: &str,
        product: &Product,
        facts: Option<&ResolvedTarget>,
    ) -> Result<Option<PathBuf>, GenError> {
        let (template, prefix, executable_key, package) = match product.bundle {
            BundleKind::None => return Ok(None),
            BundleKind::App => (
                "MacOSXBundleInfo.plist.in",
                "MACOSX_BUNDLE",
                "MACOSX_BUNDLE_EXECUTABLE_NAME",
                "APPL",
            ),
            BundleKind::Framework { .. } => (
                "MacOSXFrameworkInfo.plist.in",
                "MACOSX_FRAMEWORK",
                "MACOSX_FRAMEWORK_NAME",
                "FMWK",
            ),
            BundleKind::Bundle { .. } => (
                "MacOSXBundleInfo.plist.in",
                "MACOSX_BUNDLE",
                "MACOSX_BUNDLE_EXECUTABLE_NAME",
                "BNDL",
            ),
        };
        if let Some(plist) = facts
            .and_then(|facts| facts.ide.as_ref())
            .and_then(|ide| ide.info_plist.as_ref())
        {
            let path = self.bin.join(text(plist));
            if self.host.metadata(&path, rcmake_host::Follow::Yes).is_ok() {
                return Ok(Some(path));
            }
        }
        let custom = raw.props.get(format!("{prefix}_INFO_PLIST").as_bytes());
        let template_path = custom
            .map(|path| to_native(&self.model.dir(raw.dir).source_dir).join(text(path)))
            .or_else(|| {
                self.model
                    .definition(raw.dir, b"CMAKE_ROOT")
                    .map(|root| to_native(root).join("Modules").join(template))
            });
        let input = match template_path {
            Some(path) => self
                .host
                .read(&path, ReadRange::All { limit: 1024 * 1024 })
                .map(|data| text(&data))
                .map_err(|error| {
                    GenError::Invalid(format!("Cannot read {}: {error}", path.display()).into())
                })?,
            None => format!(
                "{}{}<dict><key>CFBundleExecutable</key><string>${{{executable_key}}}</string>\n<key>CFBundleIdentifier</key><string>$(PRODUCT_BUNDLE_IDENTIFIER)</string>\n<key>CFBundlePackageType</key><string>{package}</string>\n<key>CFBundleVersion</key><string>1</string></dict>\n</plist>\n",
                workspace::XML,
                workspace::PLIST
            ),
        };
        let lookup = |key: &str| {
            if key == executable_key {
                return "$(EXECUTABLE_NAME)".into();
            }
            raw.props
                .get(key.as_bytes())
                .or_else(|| self.model.definition(raw.dir, key.as_bytes()))
                .map_or_else(String::new, |value| workspace::escape(&text(value)))
        };
        let info = expand_plist(&input, lookup);
        let path = to_native(&self.model.dir(raw.dir).binary_dir)
            .join(format!("CMakeFiles/{name}.dir/Info.plist"));
        let path = self.bin.join(path);
        write_file(self.host, &path, &info)?;
        Ok(Some(path))
    }
}

fn expand_plist(mut input: &str, lookup: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    while let Some((start, open)) = input.match_indices(['@', '$']).next() {
        let Some(tail) = input.get(start + open.len()..) else {
            break;
        };
        out.push_str(input.get(..start).unwrap_or_default());
        let token = if open == "@" {
            tail.split_once('@')
        } else {
            tail.strip_prefix('{').and_then(|tail| tail.split_once('}'))
        };
        if let Some((key, rest)) = token.filter(|(key, _)| {
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            out.push_str(&lookup(key));
            input = rest;
        } else {
            out.push_str(open);
            input = tail;
        }
    }
    out.push_str(input);
    out
}

fn target_fields(
    reference: &Reference,
    configs: Reference,
    phases: impl IntoIterator<Item = Reference>,
    deps: Vec<Reference>,
) -> Attrs {
    attrs([
        ("buildConfigurationList", configs.into()),
        ("buildPhases", list(phases)),
        ("dependencies", list(deps)),
        ("name", reference.comment.clone().into()),
        ("productName", reference.comment.clone().into()),
    ])
}

fn group(
    objects: &mut Objects,
    key: &str,
    name: &str,
    children: Vec<Reference>,
) -> Result<Reference, GenError> {
    let mut attributes = attrs([
        ("children", list(children)),
        ("sourceTree", "<group>".into()),
    ]);
    if !name.is_empty() {
        attributes.insert("name".into(), name.into());
    }
    objects.add(Isa::PBXGroup, key, name, attributes)
}

fn file_ref(
    objects: &mut Objects,
    key: &str,
    path: &str,
    file_type: &str,
    tree: &str,
) -> Result<Reference, GenError> {
    let name = Path::new(path)
        .file_name()
        .map_or(path.into(), |v| v.to_string_lossy());
    objects.add(
        Isa::PBXFileReference,
        key,
        &name,
        attrs([
            ("explicitFileType", file_type.into()),
            ("name", name.to_string().into()),
            ("path", path.into()),
            ("sourceTree", tree.into()),
        ]),
    )
}

#[derive(Default)]
struct Group {
    files: Vec<Reference>,
    groups: BTreeMap<String, Group>,
}

impl Group {
    fn add(&mut self, path: &str, file: Reference) -> Result<(), GenError> {
        let mut group = self;
        for (depth, part) in path
            .split(['/', '\\'])
            .filter(|part| !part.is_empty())
            .enumerate()
        {
            if depth >= 64 {
                return Err(GenError::Invalid(
                    "Xcode group nesting exceeds 64 levels".into(),
                ));
            }
            group = group.groups.entry(part.into()).or_default();
        }
        group.files.push(file);
        Ok(())
    }

    fn finish(self, objects: &mut Objects, key: &str, name: &str) -> Result<Reference, GenError> {
        let mut children = self.files;
        for (name, child) in self.groups {
            children.push(child.finish(objects, &format!("{key}/{name}"), &name)?);
        }
        group(objects, key, name, children)
    }
}

#[derive(Default)]
struct BuildFiles {
    sources: Vec<Reference>,
    frameworks: Vec<Reference>,
    headers: Vec<Reference>,
    resources: Vec<Reference>,
}

struct Source {
    path: String,
    kind: IdeSourceKind,
    group: String,
    visibility: Option<&'static str>,
}

fn source_files(
    model: &ProjectModel,
    raw: &Target,
    target: &ResolvedTarget,
    bin: &Path,
) -> Vec<Source> {
    let dir = model.dir(raw.dir);
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |path: String, mut kind, group| {
        if !seen.insert(path.clone()) {
            return;
        }
        let props = dir.source_props.get(path.as_bytes());
        if props
            .and_then(|props| props.get(b"MACOSX_PACKAGE_LOCATION"))
            .is_some_and(|location| location.as_slice() == b"Resources")
        {
            kind = IdeSourceKind::Resource;
        }
        let visibility = [
            (b"PUBLIC_HEADER".as_slice(), "Public"),
            (b"PRIVATE_HEADER", "Private"),
        ]
        .into_iter()
        .find_map(|(key, visibility)| {
            let names = settings::strings(raw.props.get(key)?);
            names
                .iter()
                .any(|name| Path::new(&path) == to_native(&dir.source_dir).join(name))
                .then_some(visibility)
        });
        files.push(Source {
            path,
            kind,
            group,
            visibility,
        });
    };
    if let Some(ide) = target.ide.as_deref() {
        for source in &ide.sources {
            add(text(&source.path), source.kind.clone(), text(&source.group));
        }
    } else {
        for unit in &target.compile_units {
            if let Some(group) = target.compile_groups.get(unit.group) {
                add(
                    text(&unit.source.value),
                    IdeSourceKind::Compile {
                        language: group.language.clone(),
                    },
                    String::new(),
                );
            }
        }
        for name in raw
            .sources
            .iter()
            .flat_map(|item| settings::strings(&item.value))
            .filter(|name| !name.contains("$<"))
        {
            let path = to_native(&dir.source_dir).join(&name);
            let kind = match path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default()
            {
                "h" | "hh" | "hpp" | "hxx" | "inl" => IdeSourceKind::Header,
                "o" | "obj" => IdeSourceKind::ExternalObject,
                _ => IdeSourceKind::Other,
            };
            add(path.to_string_lossy().into_owned(), kind, String::new());
        }
    }
    let listfile = to_native(&dir.source_dir).join("CMakeLists.txt");
    add(
        listfile.to_string_lossy().into_owned(),
        IdeSourceKind::ListFile,
        String::new(),
    );
    for file in &mut files {
        if file.kind == IdeSourceKind::Object
            && let Ok(rel) = Path::new(&file.path).strip_prefix(bin)
        {
            let config = text(&target.config.0);
            let mut path = bin.to_owned();
            for part in rel.components() {
                let part = part.as_os_str();
                path.push(if part == config.as_str() {
                    "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)".as_ref()
                } else {
                    part
                });
            }
            file.path = path.to_string_lossy().into_owned();
        }
    }
    files
}

fn same_sources(first: &[Source], second: &[Source]) -> bool {
    first.len() == second.len()
        && first
            .iter()
            .zip(second)
            .all(|(a, b)| a.path == b.path && a.kind == b.kind)
}

fn source_lines(sources: &[Source]) -> String {
    sources
        .iter()
        .filter(|source| source.kind != IdeSourceKind::ListFile)
        .map(|file| format!("    {}", file.path))
        .collect::<Vec<_>>()
        .join("\n")
}

fn file_type(file: &Source) -> &'static str {
    match &file.kind {
        IdeSourceKind::Compile { language } => match language.as_slice() {
            b"C" => "sourcecode.c.c",
            b"CXX" => "sourcecode.cpp.cpp",
            b"OBJC" => "sourcecode.c.objc",
            b"OBJCXX" => "sourcecode.cpp.objcpp",
            b"Swift" => "sourcecode.swift",
            b"ASM" => "sourcecode.asm",
            _ => "text",
        },
        IdeSourceKind::Object | IdeSourceKind::ExternalObject => "compiled.mach-o.objfile",
        IdeSourceKind::Header => "sourcecode.c.h",
        IdeSourceKind::ListFile => "sourcecode.text",
        _ => match Path::new(&file.path)
            .extension()
            .and_then(|ext| ext.to_str())
        {
            Some("plist") => "text.plist.xml",
            Some("xcassets") => "folder.assetcatalog",
            Some("storyboard") => "file.storyboard",
            Some("xib") => "file.xib",
            _ => "file",
        },
    }
}
