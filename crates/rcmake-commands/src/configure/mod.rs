//! Configure-time toolchain commands: `enable_language`, `build_command`, `try_compile`,
//! and `try_run` (ADR-008).

#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_base::list::{self, Empty};
use rcmake_eval::{CmdError, CmdFlags, Cx, IncludeOptions, Registry};

mod build_command;
mod enable_language;
mod generator_env;
#[cfg(all(feature = "native", any(unix, windows)))]
mod lookahead;
#[cfg(all(feature = "native", any(unix, windows)))]
pub(crate) mod platform_queries;
#[cfg(all(feature = "native", any(unix, windows)))]
mod probe_pool;
mod session;
pub use session::ConfigureRuntime;
#[cfg(test)]
mod tests;
mod try_compile;

pub(crate) use enable_language::enable_languages;

pub(crate) fn register(registry: &mut Registry) {
    let project_only = CmdFlags { scriptable: false };
    registry.add("try_compile", try_compile::try_compile, project_only);
    registry.add("try_run", try_compile::try_run, project_only);
    registry.add(
        "enable_language",
        enable_language::enable_language,
        project_only,
    );
    registry.add("build_command", build_command::build_command, project_only);
}

/// `project()` include hooks (`CMAKE_PROJECT_INCLUDE_BEFORE`, `CMAKE_PROJECT_INCLUDE`,
/// and the `CMAKE_PROJECT_<NAME>_*` forms): include each file in the list that `var`
/// names.
///
/// A relative name without a `.cmake` suffix is a module name. The other names are
/// paths relative to the current source directory. Every file is tried; the last
/// failure becomes the command error.
///
/// # Errors
///
/// Returns the `project()` error text of the last file that could not be included.
pub(crate) fn project_include_hook(cx: &mut Cx<'_>, var: &[u8]) -> Result<(), CmdError> {
    let Some(files) = cx.var(var) else {
        return Ok(());
    };
    let mut failure = None;
    for file in list::split(&files, Empty::Drop) {
        let mut file: BString = file;
        let full = matches!(file.first(), Some(b'/' | b'\\'))
            || (file.len() >= 2 && file.get(1) == Some(&b':'));
        if !full && !file.ends_with(b".cmake") {
            let module = [file.as_slice(), b".cmake"].concat();
            match enable_language::modules_file(cx, &module) {
                Some(found) => file = found,
                None => {
                    failure = Some(CmdError::Status(
                        [b"could not find requested module:\n  ".as_slice(), &file]
                            .concat()
                            .into(),
                    ));
                    continue;
                }
            }
        }
        let path = rcmake_host::path::to_native(&file);
        match cx.include_file(&path, IncludeOptions::default()) {
            Ok(_) if !cx.fatal_occurred() => {}
            Ok(_) => failure = Some(CmdError::Reported),
            Err(error) => failure = Some(error),
        }
    }
    failure.map_or(Ok(()), Err)
}
