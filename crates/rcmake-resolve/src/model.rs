use std::collections::BTreeMap;
use std::sync::Arc;

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
    /// Install facts of targets for the File API.
    pub install_targets: crate::InstallTargetFacts,
    /// `install(EXPORT)` and `export()` plans.
    pub exports: Vec<ExportPlan>,
    /// Output paths that depend on the sources of a target that uses them.
    pub file_generate_cycles: IndexSet<(rcmake_model::DirId, BString)>,
    /// Generated command sources by target, in dependency trace order.
    pub traced_sources: IndexMap<TargetId, IndexSet<BString>>,
    /// Target-level build order, utility items, and traced sources
    /// (`cmComputeTargetDepends`, `cmTargetTraceDependencies`).
    pub target_depends: crate::TargetDependGraph,
    /// Non-target metadata.
    pub metadata: ConfigureMetadata,
    /// Resolved entries for which the caller requested provenance.
    pub provenance_entries: Vec<ProvenanceEntry>,
    /// Dependency paths for recorded entries.
    pub chains: Option<Box<ResolveChains>>,
    /// Post-overlay project model used during resolution (e.g., after autogen or rule
    /// instantiation), when any overlay modified the input model.
    pub resolved_model: Option<Box<rcmake_model::ProjectModel>>,
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
    /// Arguments from the root directory's `CMAKE_CTEST_ARGUMENTS` list.
    pub ctest_arguments: Vec<BString>,
    /// Output command step indices that need the GNU Make jobserver.
    pub jobserver_steps: IndexSet<usize>,
    /// Auxiliary files that the configure step writes.
    pub generated_files: Vec<BString>,
    /// Auxiliary paths that the build system removes on clean.
    pub clean_files: Vec<BString>,
    /// Whether `--compile-no-warning-as-error` was active during configure.
    pub ignore_compile_warning_as_error: bool,
    /// Whether `--link-no-warning-as-error` was active during configure.
    pub ignore_link_warning_as_error: bool,
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

/// Supply build tool capabilities during resolution.
pub trait GeneratorFacts: Send + Sync {
    /// Query whether Ninja supports dynamic dependencies for C++ modules.
    fn ninja_cxx_dyndep(
        &self,
        host: &dyn rcmake_host::Host,
        request: &rcmake_host::RunRequest,
    ) -> Option<bool>;
}

/// Options of [`resolve_with_options`](crate::resolve_with_options).
#[derive(Clone, Default)]
pub struct ResolveOptions {
    /// Compute [`ResolvedTarget::ide`] for this IDE flavor.
    pub ide: Option<ide::IdeFlavor>,
    /// Collect each resolved entry and its dependency path.
    pub provenance: bool,
    /// Capability source. Without a source, the resolver queries its host.
    pub generator_facts: Option<Arc<dyn GeneratorFacts>>,
}

impl std::fmt::Debug for ResolveOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolveOptions")
            .field("ide", &self.ide)
            .field("provenance", &self.provenance)
            .finish_non_exhaustive()
    }
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
    /// Index of the first source- or file-set-level compile flag, if present.
    pub source_flag_start: Option<usize>,
    /// Source- and file-set-level `COMPILE_OPTIONS` entry spans `(start, len)` inside `flags`.
    pub source_option_groups: Vec<(usize, usize)>,
    /// Preprocessor definitions (`NAME` or `NAME=value`).
    pub defines: Vec<BT<BString>>,
    /// Index of the first source-level definition, if present.
    pub source_define_start: Option<usize>,
    /// Include directories in search order.
    pub includes: Vec<InterfaceInclude>,
    /// Number of source- or file-set-level include directories prepended to `includes`.
    pub source_include_count: usize,
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

/// The quoting rules of the tool that reads a response file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseQuoting {
    /// GCC-compatible response-file quoting.
    Gnu,
    /// Microsoft command-line argument quoting.
    Msvc,
}

/// The inputs represented by a response-file span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseRole {
    /// Object files in link order.
    Objects,
    /// Libraries and their accompanying flags in link order.
    Libraries,
}

/// An expansion of one link-rule placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseSpan {
    /// Input role.
    pub role: ResponseRole,
    /// Argument indices in the invocation, including any launcher prefix.
    pub argv: std::ops::Range<usize>,
    /// Byte indices in the POSIX shell command.
    pub shell: std::ops::Range<usize>,
}

