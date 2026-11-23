//! Three-layer resolution IR for Rcmake (ADR-009).
//!
//! - Layer 1 is the raw [`ProjectModel`] (`rcmake-model`).
//! - Layer 2 is [`ResolvedTarget`]: per-configuration, genex-evaluated target facts with
//!   backtraces. It is the single source of truth for the File API codemodel and
//!   `install(EXPORT)`.
//! - Layer 3 is [`BuildGraph`]: lowered, POSIX-`sh`-escaped command invocations,
//!   expanded rule templates, link fragments with semantic roles, and the
//!   [`InstallManifest`].
//!
//! Non-target data (cache, listfile inputs, toolchains, configure log) lives in
//! [`ConfigureMetadata`].

#![forbid(unsafe_code)]

use indexmap::{IndexMap, IndexSet, map::Entry};
use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_cxxmodules::{ModmapFormat, ModuleVisibility, TargetDependInfo};
use rcmake_diag::{Backtrace, Location, MessageKind, PolicyId, PolicyStatus};
use rcmake_genex::{ConfigId, EvalContext, GenexNode, PreprocessMode, TargetHandle};
use rcmake_host::{Host, path::to_native};
use rcmake_model::{
    BT, DirId, ExportSet, FileSet, FileSetId, FileSetKind, InstallKind, InstallRule, ProjectModel,
    Target, TargetId, TargetKind, TargetOrigin, UsageField, UsageScope,
};

use sources::{SourcePaths, TargetInputs, external_object};
use usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver};

mod sources;

pub mod lower;
pub mod usage;

#[cfg(test)]
mod resolve_tests;

/// Error of resolution or lowering.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// A generator expression failed.
    #[error(transparent)]
    Genex(#[from] rcmake_genex::GenexError),
    /// A diagnostic was issued with CMake's message text.
    #[error("{0}")]
    Message(BString),
    /// The stage is not implemented yet.
    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),
}

/// Everything a generator needs (output of [`resolve`] plus lowering).
#[derive(Clone, Debug, Default)]
pub struct GenerationResult {
    /// Configurations in generation order (one for single-config generators).
    pub configs: Vec<ConfigId>,
    /// Resolved targets for each configuration, in model order.
    pub targets: Vec<ResolvedTarget>,
    /// One lowered build graph per configuration.
    pub graphs: Vec<BuildGraph>,
    /// Install actions for `cmake_install.cmake`.
    pub install: InstallManifest,
    /// `install(EXPORT)` and `export()` plans.
    pub exports: Vec<ExportPlan>,
    /// Non-target metadata.
    pub metadata: ConfigureMetadata,
}

/// Non-target configure metadata (`cmakeFiles-v1`, `cache-v2`, `toolchains-v1`,
/// `configureLog-v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigureMetadata {
    /// Listfiles read by the configure step, in order (`RERUN_CMAKE` inputs).
    pub listfiles: Vec<BString>,
    /// Toolchain facts by language.
    pub toolchains: IndexMap<BString, Toolchain>,
    /// Absolute path of `CMakeConfigureLog.yaml`, if written.
    pub configure_log: Option<BString>,
    /// Executable path from `CMAKE_COMMAND`.
    pub cmake_command: Option<BString>,
    /// Executable path from `CMAKE_CTEST_COMMAND`.
    pub ctest_command: Option<BString>,
}

/// Compiler facts of one language (`toolchains-v1`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Toolchain {
    /// `CMAKE_<LANG>_COMPILER`.
    pub compiler_path: BString,
    /// `CMAKE_<LANG>_COMPILER_ID`.
    pub compiler_id: BString,
    /// `CMAKE_<LANG>_COMPILER_VERSION`.
    pub compiler_version: BString,
    /// `CMAKE_<LANG>_IMPLICIT_INCLUDE_DIRECTORIES`.
    pub implicit_includes: Vec<BString>,
    /// `CMAKE_<LANG>_IMPLICIT_LINK_DIRECTORIES`.
    pub implicit_link_dirs: Vec<BString>,
    /// `CMAKE_<LANG>_SOURCE_FILE_EXTENSIONS`.
    pub source_extensions: Vec<BString>,
    /// `CMAKE_<LANG>_SCANDEP_SOURCE`: rule template of the module dependency scan.
    pub scandep_rule: Option<BString>,
    /// `CMAKE_<LANG>_MODULE_MAP_FORMAT` (`clang`, `gcc`, or `msvc`).
    pub module_map_format: Option<BString>,
    /// `CMAKE_<LANG>_MODULE_MAP_FLAG`: flag template that passes the module map.
    pub module_map_flag: Option<BString>,
    /// `CMAKE_<LANG>_MODULE_BMI_ONLY_FLAG`: flag template that makes a compile step
    /// write only a module file.
    pub compile_bmi_rule: Option<BString>,
}

/// Per-configuration facts of one target (layer 2).
#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    /// Model target.
    pub id: TargetId,
    /// Configuration.
    pub config: ConfigId,
    /// Name, kind, directories, and flags.
    pub metadata: TargetMetadata,
    /// Output files.
    pub artifacts: ArtifactSet,
    /// Compile groups (sources that share flags).
    pub compile_groups: Vec<CompileGroup>,
    /// Translation units.
    pub compile_units: Vec<CompileUnit>,
    /// Link or archive step (`None` for interface, object, and utility targets).
    pub link: Option<LinkStep>,
    /// Targets that must build first, in order.
    pub dependencies: Vec<TargetId>,
    /// C++20 module facts (`None` when no source of the target is scanned).
    pub cxx_modules: Option<CxxModuleInfo>,
}

/// C++20 module facts of one target and configuration (ADR-012).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CxxModuleInfo {
    /// Units of `CXX_MODULES` file sets.
    pub units: Vec<ModuleUnit>,
    /// Directory of the module files, relative to the build directory.
    pub module_dir: BString,
    /// Path of `CXXDependInfo.json`, relative to the build directory.
    pub tdi: BString,
    /// Path of the collated Ninja dyndep file, relative to the build directory.
    pub dyndep: BString,
    /// Build directories of targets whose module tables this target reads.
    pub linked_target_dirs: Vec<BString>,
    /// Build directories of targets whose module tables this target forwards.
    pub forward_from: Vec<BString>,
    /// Install rule of the module files (`install(TARGETS ... CXX_MODULES_BMI)`).
    pub bmi_install: Option<BmiInstall>,
}

/// One unit of a `CXX_MODULES` file set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleUnit {
    /// Index into [`ResolvedTarget::compile_units`].
    pub unit: usize,
    /// File set name.
    pub file_set: BString,
    /// The file set is `PUBLIC` or `INTERFACE`.
    pub public: bool,
    /// Visibility of the file set.
    pub visibility: ModuleVisibility,
    /// Source directory relative to the matching file set base directory.
    pub relative_directory: BString,
    /// Install destination of the source file set.
    pub destination: Option<BString>,
    /// The unit compiles only to a module file (no object file).
    pub bmi_only: bool,
}

/// Install rule of the module files of a target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BmiInstall {
    /// `DESTINATION`.
    pub destination: BString,
    /// Install component.
    pub component: BString,
    /// Permission keywords.
    pub permissions: Vec<BString>,
    /// `EXCLUDE_FROM_ALL`.
    pub exclude_from_all: bool,
    /// `OPTIONAL`.
    pub optional: bool,
}

/// Descriptive target facts (`codemodel` target object fields).
#[derive(Clone, Debug)]
pub struct TargetMetadata {
    /// Target name.
    pub name: BString,
    /// Target type.
    pub kind: TargetKind,
    /// Absolute source directory of the defining directory.
    pub source_dir: BString,
    /// Absolute binary directory of the defining directory.
    pub binary_dir: BString,
    /// Backtrace of the `add_*()` call.
    pub backtrace: Backtrace,
    /// `FOLDER` property.
    pub folder: Option<BString>,
    /// Not part of the `all` target.
    pub exclude_from_all: bool,
    /// Created by the generator (`isGeneratorProvided`).
    pub generator_provided: bool,
}

/// Output files of a target for one configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactSet {
    /// Main output (`$<TARGET_FILE>`).
    pub file: Option<BString>,
    /// File to link against (`$<TARGET_LINKER_FILE>`).
    pub linker_file: Option<BString>,
    /// Versioned soname file or symlink (`$<TARGET_SONAME_FILE>`).
    pub soname_file: Option<BString>,
    /// Import library (`$<TARGET_IMPORT_FILE>`).
    pub import_file: Option<BString>,
    /// Debug symbols file (`$<TARGET_PDB_FILE>`).
    pub pdb_file: Option<BString>,
    /// Object file directory.
    pub object_dir: Option<BString>,
}

/// One include directory as seen by a compile step.
#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceInclude {
    /// Absolute path.
    pub path: BT<BString>,
    /// Passed as a system include (`-isystem`).
    pub system: bool,
}

