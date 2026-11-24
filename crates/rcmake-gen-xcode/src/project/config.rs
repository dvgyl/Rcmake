//! Xcode configuration files and project diagnostics.

use std::{collections::btree_map::Entry, sync::Arc};

use rcmake_base::path_lex::Lexical;
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, RenderOptions, render};
use rcmake_gen_common::GenError;
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_model::DirId;
use rcmake_resolve::ide::XcodeConfigFiles;

use crate::{
    object::{Attrs, Reference, Value},
    settings, text,
};

use super::{Group, Project, file_ref};

impl Project<'_> {
    pub(super) fn project_settings(
        &mut self,
        dir: DirId,
        key: &str,
        group: &mut Group,
    ) -> Result<Vec<(Attrs, Option<Reference>)>, GenError> {
        const CONFIG_REFERENCE: &str = "\0xcconfig";
        let xcconfig = self.model.definition(dir, b"CMAKE_XCODE_XCCONFIG");
        let with_config_reference = xcconfig
            .filter(|value| value.windows(2).any(|token| token == b"$<"))
            .map(|value| {
                // Evaluate the configuration file with the same context as project attributes.
                let mut model = self.model.clone();
                model.dir_mut(dir).vars.insert(
                    format!("CMAKE_XCODE_ATTRIBUTE_{CONFIG_REFERENCE}").into(),
                    value.clone(),
                );
                model
            });
        let model = with_config_reference.as_ref().unwrap_or(self.model);
        let mut configurations = Vec::new();
        for config in self.configs.clone() {
            let mut attrs = settings::project(model, dir, self.bin, &config)?;
            let path = if with_config_reference.is_some() {
                match attrs.remove(CONFIG_REFERENCE) {
                    Some(Value::Text(path) | Value::QuotedText(path)) => Some(path),
                    _ => None,
                }
            } else {
                xcconfig.map(|value| text(value))
            };
            let reference = path
                .map(|path| self.xcconfig(dir, key, &config, &path, None, group))
                .transpose()?
                .flatten();
            configurations.push((attrs, reference));
        }
        Ok(configurations)
    }

    pub(super) fn xcconfig(
        &mut self,
        dir: DirId,
        key: &str,
        config: &str,
        path: &str,
        target: Option<&str>,
        group: &mut Group,
    ) -> Result<Option<Reference>, GenError> {
        if path.is_empty() {
            return Ok(None);
        }
        let directory = self.model.dir(dir);
        let files = match self.config_sources.entry(config.into()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(
                XcodeConfigFiles::new(self.model, self.host, config.as_bytes())
                    .map_err(|error| GenError::Invalid(error.to_string().into()))?,
            ),
        };
        let Some(path) = files.find(directory, path.as_bytes()) else {
            if let Some(target) = target {
                return Err(GenError::Invalid(format!(
                    "CMake Error: target sources for target {target} do not contain xcconfig file: '{path}' (configuration: {config})"
                ).into()));
            }
            let source = Lexical::new(&directory.source_dir).append(path.as_bytes());
            return Err(self.project_error(
                dir,
                GenError::Invalid(format!("Cannot find source file:\n\n  {source}").into()),
            ));
        };
        let reference = file_ref(
            &mut self.objects,
            &format!("{key}/xcconfig/{config}"),
            &text(&path),
            ("lastKnownFileType", "text.xcconfig"),
            "<absolute>",
        )?;
        group.add("Configurations", reference.clone())?;
        Ok(Some(reference))
    }

    pub(super) fn project_error(&self, dir: DirId, error: GenError) -> GenError {
        match error {
            GenError::Invalid(body) => {
                let file = to_native(&self.model.dir(dir).source_dir).join("CMakeLists.txt");
                let diagnostic = Diagnostic {
                    kind: MessageKind::Error,
                    effect: Effect::Fatal,
                    category: None,
                    policy: None,
                    location: Location::FileLine {
                        file: Arc::from(from_native_cmake(&file).as_slice()),
                        line: 0,
                    },
                    body,
                };
                GenError::Invalid(render(
                    &diagnostic,
                    &RenderOptions {
                        relative_base: Some(self.model.dir(DirId::ROOT).source_dir.clone()),
                        ..Default::default()
                    },
                ))
            }
            error => error,
        }
    }

    pub(super) fn source_error(&self, dir: DirId, error: GenError) -> GenError {
        if matches!(&error, GenError::Invalid(body) if body.starts_with(b"Xcode does not support per-config "))
        {
            return self.project_error(dir, error);
        }
        error
    }
}
