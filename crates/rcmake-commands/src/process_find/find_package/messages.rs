use std::path::Path;

use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, Cx, HermeticMode, hermetic::HermeticContext};
use rcmake_host::{FileKind, Follow, path};

use super::config::ConfigSearch;
use super::request::list_lines;
use super::{Request, cat};

pub(super) fn report_missing(
    cx: &mut Cx<'_>,
    request: &Request,
    mut message: BString,
) -> Result<(), CmdError> {
    if request.required {
        if let Some(setting) = &request.required_by {
            message = cat(&[
                message.trim_ascii_end(),
                b"\nThis package is considered required because the ",
                setting,
                b" variable has been enabled.\n",
            ]);
        }
        return Err(crate::command_error(cx, message));
    }
    if !request.quiet && !request.optional {
        cx.issue(MessageKind::Warning, Effect::None, None, message);
    }
    Ok(())
}

pub(super) fn warn_config_fallback(cx: &mut Cx<'_>, name: &[u8]) {
    let lower = name.to_ascii_lowercase();
    crate::author_warning(
        cx,
        cat(&[
            b"find_package called without either MODULE or CONFIG option and no Find",
            name,
            b".cmake module is in CMAKE_MODULE_PATH.  Add MODULE to exclusively request Module mode and fail if Find",
            name,
            b".cmake is missing.  Add CONFIG to exclusively request Config mode and search for a package configuration file provided by ",
            name,
            b" (",
            name,
            b"Config.cmake or ",
            &lower,
            b"-config.cmake).\n(Variable CMAKE_FIND_PACKAGE_WARN_NO_MODULE enabled this warning.)",
        ]),
    );
}

pub(super) fn warn_module_path(cx: &mut Cx<'_>, name: &[u8]) {
    crate::author_warning(
        cx,
        cat(&[
            b"Find",
            name,
            b".cmake must either be part of this project itself, in this case adjust CMAKE_MODULE_PATH so that it points to the correct location inside its source tree.\nOr it must be installed by a package which has already been found via find_package().  In this case make sure that package has indeed been found and adjust CMAKE_MODULE_PATH to contain the location where that package has installed Find",
            name,
            b".cmake.  This must be a location provided by that package.  This error in general means that the buildsystem of this project is relying on a Find-module without ensuring that it is actually available.\n",
        ]),
    );
}

pub(super) fn warn_module_case_mismatch(cx: &mut Cx<'_>, requested: &[u8], module: &Path) {
    if cx.services().speed.hermetic != HermeticMode::Off
        || cx.session_data::<HermeticContext>().is_some()
    {
        return;
    }
    let sysname = cx.host().system().sysname;
    if !matches!(sysname.as_slice(), b"Darwin" | b"Windows") {
        return;
    }
    let Some(parent) = module.parent() else {
        return;
    };
    let host = cx.host();
    let Ok(entries) = host.read_dir(parent) else {
        return;
    };
    if entries
        .iter()
        .any(|entry| entry.name.as_slice() == requested)
    {
        return;
    }
    let actual_name = entries
        .iter()
        .find(|entry| entry.name.as_slice().eq_ignore_ascii_case(requested))
        .map(|entry| entry.name.clone())
        .or_else(|| {
            let target = host.metadata(module, Follow::No).ok()?;
            if target.ino != 0
                && entries.len() <= 256
                && let Some(found) = entries.iter().find(|entry| {
                    host.metadata(&parent.join(path::to_native(&entry.name)), Follow::No)
                        .is_ok_and(|meta| meta.ino == target.ino && meta.dev == target.dev)
                })
            {
                return Some(found.name.clone());
            }
            if sysname.as_slice() == b"Windows" && target.kind != FileKind::Symlink {
                let canonical = host.canonicalize(module).ok()?;
                let name = path::from_native(Path::new(canonical.file_name()?));
                if name.as_slice() != requested {
                    return Some(name);
                }
            }
            None
        });
    let Some(actual_name) = actual_name else {
        return;
    };
    let disk_path = path::from_native_cmake(&parent.join(path::to_native(&actual_name)));
    crate::author_warning(
        cx,
        cat(&[
            b"The module name\n  ",
            requested,
            b"\ndoes not match the case of the module file name on disk\n  ",
            &disk_path,
            b"\nThis may fail on case-sensitive file systems.  Use the module name\n  ",
            &actual_name,
            b"\ninstead.",
        ]),
    );
}

impl ConfigSearch<'_> {
    pub(super) fn missing_message(&self, tried_module: bool) -> BString {
        let name = self.name;
        let preface = if tried_module && self.considered.is_empty() {
            cat(&[
                b"By not providing \"Find", name,
                b".cmake\" in CMAKE_MODULE_PATH this project has asked CMake to find a package configuration file provided by \"",
                name, b"\", but CMake did not find one.\n",
            ])
        } else {
            BString::default()
        };
        let requested = self
            .request
            .version
            .as_ref()
            .map_or_else(BString::default, |v| {
                let kind: &[u8] = if self.request.exact {
                    b"exactly matches requested version "
                } else if v.max.is_some() {
                    b"is compatible with requested version range "
                } else {
                    b"is compatible with requested version "
                };
                cat(&[b" that ", kind, b"\"", &v.full, b"\""])
            });
        if !self.considered.is_empty() {
            let requested: &[u8] = if requested.is_empty()
                && self
                    .considered
                    .iter()
                    .any(|(_, _, reason)| reason.is_some())
            {
                b" that is compatible with requested version \"\""
            } else {
                &requested
            };
            let files: Vec<_> = self
                .considered
                .iter()
                .map(|(file, version, reason)| {
                    let mut entry = cat(&[file, b", version: ", version]);
                    if let Some(reason) = reason {
                        entry.extend_from_slice(b"\n    ");
                        entry.extend_from_slice(reason);
                    }
                    entry
                })
                .collect();
            return cat(&[
                &preface,
                b"Could not find a configuration file for package \"",
                name,
                b"\"",
                requested,
                b".\nThe following configuration files were considered but not accepted:",
                &list_lines(&files),
            ]);
        }
        let mut configs = self.cps_names.clone();
        configs.extend_from_slice(&self.configs);
        let requested = self
            .request
            .version
            .as_ref()
            .map_or_else(BString::default, |v| {
                cat(&[b" (requested version ", &v.full, b")"])
            });
        cat(&[
            &preface,
            b"Could not find a package configuration file provided by \"",
            name,
            b"\"",
            &requested,
            b" with any of the following names:",
            &list_lines(&configs),
            b"Add the installation prefix of \"",
            name,
            b"\" to CMAKE_PREFIX_PATH or set \"",
            name,
            b"_DIR\" to a directory containing one of the above files.  If \"",
            name,
            b"\" provides a separate development package or SDK, be sure it has been installed.",
        ])
    }
}
