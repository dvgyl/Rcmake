//! CMake Presets **schema v13** (`CMakePresets.json` / `CMakeUserPresets.json`) loader,
//! include-graph resolver, inheritance merger, condition evaluator, and macro expander
//! (including `${configurePresetName}` in schema v13).

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::Path;

use rcmake_base::BString;
use rcmake_host::Host;
use thiserror::Error;

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

/// Snapshot of environment variables used during preset condition and `$env{}`/`$penv{}` evaluation.
#[derive(Clone, Debug, Default)]
pub struct EnvSnapshot {
    /// Parent process environment variables.
    pub vars: BTreeMap<BString, BString>,
}

/// Loaded preset graph from `CMakePresets.json`, `CMakeUserPresets.json`, and `include`d files.
#[derive(Clone, Debug, Default)]
pub struct Presets {
    /// Highest schema version seen in root preset files.
    pub version: u32,
}

/// Resolved, inheritance-merged, macro-expanded preset ready for CLI execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPreset {
    /// Preset category.
    pub kind: PresetKind,
    /// Preset name.
    pub name: BString,
    /// Optional human-readable display name.
    pub display_name: Option<BString>,
    /// Merged environment overlay (`None` means explicitly unset via JSON `null`).
    pub environment: BTreeMap<BString, Option<BString>>,
}

/// Error returned while loading or resolving CMake presets.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct PresetsError {
    /// Exact CMake-style preset error wording.
    pub message: String,
}

/// Load `CMakePresets.json` and `CMakeUserPresets.json` from `source_dir` via `host`.
pub fn load(_source_dir: &Path, _host: &dyn Host) -> Result<Presets, PresetsError> {
    Err(PresetsError {
        message: "presets loader not yet implemented (scheduled for M1-W1)".to_owned(),
    })
}

/// Resolve a named preset by kind, applying inheritance, conditions, and macro expansion.
pub fn resolve(
    _presets: &Presets,
    _kind: PresetKind,
    _name: &[u8],
    _env: &EnvSnapshot,
) -> Result<ResolvedPreset, PresetsError> {
    Err(PresetsError {
        message: "presets resolver not yet implemented (scheduled for M1-W1)".to_owned(),
    })
}
