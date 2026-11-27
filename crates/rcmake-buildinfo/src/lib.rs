//! Bounded readers and semantic comparators for CMake build-tree artifacts.
//!
//! This crate reads File API reply documents, `CMakeCache.txt`, `compile_commands.json`,
//! and native `rcmake` metadata through [`rcmake_host::Host`] with strict size, depth,
//! and containment checks. It never runs `cmake`. Each section of a snapshot carries
//! its own evidence state, so a comparison can tell a real difference from missing data.

#![forbid(unsafe_code)]

mod cache_file;
mod compdb;
mod diff;
mod fileapi;
mod packages;
pub mod reader;
mod render;
mod snapshot;
mod source;

pub use cache_file::parse as parse_cache;
pub use diff::{
    CacheChange, CacheSet, CompileChange, ConfigDiff, DiffOptions, ListChange, PackageChange, Pair,
    TargetChange, TargetEntry, ToolchainChange, compare, normalize,
};
pub use reader::{ArtifactReader, ReadError, ReadLimits};
pub use render::{DIFF_CONFIG_SCHEMA, render_json, render_text};
pub use snapshot::{
    Attribution, BuildSnapshot, CacheRecord, CompileSettings, DataSource, Evidence, IncludeDir,
    LinkFragment, PackageObservation, PackageSnapshot, Producer, Section, TargetSnapshot,
    ToolchainSnapshot,
};
pub use source::{LoadError, load};

/// File API query files that `rcmake diff-config --prepare` creates.
pub const QUERY_FILES: [&str; 4] = ["codemodel-v2", "cache-v2", "toolchains-v1", "cmakeFiles-v1"];
