use indexmap::{IndexMap, IndexSet};
use rcmake_base::BString;
use rcmake_cxxmodules::{ModmapFormat, ModuleVisibility, TargetDependInfo};
use rcmake_diag::Backtrace;
use rcmake_genex::ConfigId;
use rcmake_model::{BT, CommandId, TargetId, TargetKind, UsageField, UsageScope};

use crate::{InstallManifest, ide};

mod device_response;

/// Error of resolution or lowering.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// A generator expression failed.
    #[error(transparent)]
    Genex(#[from] rcmake_genex::GenexError),
    /// An error that the caller must report.
    #[error("{0}")]
    Message(BString),
    /// An error that the resolver has already reported.
    #[error("{0}")]
    Reported(BString),
}

/// Everything a generator needs (output of [`resolve`](crate::resolve) plus lowering).
#[derive(Clone, Debug, Default)]
pub struct GenerationResult {
    /// Configurations in generation order (one for single-config generators).
    pub configs: Vec<ConfigId>,
    /// Resolved targets for each configuration, in model order.
    pub targets: Vec<ResolvedTarget>,
    /// One lowered build graph per configuration.
    pub graphs: Vec<BuildGraph>,
    /// Cross-config graphs indexed by command and output configuration.
    pub cross_graphs: IndexMap<(ConfigId, ConfigId), BuildGraph>,
    /// Additional file dependencies of each compile output.
    pub compile_inputs: std::collections::BTreeMap<BString, Vec<BString>>,
    /// Install actions for `cmake_install.cmake`.
    pub install: InstallManifest,
    /// `install(EXPORT)` and `export()` plans.
    pub exports: Vec<ExportPlan>,
    /// Non-target metadata.
    pub metadata: ConfigureMetadata,
    /// Resolved entries for which the caller requested provenance.
    pub provenance_entries: Vec<ProvenanceEntry>,
    /// Dependency paths for recorded entries.
    pub chains: Option<Box<ResolveChains>>,
}

pub(crate) struct CrossConfiguration<'a> {
    pub(crate) command: &'a ConfigId,
    pub(crate) shared_outputs: &'a IndexSet<CommandId>,
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
    /// Output command step indices that need the GNU Make jobserver.
    pub jobserver_steps: IndexSet<usize>,
    /// Auxiliary files that the configure step writes.
    pub generated_files: Vec<BString>,
    /// Auxiliary paths that the build system removes on clean.
    pub clean_files: Vec<BString>,
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
    /// Fortran module metadata when the target needs collation.
    pub fortran: Option<FortranTargetInfo>,
    /// IDE facts. `None` unless [`ResolveOptions::ide`] is set.
    pub ide: Option<Box<ide::IdeFacts>>,
}

/// Options of [`resolve_with_options`](crate::resolve_with_options).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolveOptions {
    /// Compute [`ResolvedTarget::ide`] for this IDE flavor.
    pub ide: Option<ide::IdeFlavor>,
    /// Collect each resolved entry and its dependency path.
    pub provenance: bool,
}

/// Kind of resolved entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainKind {
    /// Entry from a usage requirement.
    Usage(UsageField),
    /// Link dependency or argument.
    LinkItem,
}

/// Identity of one entry in the resolution result.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChainKey {
    /// Target that consumes the entry.
    pub target: TargetId,
    /// Configuration of the target.
    pub config: ConfigId,
    /// Kind of entry.
    pub kind: ChainKind,
    /// Position among entries of this kind for the target and configuration.
    pub ordinal: u32,
}

/// One dependency on the path to the entry.
#[derive(Clone, Debug)]
pub struct Hop {
    /// Dependency target reached by this edge.
    pub from: TargetId,
    /// Scope of the link edge.
    pub via: UsageScope,
    /// Call stack of the link edge.
    pub link_bt: Backtrace,
}

/// Paths from consuming targets to the recorded entries.
pub type ResolveChains = IndexMap<ChainKey, Vec<Hop>>;

/// Value and target references of a resolved entry.
#[derive(Clone, Debug)]
pub struct ProvenanceEntry {
    /// Identity of the entry and its dependency path.
    pub key: ChainKey,
    /// Evaluated value and its call stack.
    pub value: BT<BString>,
    /// Targets that this link entry names.
    pub link_targets: Vec<TargetId>,
}