/// Sources of one target that compile with the same language and flags.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompileGroup {
    /// Compile language.
    pub language: BString,
    /// Compile flag fragments in command-line order.
    pub flags: Vec<BT<BString>>,
    /// Preprocessor definitions (`NAME` or `NAME=value`).
    pub defines: Vec<BT<BString>>,
    /// Include directories in search order.
    pub includes: Vec<InterfaceInclude>,
    /// Indices into [`ResolvedTarget::compile_units`].
    pub units: Vec<usize>,
}

/// One translation unit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompileUnit {
    /// Absolute source path.
    pub source: BT<BString>,
    /// Object file path relative to the build directory.
    pub object: BString,
    /// Index into [`ResolvedTarget::compile_groups`].
    pub group: usize,
    /// The unit needs a C++ module dependency scan (`CXX_SCAN_FOR_MODULES`).
    pub scan: bool,
}

/// Kind of a link step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// Link an executable.
    Executable,
    /// Link a shared library.
    SharedLibrary,
    /// Link a loadable module.
    ModuleLibrary,
    /// Create a static archive.
    StaticArchive,
}

/// Semantic role of a link command fragment (`codemodel` `commandFragments.role`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkFragmentRole {
    /// `flags`.
    Flags,
    /// `libraries`.
    Libraries,
    /// `libraryPath`.
    LibraryPath,
    /// `frameworkPath`.
    FrameworkPath,
}

/// One fragment of a link command line.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkFragment {
    /// Fragment text, already escaped for the shell.
    pub text: BT<BString>,
    /// Role.
    pub role: LinkFragmentRole,
}

/// The link or archive step of a target.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkStep {
    /// Step kind.
    pub kind: LinkKind,
    /// Link language (`LINKER_LANGUAGE`).
    pub language: BString,
    /// Command fragments in order.
    pub fragments: Vec<LinkFragment>,
    /// Output path relative to the build directory.
    pub output: BString,
}

/// One process invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invocation {
    /// Program and arguments.
    pub argv: Vec<BString>,
    /// The same command as one POSIX `sh` command line.
    pub shell: BString,
    /// Working directory (`None` means the build directory).
    pub working_dir: Option<BString>,
}

/// A lowered custom command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomStep {
    /// Output files.
    pub outputs: Vec<BString>,
    /// Byproduct files.
    pub byproducts: Vec<BString>,
    /// Input files and target-level dependencies.
    pub depends: Vec<BString>,
    /// Commands in order.
    pub invocations: Vec<Invocation>,
    /// `COMMENT` text.
    pub comment: Option<BString>,
    /// `DEPFILE`.
    pub depfile: Option<BString>,
    /// `USES_TERMINAL`.
    pub uses_terminal: bool,
    /// `JOB_POOL`.
    pub job_pool: Option<BString>,
}

/// One step of a [`BuildGraph`].
#[derive(Clone, Debug, PartialEq)]
pub enum BuildStep {
    /// Compile one unit.
    Compile {
        /// Output object path.
        object: BString,
        /// Source path.
        source: BString,
        /// Compiler invocation.
        invocation: Invocation,
        /// Compiler-written dependency file.
        depfile: Option<BString>,
        /// Module map file that the compiler reads (scanned units only).
        modmap: Option<BString>,
        /// Ninja dyndep file of the unit's target (scanned units only).
        dyndep: Option<BString>,
    },
    /// Scan one unit for C++ module dependencies (writes a P1689r5 `.ddi` file).
    Scan {
        /// Source path.
        source: BString,
        /// Output `.ddi` path.
        ddi: BString,
        /// Object path of the compile step that the scan serves.
        object: BString,
        /// Scanner invocation.
        invocation: Invocation,
        /// Scanner-written dependency file.
        depfile: Option<BString>,
    },
    /// Collate the `.ddi` files of one target (`cmake -E cmake_ninja_dyndep`).
    Collate {
        /// Target name.
        target: BString,
        /// Input `.ddi` paths.
        ddis: Vec<BString>,
        /// Output dyndep file.
        dd: BString,
        /// Output module map files, one for each scanned unit.
        modmaps: Vec<BString>,
        /// `CXXDependInfo.json` path.
        tdi: BString,
        /// Data to write to `CXXDependInfo.json`.
        depend_info: Box<TargetDependInfo>,
        /// Module map format of the compiler.
        modmap_format: ModmapFormat,
        /// The TDI file and the module tables that collation reads.
        implicit_inputs: Vec<BString>,
    },
    /// Link or archive.
    Link {
        /// Inputs (objects and libraries) in order.
        inputs: Vec<BString>,
        /// The link step.
        step: LinkStep,
        /// Linker invocations (archive, ranlib, symlinks).
        invocations: Vec<Invocation>,
    },
    /// Run a custom command.
    Custom(CustomStep),
}

/// A buildable target of a [`BuildGraph`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphTarget {
    /// Target name.
    pub name: BString,
    /// Indices into [`BuildGraph::steps`].
    pub steps: Vec<usize>,
    /// Indices into [`BuildGraph::targets`] that build first.
    pub depends: Vec<usize>,
    /// Part of the `all` target.
    pub in_all: bool,
}

/// Lowered build graph of one configuration (layer 3).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BuildGraph {
    /// Configuration.
    pub config: ConfigId,
    /// All steps.
    pub steps: Vec<BuildStep>,
    /// Targets in model order.
    pub targets: Vec<GraphTarget>,
}

/// One install action of `cmake_install.cmake`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallAction {
    /// Install component.
    pub component: BString,
    /// Destination (relative to `CMAKE_INSTALL_PREFIX` unless absolute).
    pub destination: BString,
    /// Configurations the action applies to (empty means all).
    pub configurations: Vec<BString>,
    /// What to install.
    pub kind: InstallActionKind,
    /// Skip missing files (`OPTIONAL`).
    pub optional: bool,
}

/// Payload of an [`InstallAction`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallActionKind {
    /// Copy files (`file(INSTALL TYPE FILE|PROGRAM|SHARED_LIBRARY|...)`).
    Files {
        /// `file(INSTALL TYPE ...)` value.
        file_type: BString,
        /// Source files.
        files: Vec<BString>,
        /// Destination name (`RENAME`).
        rename: Option<BString>,
        /// Permission keywords.
        permissions: Vec<BString>,
    },
    /// Copy directories.
    Directory {
        /// Source directories.
        dirs: Vec<BString>,
        /// Raw `install(DIRECTORY)` options (`PATTERN`, `FILES_MATCHING`, ...).
        options: Vec<BString>,
    },
    /// Include a script.
    Script(BString),
    /// Run code.
    Code(BString),
    /// Install an export file.
    Export {
        /// Index into [`GenerationResult::exports`].
        plan: usize,
    },
}

/// All install actions in rule order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallManifest {
    /// Actions.
    pub actions: Vec<InstallAction>,
}

/// One exported target of an [`ExportPlan`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportedTarget {
    /// Exported name without namespace (`EXPORT_NAME`).
    pub name: BString,
    /// Target type keyword for `add_library(... IMPORTED)`.
    pub kind: BString,
    /// `INTERFACE_*` and other exported properties after preprocessing.
    pub properties: IndexMap<BString, BString>,
    /// Per-configuration `IMPORTED_*` properties.
    pub config_properties: IndexMap<BString, IndexMap<BString, BString>>,
}

/// An `install(EXPORT)` or `export()` file plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportPlan {
    /// Export set name (empty for `export(TARGETS)`).
    pub set: BString,
    /// `NAMESPACE`.
    pub namespace: BString,
    /// Output file name.
    pub file: BString,
    /// Directory for the C++ module export files.
    pub cxx_modules_directory: Option<BString>,
    /// `true` for `install(EXPORT)`, `false` for `export()`.
    pub install: bool,
    /// Targets in order.
    pub targets: Vec<ExportedTarget>,
}

