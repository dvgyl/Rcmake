//! Xcode configuration files and project diagnostics.

use std::{collections::btree_map::Entry, sync::Arc};

use rcmake_base::{BString, path_lex::Lexical};
use rcmake_diag::{Diagnostic, Effect, Location, MessageKind, RenderOptions, render};
use rcmake_gen_common::GenError;
use rcmake_genex::{ConfigId, EvalContext};
use rcmake_host::path::{from_native_cmake, to_native};
use rcmake_model::{DirId, GeneratorKind};
use rcmake_resolve::{ide::XcodeConfigFiles, usage::UsageResolver};

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
        let xcconfig_genex = xcconfig.filter(|value| rcmake_genex::find_genex(value).is_some());
        let direct_eval = self.model.generator == Some(GeneratorKind::Xcode);
        let with_config_reference = xcconfig_genex.filter(|_| !direct_eval).map(|value| {
            // When callers invoke the Xcode generator directly without setting
            // `model.generator = Some(GeneratorKind::Xcode)`, route through
            // `xcode_project_attributes` so the evaluation context stays Xcode.
            let mut model = self.model.clone();
            model.dir_mut(dir).vars.insert(
                format!("CMAKE_XCODE_ATTRIBUTE_{CONFIG_REFERENCE}").into(),
                value.clone(),
            );
            model
        });
        let model = with_config_reference.as_ref().unwrap_or(self.model);
        let mut xcconfig_ast = None;
        let mut usage = None;
        let mut configurations = Vec::new();
        for config in self.configs.clone() {
            let mut attrs = settings::project(model, dir, self.bin, &config)?;
            let path = if with_config_reference.is_some() {
                match attrs.remove(CONFIG_REFERENCE) {
                    Some(Value::Text(path) | Value::QuotedText(path)) => Some(path),
                    _ => None,
                }
            } else if let Some(expr) = xcconfig_genex {
                let ast = match &xcconfig_ast {
                    Some(ast) => ast,
                    None => {
                        let parsed = rcmake_genex::parse(expr).map_err(|error| {
                            GenError::Invalid(error.without_location().to_string().into())
                        })?;
                        xcconfig_ast.insert(parsed)
                    }
                };
                let resolver = usage.get_or_insert_with(|| UsageResolver::new(self.model));
                let ctx = EvalContext {
                    config: ConfigId(BString::from(config.as_bytes())),
                    ..EvalContext::default()
                };
                let (value, _) = rcmake_genex::evaluate(ast, &ctx, &mut resolver.host(dir))
                    .map_err(|error| {
                        GenError::Invalid(error.without_location().to_string().into())
                    })?;
                Some(text(&value))
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
                GenError::Invalid(format!("Cannot find source file:\n  {source}").into()),
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
                    annotations: None,
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
