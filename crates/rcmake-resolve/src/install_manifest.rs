use std::{collections::BTreeMap, ops::Range};

use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::{DirId, TargetId};

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
    /// Strip debug information from an installed target binary.
    Strip {
        /// Configured strip program.
        program: BString,
        /// Options passed to the strip program.
        flags: Vec<BString>,
        /// Binary path relative to the action destination.
        name: BString,
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
    /// Target installers in rule and artifact group order.
    pub target_installers: Vec<TargetInstaller>,
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

/// Whether a library installer copies the linker symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamelinkMode {
    /// The installer has no separate linker symlink.
    None,
    /// Copy only the linker symlink.
    Only,
    /// Copy the library and its SONAME link.
    Skip,
}

/// The files that an install target rule selects for one artifact group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetInstaller {
    /// Index of the rule in the project model.
    pub rule: usize,
    /// Installed target.
    pub target: TargetId,
    /// Build configuration.
    pub config: ConfigId,
    /// Install destination.
    pub destination: BString,
    /// Install component.
    pub component: BString,
    /// Whether the files can be missing.
    pub optional: bool,
    /// Whether a full install skips this group.
    pub exclude_from_all: bool,
    /// Whether this group installs an import library.
    pub import_library: bool,
    /// Library linker symlink selection.
    pub namelink: NamelinkMode,
    /// Ordered source paths and paths under the install destination.
    ///
    /// Empty if the installer selects no file, for example for `NAMELINK_ONLY`
    /// on a library without a version.
    pub paths: Vec<(BString, BString)>,
}