/// Response-file settings of a link or archive rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkResponse {
    /// Flag that precedes the response path. Trailing whitespace separates the arguments.
    pub flag: BString,
    /// Argument quoting used within the response file.
    pub quoting: ResponseQuoting,
    /// Explicit object response-file setting, when present.
    pub objects: Option<bool>,
    /// Explicit library response-file setting, when present.
    pub libraries: Option<bool>,
    /// Ordered input expansions.
    pub spans: Vec<ResponseSpan>,
    /// The command is part of an archive create, append, and finish sequence.
    pub archive_batch: bool,
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
    /// Input spans and tool settings for link response files.
    pub link_response: Option<Box<LinkResponse>>,
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
    /// Declared outputs that have no file to update.
    pub symbolic_outputs: Vec<BString>,
    /// Target dependencies with the configuration used by this command.
    pub target_dependencies: Vec<(TargetId, ConfigId)>,
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
        /// Additional files that the compiler creates with the object.
        byproducts: Vec<BString>,
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

/// Targets that one custom command names (`cmCustomCommandGenerator::GetUtilities`
/// and `cmTargetTraceDependencies::IsUtility`). Imported targets are included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandUtilities {
    /// Executable targets used as a program and targets in generator expressions.
    /// These utilities have the backtrace of the command.
    pub commands: IndexSet<TargetId>,
    /// Targets named in `DEPENDS`. These utilities have no backtrace.
    pub depends: IndexSet<TargetId>,
}

/// The runtime path options of one link line, as POSIX or Windows shell text
/// (`cmLinkLineComputer::ComputeLinkLibraries`). CMake writes the `-rpath` option
/// before the link libraries and the `-rpath-link` option after them. A Ninja
/// codemodel writes each `$` of this text as `$$`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkRuntimePaths {
    /// The `-rpath` options (one combined option for a separator-based flag).
    pub rpath: Option<BString>,
    /// The `-rpath-link` option.
    pub rpath_link: Option<BString>,
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

/// Owners of one registered custom output command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomOutputOwners {
    /// Command identity in the project model.
    pub command: rcmake_model::CommandId,
    /// Target identities of the actual owners.
    pub targets: IndexSet<TargetId>,
    /// Use only the command's explicit dependencies.
    pub explicit_only: bool,
    /// The command uses a shared output in Ninja Multi-Config.
    pub shared_output: bool,
}

/// Precompiled header files and flags for the File API.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PchDetails {
    /// Generated sources and the flags that compile them.
    pub create: Vec<PchSource>,
    /// Flags used by each translation unit that consumes a precompiled header.
    pub use_flags: BTreeMap<BString, BString>,
}

/// A generated source that creates a precompiled header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PchSource {
    /// Absolute source path.
    pub path: BString,
    /// Absolute precompiled header path.
    pub header: BString,
    /// Compile group of the units that use this header.
    pub group: usize,
    /// Shell-escaped PCH creation flags.
    pub flags: BString,
}

/// Kind of grouped Swift compiler action under `CMP0157` `NEW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwiftActionKind {
    /// Separate `-emit-module` action producing a `.swiftmodule`.
    Module,
    /// Grouped `-c` object compilation action producing one or more `.o` files.
    Objects,
    /// Combined compile + emit-module action (`Swift_SEPARATE_MODULE_EMISSION` off / `CMP0215` OLD).
    Combined,
}

