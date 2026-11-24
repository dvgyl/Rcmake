//! Shared helpers of the IDE generators (Xcode and Visual Studio), and the pure
//! description of `cmake --build` that every generator crate plans (ADR-014, ADR-015).

use indexmap::IndexSet;
use rcmake_base::{BString, truth};
use rcmake_host::{FileKind, Follow, Host, path::to_native};
use rcmake_model::{DirId, GeneratorKind, ProjectModel, TargetId};
use rcmake_resolve::{ConfigureMetadata, GenerationResult};

/// The role of a target that the generator creates and the project does not declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UtilityKind {
    /// `ALL_BUILD`: builds every target that is not `EXCLUDE_FROM_ALL`.
    AllBuild,
    /// `ZERO_CHECK`: runs CMake again when a configure input changed.
    ZeroCheck,
    /// `install` (Xcode) or `INSTALL` (Visual Studio).
    Install,
    /// `RUN_TESTS`: runs `ctest`.
    RunTests,
    /// `package` (Xcode) or `PACKAGE` (Visual Studio).
    Package,
    /// `package_source` (Xcode) or `PACKAGE_SOURCE` (Visual Studio).
    PackageSource,
}

/// A target that the IDE generator creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtilityTarget {
    /// Target name in the IDE project.
    pub name: BString,
    /// Role of the target.
    pub kind: UtilityKind,
}

/// The utility targets of an IDE generator, in creation order.
///
/// - `ALL_BUILD` always exists.
/// - `ZERO_CHECK` exists unless `CMAKE_SUPPRESS_REGENERATION` is true.
/// - `install` exists only when the project has install rules.
/// - `RUN_TESTS` exists only after `enable_testing()` in the top directory.
/// - `package` and `package_source` exist only when `CPackConfig.cmake` and
///   `CPackSourceConfig.cmake` exist in the top build directory.
///
/// Names are lowercase for Xcode and uppercase for Visual Studio. A non-IDE generator
/// gets an empty list.
pub fn utility_targets(
    kind: GeneratorKind,
    model: &ProjectModel,
    host: &dyn Host,
) -> Vec<UtilityTarget> {
    if !kind.is_ide() {
        return Vec::new();
    }
    let upper = matches!(kind, GeneratorKind::VisualStudio(_));
    let named = |lower: &str, kind: UtilityKind| UtilityTarget {
        name: if upper {
            lower.to_ascii_uppercase().into()
        } else {
            lower.into()
        },
        kind,
    };
    let root = model.dir(DirId::ROOT);
    let is_on = |key: &[u8]| {
        model
            .definition(DirId::ROOT, key)
            .or_else(|| root.props.get(key))
            .is_some_and(|value| truth::is_on(value))
    };
    let has_file = |name: &str| {
        let path = to_native(&root.binary_dir).join(name);
        path.is_absolute()
            && host
                .metadata(&path, Follow::Yes)
                .is_ok_and(|meta| meta.kind == FileKind::File)
    };
    let mut out = vec![named("ALL_BUILD", UtilityKind::AllBuild)];
    if !is_on(b"CMAKE_SUPPRESS_REGENERATION") {
        out.push(named("ZERO_CHECK", UtilityKind::ZeroCheck));
    }
    if !model.installs.is_empty() {
        out.push(named("install", UtilityKind::Install));
    }
    if is_on(b"CMAKE_TESTING_ENABLED") {
        out.push(named("RUN_TESTS", UtilityKind::RunTests));
    }
    if has_file("CPackConfig.cmake") {
        out.push(named("package", UtilityKind::Package));
    }
    if has_file("CPackSourceConfig.cmake") {
        out.push(named("package_source", UtilityKind::PackageSource));
    }
    out
}

/// One IDE project: a directory with a `project()` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdeProject {
    /// The directory of the `project()` call.
    pub dir: DirId,
    /// `PROJECT_NAME` in that directory.
    pub name: BString,
}

