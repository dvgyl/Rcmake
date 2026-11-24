use rcmake_base::BString;
use rcmake_diag::{Effect, MessageKind};
use rcmake_eval::{CmdError, Cx};

use super::config::ConfigSearch;
use super::request::list_lines;
use super::{Request, diagnostic, text};

pub(super) fn report_missing(
    cx: &mut Cx<'_>,
    request: &Request,
    message: BString,
) -> Result<(), CmdError> {
    if request.required {
        return Err(diagnostic(cx, message));
    }
    if !request.quiet && !request.optional {
        cx.issue(MessageKind::Warning, Effect::None, None, message);
    }
    Ok(())
}

impl ConfigSearch<'_> {
    pub(super) fn missing_message(&self, tried_module: bool) -> BString {
        let name = self.name;
        let cps_rejected = self
            .considered
            .iter()
            .any(|(path, _, _)| path.ends_with(b".cps"));
        let preface = if tried_module && !cps_rejected {
            text(&[
                b"By not providing \"Find", name,
                b".cmake\" in CMAKE_MODULE_PATH this project has asked CMake to find a package configuration file provided by \"",
                name, b"\", but CMake did not find one.\n\n",
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
                b".\n\nThe following configuration files were considered but not accepted:\n",
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
            b" with any of the following names:\n",
            &list_lines(&configs),
            b"\nAdd the installation prefix of \"",
            name,
            b"\" to CMAKE_PREFIX_PATH or set \"",
            name,
            b"_DIR\" to a directory containing one of the above files.  If \"",
            name,
            b"\" provides a separate development package or SDK, be sure it has been installed.\n",
        ])
    }
}
