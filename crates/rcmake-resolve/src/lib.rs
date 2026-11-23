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
use rcmake_diag::Backtrace;
use rcmake_genex::{ConfigId, EvalContext, GenexHost, GenexNode, PreprocessMode, TargetHandle};
use rcmake_host::{FileKind, Follow, Host, path::to_native};
use rcmake_model::{
    BT, DirId, Directory, ExportSet, InstallKind, InstallRule, ProjectModel, Target, TargetId,
    TargetKind, UsageField,
};

use usage::{ResolvedLinkItem, ResolvedLinkKind, UsageResolver};

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
    let mut usage = UsageResolver::new(model);
    let ids: Vec<_> = model.targets().map(|(id, _)| id).collect();
    let mut targets = Vec::new();
    let mut graphs = Vec::with_capacity(configs.len());
    for config in &configs {
        let ctx = EvalContext {
            config: config.clone(),
            ..EvalContext::default()
        };
        let mut resolved: IndexMap<_, _> = model
            .targets()
            .filter(|(_, target)| !target.is_imported())
            .map(|(id, target)| {
                Ok((
                    id,
                    resolve_target(model, host, &mut usage, id, target, &ctx)?,
                ))
            })
            .collect::<Result<_, ResolveError>>()?;
        let links = model
            .targets()
            .filter(|(_, target)| !target.is_imported())
            .map(|(id, target)| resolve_link(model, &mut usage, &ids, &resolved, id, target, &ctx))
            .collect::<Result<Vec<_>, _>>()?;
        for (target, (link, dependencies)) in resolved.values_mut().zip(links) {
            target.link = link;
            target.dependencies = dependencies;
        }
        let resolved = resolved.into_values().collect::<Vec<_>>();
        graphs.push(lower::lower(model, &resolved, config)?);
        targets.extend(resolved);
    }

    let install = lower::lower_install_manifest(model, &targets);
    let exports = resolve_exports(model, &mut usage, &targets, &install)?;
    Ok(GenerationResult {
        configs,
        targets,
        graphs,
        install,
        exports,
        metadata: lower::collect_metadata(model),
    })
}

