//! Load, validate, and resolve CMake presets through schema version 13.

#![forbid(unsafe_code)]

mod condition;
mod expand;
mod graph;
mod resolve;
mod schema;
mod settings;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::Host;
use rcmake_json::Value;
use thiserror::Error;

pub use settings::*;

/// Category of a CMake preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PresetKind {
    /// `configurePresets`.
    Configure,
    /// `buildPresets`.
    Build,
    /// `testPresets`.
    Test,
    /// `packagePresets`.
    Package,
    /// `workflowPresets`.
    Workflow,
}

impl PresetKind {
    const ARRAYS: [(Self, &'static str); 5] = [
        (Self::Configure, "configurePresets"),
        (Self::Build, "buildPresets"),
        (Self::Test, "testPresets"),
        (Self::Package, "packagePresets"),
        (Self::Workflow, "workflowPresets"),
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Configure => "configure",
            Self::Build => "build",
            Self::Test => "test",
            Self::Package => "package",
            Self::Workflow => "workflow",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Configure => "Configure",
            Self::Build => "Build",
            Self::Test => "Test",
            Self::Package => "Package",
            Self::Workflow => "Workflow",
        }
    }
}

/// Snapshot of the parent process environment.
#[derive(Clone, Debug, Default)]
pub struct EnvSnapshot {
    /// Environment variables available to preset macros.
    pub vars: BTreeMap<BString, BString>,
}

impl EnvSnapshot {
    /// Capture the current environment from the host.
    #[must_use]
    pub fn from_host(host: &dyn Host) -> Self {
        Self {
            vars: host.env_all().into_iter().collect(),
        }
    }
}

/// Minimum CMake version required by a presets file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CmakeVersion {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version.
    pub patch: u32,
}

/// Metadata for a loaded presets file.
#[derive(Clone, Debug, PartialEq)]
pub struct PresetFile {
    /// Schema version of this file.
    pub version: u32,
    /// Minimum CMake version specified in this file.
    pub cmake_minimum_required: Option<CmakeVersion>,
    /// Schema URI for editors.
    pub schema_uri: Option<BString>,
    /// Vendor data from this file.
    pub vendor: BTreeMap<BString, Value>,
}

/// Loaded preset graph.
#[derive(Clone, Debug, Default)]
pub struct Presets {
    /// Highest schema version seen in the root preset files.
    pub version: u32,
    /// Metadata for root and included files, indexed by absolute path.
    pub files: BTreeMap<PathBuf, PresetFile>,
    reachable: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    source_dir: PathBuf,
    host_system: BString,
    records: BTreeMap<PresetKind, indexmap::IndexMap<BString, graph::Record>>,
}

/// Preset information for command-line listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetSummary {
    /// Preset category.
    pub kind: PresetKind,
    /// Preset name.
    pub name: BString,
    /// Optional name for display.
    pub display_name: Option<BString>,
    /// Optional description.
    pub description: Option<BString>,
}

/// Settings of a resolved preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresetSettings {
    /// Configure settings.
    Configure(Box<ConfigureSettings>),
    /// Build settings.
    Build(BuildSettings),
    /// Test settings.
    Test(Box<TestSettings>),
    /// Package settings.
    Package(PackageSettings),
    /// Workflow settings.
    Workflow(WorkflowSettings),
}

/// Preset with inherited fields and expanded macros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPreset {
    /// Preset category.
    pub kind: PresetKind,
    /// Preset name.
    pub name: BString,
    /// Optional name for display.
    pub display_name: Option<BString>,
    /// Optional description.
    pub description: Option<BString>,
    /// Environment overrides. A value of `None` unsets the variable.
    pub environment: BTreeMap<BString, Option<BString>>,
    /// Configure preset associated with a build, test, or package preset.
    pub configure_preset: Option<BString>,
    /// Whether this build, test, or package preset inherits configure environment variables.
    pub inherit_configure_environment: Option<bool>,
    /// Generator from the configure preset.
    pub generator: Option<BString>,
    /// Build directory from the configure preset.
    pub binary_dir: Option<BString>,
    /// Settings for this category.
    pub settings: PresetSettings,
}

/// Error from the preset loader or resolver.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct PresetsError {
    /// Preset diagnostic.
    pub message: String,
}

impl PresetsError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Load the project and user preset files and their includes through the host.
///
/// A relative source directory is interpreted relative to the host's initial directory.
/// If neither preset file exists, this function reports a missing preset file.
pub fn load(source_dir: &Path, host: &dyn Host) -> Result<Presets, PresetsError> {
    graph::load(source_dir, host, None)
}

/// Load presets from parsed JSON documents indexed by absolute or source-relative paths.
///
/// Use the same filenames as the project, user, and included files. This function uses
/// the host for path and environment information. It obtains all JSON from `values`.
pub fn load_from_values(
    source_dir: &Path,
    host: &dyn Host,
    values: &BTreeMap<PathBuf, Value>,
) -> Result<Presets, PresetsError> {
    graph::load(source_dir, host, Some(values))
}

/// Resolve a named preset by kind.
pub fn resolve(
    presets: &Presets,
    kind: PresetKind,
    name: &[u8],
    env: &EnvSnapshot,
) -> Result<ResolvedPreset, PresetsError> {
    presets.resolve(kind, name, env)
}

#[cfg(test)]
mod tests;
