//! Native `cpack` for Rcmake (ADR-010).
//!
//! `cpack` reads `CPackConfig.cmake` (or `--config <file>`, or a package preset), stages
//! the install tree of each project into a temporary directory, and runs one
//! [`PackageGenerator`] for each name in `CPACK_GENERATOR`.
//!
//! Modules:
//!
//! - [`cli`]: command-line parsing into [`CPackOptions`].
//! - [`config`]: evaluation of the configuration file with the full command registry.
//! - [`stage`]: install of the projects into the staging directory.
//! - [`archive_gen`]: `TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, and `ZIP`.
//! - [`source`]: source packages (`CPackSourceConfig.cmake`).
//! - [`generators`] (`src/gen/`): the generator registry and the macOS `DragNDrop` and `Bundle` generators.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::{Host, Stream};

pub mod archive_gen;
pub mod cli;
pub mod config;
// `gen` is a reserved keyword in edition 2024.
#[path = "gen/mod.rs"]
pub mod generators;
pub mod source;
pub mod stage;

#[cfg(test)]
mod tests;

/// Error of a `cpack` operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CpackError {
    /// The operation is not implemented yet.
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    /// A CPack error with the CPack message text.
    #[error("{0}")]
    Message(BString),
    /// A host operation failed.
    #[error(transparent)]
    Host(#[from] rcmake_host::HostError),
}

/// Parsed `cpack` command line.
///
/// `cpack` applies `-C` before it runs the configuration file, then `-G`, `-P`, `-R`,
/// `-B`, and `--vendor`, and `-D` last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CPackOptions {
    /// `-G <generators>`: overrides `CPACK_GENERATOR`.
    pub generators: Option<BString>,
    /// `-C <configs>`: `CPACK_BUILD_CONFIG`.
    pub build_config: Option<BString>,
    /// `--config <file>` (default `CPackConfig.cmake`).
    pub config_file: Option<BString>,
    /// `-D <var>=<value>`, in command-line order.
    pub defines: Vec<(BString, BString)>,
    /// `-P <name>`: `CPACK_PACKAGE_NAME`.
    pub package_name: Option<BString>,
    /// `-R <version>`: `CPACK_PACKAGE_VERSION`.
    pub package_version: Option<BString>,
    /// `-B <dir>`: `CPACK_PACKAGE_DIRECTORY`.
    pub package_directory: Option<BString>,
    /// `--vendor <name>`: `CPACK_PACKAGE_VENDOR`.
    pub vendor: Option<BString>,
    /// `--preset <name>`.
    pub preset: Option<BString>,
    /// `--list-presets`.
    pub list_presets: bool,
    /// `-V` / `--verbose`.
    pub verbose: bool,
    /// `--debug`.
    pub debug: bool,
    /// `--trace`.
    pub trace: bool,
    /// `--trace-expand`.
    pub trace_expand: bool,
}

/// How a generator groups install components (`CPACK_COMPONENTS_GROUPING`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComponentGrouping {
    /// `ONE_PER_GROUP`: one package for each component group.
    #[default]
    OnePerGroup,
    /// `ALL_COMPONENTS_IN_ONE`: one package with all components.
    AllComponentsInOne,
    /// `IGNORE`: one package for each component.
    Ignore,
}

/// Everything a [`PackageGenerator`] needs for one package run.
pub struct PackageContext<'a> {
    /// Host for file, process, and clock operations.
    pub host: &'a dyn Host,
    /// `CPACK_*` variables after configuration and overrides.
    pub vars: &'a BTreeMap<BString, BString>,
    /// Staging directory that holds the installed files.
    pub staging_dir: &'a Path,
    /// Directory for the package files (`CPACK_PACKAGE_DIRECTORY`).
    pub output_dir: &'a Path,
    /// Package file name without extension (`CPACK_PACKAGE_FILE_NAME`).
    pub file_name: &'a [u8],
    /// Component grouping.
    pub grouping: ComponentGrouping,
}

/// One CPack generator (`CPACK_GENERATOR` entry).
pub trait PackageGenerator {
    /// Generator name (for example `TGZ` or `DragNDrop`).
    fn name(&self) -> &'static str;

    /// Package file extension, with the leading dot (for example `.tar.gz`).
    fn extension(&self) -> &'static str;

    /// Create the package files from the staging directory. Returns the paths of the
    /// package files.
    ///
    /// # Errors
    ///
    /// Returns a [`CpackError`] when the package can not be created.
    fn package(&self, cx: &PackageContext<'_>) -> Result<Vec<PathBuf>, CpackError>;
}

/// Run `cpack` with `argv` (including `argv[0]`) and return the process exit code.
#[must_use]
pub fn main(argv: &[BString], host: Arc<dyn Host>) -> i32 {
    let args = argv.get(1..).unwrap_or_default();
    let result = cli::parse(args).and_then(|options| config::load(&host, &options));
    match result {
        Ok(_) => 0,
        Err(err) => {
            host.write_out(Stream::Stderr, format!("CPack Error: {err}\n").as_bytes());
            1
        }
    }
}
