//! Convert resolved build facts to an Xcode project.

mod bundle;
mod sources;
mod targets;

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
use rcmake_host::{Host, path::to_native};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, Target, TargetId, TargetKind};
use rcmake_resolve::{GenerationResult, ResolvedTarget};

use crate::{
    file_name, ids,
    object::{Attrs, Isa, Objects, Reference, Value, attrs, list},
    phases, plist, scheme, settings, text, workspace, write_file,
};

use sources::Group;

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
            format!(
                "{:04}",
                major
                    .saturating_mul(100)
                    .saturating_add(minor.saturating_mul(10))
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
            ("explicitFileType", "sourcecode.text"),
            "<absolute>",
        )?;
        tree.push(group(
            &mut self.objects,
            "group/rules",
            "CMake Rules",
            vec![listfile],
        )?);
        tree.push(products.clone());
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
            if *kind == UtilityKind::AllBuild
                && self
                    .model
                    .definition(root.dir, b"CMAKE_XCODE_GENERATE_SCHEME")
                    .is_some_and(|value| truth::is_on(value))
            {
                let target = Target::new(
                    reference.comment.as_str().into(),
                    TargetKind::Utility,
                    rcmake_model::TargetOrigin::Normal,
                    root.dir,
                    Default::default(),
                    Default::default(),
                );
                scheme::write(
                    self.host,
                    &self.path,
                    &target,
                    reference,
                    &reference.comment,
                    &self.default_config,
                    &self.upgrade,
                )?;
            }
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
                .collect::<Result<_, _>>()?,
        )?;
        let development_region = self
            .model
            .definition(root.dir, b"CMAKE_XCODE_ATTRIBUTE_developmentRegion")
            .map_or_else(|| "English".into(), |value| text(value).trim().to_owned());
        let known_regions = self
            .model
            .definition(root.dir, b"CMAKE_XCODE_ATTRIBUTE_knownRegions")
            .map_or_else(
                || vec!["en".into(), "Base".into()],
                |value| {
                    text(value)
                        .split(',')
                        .map(str::trim)
                        .filter(|region| !region.is_empty())
                        .map(str::to_owned)
                        .collect()
                },
            );
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
                ("developmentRegion", development_region.into()),
                ("hasScannedForEncodings", "0".into()),
                ("knownRegions", Value::ListRow(known_regions)),
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
        !self.model.dir_excluded_from_all(target.raw.dir)
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
    file_type: (&str, &str),
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
            (file_type.0, file_type.1.into()),
            ("name", name.to_string().into()),
            ("path", path.into()),
            ("sourceTree", tree.into()),
        ]),
    )
}
