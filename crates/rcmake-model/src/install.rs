//! Install rules, export sets, build-tree exports, and tests.

use rcmake_base::BString;
use rcmake_diag::Backtrace;

use crate::{BT, DirId, PropertyMap, TargetId};

/// Options shared by install rule kinds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallOptions {
    /// `DESTINATION` (`None` when the rule uses a default from `GNUInstallDirs`).
    pub destination: Option<BString>,
    /// `COMPONENT` (`None` means `CMAKE_INSTALL_DEFAULT_COMPONENT_NAME`).
    pub component: Option<BString>,
    /// `PERMISSIONS` / `FILE_PERMISSIONS` keywords.
    pub permissions: Vec<BString>,
    /// `CONFIGURATIONS`.
    pub configurations: Vec<BString>,
    /// `OPTIONAL`.
    pub optional: bool,
    /// `EXCLUDE_FROM_ALL`.
    pub exclude_from_all: bool,
    /// `RENAME`.
    pub rename: Option<BString>,
    /// `INCLUDES DESTINATION` paths from `install(TARGETS ... INCLUDES DESTINATION ...)`.
    pub includes_destination: Vec<BString>,
}

/// The `install()` signature of a rule.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallKind {
    /// `install(TARGETS)`.
    Targets {
        /// Installed targets.
        targets: Vec<TargetId>,
        /// `EXPORT <set>`.
        export: Option<BString>,
    },
    /// `install(FILES)`.
    Files(Vec<BT<BString>>),
    /// `install(PROGRAMS)`.
    Programs(Vec<BT<BString>>),
    /// `install(DIRECTORY)`.
    Directory(Vec<BT<BString>>),
    /// `install(SCRIPT)`.
    Script(BString),
    /// `install(CODE)`.
    Code(BString),
    /// `install(EXPORT <set>)`.
    Export {
        /// Export set name.
        set: BString,
        /// `NAMESPACE`.
        namespace: BString,
        /// `FILE` (default `<set>.cmake`).
        file: Option<BString>,
        /// `CXX_MODULES_DIRECTORY`.
        cxx_modules_directory: Option<BString>,
    },
    /// `install(PACKAGE_INFO <package-name> ...)`.
    PackageInfo(Box<PackageInfoSpec>),
    /// `install(SBOM <sbom-name> ...)`.
    Sbom(Box<SbomSpec>),
}

/// Project metadata source of a CPS or SBOM file (`PROJECT <name>` or
/// `NO_PROJECT_METADATA`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProjectMetadata {
    /// Neither keyword: use the metadata of the current `project()`.
    #[default]
    Current,
    /// `PROJECT <project-name>`.
    Named(BString),
    /// `NO_PROJECT_METADATA`.
    None,
}

/// Arguments of `install(PACKAGE_INFO)` and `export(PACKAGE_INFO)`, after parsing.
///
/// The install-only options (`DESTINATION`, `PERMISSIONS`, `CONFIGURATIONS`,
/// `COMPONENT`, `EXCLUDE_FROM_ALL`) are in [`InstallOptions`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageInfoSpec {
    /// `<package-name>`.
    pub package: BString,
    /// `EXPORT <export-name>`.
    pub export: BString,
    /// `PROJECT <project-name>` or `NO_PROJECT_METADATA`.
    pub project: ProjectMetadata,
    /// `APPENDIX <appendix-name>`.
    pub appendix: Option<BString>,
    /// `LOWER_CASE_FILE`.
    pub lower_case_file: bool,
    /// `VERSION <version>`.
    pub version: Option<BString>,
    /// `COMPAT_VERSION <version>`.
    pub compat_version: Option<BString>,
    /// `VERSION_SCHEMA <string>`.
    pub version_schema: Option<BString>,
    /// `DEFAULT_TARGETS <target>...`.
    pub default_targets: Vec<BString>,
    /// `DEFAULT_CONFIGURATIONS <config>...`.
    pub default_configurations: Vec<BString>,
    /// `LICENSE <license-string>`.
    pub license: Option<BString>,
    /// `DEFAULT_LICENSE <license-string>`.
    pub default_license: Option<BString>,
    /// `DESCRIPTION <description-string>`.
    pub description: Option<BString>,
    /// `HOMEPAGE_URL <url-string>`.
    pub homepage_url: Option<BString>,
    /// `CXX_MODULES_DIRECTORY <directory>`.
    pub cxx_modules_directory: Option<BString>,
}

/// Arguments of `install(SBOM)` and `export(SBOM)`, after parsing.
///
/// `DESTINATION` of `install(SBOM)` is in [`InstallOptions`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SbomSpec {
    /// `<sbom-name>`.
    pub name: BString,
    /// `EXPORTS <export-names>...`.
    pub exports: Vec<BString>,
    /// `PROJECT <project-name>` or `NO_PROJECT_METADATA`.
    pub project: ProjectMetadata,
    /// `VERSION <major>[.<minor>[.<patch>[.<tweak>]]]`.
    pub version: Option<BString>,
    /// `DATA_LICENSE <license-string>`.
    pub data_license: Option<BString>,
    /// `DEFAULT_LICENSE <license-string>`.
    pub default_license: Option<BString>,
    /// `DESCRIPTION <description-string>`.
    pub description: Option<BString>,
    /// `HOMEPAGE_URL <url-string>`.
    pub homepage_url: Option<BString>,
    /// `PACKAGE_URL <url-string>`.
    pub package_url: Option<BString>,
    /// `FORMAT <string>` (`None` selects the default SPDX 3.0.1 JSON-LD format).
    pub format: Option<BString>,
}