/// Resolve and lower the project in configuration order.
/// Use `CMAKE_BUILD_TYPE` if `configs` is empty.
pub fn resolve(
    model: &ProjectModel,
    host: &dyn Host,
    configs: &[ConfigId],
) -> Result<GenerationResult, ResolveError> {
    let configs = if configs.is_empty() {
        vec![ConfigId(
            lower::definition(model, DirId::ROOT, b"CMAKE_BUILD_TYPE")
                .cloned()
                .unwrap_or_default(),
        )]
    } else {
        configs.to_vec()
    };
    let mut paths = SourcePaths::new(host);
    let mut targets = Vec::new();
    let mut graphs = Vec::with_capacity(configs.len());
    for config in &configs {
        paths.index_generated(model, config)?;
        validate_file_sets(model, &paths, config)?;
        paths.index_objects(model, config);
        let mut usage = UsageResolver::with_source_paths(model, &paths);
        let ctx = EvalContext {
            config: config.clone(),
            ..EvalContext::default()
        };
        let mut resolved = IndexMap::new();
        let mut inputs = IndexMap::new();
        for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
            let (facts, target_inputs) =
                resolve_target(model, &paths, &mut usage, id, target, &ctx)?;
            resolved.insert(id, facts);
            inputs.insert(id, target_inputs);
        }
        let std_model = if resolved
            .values()
            .any(|target| target.compile_units.iter().any(|unit| unit.scan))
        {
            std_module_model(model, host)?
        } else {
            None
        };
        let model = std_model.as_ref().map_or(model, |(model, _)| model);
        let mut usage = if std_model.is_some() {
            UsageResolver::with_source_paths(model, &paths)
        } else {
            usage
        };
        let std_id = std_model.as_ref().map(|(_, id)| *id);
        if let Some(id) = std_id
            && let Some(target) = model.target(id)
        {
            let (facts, target_inputs) =
                resolve_target(model, &paths, &mut usage, id, target, &ctx)?;
            resolved.insert(id, facts);
            inputs.insert(id, target_inputs);
        }
        let links = model
            .targets()
            .filter(|(_, target)| !target.is_imported())
            .map(|(id, target)| {
                resolve_link(model, &mut usage, &paths, &resolved, id, target, &ctx)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let std_objects = std_id
            .and_then(|id| resolved.get(&id))
            .and_then(|target| Some((target, target.cxx_modules.as_ref()?)))
            .into_iter()
            .flat_map(|(target, info)| {
                info.units
                    .iter()
                    .filter(|unit| !unit.bmi_only)
                    .filter_map(|unit| target.compile_units.get(unit.unit))
                    .map(|unit| unit.object.clone())
            })
            .collect::<Vec<_>>();
        let module_dirs: IndexMap<_, _> = resolved
            .values()
            .filter_map(|target| {
                let info = target.cxx_modules.as_ref()?;
                (!info.units.is_empty()).then(|| (target.id, info.module_dir.clone()))
            })
            .collect();
        for ((target, inputs), link) in resolved.values_mut().zip(inputs.values_mut()).zip(links) {
            target.link = link.step;
            target.dependencies = link.dependencies;
            inputs.objects.extend(link.objects);
            if let Some(info) = &mut target.cxx_modules {
                let mut linked = link.module_links.into_iter().collect::<IndexSet<_>>();
                if let Some(id) = std_id.filter(|id| {
                    *id != target.id && target.compile_units.iter().any(|unit| unit.scan)
                }) {
                    linked.insert(id);
                    target.dependencies.push(id);
                    inputs.objects.extend(std_objects.iter().cloned());
                }
                info.linked_target_dirs.extend(
                    linked
                        .iter()
                        .filter(|id| **id != target.id)
                        .filter_map(|id| module_dirs.get(id).cloned()),
                );
            }
        }
        let resolved = resolved.into_values().collect::<Vec<_>>();
        graphs.push(lower::lower_with_inputs(
            model, &resolved, config, &inputs, &mut usage,
        )?);
        targets.extend(resolved);
    }

    let install = lower::lower_install_manifest_with_configs(model, &targets, &configs)?;
    let exports = resolve_exports(model, &mut UsageResolver::new(model), &targets, &install)?;
    let mut metadata = lower::collect_metadata(model);
    if let Some(toolchain) = metadata.toolchains.get_mut(b"CXX".as_slice())
        && toolchain.module_map_format.is_none()
        && let Some(format) = graphs
            .iter()
            .flat_map(|graph| &graph.steps)
            .find_map(|step| match step {
                BuildStep::Collate { modmap_format, .. } => Some(modmap_format),
                _ => None,
            })
    {
        toolchain.module_map_format = Some(
            match format {
                ModmapFormat::Clang => "clang",
                ModmapFormat::Gcc => "gcc",
                ModmapFormat::Msvc => "msvc",
            }
            .into(),
        );
    }
    Ok(GenerationResult {
        configs,
        targets,
        graphs,
        install,
        exports,
        metadata,
    })
}

fn validate_file_sets(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    config: &ConfigId,
) -> Result<(), ResolveError> {
    let mut usage = UsageResolver::new(model);
    let ctx = EvalContext {
        config: config.clone(),
        ..EvalContext::default()
    };
    for (id, target) in model.targets().filter(|(_, target)| !target.is_imported()) {
        for set in target.file_sets.iter().filter_map(|id| model.file_set(*id)) {
            if set.kind != FileSetKind::Headers {
                continue;
            }
            if target
                .props
                .get(b"FRAMEWORK")
                .is_some_and(|value| truth::is_on(value))
            {
                let body = format!(
                    "The file set \"{}\", of type \"HEADERS\", is incompatible with the \"FRAMEWORK\" target \"{}\".",
                    set.name, target.name,
                );
                paths.emit(
                    model,
                    MessageKind::Error,
                    generation_location(model, target),
                    body.clone().into(),
                );
                return Err(ResolveError::Message(body.into()));
            }
            if lower::definition(model, target.dir, b"CMAKE_GENERATOR")
                .is_none_or(|value| !value.starts_with(b"Ninja"))
                || !set
                    .props
                    .get(b"INDEPENDENT_FILES")
                    .is_some_and(|value| truth::is_on(value))
                || lower::definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
                    .is_some_and(|value| truth::is_on(value))
            {
                continue;
            }
            let dir = model.dir(target.dir);
            if usage
                .file_set_paths(id, b"SOURCES", &set.files, &ctx)?
                .iter()
                .any(|file| paths.is_generated(dir, &file.value))
            {
                paths.emit(
                    model, MessageKind::AuthorWarning, generation_location(model, target),
                    format!(
                        "the \"INDEPENDENT_FILES\" property of the file set \"{}\" of the target \"{}\" will be ignored because it is incompatible with the file set type \"HEADERS\".",
                        set.name, target.name,
                    ).into(),
                );
            }
        }
    }
    Ok(())
}

fn resolve_target<'a>(
    model: &'a ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'a>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<(ResolvedTarget, TargetInputs), ResolveError> {
    let dir = model.dir(target.dir);
    let metadata = TargetMetadata {
        name: target.name.clone(),
        kind: target.kind,
        source_dir: dir.source_dir.clone(),
        binary_dir: dir.binary_dir.clone(),
        backtrace: target.backtrace.clone(),
        folder: target.props.get(b"FOLDER").cloned(),
        exclude_from_all: excluded(model, target),
        generator_provided: target.kind == TargetKind::GlobalTarget
            || target.name == STD_MODULE_TARGET,
    };
    let mut resolved = ResolvedTarget {
        id,
        config: ctx.config.clone(),
        metadata,
        artifacts: lower::compute_artifacts(model, target, &ctx.config),
        compile_groups: Vec::new(),
        compile_units: Vec::new(),
        link: None,
        dependencies: Vec::new(),
        cxx_modules: None,
    };

    let export_symbol = export_symbol(target);
    let mut dependencies = IndexSet::new();
    let mut inputs = TargetInputs::default();
    let mut groups = IndexMap::new();
    let (file_sets, members) = collect_file_sets(model, paths, usage, id, ctx)?;
    let sources = usage.collect_property(id, b"SOURCES", ctx)?.entries;
    for mut source in sources
        .into_iter()
        .chain(members.values().map(|member| member.source.clone()))
    {
        source.value = paths.resolve_at(model, dir, &source)?;
        if !inputs.sources.insert(source.value.clone()) {
            continue;
        }
        let owner = paths.object_owner(&source.value);
        dependencies.extend(owner);
        if matches!(
            target.kind,
            TargetKind::Utility | TargetKind::GlobalTarget | TargetKind::InterfaceLibrary
        ) && target.name != STD_MODULE_TARGET
        {
            continue;
        }
        if owner.is_some() || external_object(dir, &source.value) {
            inputs
                .objects
                .insert(lower::build_relative(model, &source.value));
            continue;
        }
        let member = members.get(&source.value);
        let module = member.filter(|member| member.set.kind == FileSetKind::CxxModules);
        let source_props = dir.source_props.get(&source.value);
        let language = member
            .and_then(|member| member.set.props.get(b"LANGUAGE"))
            .filter(|language| !language.is_empty())
            .cloned()
            .or_else(|| lower::source_language(model, target, &source.value));
        if module.is_some() {
            validate_module_source(model, paths, target, &source.value, language.as_ref())?;
        }
        let Some(language) = language else { continue };
        let group_key = (
            language.clone(),
            member.map(|member| member.id),
            source_props
                .filter(|props| {
                    [
                        b"COMPILE_OPTIONS".as_slice(),
                        b"COMPILE_DEFINITIONS",
                        b"INCLUDE_DIRECTORIES",
                    ]
                    .iter()
                    .any(|name| props.get(name).is_some())
                })
                .map(|_| source.value.clone()),
        );
        let (group, facts) = match groups.entry(group_key) {
            Entry::Occupied(entry) => (entry.index(), entry.into_mut()),
            Entry::Vacant(entry) => {
                let compile_ctx = EvalContext {
                    language: Some(language.clone()),
                    ..ctx.clone()
                };
                let features = usage.collect(id, UsageField::CompileFeatures, &compile_ctx)?;
                if let Some(standard) = lower::required_standard(&language, &features.entries) {
                    inputs
                        .standard_requirements
                        .insert(language.clone(), standard);
                }
                let flags = usage
                    .collect(id, UsageField::CompileOptions, &compile_ctx)?
                    .entries;
                let mut defines = usage
                    .collect(id, UsageField::CompileDefinitions, &compile_ctx)?
                    .entries;
                if let Some(symbol) = &export_symbol
                    && !defines.iter().any(|define| define.value == *symbol)
                {
                    defines.insert(0, BT::new(symbol.clone(), target.backtrace.clone()));
                }
                let includes = usage.collect_includes(id, &compile_ctx)?;
                let group = entry.index();
                let mut facts = CompileGroup {
                    language,
                    flags,
                    defines,
                    includes,
                    units: Vec::new(),
                };
                sources::apply_compile_properties(
                    usage,
                    id,
                    &compile_ctx,
                    &source,
                    member,
                    source_props,
                    &mut facts,
                )?;
                let facts = entry.insert(facts);
                (group, facts)
            }
        };
        facts.units.push(resolved.compile_units.len());
        resolved.compile_units.push(CompileUnit {
            object: lower::object_path(model, target, &ctx.config, &source.value),
            source,
            group,
            scan: false,
        });
    }
    resolved.compile_groups = groups.into_values().collect();
    resolve_module_info(model, target, &inputs, &mut resolved, file_sets, &members);
    if file_sets || resolved.compile_units.iter().any(|unit| unit.scan) {
        validate_module_scanning(model, paths, target, &inputs, file_sets)?;
    }
    resolved.dependencies = dependencies.into_iter().collect();
    Ok((resolved, inputs))
}

const STD_MODULE_TARGET: &[u8] = b"@cmake_cxx_std";

struct FileSetMember<'a> {
    id: FileSetId,
    set: &'a FileSet,
    source: BT<BString>,
    relative_directory: BString,
}

