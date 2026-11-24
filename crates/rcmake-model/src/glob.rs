//! Globs that ask the build tool to check for directory changes.

use rcmake_base::BString;
use rcmake_diag::Backtrace;

/// Inputs that identify one glob.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GlobKey {
    /// Absolute search expression.
    pub expression: BString,
    /// Search subdirectories recursively.
    pub recursive: bool,
    /// Include directories in the result.
    pub directories: bool,
    /// Follow directory symlinks.
    pub follow: bool,
    /// Base of relative results, if specified.
    pub relative: Option<BString>,
}

/// A saved result and the commands that recorded it.
#[derive(Clone, Debug)]
pub struct GlobCheck {
    /// Sorted result encoded as a CMake list.
    pub files: BString,
    /// Successful calls that produced this result.
    pub calls: Vec<GlobCall>,
}

/// The variable and location for one glob call.
#[derive(Clone, Debug)]
pub struct GlobCall {
    /// Variable that received the result.
    pub variable: BString,
    /// Call site of the glob.
    pub backtrace: Backtrace,
}