/// The IDE projects, in directory order. The top directory is always first.
///
/// A directory has its own project when its `PROJECT_SOURCE_DIR` is its own source
/// directory. For Xcode, `CMAKE_XCODE_GENERATE_TOP_LEVEL_PROJECT_ONLY` keeps only the
/// top directory.
pub fn project_roots(kind: GeneratorKind, model: &ProjectModel) -> Vec<IdeProject> {
    let top_only = kind == GeneratorKind::Xcode
        && model
            .definition(DirId::ROOT, b"CMAKE_XCODE_GENERATE_TOP_LEVEL_PROJECT_ONLY")
            .is_some_and(|value| truth::is_on(value));
    let name_of = |dir: DirId| {
        model
            .definition(dir, b"PROJECT_NAME")
            .or_else(|| model.dir(dir).props.get(b"PROJECT_NAME"))
            .cloned()
            .unwrap_or_else(|| BString::from("Project"))
    };
    let mut out = vec![IdeProject {
        dir: DirId::ROOT,
        name: name_of(DirId::ROOT),
    }];
    if top_only {
        return out;
    }
    for (id, dir) in model.dirs().filter(|(id, _)| *id != DirId::ROOT) {
        if model
            .definition(id, b"PROJECT_SOURCE_DIR")
            .is_some_and(|source| *source == dir.source_dir)
        {
            out.push(IdeProject {
                dir: id,
                name: name_of(id),
            });
        }
    }
    out
}

/// The targets of the IDE project at directory `root`: the targets defined in `root`
/// and its subdirectories, then the targets that they depend on (transitive closure).
///
/// Imported targets are never part of a project. The dependencies come from the first
/// configuration of `result`.
pub fn targets_for_root(
    model: &ProjectModel,
    result: &GenerationResult,
    root: DirId,
) -> Vec<TargetId> {
    let mut out = IndexSet::new();
    let mut pending = Vec::new();
    for (id, target) in model.targets() {
        if !target.is_imported() && model.ancestors(target.dir).any(|dir| dir == root) {
            out.insert(id);
            pending.push(id);
        }
    }
    let first = result.configs.first();
    while let Some(id) = pending.pop() {
        let dependencies = result
            .targets
            .iter()
            .filter(|target| target.id == id && Some(&target.config) == first)
            .flat_map(|target| target.dependencies.iter().copied());
        for dependency in dependencies {
            let local = model
                .target(dependency)
                .is_some_and(|target| !target.is_imported());
            if local && out.insert(dependency) {
                pending.push(dependency);
            }
        }
    }
    out.into_iter().collect()
}

/// The configure inputs that make the IDE project run CMake again: the
/// `ReRunCMake.make` inputs (Xcode) and the `generate.stamp.depend` list (Visual
/// Studio). Each path occurs one time, in first-read order.
pub fn rerun_inputs(metadata: &ConfigureMetadata) -> Vec<BString> {
    let unique: IndexSet<&BString> = metadata.listfiles.iter().collect();
    unique.into_iter().cloned().collect()
}

/// The parallel level of `cmake --build`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Jobs {
    /// No `-j`/`--parallel` and no `CMAKE_BUILD_PARALLEL_LEVEL`.
    #[default]
    None,
    /// `-j` without a value, or an empty `CMAKE_BUILD_PARALLEL_LEVEL`: the build tool
    /// selects the job count.
    Default,
    /// An explicit job count.
    Count(u32),
}