/// Fortran module facts of one target and configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FortranTargetInfo {
    /// Directory for compiler-generated modules.
    pub module_dir: Option<BString>,
    /// Path of the dependency metadata file.
    pub depend_info: BString,
    /// Path of the collation result.
    pub dyndep: BString,
    /// Path of the module summary.
    pub modules_json: BString,
    /// Directories of module summaries that this target reads.
    pub linked_target_dirs: Vec<BString>,
    /// Scanner metadata for a Make dependency pass.
    pub make_depend_info: Option<Box<DependInfo>>,
}

/// Target data for a module collation step.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant, reason = "Build steps box this enum once")]
pub enum DependInfo {
    /// C++ dependency metadata.
    Cxx(TargetDependInfo),
    /// Fortran dependency metadata.
    Fortran(rcmake_fortran::FortranBuildTdi),
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
    /// Ninja scanner commands that use response files, indexed by scan output path.
    pub scan_responses: IndexMap<BString, ScanResponse>,
}

/// A scanner invocation and the arguments that Ninja writes to its response file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanResponse {
    /// Scanner invocation with a reference to its response file.
    pub invocation: Invocation,
    /// Response file contents.
    pub arguments: BString,
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
    /// The `CMAKE_INSTALL_MESSAGE` setting when the install rule was created.
    pub message: BString,
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
    /// Compile flag fragments in command-line order. The `SHELL:` prefix marks raw shell text.
    pub flags: Vec<BT<BString>>,
    /// Preprocessor definitions (`NAME` or `NAME=value`).
    pub defines: Vec<BT<BString>>,
    /// Index of the first source-level definition, if present.
    pub source_define_start: Option<usize>,
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

impl LinkKind {
    /// Return the name of the Ninja rule for this link step.
    #[must_use]
    pub fn ninja_rule(self) -> &'static [u8] {
        match self {
            Self::Executable => b"LINK_EXECUTABLE",
            Self::SharedLibrary => b"LINK_SHARED_LIBRARY",
            Self::ModuleLibrary => b"LINK_MODULE_LIBRARY",
            Self::StaticArchive => b"STATIC_ARCHIVE",
        }
    }

    pub(crate) fn for_target(kind: TargetKind) -> Option<Self> {
        match kind {
            TargetKind::Executable => Some(Self::Executable),
            TargetKind::StaticLibrary => Some(Self::StaticArchive),
            TargetKind::SharedLibrary => Some(Self::SharedLibrary),
            TargetKind::ModuleLibrary => Some(Self::ModuleLibrary),
            _ => None,
        }
    }

    pub(crate) fn rule_variable(self) -> &'static [u8] {
        match self {
            Self::Executable => b"LINK_EXECUTABLE",
            Self::SharedLibrary => b"CREATE_SHARED_LIBRARY",
            Self::ModuleLibrary => b"CREATE_SHARED_MODULE",
            Self::StaticArchive => b"CREATE_STATIC_LIBRARY",
        }
    }

    pub(crate) fn flag_class(self) -> &'static [u8] {
        match self {
            Self::Executable => b"EXE",
            Self::SharedLibrary => b"SHARED",
            Self::ModuleLibrary => b"MODULE",
            Self::StaticArchive => b"STATIC",
        }
    }

    pub(crate) fn create_flags_key(self, language: &[u8]) -> BString {
        let prefix: &[u8] = match self {
            Self::Executable => b"CMAKE_EXECUTABLE_CREATE_",
            Self::SharedLibrary => b"CMAKE_SHARED_LIBRARY_CREATE_",
            Self::ModuleLibrary => b"CMAKE_SHARED_MODULE_CREATE_",
            Self::StaticArchive => b"CMAKE_STATIC_LIBRARY_CREATE_",
        };
        [prefix, language, b"_FLAGS"].concat().into()
    }

    pub(crate) fn default_create_flag(self, darwin: bool) -> &'static [u8] {
        match self {
            Self::SharedLibrary if darwin => b"-dynamiclib",
            Self::ModuleLibrary if darwin => b"-bundle",
            Self::SharedLibrary | Self::ModuleLibrary => b"-shared",
            Self::Executable | Self::StaticArchive => b"",
        }
    }

    pub(crate) fn target_flags_property(self) -> &'static [u8] {
        match self {
            Self::StaticArchive => b"STATIC_LIBRARY_FLAGS",
            _ => b"LINK_FLAGS",
        }
    }
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
    /// Command fragments in order. Lowered archives keep only flags.
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
    /// The primary output is a symbolic name, with no file to update.
    pub symbolic: bool,
}

