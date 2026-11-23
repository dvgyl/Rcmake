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
//!
//! # Status
//!
//! Wave 0 freezes the types. [`resolve`] and [`lower::lower`] return
//! [`ResolveError::Unimplemented`] until Wave 1 and Wave 2 fill them in.

#![forbid(unsafe_code)]

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::Backtrace;
use rcmake_genex::ConfigId;
use rcmake_host::Host;
use rcmake_model::{BT, ProjectModel, TargetId, TargetKind};

pub mod lower;
pub mod usage;

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

/// Resolve `model` for each configuration in `configs` and lower the result.
pub fn resolve(
    _model: &ProjectModel,
    _host: &dyn Host,
    _configs: &[ConfigId],
) -> Result<GenerationResult, ResolveError> {
    Err(ResolveError::Unimplemented("target resolution"))
}