fn collect_file_sets<'a>(
    model: &'a ProjectModel,
    paths: &SourcePaths<'_>,
    usage: &mut UsageResolver<'a>,
    id: TargetId,
    ctx: &EvalContext,
) -> Result<(bool, IndexMap<BString, FileSetMember<'a>>), ResolveError> {
    let mut has_modules = false;
    let mut members = IndexMap::new();
    let mut ctx = ctx.clone();
    ctx.head = Some(usage::handle(id));
    for (set_id, set) in usage.source_file_sets(id, &ctx)? {
        let is_module = set.kind == FileSetKind::CxxModules;
        has_modules |= is_module;
        let owner = model
            .target(set.target)
            .ok_or_else(|| ResolveError::Message("Unknown file set target.".into()))?;
        let dir = model.dir(owner.dir);
        let mut bases = usage
            .file_set_paths(set.target, b"BASE_DIRS", &set.base_dirs, &ctx)?
            .into_iter()
            .map(|base| absolute_path(&dir.source_dir, &base.value))
            .collect::<Vec<_>>();
        if bases.is_empty() {
            bases.push(dir.source_dir.clone());
        }
        for file in usage.file_set_paths(set.target, b"SOURCES", &set.files, &ctx)? {
            let value = paths.resolve_at(model, dir, &file)?;
            let parent = Lexical::new(&value).parent();
            let relative_directory = bases.iter().find_map(|base| {
                let base = Lexical::new(base);
                let parent = Lexical::new(parent);
                base.prefix(parent).then(|| parent.relative(base)).flatten()
            });
            let relative_directory =
                match relative_directory {
                    Some(path) if path == "." => BString::default(),
                    Some(path) => path,
                    None if !is_module => BString::default(),
                    None => return Err(ResolveError::Message(
                        format!(
                            "File {value} must be in one of the base directories of file set {}.",
                            set.name
                        )
                        .into(),
                    )),
                };
            let member = FileSetMember {
                id: set_id,
                set,
                source: BT::new(value.clone(), file.backtrace.clone()),
                relative_directory,
            };
            match members.entry(value) {
                Entry::Occupied(mut entry) if is_module => {
                    entry.insert(member);
                }
                Entry::Vacant(entry) => {
                    entry.insert(member);
                }
                _ => {}
            }
        }
    }
    Ok((has_modules, members))
}

fn scan_rule<'a>(model: &'a ProjectModel, target: &Target) -> Option<&'a BString> {
    lower::definition(model, target.dir, b"CMAKE_CXX_SCANDEP_SOURCE")
}

fn missing_scanner(target: &Target) -> ResolveError {
    ResolveError::Message(format!(
        "The target named \"{}\" has C++ sources that may use modules, but the compiler does not provide a way to discover the import graph dependencies.",
        target.name
    ).into())
}

fn generation_location(model: &ProjectModel, target: &Target) -> Location {
    let dir = &model.dir(target.dir).source_dir;
    Location::FileLine {
        file: absolute_path(dir, b"CMakeLists.txt").as_slice().into(),
        line: 0,
    }
}

fn module_error(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    body: BString,
) -> ResolveError {
    paths.emit(
        model,
        MessageKind::Error,
        generation_location(model, target),
        body.clone(),
    );
    ResolveError::Message(body)
}

fn validate_module_source(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    source: &[u8],
    language: Option<&BString>,
) -> Result<(), ResolveError> {
    let name = &target.name;
    let source = BString::from(source);
    let cxx_enabled = [b"CMAKE_CXX_COMPILER".as_slice(), b"CMAKE_CXX_COMPILER_ID"]
        .iter()
        .any(|key| lower::definition(model, target.dir, key).is_some());
    let unscheduled = || {
        format!(
            "Target \"{name}\" has source file\n\n  {source}\n\nin a \"FILE_SET TYPE CXX_MODULES\" but it is not scheduled for compilation."
        )
    };
    if !cxx_enabled {
        module_error(model, paths, target, format!(
            "The target named \"{name}\" has C++ sources that use modules, but the \"CXX\" language has not been enabled."
        ).into());
        return Err(module_error(model, paths, target, unscheduled().into()));
    }
    match language {
        Some(language) if language == b"CXX" => Ok(()),
        Some(_) => Err(module_error(model, paths, target, format!(
            "Target \"{name}\" contains the source\n\n  {source}\n\nin a file set of type \"CXX_MODULES\" but the source is not classified as a \"CXX\" source."
        ).into())),
        None => Err(module_error(model, paths, target, unscheduled().into())),
    }
}

fn validate_module_scanning(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    inputs: &TargetInputs,
    has_module_sets: bool,
) -> Result<(), ResolveError> {
    if has_module_sets && !allows_module_scanning(model, target, inputs) {
        let standard = cxx_standard(model, target).map_or_else(|| "98".into(), BString::clone);
        return Err(module_error(model, paths, target, format!(
            "The target named \"{}\" has C++ sources that use modules, but does not include \"cxx_std_20\" (or newer) among its `target_compile_features`; found \"cxx_std_{standard}\".",
            target.name,
        ).into()));
    }
    if scan_rule(model, target).is_none() {
        let error = missing_scanner(target);
        paths.emit(
            model, MessageKind::Error, generation_location(model, target),
            format!("{error}  See the cmake-cxxmodules(7) manual for details.  Use the CMAKE_CXX_SCAN_FOR_MODULES variable to enable or disable scanning.").into(),
        );
        return Err(error);
    }
    Ok(())
}

fn cxx_standard<'a>(model: &'a ProjectModel, target: &'a Target) -> Option<&'a BString> {
    target
        .props
        .get(b"CXX_STANDARD")
        .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_STANDARD"))
        .filter(|standard| !standard.is_empty())
        .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_STANDARD_DEFAULT"))
}

fn allows_module_scanning(model: &ProjectModel, target: &Target, inputs: &TargetInputs) -> bool {
    let supports_modules = |standard: &[u8]| matches!(standard, b"20" | b"23" | b"26");
    cxx_standard(model, target).is_some_and(|value| supports_modules(value))
        || inputs
            .standard_requirements
            .get(b"CXX".as_slice())
            .is_some_and(|value| supports_modules(value))
}

