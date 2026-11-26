//! Verification script for `file(GLOB CONFIGURE_DEPENDS)`.

use std::path::Path;

use rcmake_base::BString;
use rcmake_host::{Host, WriteMode, path::from_native};
use rcmake_model::ProjectModel;

use crate::{GenError, script_argument, write_output};

/// Path of the verification script, relative to the build directory.
pub const SCRIPT: &str = "CMakeFiles/VerifyGlobs.cmake";
/// Stamp touched by the verification script when a result changes.
pub const STAMP: &str = "CMakeFiles/cmake.verify_globs";

/// Write the glob verification script and create its initial stamp.
pub fn write_verification(
    model: Option<&ProjectModel>,
    build: &Path,
    host: &dyn Host,
) -> Result<bool, GenError> {
    let Some(model) = model.filter(|model| !model.glob_checks.is_empty()) else {
        return Ok(false);
    };
    let mut out = BString::from("cmake_minimum_required(VERSION 4.5)\n");
    for (key, check) in &model.glob_checks {
        out.extend_from_slice(if key.recursive {
            b"file(GLOB_RECURSE current"
        } else {
            b"file(GLOB current"
        });
        out.extend_from_slice(if key.directories {
            b" LIST_DIRECTORIES true"
        } else {
            b" LIST_DIRECTORIES false"
        });
        if key.recursive && key.follow {
            out.extend_from_slice(b" FOLLOW_SYMLINKS");
        }
        if let Some(relative) = &key.relative {
            out.extend_from_slice(b" RELATIVE ");
            script_argument(&mut out, relative);
        }
        out.push(b' ');
        script_argument(&mut out, &key.expression);
        out.extend_from_slice(b")\nif(NOT current STREQUAL ");
        script_argument(&mut out, &check.files);
        out.extend_from_slice(b")\n  message(\"-- GLOB mismatch!\")\n  file(TOUCH ");
        script_argument(&mut out, &from_native(&build.join(STAMP)));
        out.extend_from_slice(
            b")\n  if(DEFINED _RCMAKE_GLOB_MISMATCH_MARKER)\n    file(WRITE \"${_RCMAKE_GLOB_MISMATCH_MARKER}\" \"\")\n  endif()\n  return()\nendif()\n",
        );
    }
    let script = build.join(SCRIPT);
    write_output(host, &script, &out)?;
    let stamp = build.join(STAMP);
    if let Err(error) = host.write(&stamp, b"", WriteMode::CreateNew)
        && error.kind != std::io::ErrorKind::AlreadyExists
    {
        return Err(GenError::Write {
            path: from_native(&stamp),
            message: error.to_string().into(),
        });
    }
    Ok(true)
}