/// A pure description of one `cmake --build` call.
#[derive(Clone, Copy, Debug)]
pub struct BuildRequest<'a> {
    /// Generator from `CMAKE_GENERATOR` in the cache.
    pub generator: GeneratorKind,
    /// Absolute build directory.
    pub build_dir: &'a [u8],
    /// `CMAKE_PROJECT_NAME` from the cache.
    pub project_name: &'a [u8],
    /// `CMAKE_MAKE_PROGRAM` from the cache (empty when it is not set).
    pub make_program: &'a [u8],
    /// `--config` value.
    pub config: Option<&'a [u8]>,
    /// `--target` values, in order.
    pub targets: &'a [BString],
    /// Parallel level.
    pub jobs: Jobs,
    /// `--clean-first`.
    pub clean_first: bool,
    /// `-v` / `--verbose` (or `VERBOSE` in the environment).
    pub verbose: bool,
    /// Arguments after `--`, for the native build tool.
    pub native_args: &'a [BString],
    /// Visual Studio only: `CMAKE_VS_PLATFORM_NAME` from the cache.
    pub platform: &'a [u8],
}

/// One native build-tool process that `cmake --build` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildCommand {
    /// Program (`argv[0]`) and arguments.
    pub argv: Vec<BString>,
    /// Absolute working directory.
    pub working_dir: BString,
}