fn resolve_target(
    model: &ProjectModel,
    host: &dyn Host,
    usage: &mut UsageResolver<'_>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<ResolvedTarget, ResolveError> {
    let dir = model.dir(target.dir);
    let metadata = TargetMetadata {
        name: target.name.clone(),
        kind: target.kind,
        source_dir: dir.source_dir.clone(),
        binary_dir: dir.binary_dir.clone(),
        backtrace: target.backtrace.clone(),
        folder: target.props.get(b"FOLDER").cloned(),
        exclude_from_all: excluded(model, target),
        generator_provided: target.kind == TargetKind::GlobalTarget,
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
    };

    let export_symbol = export_symbol(target);
    let mut seen = IndexSet::new();
    let mut groups = IndexMap::new();
    for mut source in usage.collect_property(id, b"SOURCES", ctx)?.entries {
        source.value = source_path(host, dir, &source.value)?;
        if !seen.insert(source.value.clone())
            || matches!(
                target.kind,
                TargetKind::Utility | TargetKind::GlobalTarget | TargetKind::InterfaceLibrary
            )
            || source_flag(dir, &source.value, b"EXTERNAL_OBJECT")
        {
            continue;
        }
        let Some(language) = lower::source_language(model, target, &source.value) else {
            continue;
        };
        let (group, facts) = match groups.entry(language.clone()) {
            Entry::Occupied(entry) => (entry.index(), entry.into_mut()),
            Entry::Vacant(entry) => {
                let compile_ctx = EvalContext {
                    language: Some(language.clone()),
                    ..ctx.clone()
                };
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
                let facts = entry.insert(CompileGroup {
                    language,
                    flags,
                    defines,
                    includes,
                    units: Vec::new(),
                });
                (group, facts)
            }
        };
        facts.units.push(resolved.compile_units.len());
        resolved.compile_units.push(CompileUnit {
            object: lower::object_path(model, target, &ctx.config, &source.value),
            source,
            group,
        });
    }
    resolved.compile_groups = groups.into_values().collect();
    Ok(resolved)
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

fn source_flag(dir: &Directory, source: &BString, property: &[u8]) -> bool {
    dir.source_props
        .get(source)
        .and_then(|props| props.get(property))
        .is_some_and(|value| truth::is_on(value))
}

fn source_path(host: &dyn Host, dir: &Directory, source: &[u8]) -> Result<BString, ResolveError> {
    let source_path = absolute_path(&dir.source_dir, source);
    let binary_path = absolute_path(&dir.binary_dir, source);
    for path in [&source_path, &binary_path] {
        if source_flag(dir, path, b"GENERATED") {
            return Ok(path.clone());
        }
    }
    for path in [&source_path, &binary_path] {
        if host
            .metadata(&to_native(path), Follow::Yes)
            .is_ok_and(|metadata| metadata.kind == FileKind::File)
        {
            return Ok(path.clone());
        }
        if source_path == binary_path {
            break;
        }
    }
    Err(ResolveError::Message(
        [b"Cannot find source file:\n\n  ", source].concat().into(),
    ))
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

fn resolve_link(
    model: &ProjectModel,
    usage: &mut UsageResolver<'_>,
    ids: &[TargetId],
    resolved: &IndexMap<TargetId, ResolvedTarget>,
    id: TargetId,
    target: &Target,
    ctx: &EvalContext,
) -> Result<(Option<LinkStep>, Vec<TargetId>), ResolveError> {
    let facts = &resolved[&id];
    let language = lower::linker_language(
        model,
        target,
        facts
            .compile_groups
            .iter()
            .map(|group| group.language.as_slice()),
    );
    let ctx = EvalContext {
        link_language: language.clone(),
        ..ctx.clone()
    };
    let closure = usage
        .host(target.dir)
        .link_closure(TargetHandle(id.index() as u32), &ctx)?;
    let dependencies = target
        .utility_deps
        .iter()
        .filter_map(|dep| model.find_target(target.dir, &dep.value))
        .chain(
            closure
                .iter()
                .filter_map(|handle| ids.get(handle.0 as usize).copied()),
        )
        .filter(|dep| *dep != id && resolved.contains_key(dep))
        .collect::<IndexSet<_>>()
        .into_iter()
        .collect();
    let kind = match target.kind {
        TargetKind::Executable => LinkKind::Executable,
        TargetKind::StaticLibrary => LinkKind::StaticArchive,
        TargetKind::SharedLibrary => LinkKind::SharedLibrary,
        TargetKind::ModuleLibrary => LinkKind::ModuleLibrary,
        _ => return Ok((None, dependencies)),
    };
    let language = language.ok_or_else(|| {
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
    for item in &usage.collect_link_items(id, &ctx)? {
        append_link_item(model, resolved, &ctx.config, item, &mut fragments);
    }
    let output = facts
        .artifacts
        .file
        .as_ref()
        .map_or_else(BString::default, |path| build_path(model, path));
    Ok((
        Some(LinkStep {
            kind,
            language,
            fragments,
            output,
        }),
        dependencies,
    ))
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
    let (text, role) =
        match &item.kind {
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
            ResolvedLinkKind::Objects(id) => {
                if let Some(target) = resolved.get(id) {
                    fragments.extend(target.compile_units.iter().map(|unit| {
                        link_fragment(shell_quote(&unit.object), &item.text, Libraries)
                    }));
                } else if let Some(target) = model.target(*id) {
                    let key = [
                        b"IMPORTED_OBJECTS_",
                        config.0.to_ascii_uppercase().as_slice(),
                    ]
                    .concat();
                    if let Some(objects) = target
                        .props
                        .get(&key)
                        .or_else(|| target.props.get(b"IMPORTED_OBJECTS"))
                    {
                        fragments.extend(list::split(objects, list::Empty::Drop).into_iter().map(
                            |object| link_fragment(shell_quote(&object), &item.text, Libraries),
                        ));
                    }
                }
                return;
            }
            ResolvedLinkKind::Interface(_) => return,
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
                            b"FILE" | b"NAMESPACE" | b"APPEND" | b"EXPORT_LINK_INTERFACE_LIBRARIES"
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
        while let Some((key, rest)) = options.split_first() {
            options = rest;
            match *key {
                b"FILE" | b"NAMESPACE" => {
                    let (arg, rest) = options.split_first().ok_or_else(invalid_export_record)?;
                    if *key == b"FILE" {
                        file = (*arg).into();
                    } else {
                        namespace = (*arg).into();
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