fn resolve_module_info(
    model: &ProjectModel,
    target: &Target,
    inputs: &TargetInputs,
    resolved: &mut ResolvedTarget,
    has_module_sets: bool,
    members: &IndexMap<BString, FileSetMember<'_>>,
) {
    let allows_scanning = allows_module_scanning(model, target, inputs);
    let by_default = target.policies.policies.get(PolicyId::new(155)) == PolicyStatus::New
        && scan_rule(model, target).is_some();
    let target_setting = target
        .props
        .get(b"CXX_SCAN_FOR_MODULES")
        .or_else(|| lower::definition(model, target.dir, b"CMAKE_CXX_SCAN_FOR_MODULES"));
    let dir = model.dir(target.dir);
    let mut destinations = IndexMap::new();
    let mut units = Vec::new();
    for (index, unit) in resolved.compile_units.iter_mut().enumerate() {
        let member = members.get(&unit.source.value);
        if let Some(member) = member.filter(|member| member.set.kind == FileSetKind::CxxModules) {
            let visibility = match member.set.scope {
                UsageScope::Private => ModuleVisibility::Private,
                UsageScope::Public => ModuleVisibility::Public,
                UsageScope::Interface => ModuleVisibility::Interface,
            };
            let bmi_only = target.name == STD_MODULE_TARGET
                && lower::definition(model, target.dir, b"CMAKE_CXX_COMPILER_ID")
                    .is_none_or(|id| id != b"MSVC".as_slice());
            units.push(ModuleUnit {
                unit: index,
                file_set: member.set.name.clone(),
                public: visibility != ModuleVisibility::Private,
                visibility,
                relative_directory: member.relative_directory.clone(),
                destination: destinations
                    .entry(&member.set.name)
                    .or_insert_with(|| {
                        module_install(model, resolved.id, &resolved.config, Some(&member.set.name))
                            .map(|install| install.destination)
                    })
                    .clone(),
                bmi_only,
            });
            if bmi_only {
                unit.object.extend_from_slice(b".bmi");
            }
            unit.scan = true;
            continue;
        }
        if allows_scanning
            && resolved
                .compile_groups
                .get(unit.group)
                .is_some_and(|group| group.language == "CXX")
        {
            unit.scan = member
                .and_then(|member| member.set.props.get(b"CXX_SCAN_FOR_MODULES"))
                .or_else(|| {
                    dir.source_props
                        .get(&unit.source.value)
                        .and_then(|props| props.get(b"CXX_SCAN_FOR_MODULES"))
                })
                .or(target_setting)
                .map_or(by_default, |value| truth::is_on(value));
        }
    }
    if has_module_sets || resolved.compile_units.iter().any(|unit| unit.scan) {
        let module_dir = resolved.artifacts.object_dir.clone().unwrap_or_default();
        let directory = Lexical::new(&module_dir);
        resolved.cxx_modules = Some(CxxModuleInfo {
            units,
            tdi: directory.append(b"CXXDependInfo.json"),
            dyndep: directory.append(b"CXX.dd"),
            module_dir,
            bmi_install: module_install(model, resolved.id, &resolved.config, None),
            ..CxxModuleInfo::default()
        });
    }
}

fn module_install(
    model: &ProjectModel,
    id: TargetId,
    config: &ConfigId,
    file_set: Option<&[u8]>,
) -> Option<BmiInstall> {
    model.installs.iter().enumerate().find_map(|(index, rule)| {
        let InstallKind::Targets { targets, .. } = &rule.kind else {
            return None;
        };
        if !targets.contains(&id) {
            return None;
        }
        let prefix = format!("_RCMAKE_INSTALL_{index}_ARG");
        let mut args = (0..)
            .map_while(|i| {
                model
                    .global
                    .props
                    .get(format!("{prefix}{i}").as_bytes())
                    .map(|value| value.as_slice())
            })
            .peekable();
        if let Some(name) = file_set {
            loop {
                args.find(|arg| *arg == b"FILE_SET")?;
                if args.next()? == name {
                    break;
                }
            }
        } else {
            args.find(|arg| *arg == b"CXX_MODULES_BMI")?;
        }
        let options = &rule.options;
        let mut install = BmiInstall {
            component: options
                .component
                .clone()
                .or_else(|| {
                    lower::definition(model, rule.dir, b"CMAKE_INSTALL_DEFAULT_COMPONENT_NAME")
                        .cloned()
                })
                .unwrap_or_else(|| "Unspecified".into()),
            permissions: options.permissions.clone(),
            exclude_from_all: options.exclude_from_all,
            optional: options.optional,
            ..BmiInstall::default()
        };
        let mut has_destination = file_set.is_some() && options.destination.is_some();
        if has_destination {
            install.destination = options.destination.clone().unwrap_or_default();
        }
        let mut configs = options.configurations.clone();
        let is_keyword = |arg: &[u8]| {
            matches!(
                arg,
                b"ARCHIVE"
                    | b"LIBRARY"
                    | b"RUNTIME"
                    | b"FRAMEWORK"
                    | b"BUNDLE"
                    | b"OBJECTS"
                    | b"PUBLIC_HEADER"
                    | b"PRIVATE_HEADER"
                    | b"RESOURCE"
                    | b"FILE_SET"
                    | b"CXX_MODULES_BMI"
                    | b"INCLUDES"
                    | b"DESTINATION"
                    | b"COMPONENT"
                    | b"PERMISSIONS"
                    | b"OPTIONAL"
                    | b"EXCLUDE_FROM_ALL"
                    | b"CONFIGURATIONS"
            )
        };
        while let Some(arg) = args.next() {
            match arg {
                b"DESTINATION" => {
                    let destination = args.next()?;
                    install.destination = if destination.is_empty() {
                        BString::default()
                    } else {
                        Lexical::new(destination).normalize()
                    };
                    has_destination = true;
                }
                b"COMPONENT" => install.component = args.next()?.into(),
                b"OPTIONAL" => install.optional = true,
                b"EXCLUDE_FROM_ALL" => install.exclude_from_all = true,
                b"PERMISSIONS" | b"CONFIGURATIONS" => {
                    let values = std::iter::from_fn(|| args.next_if(|arg| !is_keyword(arg)))
                        .map(BString::from)
                        .collect();
                    if arg == b"PERMISSIONS" {
                        install.permissions = values;
                    } else {
                        configs = values;
                    }
                }
                _ => break,
            }
        }
        (has_destination && (configs.is_empty() || configs.iter().any(|name| config.matches(name))))
            .then_some(install)
    })
}

fn std_module_model(
    model: &ProjectModel,
    host: &dyn Host,
) -> Result<Option<(ProjectModel, TargetId)>, ResolveError> {
    use rcmake_json::{ReadOptions, Value};

    let dir = model.dir(DirId::ROOT);
    let Some(manifest) = lower::definition(model, DirId::ROOT, b"CMAKE_CXX_STDLIB_MODULES_JSON")
        .filter(|path| !truth::is_off(path))
    else {
        return Ok(None);
    };
    let error = |reason: &dyn std::fmt::Display| {
        ResolveError::Message(
            format!(
                "Failed to load C++ standard library modules metadata from \"{manifest}\": {reason}"
            )
            .into(),
        )
    };
    let path = absolute_path(&dir.source_dir, manifest);
    let data = host
        .read(
            &to_native(&path),
            rcmake_host::ReadRange::All {
                limit: rcmake_base::limits::Limits::default().max_value_bytes as u64,
            },
        )
        .map_err(|err| error(&err))?;
    let Value::Object(root) =
        rcmake_json::parse(&data, &ReadOptions::default()).map_err(|err| error(&err))?
    else {
        return Err(error(&"Expected a JSON object."));
    };
    if !matches!(
        root.get(b"version".as_slice()),
        Some(Value::Int(1) | Value::UInt(1))
    ) {
        return Err(error(&"The manifest version must be 1."));
    }
    let Some(Value::Array(entries)) = root.get(b"modules".as_slice()) else {
        return Err(error(&"The manifest has no modules array."));
    };
    let base = Lexical::new(&path).parent();
    let mut sources = Vec::new();
    let mut bases = IndexSet::new();
    let mut target = Target::new(
        STD_MODULE_TARGET.into(),
        TargetKind::ObjectLibrary,
        TargetOrigin::Normal,
        DirId::ROOT,
        Backtrace::default(),
        Default::default(),
    );
    for entry in entries {
        let Value::Object(entry) = entry else {
            return Err(error(&"Expected a module object."));
        };
        if !matches!(entry.get(b"logical-name".as_slice()), Some(Value::String(name)) if !name.is_empty())
        {
            return Err(error(&"A module has no logical name."));
        }
        let Some(Value::String(source)) = entry
            .get(b"source-path".as_slice())
            .filter(|value| matches!(value, Value::String(path) if !path.is_empty()))
        else {
            return Err(error(&"A module has no source path."));
        };
        let source = absolute_path(base, source);
        let lexical = Lexical::new(&source);
        bases.insert(BString::from(
            [lexical.root_name(), lexical.root_directory()].concat(),
        ));
        sources.push(BT::new(source, Backtrace::default()));
        if let Some(local) = entry.get(b"local-arguments".as_slice()) {
            std_module_arguments(&mut target, base, local).map_err(|reason| error(&reason))?;
        }
    }
    if sources.is_empty() {
        return Ok(None);
    }
    let mut model = model.clone();
    target.props.set("EXCLUDE_FROM_ALL".into(), "ON".into());
    target.usage.push(
        UsageField::CompileFeatures,
        BT::new("cxx_std_20".into(), Backtrace::default()),
    );
    if lower::definition(&model, DirId::ROOT, b"CMAKE_CXX_COMPILER_ID")
        .is_some_and(|id| id.ends_with(b"Clang"))
    {
        target.usage.push(
            UsageField::CompileOptions,
            BT::new(
                "-Wno-reserved-module-identifier".into(),
                Backtrace::default(),
            ),
        );
    }
    let id = model.add_target(target).map_err(|err| error(&err))?;
    model
        .add_file_set(FileSet {
            target: id,
            name: "CXX_MODULES".into(),
            kind: FileSetKind::CxxModules,
            scope: UsageScope::Public,
            base_dirs: bases
                .into_iter()
                .map(|path| BT::new(path, Backtrace::default()))
                .collect(),
            files: sources,
            props: Default::default(),
        })
        .map_err(|err| error(&err))?;
    Ok(Some((model, id)))
}