/// Complete a directory-tool build plan (Ninja and Make style).
///
/// Each command is `base` + targets + native arguments. With `--clean-first`, a
/// `base` + `clean_target` + native arguments command comes first, unless the only
/// requested target is `clean_target`. The working directory is the build directory.
#[must_use]
pub fn with_clean_first(
    req: &BuildRequest<'_>,
    base: &[BString],
    clean_target: &str,
) -> Vec<BuildCommand> {
    let command = |targets: &[BString]| BuildCommand {
        argv: base
            .iter()
            .chain(targets)
            .chain(req.native_args)
            .cloned()
            .collect(),
        working_dir: req.build_dir.into(),
    };
    let clean = [BString::from(clean_target)];
    let mut out = Vec::with_capacity(2);
    if req.clean_first && req.targets != clean.as_slice() {
        out.push(command(&clean));
    }
    out.push(command(req.targets));
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use rcmake_host::PureHost;
    use rcmake_model::{
        InstallKind, InstallOptions, InstallRule, Target, TargetKind, TargetOrigin, VsVersion,
    };
    use rcmake_resolve::{ResolvedTarget, TargetMetadata};

    fn model() -> ProjectModel {
        let mut model = ProjectModel::default();
        model.set_root_dirs("/src".into(), "/out".into());
        model
    }

    fn subdir(model: &mut ProjectModel, name: &str) -> DirId {
        model
            .add_directory(
                DirId::ROOT,
                format!("/src/{name}").into(),
                format!("/out/{name}").into(),
                Default::default(),
            )
            .expect("directory")
    }

    fn names(targets: &[UtilityTarget]) -> Vec<&[u8]> {
        targets
            .iter()
            .map(|target| target.name.as_slice())
            .collect()
    }

    #[test]
    fn utility_targets_follow_project_state_and_generator_case() {
        let host = PureHost::default();
        let mut model = model();
        assert!(utility_targets(GeneratorKind::Ninja, &model, &host).is_empty());
        assert_eq!(
            names(&utility_targets(GeneratorKind::Xcode, &model, &host)),
            [b"ALL_BUILD".as_slice(), b"ZERO_CHECK"]
        );
        model.add_install(InstallRule {
            dir: DirId::ROOT,
            backtrace: Default::default(),
            kind: InstallKind::Files(Vec::new()),
            options: InstallOptions::default(),
        });
        model
            .dir_mut(DirId::ROOT)
            .props
            .set("CMAKE_TESTING_ENABLED".into(), "1".into());
        model
            .dir_mut(DirId::ROOT)
            .props
            .set("CMAKE_SUPPRESS_REGENERATION".into(), "ON".into());
        host.seed_file(Path::new("/out/CPackConfig.cmake"), b"")
            .expect("seed");
        host.seed_file(Path::new("/out/CPackSourceConfig.cmake"), b"")
            .expect("seed");
        assert_eq!(
            names(&utility_targets(GeneratorKind::Xcode, &model, &host)),
            [
                b"ALL_BUILD".as_slice(),
                b"install",
                b"RUN_TESTS",
                b"package",
                b"package_source"
            ]
        );
        assert_eq!(
            names(&utility_targets(
                GeneratorKind::VisualStudio(VsVersion::Vs17),
                &model,
                &host
            )),
            [
                b"ALL_BUILD".as_slice(),
                b"INSTALL",
                b"RUN_TESTS",
                b"PACKAGE",
                b"PACKAGE_SOURCE"
            ]
        );
    }

    #[test]
    fn project_roots_find_project_calls() {
        let mut model = model();
        let sub = subdir(&mut model, "sub");
        let plain = subdir(&mut model, "plain");
        for (dir, name, source) in [
            (DirId::ROOT, "Top", "/src"),
            (sub, "Sub", "/src/sub"),
            (plain, "Top", "/src"),
        ] {
            let vars = &mut model.dir_mut(dir).vars;
            vars.insert("PROJECT_NAME".into(), name.into());
            vars.insert("PROJECT_SOURCE_DIR".into(), source.into());
        }
        let roots = project_roots(GeneratorKind::VisualStudio(VsVersion::Vs18), &model);
        assert_eq!(
            roots,
            [
                IdeProject {
                    dir: DirId::ROOT,
                    name: "Top".into()
                },
                IdeProject {
                    dir: sub,
                    name: "Sub".into()
                }
            ]
        );
        model.dir_mut(DirId::ROOT).vars.insert(
            "CMAKE_XCODE_GENERATE_TOP_LEVEL_PROJECT_ONLY".into(),
            "ON".into(),
        );
        assert_eq!(project_roots(GeneratorKind::Xcode, &model).len(), 1);
    }

    #[test]
    fn targets_for_root_adds_transitive_dependencies() {
        let mut model = model();
        let sub = subdir(&mut model, "sub");
        let target = |name: &str, dir| {
            Target::new(
                name.into(),
                TargetKind::StaticLibrary,
                TargetOrigin::Normal,
                dir,
                Default::default(),
                Default::default(),
            )
        };
        let base = model.add_target(target("base", DirId::ROOT)).expect("base");
        let mid = model.add_target(target("mid", DirId::ROOT)).expect("mid");
        let leaf = model.add_target(target("leaf", sub)).expect("leaf");
        let mut result = GenerationResult {
            configs: vec![rcmake_genex::ConfigId("Debug".into())],
            ..GenerationResult::default()
        };
        let resolved = |id, dependencies: Vec<TargetId>| ResolvedTarget {
            id,
            config: rcmake_genex::ConfigId("Debug".into()),
            metadata: TargetMetadata {
                name: BString::default(),
                kind: TargetKind::StaticLibrary,
                source_dir: BString::default(),
                binary_dir: BString::default(),
                backtrace: Default::default(),
                folder: None,
                exclude_from_all: false,
                generator_provided: false,
            },
            artifacts: Default::default(),
            compile_groups: Vec::new(),
            compile_units: Vec::new(),
            link: None,
            dependencies,
            cxx_modules: None,
            ide: None,
        };
        result.targets.push(resolved(base, Vec::new()));
        result.targets.push(resolved(mid, vec![base]));
        result.targets.push(resolved(leaf, vec![mid]));
        assert_eq!(targets_for_root(&model, &result, sub), [leaf, mid, base]);
        assert_eq!(
            targets_for_root(&model, &result, DirId::ROOT),
            [base, mid, leaf]
        );
    }

    #[test]
    fn rerun_inputs_drop_repeated_listfiles() {
        let metadata = ConfigureMetadata {
            listfiles: vec!["/a".into(), "/b".into(), "/a".into()],
            ..ConfigureMetadata::default()
        };
        assert_eq!(rerun_inputs(&metadata), [BString::from("/a"), "/b".into()]);
    }
}