/// Metadata for one grouped Swift compiler action in [`BuildGraph::swift_actions`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwiftAction {
    /// Role of this grouped Swift action.
    pub kind: SwiftActionKind,
    /// Swift module name (`Swift_MODULE_NAME` or target name with `-` replaced by `_`).
    pub module_name: BString,
    /// Explicit Swift source inputs (build-relative for generated files, absolute otherwise).
    pub sources: Vec<BString>,
    /// All explicit outputs produced by this action (build-relative paths).
    pub outputs: Vec<BString>,
    /// Upstream imported `.swiftmodule` files required as implicit inputs.
    pub implicit_inputs: Vec<BString>,
    /// Extra order-only inputs (for example, a target's own `.swiftmodule` on split object edges).
    pub order_only_inputs: Vec<BString>,
    /// Ninja progress description.
    pub description: BString,
    /// Response file path used when command length or response forcing applies.
    pub rsp_file: BString,
    /// Full rule invocation when compiler arguments are moved into [`Self::rsp_file`].
    pub rsp_invocation: Option<Invocation>,
    /// Arguments written to [`Self::rsp_file`] when [`Self::rsp_invocation`] is used.
    pub rsp_args: Option<BString>,
    /// Whether Ninja should set `restat = 1` on this edge.
    pub restat: bool,
    /// Whether single-output whole-module mode emits one shared module object.
    pub single_output: bool,
    /// Absolute source file paths corresponding to [`Self::sources`].
    pub abs_sources: Vec<BString>,
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
    /// Generated file set ordering by target index for CMP0154 NEW.
    pub file_set_order: IndexMap<usize, FileSetOrder>,
    /// Mach-O install name directories by link output path.
    pub install_name_dirs: IndexMap<BString, BString>,
    /// Ninja progress descriptions for link edges with custom commands.
    pub link_descriptions: IndexMap<BString, BString>,
    /// Ninja linker depfile paths keyed by primary link output path.
    pub link_depfiles: IndexMap<BString, BString>,
    /// Runtime path options of each link line, keyed by primary link output path.
    pub link_runtime_paths: IndexMap<BString, LinkRuntimePaths>,
    /// Custom rule indices that use the native configuration in a cross-config graph.
    pub native_custom_steps: IndexSet<usize>,
    /// Output command ownership indexed by graph step.
    pub custom_output_owners: IndexMap<usize, CustomOutputOwners>,
    /// Main dependencies that serve as the source of a custom output command.
    pub custom_main_sources: IndexMap<usize, BString>,
    /// Custom output command steps in source dependency discovery order.
    pub custom_source_steps: IndexMap<TargetId, IndexSet<usize>>,
    /// Targets that each lowered custom command names.
    pub command_utilities: IndexMap<CommandId, CommandUtilities>,
    /// Project target identities in graph order, before synthetic targets.
    pub target_ids: Vec<TargetId>,
    /// Steps that run custom target commands.
    pub utility_steps: IndexSet<usize>,
    /// Generated precompiled header files and flags by target.
    pub pch_details: IndexMap<TargetId, PchDetails>,
    /// Shell-escaped compiler launcher prefixes keyed by compile object path.
    pub compile_launchers: IndexMap<BString, BString>,
    /// Additional explicit outputs of a link edge (for example one-step Swift
    /// object files and `.swiftmodule`), keyed by primary link output path.
    pub link_extra_outputs: IndexMap<BString, Vec<BString>>,
    /// Byproducts declared by `PRE_BUILD`, `PRE_LINK`, or `POST_BUILD` target
    /// commands attached to a link edge, keyed by primary link output path.
    pub link_event_byproducts: IndexMap<BString, Vec<BString>>,
    /// Generated Swift `output-file-map.json` files (build-relative path to JSON text).
    pub swift_output_file_maps: IndexMap<BString, BString>,
    /// Grouped Swift compile/module action metadata indexed by step index in [`Self::steps`].
    pub swift_actions: IndexMap<usize, SwiftAction>,
    /// Ninja-formatted `compile_commands.json` command lines keyed by compile step index in [`Self::steps`].
    pub compile_db_commands: BTreeMap<usize, BString>,
}

/// File set members that affect compile ordering under CMP0154.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileSetOrder {
    /// Public files that consumers may include during compilation.
    pub public: IndexSet<BString>,
    /// Files that can build independently of other sources.
    pub independent: IndexSet<BString>,
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
    /// Target properties for an Android.mk export.
    pub android: Option<AndroidExportTarget>,
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

/// Resolved values for one Android.mk target block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AndroidExportTarget {
    /// Artifact path used by the import.
    pub artifact: BString,
    /// Ordered shared library references.
    pub shared: Vec<BString>,
    /// Ordered static library references.
    pub static_libraries: Vec<BString>,
    /// Ordered raw library references.
    pub ldlibs: Vec<BString>,
    /// Whether a static library uses C++.
    pub has_cpp: bool,
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
    /// Write Android.mk syntax.
    pub android: bool,
    /// Append to an existing Android.mk file.
    pub append: bool,
    /// Path syntax of the target system. An absent value uses the host syntax.
    pub windows_paths: Option<bool>,
    /// Targets in order.
    pub targets: Vec<ExportedTarget>,
    /// Package names and extra arguments in lookup order.
    pub package_dependencies: IndexMap<BString, Vec<BString>>,
}