fn std_module_arguments(
    target: &mut Target,
    base: &[u8],
    value: &rcmake_json::Value,
) -> Result<(), &'static str> {
    use rcmake_json::Value;

    let Value::Object(local) = value else {
        return Err("Expected a local-arguments object.");
    };
    for key in [
        b"include-directories".as_slice(),
        b"system-include-directories",
    ] {
        if let Some(value) = local.get(key) {
            let Value::Array(dirs) = value else {
                return Err("Expected an array of include paths.");
            };
            for dir in dirs {
                let Value::String(dir) = dir else {
                    return Err("Expected an include path.");
                };
                let path = absolute_path(base, dir);
                target.usage.push(
                    UsageField::IncludeDirectories,
                    BT::new(path, Backtrace::default()),
                );
            }
        }
    }
    if let Some(value) = local.get(b"definitions".as_slice()) {
        let Value::Array(definitions) = value else {
            return Err("Expected an array of definitions.");
        };
        for definition in definitions {
            let Value::Object(definition) = definition else {
                return Err("Expected a definition object.");
            };
            let Some(Value::String(name)) = definition.get(b"name".as_slice()) else {
                return Err("A definition has no name.");
            };
            if matches!(definition.get(b"undef".as_slice()), Some(Value::Bool(true))) {
                continue;
            }
            let value = match definition.get(b"value".as_slice()) {
                Some(Value::String(value)) => [name.as_slice(), b"=", value].concat().into(),
                None => name.clone(),
                _ => return Err("Expected a string definition value."),
            };
            target.usage.push(
                UsageField::CompileDefinitions,
                BT::new(value, Backtrace::default()),
            );
        }
    }
    if let Some(Value::Object(vendor)) = local.get(b"vendor".as_slice())
        && let Some(Value::Object(cmake)) = vendor.get(b"cmake".as_slice())
    {
        for (key, field) in [
            (b"compile-options".as_slice(), UsageField::CompileOptions),
            (b"compile-features".as_slice(), UsageField::CompileFeatures),
        ] {
            if let Some(value) = cmake.get(key) {
                let Value::Array(values) = value else {
                    return Err("Expected an array of compiler arguments.");
                };
                for value in values {
                    let Value::String(value) = value else {
                        return Err("Expected a compiler argument.");
                    };
                    target
                        .usage
                        .push(field, BT::new(value.clone(), Backtrace::default()));
                }
            }
        }
    }
    Ok(())
}

fn export_symbol(target: &Target) -> Option<BString> {
    if !matches!(
        target.kind,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary
    ) {
        return None;
    }
    if let Some(symbol) = target.props.get(b"DEFINE_SYMBOL") {
        return (!symbol.is_empty()).then(|| symbol.clone());
    }
    let mut symbol = BString::default();
    if target.name.first().is_some_and(u8::is_ascii_digit) {
        symbol.push(b'_');
    }
    symbol.extend(target.name.iter().map(|&byte| {
        if byte.is_ascii_alphanumeric() {
            byte
        } else {
            b'_'
        }
    }));
    symbol.extend_from_slice(b"_EXPORTS");
    Some(symbol)
}

fn excluded(model: &ProjectModel, target: &Target) -> bool {
    if let Some(value) = target.props.get(b"EXCLUDE_FROM_ALL") {
        return truth::is_on(value);
    }
    let mut current = Some(target.dir);
    while let Some(id) = current {
        let dir = model.dir(id);
        if dir.exclude_from_all {
            return true;
        }
        current = dir.parent.filter(|parent| parent.index() < id.index());
    }
    false
}

fn absolute_path(base: &[u8], path: &[u8]) -> BString {
    Lexical::new(&Lexical::new(base).append(path)).normalize()
}

fn build_path(model: &ProjectModel, path: &[u8]) -> BString {
    let root = to_native(&model.dir(DirId::ROOT).binary_dir);
    let native = to_native(path);
    native
        .strip_prefix(root)
        .map_or_else(|_| path.into(), rcmake_host::path::from_native)
}

#[derive(Default)]
struct ResolvedLink {
    step: Option<LinkStep>,
    dependencies: Vec<TargetId>,
    module_links: Vec<TargetId>,
    objects: IndexSet<BString>,
}

fn resolve_link(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    paths: &SourcePaths<'_>,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<ResolvedLink, ResolveError> {
    let facts = resolved
        .get(&id)
        .ok_or_else(|| ResolveError::Message("Missing resolved target.".into()))?;
    let languages = std::iter::once(facts)
        .chain(facts.dependencies.iter().filter_map(|id| resolved.get(id)))
        .flat_map(|facts| {
            facts
                .compile_groups
                .iter()
                .map(|group| group.language.as_slice())
        });
    let mut ctx = EvalContext {
        link_language: lower::linker_language(model, target, languages.clone()),
        ..ctx.clone()
    };
    let kind = match target.kind {
        TargetKind::Executable => Some(LinkKind::Executable),
        TargetKind::StaticLibrary => Some(LinkKind::StaticArchive),
        TargetKind::SharedLibrary => Some(LinkKind::SharedLibrary),
        TargetKind::ModuleLibrary => Some(LinkKind::ModuleLibrary),
        _ => None,
    };
    let mut items = Vec::new();
    if kind.is_some() {
        items = usage.collect_link_items(id, &ctx)?;
        let language_for = |items: &[ResolvedLinkItem]| {
            let linked_languages = link_items(items)
                .filter_map(|item| match item.kind {
                    ResolvedLinkKind::Objects(id) => resolved.get(&id),
                    _ => None,
                })
                .flat_map(|facts| {
                    facts
                        .compile_groups
                        .iter()
                        .map(|group| group.language.as_slice())
                });
            lower::linker_language(model, target, languages.clone().chain(linked_languages))
        };
        let language = language_for(&items);
        if language != ctx.link_language {
            ctx.link_language = language;
            items = usage.collect_link_items(id, &ctx)?;
            if language_for(&items) != ctx.link_language {
                return Err(ResolveError::Message(
                    format!(
                        "The link language changed during link resolution for target {}.",
                        target.name
                    )
                    .into(),
                ));
            }
        }
    }
    let linked_targets = usage.link_targets(id, &ctx)?;
    let mut link = ResolvedLink {
        dependencies: target
            .utility_deps
            .iter()
            .filter_map(|dep| model.find_target(target.dir, &dep.value))
            .chain(linked_targets.iter().copied())
            .chain(facts.dependencies.iter().copied())
            .chain(link_items(&items).filter_map(|item| paths.object_owner(&item.text.value)))
            .filter(|dep| *dep != id && resolved.contains_key(dep))
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect(),
        module_links: linked_targets
            .into_iter()
            .chain(facts.dependencies.iter().copied())
            .collect(),
        ..ResolvedLink::default()
    };
    let Some(kind) = kind else {
        return Ok(link);
    };
    let language = ctx.link_language.clone().ok_or_else(|| {
        ResolveError::Message(
            [
                b"Cannot determine link language for target ",
                target.name.as_slice(),
            ]
            .concat()
            .into(),
        )
    })?;
    let mut fragments = Vec::new();
    for option in usage.collect(id, UsageField::LinkOptions, &ctx)?.entries {
        let value = option.value.as_slice();
        let text = value
            .strip_prefix(b"SHELL:")
            .map_or_else(|| shell_quote(value), BString::from);
        fragments.push(link_fragment(text, &option, LinkFragmentRole::Flags));
    }
    for directory in usage
        .collect(id, UsageField::LinkDirectories, &ctx)?
        .entries
    {
        let text = shell_quote(&[b"-L", directory.value.as_slice()].concat());
        fragments.push(link_fragment(
            text,
            &directory,
            LinkFragmentRole::LibraryPath,
        ));
    }
    for item in &items {
        append_link_item(model, resolved, &ctx.config, item, &mut fragments);
    }
    for item in link_items(&items) {
        if let ResolvedLinkKind::Objects(id) = item.kind {
            if let Some(facts) = resolved.get(&id) {
                link.objects
                    .extend(facts.compile_units.iter().map(|unit| unit.object.clone()));
            } else if let Some(objects) = usage.target_objects(id, &ctx.config, None) {
                link.objects
                    .extend(list::split(&objects, list::Empty::Drop));
            }
        }
    }
    let output = facts
        .artifacts
        .file
        .as_ref()
        .map_or_else(BString::default, |path| build_path(model, path));
    link.step = Some(LinkStep {
        kind,
        language,
        fragments,
        output,
    });
    Ok(link)
}

