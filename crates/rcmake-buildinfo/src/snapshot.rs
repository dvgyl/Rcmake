//! Normalized build settings of one configured build directory.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rcmake_base::BString;

/// Tool that configured the build directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Producer {
    /// `cmake`, `rcmake`, or `unknown`.
    pub tool: &'static str,
    /// Version from the cache, when known.
    pub version: Option<BString>,
}

/// Where the data of a section came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataSource {
    /// File API reply.
    FileApi,
    /// `compile_commands.json`.
    CompileCommands,
    /// `CMakeCache.txt` and package files.
    Cache,
    /// Native `rcmake` find trace.
    FindTrace,
    /// Compiler files under `CMakeFiles/<version>/`.
    CompilerFiles,
}

impl DataSource {
    /// Stable name used in reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FileApi => "file-api",
            Self::CompileCommands => "compile-commands",
            Self::Cache => "cache",
            Self::FindTrace => "find-trace",
            Self::CompilerFiles => "compiler-files",
        }
    }
}

/// Completeness and freshness of one section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// Exact, current data.
    Complete(DataSource),
    /// Comparable data with a known limit, such as inferred target names or a stale reply.
    Partial(DataSource, BString),
    /// No data that can answer the section.
    Unavailable(BString),
}

impl Evidence {
    /// Return whether the section can be compared.
    #[must_use]
    pub const fn comparable(&self) -> bool {
        !matches!(self, Self::Unavailable(_))
    }

    /// Return whether the section is exact and current.
    #[must_use]
    pub const fn complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    /// Stable state name used in reports.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Complete(_) => "complete",
            Self::Partial(..) => "partial",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// Sections that a comparison can cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// Target list and kinds.
    Targets,
    /// Compile settings per source.
    Compile,
    /// Link fragments and dependencies.
    Link,
    /// Compilers.
    Toolchain,
    /// Packages.
    Packages,
    /// Cache entries.
    Cache,
}

impl Section {
    /// All sections in report order.
    pub const ALL: [Self; 6] = [
        Self::Targets,
        Self::Compile,
        Self::Link,
        Self::Toolchain,
        Self::Packages,
        Self::Cache,
    ];

    /// Stable name used on the command line and in reports.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Targets => "targets",
            Self::Compile => "compile",
            Self::Link => "link",
            Self::Toolchain => "toolchain",
            Self::Packages => "packages",
            Self::Cache => "cache",
        }
    }

    /// Parse a section name.
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|section| section.label().as_bytes() == name)
    }
}

/// How the target of a compile record is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// The File API names the target.
    Exact,
    /// The object path names the target.
    Inferred,
}

/// One include directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IncludeDir {
    /// Directory path.
    pub path: BString,
    /// The directory is a system include directory.
    pub system: bool,
}

/// Compile settings that apply to a set of sources.
///
/// Sources of one File API compile group share one value through an [`Arc`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct CompileSettings {
    /// Compile language, when known.
    pub language: Option<BString>,
    /// Language standard as given, for example `17`.
    pub standard: Option<BString>,
    /// Definitions in order.
    pub defines: Vec<BString>,
    /// Include directories in order.
    pub includes: Vec<IncludeDir>,
    /// Remaining command fragments in order, with duplicates.
    pub fragments: Vec<BString>,
    /// The command could not be split with a known dialect, or a limit, a response
    /// file, or the row structure made it uncertain. `fragments` then holds the raw
    /// command text or the ordered argument tokens.
    pub opaque: bool,
    /// Compiler token of a compile database row, when the row names a known compiler.
    pub compiler: Option<BString>,
    /// Launcher tokens before the compiler token, for example `ccache`.
    pub launcher: Vec<BString>,
}

/// Compile invocations of one source, in the order of the evidence.
///
/// A File API source has one invocation. A compile database can have more, for
/// example one row for each architecture or object file.
pub type SourceInvocations = Vec<Arc<CompileSettings>>;

/// Ordered link fragment with its role.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinkFragment {
    /// Role from the File API, for example `libraries` or `flags`.
    pub role: BString,
    /// Fragment text.
    pub fragment: BString,
}

