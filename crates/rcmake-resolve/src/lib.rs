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
mod pipeline;
mod resolve_link;
mod sources;
mod target;
mod unity;

pub mod ide;
pub mod lower;
pub mod progress;
pub mod usage;

pub use install_manifest::{InstallAction, InstallActionKind, InstallManifest};
pub use model::*;
pub use pipeline::{resolve, resolve_with_options};

#[cfg(test)]
mod resolve_tests;