fn link_items(items: &[ResolvedLinkItem]) -> impl Iterator<Item = &ResolvedLinkItem> {
    items.iter().flat_map(|item| match &item.kind {
        ResolvedLinkKind::Group { members, .. } => members.as_slice(),
        _ => std::slice::from_ref(item),
    })
}

fn append_link_item(
    model: &ProjectModel,
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    config: &ConfigId,
    item: &ResolvedLinkItem,
    fragments: &mut Vec<LinkFragment>,
) {
    use LinkFragmentRole::Libraries;
    let value = item.text.value.as_slice();
    let (text, role) = match &item.kind {
        ResolvedLinkKind::Library(id) => {
            let Some(raw) = model.target(*id) else { return };
            let artifacts = resolved.get(id).map_or_else(
                || lower::compute_artifacts(model, raw, config),
                |target| target.artifacts.clone(),
            );
            let path = artifacts.linker_file.or(artifacts.file);
            if let Some(path) = path {
                append_library_path(&path, &item.text, fragments);
                return;
            }
            (raw_library(value), Libraries)
        }
        ResolvedLinkKind::Objects(_) | ResolvedLinkKind::Interface(_) => return,
        ResolvedLinkKind::Group { members, .. } => {
            for member in members {
                append_link_item(model, resolved, config, member, fragments);
            }
            return;
        }
        ResolvedLinkKind::ObjectFile => (shell_quote(value), Libraries),
        ResolvedLinkKind::Raw if value.contains(&b'/') && !value.starts_with(b"-") => {
            append_library_path(value, &item.text, fragments);
            return;
        }
        ResolvedLinkKind::Raw => (raw_library(value), Libraries),
    };
    fragments.push(link_fragment(text, &item.text, role));
}

fn append_library_path(path: &[u8], origin: &BT<BString>, fragments: &mut Vec<LinkFragment>) {
    let path_view = Lexical::new(path);
    if let Some(name) = path_view.filename().strip_suffix(b".framework")
        && !name.is_empty()
    {
        let directory = shell_quote(&[b"-F", path_view.parent()].concat());
        let role = LinkFragmentRole::FrameworkPath;
        if !fragments
            .iter()
            .any(|fragment| fragment.role == role && fragment.text.value == directory)
        {
            fragments.push(link_fragment(directory, origin, role));
        }
        let framework = [b"-framework ", shell_quote(name).as_slice()]
            .concat()
            .into();
        fragments.push(link_fragment(
            framework,
            origin,
            LinkFragmentRole::Libraries,
        ));
    } else {
        fragments.push(link_fragment(
            shell_quote(path),
            origin,
            LinkFragmentRole::Libraries,
        ));
    }
}

fn raw_library(value: &[u8]) -> BString {
    if value.starts_with(b"-") || value.contains(&b'/') || value.ends_with(b".lib") {
        shell_quote(value)
    } else {
        shell_quote(&[b"-l", value].concat())
    }
}

fn link_fragment(text: BString, origin: &BT<BString>, role: LinkFragmentRole) -> LinkFragment {
    LinkFragment {
        text: BT::new(text, origin.backtrace.clone()),
        role,
    }
}

fn shell_quote(value: &[u8]) -> BString {
    if !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(byte))
    {
        return value.into();
    }
    let mut quoted = BString::from("'");
    for &byte in value {
        if byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

fn resolve_exports(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    targets: &[ResolvedTarget],
    install: &InstallManifest,
) -> Result<Vec<ExportPlan>, ResolveError> {
    let mut plans = Vec::new();
    for rule in &model.installs {
        let InstallKind::Export {
            set,
            namespace,
            file,
            cxx_modules_directory,
        } = &rule.kind
        else {
            continue;
        };
        let members = export_set(model, set)?;
        let mut include_rules = IndexMap::<TargetId, Vec<_>>::new();
        for &index in &members.install_rules {
            let Some(rule) = model.installs.get(index) else {
                continue;
            };
            if let InstallKind::Targets { targets, .. } = &rule.kind
                && !rule.options.includes_destination.is_empty()
            {
                for &id in targets {
                    include_rules.entry(id).or_default().push(rule);
                }
            }
        }
        let mut plan = ExportPlan {
            set: set.clone(),
            namespace: namespace.clone(),
            file: file.clone().unwrap_or_else(|| export_filename(set)),
            cxx_modules_directory: cxx_modules_directory.clone(),
            install: true,
            ..ExportPlan::default()
        };
        for &id in &members.targets {
            let mut target = export_target(
                model,
                id,
                targets.iter().filter(|target| {
                    target.id == id
                        && (rule.options.configurations.is_empty()
                            || rule
                                .options
                                .configurations
                                .iter()
                                .any(|c| target.config.matches(c)))
                }),
                Some(install),
            )?;
            if let Some(rules) = include_rules.get(&id) {
                add_include_destinations(usage, id, rules, &mut target)?;
            }
            plan.targets.push(target);
        }
        plans.push(plan);
    }

    let mut build_plans = IndexMap::<BString, ExportPlan>::new();
    let props = &model.global.props;
    let count = props
        .get(b"_RCMAKE_BUILD_EXPORT_COUNT")
        .and_then(|value| property_index(value))
        .unwrap_or_default();
    for index in 0..count {
        let section = format!("_RCMAKE_BUILD_EXPORT_{index}");
        let dir_index = props
            .get(format!("{section}_DIRECTORY").as_bytes())
            .and_then(|value| property_index(value))
            .ok_or_else(invalid_export_record)?;
        let (dir, directory) = model
            .dirs()
            .nth(dir_index)
            .ok_or_else(invalid_export_record)?;
        let args: Vec<_> = (0..)
            .map_while(|arg| props.get(format!("{section}_ARG{arg}").as_bytes()))
            .map(|value| value.as_slice())
            .collect();
        let (set, ids, mut options) = match args.as_slice() {
            [b"EXPORT", set, options @ ..] => (
                BString::from(*set),
                export_set(model, set)?.targets.clone(),
                options,
            ),
            [b"TARGETS", rest @ ..] => {
                let end = rest
                    .iter()
                    .position(|name| {
                        matches!(
                            *name,
                            b"FILE"
                                | b"NAMESPACE"
                                | b"APPEND"
                                | b"EXPORT_LINK_INTERFACE_LIBRARIES"
                                | b"CXX_MODULES_DIRECTORY"
                        )
                    })
                    .unwrap_or(rest.len());
                let (names, options) = rest.split_at(end);
                let ids = names
                    .iter()
                    .map(|name| {
                        model
                            .find_target(dir, name)
                            .ok_or_else(invalid_export_record)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (BString::default(), ids, options)
            }
            _ => return Err(invalid_export_record()),
        };
        let mut namespace = BString::default();
        let mut file = export_filename(&set);
        let mut cxx_modules_directory = None;
        while let Some((key, rest)) = options.split_first() {
            options = rest;
            match *key {
                b"FILE" | b"NAMESPACE" | b"CXX_MODULES_DIRECTORY" => {
                    let (arg, rest) = options.split_first().ok_or_else(invalid_export_record)?;
                    if *key == b"FILE" {
                        file = (*arg).into();
                    } else if *key == b"NAMESPACE" {
                        namespace = (*arg).into();
                    } else {
                        cxx_modules_directory = Some((*arg).into());
                    }
                    options = rest;
                }
                b"APPEND" | b"EXPORT_LINK_INTERFACE_LIBRARIES" => {}
                _ => return Err(invalid_export_record()),
            }
        }
        let file = absolute_path(&directory.binary_dir, &file);
        let plan = build_plans
            .entry(file.clone())
            .or_insert_with(|| ExportPlan {
                set,
                namespace,
                file,
                cxx_modules_directory,
                ..ExportPlan::default()
            });
        for id in ids {
            let target = export_target(
                model,
                id,
                targets.iter().filter(|target| target.id == id),
                None,
            )?;
            if !plan
                .targets
                .iter()
                .any(|existing| existing.name == target.name)
            {
                plan.targets.push(target);
            }
        }
    }
    plans.extend(build_plans.into_values());
    Ok(plans)
}

fn add_include_destinations(
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    rules: &[&InstallRule],
    target: &mut ExportedTarget,
) -> Result<(), ResolveError> {
    const KEY: &[u8] = b"INTERFACE_INCLUDE_DIRECTORIES";
    const PREFIX: &[u8] = b"${_IMPORT_PREFIX}";
    let handle = TargetHandle(id.index() as u32);
    let ctx = EvalContext {
        head: Some(handle),
        current: Some(handle),
        ..EvalContext::default()
    };
    let mut includes = BString::default();
    for rule in rules {
        let mut host = usage.host(rule.dir);
        for destination in &rule.options.includes_destination {
            let value = rcmake_genex::rewrite_install_prefix(destination, PREFIX);
            let ast = rcmake_genex::strip(&rcmake_genex::parse(&value)?, PreprocessMode::Install);
            let (paths, flags) = rcmake_genex::evaluate(&ast, &ctx, &mut host)?;
            if flags.context_sensitive || flags.head_sensitive || flags.link_language_sensitive {
                return Err(ResolveError::Message(
                    "INCLUDES DESTINATION requires paths that do not depend on the evaluation context."
                        .into(),
                ));
            }
            for path in list::split(&paths, list::Empty::Drop) {
                if !includes.is_empty() {
                    includes.push(b';');
                }
                if !Lexical::new(&path).absolute() && !path.starts_with(PREFIX) {
                    includes.extend_from_slice(PREFIX);
                    includes.push(b'/');
                }
                includes.extend_from_slice(&path);
            }
        }
    }
    if !includes.is_empty() {
        match target.properties.entry(KEY.into()) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().push(b';');
                entry.get_mut().extend_from_slice(&includes);
            }
            Entry::Vacant(entry) => {
                entry.insert(includes);
            }
        }
    }
    Ok(())
}

fn property_index(value: &[u8]) -> Option<usize> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn invalid_export_record() -> ResolveError {
    ResolveError::Message("Invalid build export record.".into())
}

fn export_set<'a>(model: &'a ProjectModel, set: &[u8]) -> Result<&'a ExportSet, ResolveError> {
    model
        .exports
        .get(set)
        .ok_or_else(|| ResolveError::Message([b"Unknown export set: ", set].concat().into()))
}