/// How the device object is built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceLinkMode {
    /// The compiler driver writes the device object.
    Driver {
        /// Link command.
        invocation: Invocation,
        /// Inputs that the driver can read from a response file.
        response_file: Option<DeviceLinkResponseFile>,
    },
    /// Separate tools link each architecture and compile the registration stub.
    Explicit {
        /// Device binaries in architecture order.
        cubins: Vec<CubinLink>,
        /// Command that combines the device binaries.
        fatbinary: Invocation,
        /// Header generated by the fatbinary command.
        fatbin_header: BString,
        /// Header generated by the first device linker invocation.
        register_header: BString,
        /// Command that compiles the registration stub into the device object.
        compile: Invocation,
    },
}

/// Arguments that a generator can move into device-link response files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceLinkResponseFile {
    /// Toolchain prefix for a response file path. Trailing space separates arguments.
    pub flag: BString,
    /// Arguments from `<OBJECTS>` in command order.
    pub objects: Vec<BString>,
    /// Library arguments from `<LINK_LIBRARIES>`. Link directories stay on the command line.
    pub libraries: Vec<BString>,
}

/// One invocation of an explicit device linker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CubinLink {
    /// Architecture name, without the sm_ prefix.
    pub arch: BString,
    /// Device binary path.
    pub output: BString,
    /// Device linker command.
    pub invocation: Invocation,
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
        /// Preprocessed input used by the compiler, when present.
        compile_input: Option<BString>,
        /// Compiler invocation.
        invocation: Invocation,
        /// Compiler-written dependency file.
        depfile: Option<BString>,
        /// Module map file that the compiler reads (scanned units only).
        modmap: Option<BString>,
        /// Ninja dyndep file of the unit's target (scanned units only).
        dyndep: Option<BString>,
    },
    /// Scan one unit for module dependencies.
    Scan {
        /// Language of the source.
        lang: BString,
        /// Output of preprocessing, when the scanner writes it.
        preprocessed: Option<BString>,
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
        /// Language of the scanned sources.
        lang: BString,
        /// Target name.
        target: BString,
        /// Input `.ddi` paths.
        ddis: Vec<BString>,
        /// Output dyndep file.
        dd: BString,
        /// Output module map files, one for each scanned unit.
        modmaps: Vec<BString>,
        /// Path of the dependency metadata file.
        tdi: BString,
        /// Data for the dependency metadata file.
        depend_info: Box<DependInfo>,
        /// Module map format when required by the compiler.
        modmap_format: Option<ModmapFormat>,
        /// The TDI file and the module tables that collation reads.
        implicit_inputs: Vec<BString>,
    },
    /// Link or archive.
    Link {
        /// Inputs (objects and libraries) in order.
        inputs: Vec<BString>,
        /// File prerequisites that do not become linker arguments.
        implicit_inputs: Vec<BString>,
        /// The link step.
        step: LinkStep,
        /// Linker invocations (archive, ranlib, symlinks).
        invocations: Vec<Invocation>,
    },
    /// Build a device object before a host link.
    DeviceLink {
        /// Language that supplies the device linker.
        lang: BString,
        /// Device object path.
        output: BString,
        /// File prerequisites.
        inputs: Vec<BString>,
        /// Device link commands and outputs.
        mode: DeviceLinkMode,
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
    /// Alternative XCFramework locations by configuration.
    pub xcframework_locations: IndexMap<BString, BString>,
    /// Source file sets to declare on the imported target.
    pub source_sets: Vec<ExportedSourceSet>,
}

/// A source file set for one imported target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedSourceSet {
    /// File set type.
    pub kind: rcmake_model::FileSetKind,
    /// Name of the file set.
    pub name: BString,
    /// Exported base directories.
    pub bases: Vec<BString>,
    /// Exported file paths.
    pub files: Vec<BString>,
    /// Interface properties and language.
    pub properties: IndexMap<BString, BString>,
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
    /// Package names and extra arguments in lookup order.
    pub package_dependencies: IndexMap<BString, Vec<BString>>,
}
