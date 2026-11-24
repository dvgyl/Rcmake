use std::{collections::BTreeMap, ops::Range};

use rcmake_base::BString;
use rcmake_model::DirId;

/// One install action of `cmake_install.cmake`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallAction {
    /// Install component.
    pub component: BString,
    /// Run in every component install.
    pub all_components: bool,
    /// Skip this action during a full install.
    pub exclude_from_all: bool,
    /// Destination (relative to `CMAKE_INSTALL_PREFIX` unless absolute).
    pub destination: BString,
    /// Configurations the action applies to (empty means all).
    pub configurations: Vec<BString>,
    /// What to install.
    pub kind: InstallActionKind,
    /// Skip missing files (`OPTIONAL`).
    pub optional: bool,
    /// Install message setting (`ALWAYS`, `LAZY`, `NEVER`, or empty for the default).
    pub message: BString,
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
        /// Index into [`crate::GenerationResult::exports`].
        plan: usize,
    },
}

/// All install actions in rule order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstallManifest {
    /// Actions.
    pub actions: Vec<InstallAction>,
    /// Local actions and child scripts in each directory's install order.
    pub directories: BTreeMap<DirId, Vec<InstallEntry>>,
}

/// An entry in a directory's install script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallEntry {
    /// A range of local actions in the flat manifest.
    Actions(Range<usize>),
    /// A child script included during serial installation.
    Child(DirId),
}