/// Build settings of one target in one configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSnapshot {
    /// Target type, for example `STATIC_LIBRARY`. `None` from a compile database.
    pub kind: Option<BString>,
    /// How the target name is known.
    pub attribution: Attribution,
    /// Compile invocations of each source, keyed by source path.
    pub sources: BTreeMap<BString, SourceInvocations>,
    /// Link fragments in order, when the target links.
    pub link: Option<Vec<LinkFragment>>,
    /// Names of the targets this target depends on.
    pub dependencies: BTreeSet<BString>,
    /// Artifact paths.
    pub artifacts: Vec<BString>,
    /// Definition site `file:line`, for display.
    pub backtrace: Option<BString>,
}

/// One compiler.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolchainSnapshot {
    /// Language.
    pub language: BString,
    /// Compiler ID.
    pub compiler_id: Option<BString>,
    /// Compiler version.
    pub version: Option<BString>,
    /// Compiler path.
    pub path: Option<BString>,
    /// Compiler target triple.
    pub target: Option<BString>,
    /// Command-line dialect evidence: `MSVC` or `GNU`.
    pub frontend: Option<BString>,
}

/// Observed result of one package search from a current trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageObservation {
    /// 1-based position of the call among the calls for the package, when known.
    pub package_call: Option<u32>,
    /// Final outcome: `found`, `not_found`, `disabled`, or `error`.
    pub outcome: BString,
    /// Final version, when found.
    pub version: Option<BString>,
    /// Selected package file, when one was selected.
    pub selected: Option<BString>,
}

/// What a build directory tells about one package.
///
/// The fields come from separate evidence and never imply each other. A cached
/// `<Pkg>_DIR` does not prove that the package was found, and a version file does not
/// prove the version that was used.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageSnapshot {
    /// Cached `<Pkg>_DIR` value.
    pub cached_dir: Option<BString>,
    /// Version from the literal `set(PACKAGE_VERSION ...)` of the version file in that directory.
    pub version_hint: Option<BString>,
    /// Observations of each call from a current trace, in call order.
    pub observed: Vec<PackageObservation>,
}

/// One call of a validated native find trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceCall {
    /// Package name.
    pub name: BString,
    /// Observation of the call.
    pub observation: PackageObservation,
}

/// Native find trace evidence for [`crate::load`].
///
/// The caller decodes the trace with the typed parser and checks its freshness, so
/// this crate never trusts unvalidated trace rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TraceInput {
    /// The build tree has no valid trace.
    #[default]
    Missing,
    /// The trace is evidence from a past run, with the reason.
    Past(BString),
    /// The trace describes the current tree.
    Current {
        /// Recorded calls in entry order.
        calls: Vec<TraceCall>,
        /// The run finished and every started call has a record.
        complete: bool,
    },
}

/// One cache entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheRecord {
    /// Entry type.
    pub ty: BString,
    /// Entry value.
    pub value: BString,
    /// Help text.
    pub help: BString,
}

/// All build settings of one build directory.
#[derive(Clone, Debug)]
pub struct BuildSnapshot {
    /// Build directory as given.
    pub path: BString,
    /// Canonical build directory.
    pub build_root: BString,
    /// Tool and version.
    pub producer: Producer,
    /// Generator name.
    pub generator: Option<BString>,
    /// Project source directory.
    pub source_root: Option<BString>,
    /// Configurations. One empty name for a single-config build without a build type.
    pub configs: Vec<BString>,
    /// Targets by configuration and name.
    pub targets: BTreeMap<(BString, BString), TargetSnapshot>,
    /// Compilers.
    pub toolchains: Vec<ToolchainSnapshot>,
    /// Packages by name.
    pub packages: BTreeMap<BString, PackageSnapshot>,
    /// Find trace calls that the record limits left out. Their packages are unknown.
    pub packages_unknown: bool,
    /// Cache entries by name.
    pub cache: BTreeMap<BString, CacheRecord>,
    /// Evidence of each section.
    pub evidence: BTreeMap<Section, Evidence>,
    /// Notes about the data.
    pub warnings: Vec<BString>,
}

impl BuildSnapshot {
    /// Evidence of a section.
    #[must_use]
    pub fn evidence(&self, section: Section) -> Evidence {
        self.evidence
            .get(&section)
            .cloned()
            .unwrap_or_else(|| Evidence::Unavailable("No data was read.".into()))
    }
}
