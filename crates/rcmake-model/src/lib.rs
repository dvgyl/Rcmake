//! CMake cache store (`CMakeCache.txt`), property maps, global state, and the arena-based
//! [`ProjectModel`] (ADR-007).
//!
//! The project model holds the raw configure-time state: directories, targets, custom
//! commands, tests, file sets, install rules, and export sets. Each arena hands out a
//! typed id ([`DirId`], [`TargetId`], [`CommandId`], [`TestId`], [`FileSetId`]). Ids are
//! only created by the arenas, and arenas never shrink.

#![forbid(unsafe_code)]

use indexmap::IndexMap;
use rcmake_base::BString;
use rcmake_diag::Backtrace;

mod command;
mod directory;
mod export_dependency;
mod generator;
mod glob;
mod ids;
mod install;
mod property_print;
pub use property_print::PropertyPrint;
mod project;
mod props;
mod provenance;
pub use provenance::{LanguageRegistry, PropEvent, PropLedger, PropOp, PropValuePreview};
mod target;

pub use command::{CustomCommand, CustomCommandKind, TargetEvent};
pub use directory::Directory;
pub use export_dependency::{ExportDependencyMode, ExportDependencySetup};
pub use generator::{
    GeneratorKind, GeneratorSpec, GeneratorSpecError, ParsedToolset, SpecField, ToolsetSupport,
    VsPlatform, VsToolset, VsVersion, XcodeBuildSystem, XcodeVersion, parse_vs_platform,
    parse_vs_toolset, parse_xcode_toolset,
};
pub use glob::{GlobCall, GlobCheck, GlobKey};
pub use ids::{BT, CommandId, DirId, FileSetId, TargetId, TestId};
pub use install::{
    BuildExport, DiscoverySpec, ExportKind, ExportSet, InstallKind, InstallOptions, InstallRule,
    PackageInfoSpec, ProjectMetadata, RuntimeDependencyFilters, RuntimeDependencyInstall, SbomSpec,
    TestEntry, TestKind,
};
pub use project::{ModelError, ProjectModel, replay};
pub use props::{AppendMode, PropOrigin, PropScope};
pub use target::{
    FileSet, FileSetKind, ImportedVisibility, LinkItem, Target, TargetBinding, TargetKind,
    TargetOrigin, TargetPolicySnapshot, UsageField, UsageProps, UsageScope,
};

/// Cache entry type matching `CMakeCache.txt` (`<NAME>:<TYPE>=<VALUE>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheType {
    /// Boolean option (`BOOL`).
    Bool,
    /// Directory path (`PATH`).
    Path,
    /// File path (`FILEPATH`).
    Filepath,
    /// Arbitrary string (`STRING`).
    String,
    /// Internal cache variable (`INTERNAL`).
    Internal,
    /// Read-only static entry (`STATIC`).
    Static,
    /// Uninitialized CLI `-DVAR=val` without explicit type (`UNINITIALIZED`).
    Uninitialized,
}

impl CacheType {
    /// Parse a CMake cache type token (case-insensitive).
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        if name.eq_ignore_ascii_case(b"BOOL") {
            Some(Self::Bool)
        } else if name.eq_ignore_ascii_case(b"PATH") {
            Some(Self::Path)
        } else if name.eq_ignore_ascii_case(b"FILEPATH") {
            Some(Self::Filepath)
        } else if name.eq_ignore_ascii_case(b"STRING") {
            Some(Self::String)
        } else if name.eq_ignore_ascii_case(b"INTERNAL") {
            Some(Self::Internal)
        } else if name.eq_ignore_ascii_case(b"STATIC") {
            Some(Self::Static)
        } else if name.eq_ignore_ascii_case(b"UNINITIALIZED") {
            Some(Self::Uninitialized)
        } else {
            None
        }
    }

    /// Canonical uppercase spelling written into `CMakeCache.txt`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "BOOL",
            Self::Path => "PATH",
            Self::Filepath => "FILEPATH",
            Self::String => "STRING",
            Self::Internal => "INTERNAL",
            Self::Static => "STATIC",
            Self::Uninitialized => "UNINITIALIZED",
        }
    }
}

/// Deterministic ordered map of CMake property names to byte values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyMap {
    entries: IndexMap<BString, BString>,
}