fn export_filename(set: &[u8]) -> BString {
    [set, b".cmake"].concat().into()
}

fn export_target<'a>(
    model: &ProjectModel,
    id: TargetId,
    resolved: impl Iterator<Item = &'a ResolvedTarget>,
    install: Option<&InstallManifest>,
) -> Result<ExportedTarget, ResolveError> {
    let target = model
        .target(id)
        .filter(|target| !target.is_imported())
        .ok_or_else(invalid_export_record)?;
    let kind = match target.kind {
        TargetKind::Executable => "EXECUTABLE",
        TargetKind::StaticLibrary => "STATIC",
        TargetKind::SharedLibrary => "SHARED",
        TargetKind::ModuleLibrary => "MODULE",
        TargetKind::ObjectLibrary if install.is_none() => "OBJECT",
        TargetKind::ObjectLibrary | TargetKind::InterfaceLibrary => "INTERFACE",
        _ => return Err(invalid_export_record()),
    };
    let mut exported = ExportedTarget {
        name: target
            .props
            .get(b"EXPORT_NAME")
            .cloned()
            .unwrap_or_else(|| target.name.clone()),
        kind: kind.into(),
        ..ExportedTarget::default()
    };
    let mode = if install.is_some() {
        PreprocessMode::Install
    } else {
        PreprocessMode::Build
    };
    for key in target
        .props
        .names()
        .into_iter()
        .filter(|key| key.starts_with(b"INTERFACE_"))
    {
        if let Some(value) = target.props.get(&key) {
            add_export_property(&mut exported, key, value, mode)?;
        }
    }
    for (field, entries) in &target.usage.fields {
        let key = field.name();
        if key.starts_with("INTERFACE_") {
            let value = list::join(entries.iter().map(|entry| &entry.value));
            add_export_property(&mut exported, key.into(), &value, mode)?;
        }
    }
    if let Some(items) = &target.usage.interface_link_libraries {
        let value = list::join(items.iter().map(|item| &item.text.value));
        add_export_property(
            &mut exported,
            "INTERFACE_LINK_LIBRARIES".into(),
            &value,
            mode,
        )?;
    }
    for facts in resolved {
        let mut properties = IndexMap::new();
        for (key, path) in [
            ("IMPORTED_LOCATION", &facts.artifacts.file),
            ("IMPORTED_IMPLIB", &facts.artifacts.import_file),
        ] {
            if let Some(path) = path
                && let Some(path) = exported_location(install, facts, path)
            {
                properties.insert(key.into(), path);
            }
        }
        if properties.contains_key(b"IMPORTED_LOCATION".as_slice()) {
            if let Some(soname) = &facts.artifacts.soname_file {
                properties.insert(
                    "IMPORTED_SONAME".into(),
                    Lexical::new(soname).filename().into(),
                );
            }
            if kind == "STATIC" {
                let languages: IndexSet<_> = facts
                    .compile_groups
                    .iter()
                    .map(|group| &group.language)
                    .collect();
                properties.insert(
                    "IMPORTED_LINK_INTERFACE_LANGUAGES".into(),
                    list::join(languages),
                );
            }
        } else if kind == "OBJECT" && !facts.compile_units.is_empty() {
            let base = &model.dir(DirId::ROOT).binary_dir;
            let paths = facts
                .compile_units
                .iter()
                .map(|unit| absolute_path(base, &unit.object));
            properties.insert("IMPORTED_OBJECTS".into(), list::join(paths));
        }
        if !properties.is_empty() {
            exported
                .config_properties
                .insert(facts.config.0.clone(), properties);
        }
    }
    Ok(exported)
}

fn exported_location(
    install: Option<&InstallManifest>,
    target: &ResolvedTarget,
    path: &BString,
) -> Option<BString> {
    let Some(install) = install else {
        return Some(path.clone());
    };
    install.actions.iter().find_map(|action| {
        let InstallActionKind::Files {
            files, file_type, ..
        } = &action.kind
        else {
            return None;
        };
        if matches!(file_type.as_slice(), b"FILE" | b"PROGRAM")
            || !files.contains(path)
            || (!action.configurations.is_empty()
                && !action
                    .configurations
                    .iter()
                    .any(|name| target.config.matches(name)))
        {
            return None;
        }
        Some(absolute_path(
            &action.destination,
            Lexical::new(path).filename(),
        ))
    })
}

fn add_export_property(
    target: &mut ExportedTarget,
    key: BString,
    value: &[u8],
    mode: PreprocessMode,
) -> Result<(), ResolveError> {
    let ast = rcmake_genex::strip(&rcmake_genex::parse(value)?, mode);
    let mut items = Vec::new();
    let mut item = BString::default();
    for node in &ast.nodes {
        match node {
            GenexNode::Text(text) => {
                let mut escaped = false;
                for &byte in text.iter() {
                    if byte == b';' && !escaped {
                        if !item.is_empty() {
                            items.push(std::mem::take(&mut item));
                        }
                    } else {
                        item.push(byte);
                    }
                    escaped = byte == b'\\' && !escaped;
                }
            }
            GenexNode::Expr(_) => write_genex(std::slice::from_ref(node), &mut item),
        }
    }
    if !item.is_empty() {
        items.push(item);
    }
    if mode == PreprocessMode::Install {
        for item in &mut items {
            *item = rcmake_genex::rewrite_install_prefix(item, b"${_IMPORT_PREFIX}");
            if matches!(
                key.as_slice(),
                b"INTERFACE_INCLUDE_DIRECTORIES" | b"INTERFACE_SYSTEM_INCLUDE_DIRECTORIES"
            ) && !Lexical::new(item).absolute()
                && !item.starts_with(b"$")
            {
                *item = [b"${_IMPORT_PREFIX}/", item.as_slice()].concat().into();
            }
        }
    }
    if !items.is_empty() {
        target.properties.insert(key, list::join(items));
    }
    Ok(())
}

fn write_genex(nodes: &[GenexNode], output: &mut BString) {
    for node in nodes {
        match node {
            GenexNode::Text(text) => output.extend_from_slice(text),
            GenexNode::Expr(expr) => {
                output.extend_from_slice(b"$<");
                write_genex(&expr.identifier, output);
                if let Some(args) = &expr.params {
                    output.push(b':');
                    for (index, arg) in args.iter().enumerate() {
                        if index != 0 {
                            output.push(b',');
                        }
                        write_genex(arg, output);
                    }
                }
                output.push(b'>');
            }
        }
    }
}
