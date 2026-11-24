use rcmake_base::BString;

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
}
