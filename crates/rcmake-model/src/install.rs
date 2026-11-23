//! Install rules, export sets, and tests.

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
    },
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

/// A test (`add_test()`).
#[derive(Clone, Debug)]
pub struct TestEntry {
    /// Test name.
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
    /// Test properties.
    pub props: PropertyMap,
}
