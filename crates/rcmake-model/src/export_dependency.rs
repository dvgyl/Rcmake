//! Package dependencies requested by an export set.

use rcmake_base::BString;

/// Select when an export includes a package dependency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportDependencyMode {
    /// Include the dependency when the export refers to one of its targets.
    #[default]
    Auto,
    /// Always include the dependency.
    On,
    /// Omit the dependency.
    Off,
}

/// Settings for one package in `export(SETUP ... PACKAGE_DEPENDENCY)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportDependencySetup {
    /// When to include the package.
    pub mode: ExportDependencyMode,
    /// Extra arguments for `find_package()`.
    pub args: Vec<BString>,
}
