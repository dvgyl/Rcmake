//! Typed property scopes for `set_property()` / `get_property()` (ADR-007).

use rcmake_base::BString;
use rcmake_diag::Backtrace;

use crate::{DirId, FileSetId, TargetId};

/// The object that owns a property.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PropScope {
    /// `GLOBAL`.
    Global,
    /// `DIRECTORY`.
    Directory(DirId),
    /// `TARGET`.
    Target(TargetId),
    /// `SOURCE`: properties of `path` as seen from directory `dir`.
    Source {
        /// Directory that owns the source file properties.
        dir: DirId,
        /// Absolute source path, CMake style.
        path: BString,
    },
    /// `TEST`: test `name` in directory `dir`.
    Test {
        /// Directory of the test.
        dir: DirId,
        /// Test name.
        name: BString,
    },
    /// `CACHE`: properties of a cache entry.
    Cache(BString),
    /// `FILE_SET`: properties of one file set of a target.
    FileSet(FileSetId),
    /// `INSTALL`: properties of one installed file.
    Install {
        /// Name of the installed file.
        file: BString,
    },
}

/// `set_property(APPEND)` or `set_property(APPEND_STRING)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendMode {
    /// `APPEND`: add a list item.
    List,
    /// `APPEND_STRING`: add raw text.
    String,
}

/// Call site of a property write. Usage requirements record the backtrace, and link
/// items record the directory for target name lookup (`CMP0079`).
#[derive(Clone, Copy, Debug)]
pub struct PropOrigin<'a> {
    /// Directory of the call.
    pub dir: DirId,
    /// Backtrace of the call.
    pub backtrace: &'a Backtrace,
}
