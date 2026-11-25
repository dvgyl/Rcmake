use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, Cx};

use super::config::ConfigSearch;
use super::request::list_lines;
use super::{Request, diagnostic, text};

pub(super) fn report_missing(
    cx: &mut Cx<'_>,
    request: &Request,
    mut message: BString,
) -> Result<(), CmdError> {
    if request.required {
        if let Some(setting) = &request.required_by {
            message = text(&[
                message.trim_ascii_end(),
                b"\nThis package is considered required because the ",
                setting,
                b" variable has been enabled.\n",
            ]);
        }
        return Err(diagnostic(cx, message));
    }
    if !request.quiet && !request.optional {
        cx.issue(MessageKind::Warning, Effect::None, None, message);
    }
    Ok(())
}

pub(super) fn warn_config_fallback(cx: &mut Cx<'_>, name: &[u8]) {
    let lower = name.to_ascii_lowercase();
    cx.issue(
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Author),
        text(&[
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
    cx.issue(
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Author),
        text(&[
            b"Find",
            name,
            b".cmake must either be part of this project itself, in this case adjust CMAKE_MODULE_PATH so that it points to the correct location inside its source tree.\nOr it must be installed by a package which has already been found via find_package().  In this case make sure that package has indeed been found and adjust CMAKE_MODULE_PATH to contain the location where that package has installed Find",
            name,
            b".cmake.  This must be a location provided by that package.  This error in general means that the buildsystem of this project is relying on a Find-module without ensuring that it is actually available.\n",
        ]),
    );
}

impl ConfigSearch<'_> {
    pub(super) fn missing_message(&self, tried_module: bool) -> BString {
        let name = self.name;
        let preface = if tried_module && self.considered.is_empty() {
            text(&[
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
                text(&[b" that ", kind, b"\"", &v.full, b"\""])
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
                    let mut entry = text(&[file, b", version: ", version]);
                    if let Some(reason) = reason {
                        entry.extend_from_slice(b"\n    ");
                        entry.extend_from_slice(reason);
                    }
                    entry
                })
                .collect();
            return text(&[
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
                text(&[b" (requested version ", &v.full, b")"])
            });
        text(&[
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
