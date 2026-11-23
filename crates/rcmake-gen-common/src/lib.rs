//! Generator-independent outputs shared by the Ninja and Makefile generators (ADR-009).
//!
//! Each module owns one output family, so Wave 3 and Wave 4 workers can work on them
//! in parallel.
//!
//! # Status
//!
//! Wave 0 freezes the module layout. Every writer returns
//! [`GenError::Unimplemented`] until its wave.

#![forbid(unsafe_code)]

use rcmake_base::BString;

pub mod compile_commands;
pub mod configure_log;
pub mod ctest_file;
pub mod export;
pub mod fileapi;
pub mod install_script;

/// Error of a generator or writer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GenError {
    /// Writing an output file failed.
    #[error("cannot write {path}: {message}")]
    Write {
        /// Output path.
        path: BString,
        /// Host error text.
        message: BString,
    },
    /// The output is not implemented yet.
    #[error("{0} is not implemented yet")]
    Unimplemented(&'static str),
}

/// Write `content` to `path` through `host`, replacing the file only when the content
/// changed (keeps build tool timestamps stable).
pub fn write_if_changed(
    host: &dyn rcmake_host::Host,
    path: &std::path::Path,
    content: &[u8],
) -> Result<(), GenError> {
    let limit = u64::try_from(content.len())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    if let Ok(old) = host.read(path, rcmake_host::ReadRange::All { limit })
        && old == content
    {
        return Ok(());
    }
    host.write(path, content, rcmake_host::WriteMode::Truncate)
        .map_err(|err| GenError::Write {
            path: rcmake_host::path::from_native(path),
            message: err.to_string().into(),
        })
}