/// File format of a build-tree export (`export()` signature).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExportKind {
    /// `export(EXPORT|TARGETS ...)`: a CMake `<file>.cmake` targets file.
    #[default]
    CMake,
    /// `export(PACKAGE_INFO ...)`: a CPS `<package>.cps` file.
    PackageInfo(PackageInfoSpec),
    /// `export(SBOM ...)`: an SBOM document.
    Sbom(SbomSpec),
}

/// One `export()` call that writes a file in the build tree, after argument parsing.
#[derive(Clone, Debug)]
pub struct BuildExport {
    /// Directory of the call.
    pub dir: DirId,
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// Output format and its arguments.
    pub kind: ExportKind,
    /// `EXPORT <set>` (`None` for `export(TARGETS)`, `PACKAGE_INFO`, and `SBOM`).
    pub set: Option<BString>,
    /// Targets of `export(TARGETS)`.
    pub targets: Vec<TargetId>,
    /// `NAMESPACE`.
    pub namespace: BString,
    /// Absolute output file (`FILE`, or the default name for the kind).
    pub file: Option<BString>,
    /// `APPEND`.
    pub append: bool,
    /// `CXX_MODULES_DIRECTORY`.
    pub cxx_modules_directory: Option<BString>,
}

/// One `install()` call, after argument parsing.
#[derive(Clone, Debug)]
pub struct InstallRule {
    /// Directory of the call.
    pub dir: DirId,
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// Signature and its items.
    pub kind: InstallKind,
    /// Shared options.
    pub options: InstallOptions,
}

/// An export set (`install(TARGETS ... EXPORT <set>)`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportSet {
    /// Set name.
    pub name: BString,
    /// Member targets in insertion order.
    pub targets: Vec<TargetId>,
    /// Indices into [`crate::ProjectModel::installs`] of the rules that add members.
    pub install_rules: Vec<usize>,
}

/// How ctest gets the tests of a [`TestEntry`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TestKind {
    /// One test defined at configure time (`add_test()`).
    #[default]
    Single,
    /// Tests that ctest discovers at test time (`discover_tests()`).
    Discover(Box<DiscoverySpec>),
}

/// Arguments of `discover_tests()` that control discovery at test time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverySpec {
    /// `DISCOVERY_ARGS <arg>...`: arguments appended to `COMMAND` for discovery.
    pub discovery_args: Vec<BString>,
    /// `DISCOVERY_MATCH <regex>`: regex applied to each output line.
    pub discovery_match: BString,
    /// `DISCOVERY_PROPERTIES <key> <value>...`: properties of the discovery run.
    pub discovery_props: Vec<(BString, BString)>,
    /// `TEST_NAME <replacement>`: name of each discovered test.
    pub test_name: BString,
    /// `TEST_ARGS <replacement>...`: arguments of each discovered test.
    pub test_args: Vec<BString>,
    /// `TEST_PROPERTIES <key> <replacement>...`: properties of each discovered test.
    pub test_props: Vec<(BString, BString)>,
}

/// A test (`add_test()`) or a test discovery rule (`discover_tests()`).
///
/// A [`TestKind::Discover`] entry has a generated name. It is not in
/// [`crate::Directory::tests`], so `if(TEST)`, the `TESTS` property, and
/// `set_tests_properties()` do not see it. [`crate::Directory::discoveries`] lists it.
#[derive(Clone, Debug)]
pub struct TestEntry {
    /// Test name (generated for [`TestKind::Discover`]).
    pub name: BString,
    /// Directory of the call.
    pub dir: DirId,
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// Command and arguments (can hold generator expressions).
    pub command: Vec<BString>,
    /// `CONFIGURATIONS`.
    pub configurations: Vec<BString>,
    /// `WORKING_DIRECTORY`.
    pub working_directory: Option<BString>,
    /// `COMMAND_EXPAND_LISTS`.
    pub command_expand_lists: bool,
    /// `add_test(<name> <command>...)` (old signature, no generator expressions).
    pub old_style: bool,
    /// Policy settings from the test declaration.
    pub policies: rcmake_diag::PolicyMap,
    /// Test properties.
    pub props: PropertyMap,
    /// `BUILD_DEPENDS`: targets or files that must build before the test runs.
    pub build_depends: Vec<BT<BString>>,
    /// Single test or discovery rule.
    pub kind: TestKind,
}

impl TestEntry {
    /// `true` for a `discover_tests()` entry.
    #[must_use]
    pub const fn is_discovery(&self) -> bool {
        matches!(self.kind, TestKind::Discover(_))
    }
}