impl PropertyMap {
    /// Read a property value.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<&BString> {
        self.entries.get(name)
    }

    /// Set a property to `value`.
    pub fn set(&mut self, name: BString, value: BString) {
        self.entries.insert(name, value);
    }

    /// Remove a property.
    pub fn unset(&mut self, name: &[u8]) {
        self.entries.shift_remove(name);
    }

    /// Append a semicolon-separated list item (`set_property(... APPEND)`).
    pub fn append(&mut self, name: BString, value: &[u8]) {
        if let Some(existing) = self.entries.get_mut(&name) {
            if !existing.is_empty() && !value.is_empty() {
                existing.push(b';');
            }
            existing.extend_from_slice(value);
        } else {
            self.entries.insert(name, BString::from(value));
        }
    }

    /// Append raw text without a semicolon (`set_property(... APPEND_STRING)`).
    pub fn append_string(&mut self, name: BString, value: &[u8]) {
        if let Some(existing) = self.entries.get_mut(&name) {
            existing.extend_from_slice(value);
        } else {
            self.entries.insert(name, BString::from(value));
        }
    }

    /// Return all property names in insertion order.
    #[must_use]
    pub fn names(&self) -> Vec<BString> {
        self.entries.keys().cloned().collect()
    }
}

/// Single entry inside [`Cache`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    /// Entry byte value.
    pub value: BString,
    /// Cache entry type.
    pub ty: CacheType,
    /// Help docstring (`HELPSTRING`).
    pub help: BString,
    /// Whether marked `ADVANCED`.
    pub advanced: bool,
    /// Additional cache entry properties (`STRINGS`, `MODIFIED`, etc.).
    pub props: PropertyMap,
}

/// Ordered store of CMake cache variables (`CMakeCache.txt`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cache {
    entries: IndexMap<BString, CacheEntry>,
}

impl Cache {
    /// Get a cache entry by name.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<&CacheEntry> {
        self.entries.get(name)
    }

    /// Get a mutable reference to a cache entry by name.
    pub fn get_mut(&mut self, name: &[u8]) -> Option<&mut CacheEntry> {
        self.entries.get_mut(name)
    }

    /// Insert or replace a cache entry.
    pub fn insert(&mut self, name: BString, entry: CacheEntry) {
        self.entries.insert(name, entry);
    }

    /// Remove a cache entry (`unset(VAR CACHE)`).
    pub fn remove(&mut self, name: &[u8]) -> Option<CacheEntry> {
        self.entries.shift_remove(name)
    }

    /// Return `true` if a cache entry with `name` exists.
    #[must_use]
    pub fn contains(&self, name: &[u8]) -> bool {
        self.entries.contains_key(name)
    }

    /// Iterate over all cache entries in deterministic insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&BString, &CacheEntry)> {
        self.entries.iter()
    }
}

/// Property definition registered via `define_property()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinedProperty {
    /// Scope kind (`GLOBAL`, `DIRECTORY`, `TARGET`, `SOURCE`, `TEST`, `VARIABLE`, `CACHED_VARIABLE`).
    pub scope: BString,
    /// Property name.
    pub name: BString,
    /// Whether the property inherits from parent scopes (`INHERITED`).
    pub inherited: bool,
    /// Brief documentation string.
    pub brief_docs: BString,
    /// Full documentation string.
    pub full_docs: BString,
    /// Variable that initializes the property on target creation
    /// (`INITIALIZE_FROM_VARIABLE`, `TARGET` scope only).
    pub initialize_from_variable: Option<BString>,
}

/// One `CUSTOM_CONTENT <name> <type> <content>` entry of `cmake_instrumentation()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstrumentationContent {
    /// Content name.
    pub name: BString,
    /// Content type keyword (`STRING`, `BOOL`, `LIST`, or `JSON`).
    pub ty: BString,
    /// Raw content.
    pub content: BString,
}

/// One `cmake_instrumentation()` call, after argument parsing. `rcmake-instrument`
/// writes it as a query file under `<build>/.cmake/instrumentation/v1/query/generated`.
#[derive(Clone, Debug, Default)]
pub struct InstrumentationRequest {
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// `API_VERSION`.
    pub api_version: BString,
    /// `DATA_VERSION` (`major` or `major.minor`).
    pub data_version: BString,
    /// `HOOKS` (for example `postGenerate`, `postCMakeBuild`).
    pub hooks: Vec<BString>,
    /// `OPTIONS` (for example `staticSystemInformation`, `trace`).
    pub options: Vec<BString>,
    /// `CALLBACK` command lines, one for each keyword.
    pub callbacks: Vec<BString>,
    /// `CUSTOM_CONTENT` entries.
    pub custom_content: Vec<InstrumentationContent>,
}

/// Global properties and defined-property metadata shared across the build/script session.
#[derive(Clone, Debug, Default)]
pub struct GlobalState {
    /// Global properties (`set_property(GLOBAL ...)`).
    pub props: PropertyMap,
    /// Properties registered via `define_property()`.
    pub defined_props: Vec<DefinedProperty>,
    /// `cmake_instrumentation()` calls in call order.
    pub instrumentation: Vec<InstrumentationRequest>,
}

impl GlobalState {
    /// The `define_property()` entry for `name` in `scope`, if any.
    #[must_use]
    pub fn defined_property(&self, scope: &[u8], name: &[u8]) -> Option<&DefinedProperty> {
        self.defined_props
            .iter()
            .find(|entry| entry.scope == scope && entry.name == name)
    }
}
