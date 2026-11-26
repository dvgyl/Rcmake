//! Three-layer resolution IR for Rcmake (ADR-009).
//!
//! - Layer 1 is the raw [`ProjectModel`](rcmake_model::ProjectModel) (`rcmake-model`).
//! - Layer 2 is [`ResolvedTarget`]: per-configuration, genex-evaluated target facts with
//!   backtraces. It is the single source of truth for the File API codemodel and
//!   `install(EXPORT)`.
//! - Layer 3 is [`BuildGraph`]: lowered, POSIX-`sh`-escaped command invocations,
//!   expanded rule templates, link fragments with semantic roles, and the
//!   [`InstallManifest`].
//!
//! Non-target data (cache, listfile inputs, toolchains, configure log) lives in
//! [`ConfigureMetadata`].

#![forbid(unsafe_code)]

mod cxx_modules;
mod export_links;
mod exports;
pub mod file_sets;
mod install_manifest;
mod model;
mod parallel;
mod pipeline;
mod resolve_link;
mod scoped_usage_host;
mod sources;
mod target;
pub mod target_depends;
mod unity;
mod xcframework;

pub mod graphviz;
pub mod ide;
pub mod lower;
pub mod progress;
pub mod usage;

pub use cxx_modules::query_ninja_cxx_dyndep;
pub use install_manifest::{
    InstallAction, InstallActionKind, InstallEntry, InstallManifest, InstallTargetFacts,
    NamelinkMode, TargetFileInstaller, TargetFileKind, TargetInstaller,
};
pub use model::*;
pub use pipeline::{resolve, resolve_with_facts, resolve_with_options, validate_codegen_name};
pub use target_depends::{TargetDepend, TargetDependGraph, UtilityItem};
pub use usage::includes::finalize_current_dir_interfaces;

/// Return `true` when CMake treats `path` as a full path (`cmSystemTools::FileIsFullPath`).
///
/// A leading `/` is a full path on all hosts (a network or root path on Windows).
/// A Windows host also accepts a drive letter (`C:`) and a leading `\\`.
/// A POSIX host also accepts a leading `~`. The host decides, not the model.
pub(crate) fn file_is_full_path(path: &[u8]) -> bool {
    if cfg!(windows) {
        path.len() >= 2
            && (path.get(1) == Some(&b':') || matches!(path.first(), Some(b'\\' | b'/')))
    } else {
        matches!(path.first(), Some(b'/' | b'~'))
    }
}

#[cfg(test)]
mod resolve_tests;
