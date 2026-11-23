//! Resolve preset references, macros, conditions, and executable settings.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use rcmake_base::BString;
use rcmake_host::path;

use crate::expand::{self, Macros};
use crate::graph::{self, Record};
use crate::schema::{self, Decode};
use crate::{
    ConfigureSettings, EnvSnapshot, PresetKind, PresetSettings, PresetSummary, Presets,
    PresetsError, ResolvedPreset, WorkflowSettings,
};

impl Presets {
    /// List presets of this kind in declaration order.
    ///
    /// The list contains presets that can be resolved in the supplied environment.
    #[must_use]
    pub fn list_visible(&self, kind: PresetKind, env: &EnvSnapshot) -> Vec<PresetSummary> {
        self.records
            .get(&kind)
            .into_iter()
            .flat_map(|records| records.values())
            .filter(|record| {
                self.resolve(kind, record.summary.name.as_slice(), env)
                    .is_ok()
            })
            .map(|record| record.summary.clone())
            .collect()
    }

    /// Get the vendor data declared on a preset.
    #[must_use]
    pub fn preset_vendor(
        &self,
        kind: PresetKind,
        name: &[u8],
    ) -> Option<&BTreeMap<BString, rcmake_json::Value>> {
        self.get(kind, name)?.vendor.as_ref()
    }

    /// Resolve a preset and apply its conditions in the supplied environment.
    pub fn resolve(
        &self,
        kind: PresetKind,
        name: &[u8],
        env: &EnvSnapshot,
    ) -> Result<ResolvedPreset, PresetsError> {
        let record = self.get(kind, name).ok_or_else(|| {
            PresetsError::new(format!(
                "No such {} preset: \"{}\"",
                kind.label(),
                String::from_utf8_lossy(name)
            ))
        })?;
        if record.hidden {
            return Err(PresetsError::new(format!(
                "Cannot use hidden preset in {}: \"{}\"",
                kind.label(),
                record.summary.name
            )));
        }
        self.resolve_record(record, env).map_err(|error| {
            if expand::is_vendor_error(&error) {
                record.invalid("Preset contains an unsupported vendor macro")
            } else if error.message == "Invalid macro expansion" {
                PresetsError::new(format!(
                    "{}\nInvalid macro expansion in \"{}\"",
                    record.invalid("Invalid macro expansion"),
                    record.summary.name
                ))
            } else {
                error
            }
        })
    }

    pub(crate) fn validate_references(
        &self,
        reachable: &BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    ) -> Result<(), PresetsError> {
        for records in self.records.values() {
            for record in records.values() {
                match record.read(&mut schema::literal)? {
                    PresetSettings::Configure(settings) => validate_configure(record, &settings)?,
                    PresetSettings::Workflow(workflow) => {
                        self.validate_workflow(record, &workflow, reachable)?
                    }
                    _ if !record.hidden => {
                        let name = raw_configure_name(record)?;
                        if !name.contains(&b'$') {
                            self.configure_reference(record, &name, reachable)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn get(&self, kind: PresetKind, name: &[u8]) -> Option<&Record> {
        self.records.get(&kind)?.get(name)
    }

    fn configure_reference(
        &self,
        record: &Record,
        name: &BString,
        reach: &BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    ) -> Result<&Record, PresetsError> {
        let kind = record.summary.kind.title();
        let own = &record.summary.name;
        let config = self
            .get(PresetKind::Configure, name.as_slice())
            .ok_or_else(|| {
                PresetsError::new(format!(
                    "{kind} preset \"{own}\" references unknown configure preset \"{name}\""
                ))
            })?;
        if !graph::reachable(reach, &record.source, &config.source) {
            return Err(PresetsError::new(format!(
                "{kind} preset \"{own}\" references configure preset \"{name}\", but \"{name}\" is not reachable from the preset's file"
            )));
        }
        Ok(config)
    }

    fn validate_workflow(
        &self,
        record: &Record,
        workflow: &WorkflowSettings,
        reach: &BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    ) -> Result<(), PresetsError> {
        let name = &record.summary.name;
        let Some((first, rest)) = workflow.steps.split_first() else {
            return Err(PresetsError::new(format!(
                "No workflow steps specified for \"{name}\""
            )));
        };
        if first.kind != PresetKind::Configure {
            let kind = first.kind.label();
            let name = &first.name;
            return Err(PresetsError::new(format!(
                "First workflow step of type \"{kind}\" named \"{name}\" must be a configure step"
            )));
        }
        for (index, step) in workflow.steps.iter().enumerate() {
            let kind = step.kind.label();
            let step_name = &step.name;
            if index > 0 && step.kind == PresetKind::Configure {
                return Err(PresetsError::new(format!(
                    "Workflow step of type \"configure\" named \"{step_name}\" must be the first step"
                )));
            }
            let target = self.get(step.kind, step.name.as_slice()).ok_or_else(|| {
                PresetsError::new(format!(
                    "Invalid workflow step of type \"{kind}\" named \"{step_name}\""
                ))
            })?;
            if !graph::reachable(reach, &record.source, &target.source) {
                return Err(PresetsError::new(format!(
                    "Workflow step of type \"{kind}\" named \"{step_name}\" is unreachable from preset's file"
                )));
            }
        }
        for step in rest {
            let Some(target) = self.get(step.kind, step.name.as_slice()) else {
                continue;
            };
            if let Some(field) = target.fields.get(b"configurePreset".as_slice()) {
                let associated = schema::text(field)?;
                if associated != &first.name && !associated.contains(&b'$') {
                    return Err(workflow_config_mismatch(
                        step.kind,
                        &step.name,
                        associated,
                        &first.name,
                    ));
                }
            }
        }
        Ok(())
    }

    fn resolve_record(
        &self,
        record: &Record,
        env: &EnvSnapshot,
    ) -> Result<ResolvedPreset, PresetsError> {
        let kind = record.summary.kind;
        if kind == PresetKind::Workflow {
            let PresetSettings::Workflow(workflow) = record.read(&mut schema::literal)? else {
                return Err(record.invalid(""));
            };
            let first = workflow.steps.first().ok_or_else(|| record.invalid(""))?;
            for step in &workflow.steps {
                let resolved = self.resolve(step.kind, step.name.as_slice(), env)?;
                if let Some(associated) = &resolved.configure_preset
                    && associated != &first.name
                {
                    return Err(workflow_config_mismatch(
                        step.kind,
                        &step.name,
                        associated,
                        &first.name,
                    ));
                }
            }
            return Ok(result(
                record,
                None,
                None,
                None,
                None,
                BTreeMap::new(),
                PresetSettings::Workflow(workflow),
            ));
        }

        let own_environment = expand::environment_fields(record)?;
        let configure_name = if kind == PresetKind::Configure {
            record.summary.name.clone()
        } else {
            let field = record
                .fields
                .get(b"configurePreset".as_slice())
                .ok_or_else(|| missing_configure(record))?;
            Macros::new(self, env, Some(record), own_environment.clone()).expand(field)?
        };
        let mut variables = own_environment;
        let (associated, inherit_environment) = if kind == PresetKind::Configure {
            (None, None)
        } else {
            let config = self.configure_reference(record, &configure_name, &self.reachable)?;
            let inherit = record
                .fields
                .get(b"inheritConfigureEnvironment".as_slice())
                .map(|field| bool::decode(field, &mut schema::literal))
                .transpose()?
                .unwrap_or(true);
            if inherit {
                for (name, field) in expand::environment_fields(config)? {
                    variables.entry(name).or_insert(field);
                }
            }
            (
                Some(self.resolve(PresetKind::Configure, configure_name.as_slice(), env)?),
                Some(inherit),
            )
        };
        let generator = if let Some(configure) = &associated {
            configure.generator.clone()
        } else {
            record
                .fields
                .get(b"generator".as_slice())
                .map(schema::literal)
                .transpose()?
        };

        let mut macros = Macros::new(self, env, Some(record), variables);
        macros.configure_name = Some(&configure_name);
        if kind != PresetKind::Package {
            macros.generator = generator.as_ref();
        }
        let environment = macros.environment()?;
        let mut settings = record.read(&mut |field| macros.expand(field))?;
        for condition in record.conditions.values() {
            if !condition.evaluate(&mut |field| macros.expand(field))? {
                return Err(PresetsError::new(format!(
                    "Cannot use disabled preset in {}: \"{}\"",
                    kind.label(),
                    record.summary.name
                )));
            }
        }
        let binary_dir = if let PresetSettings::Configure(configure) = &mut settings {
            self.source_relative(&mut configure.binary_dir);
            self.source_relative(&mut configure.install_dir);
            configure.binary_dir.clone()
        } else {
            associated.and_then(|configure| configure.binary_dir)
        };
        Ok(result(
            record,
            (kind != PresetKind::Configure).then_some(configure_name),
            inherit_environment,
            generator,
            binary_dir,
            environment,
            settings,
        ))
    }

    fn source_relative(&self, value: &mut Option<BString>) {
        if let Some(text) = value
            && !text.is_empty()
        {
            *text = path::from_native(&graph::normalize(
                &self.source_dir.join(path::to_native(text.as_slice())),
            ));
        }
    }
}

fn result(
    record: &Record,
    configure_preset: Option<BString>,
    inherit_configure_environment: Option<bool>,
    generator: Option<BString>,
    binary_dir: Option<BString>,
    environment: BTreeMap<BString, Option<BString>>,
    settings: PresetSettings,
) -> ResolvedPreset {
    let summary = &record.summary;
    ResolvedPreset {
        kind: summary.kind,
        name: summary.name.clone(),
        display_name: summary.display_name.clone(),
        description: summary.description.clone(),
        configure_preset,
        inherit_configure_environment,
        generator,
        binary_dir,
        environment,
        settings,
    }
}

fn raw_configure_name(record: &Record) -> Result<BString, PresetsError> {
    record
        .fields
        .get(b"configurePreset".as_slice())
        .map(schema::literal)
        .transpose()?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| missing_configure(record))
}

fn missing_configure(record: &Record) -> PresetsError {
    let kind = record.summary.kind.title();
    record.invalid(&format!(
        "{kind} presets must either be hidden or have an associated configure preset"
    ))
}

fn validate_configure(record: &Record, settings: &ConfigureSettings) -> Result<(), PresetsError> {
    if record.hidden {
        return Ok(());
    }
    if record.source.version <= 2 {
        for (key, value) in [
            ("generator", &settings.generator),
            ("binaryDir", &settings.binary_dir),
        ] {
            if value.as_ref().is_none_or(|value| value.is_empty()) {
                return Err(PresetsError::new(format!(
                    "Preset \"{}\" missing field \"{key}\"\n{}",
                    record.summary.name,
                    record.invalid("")
                )));
            }
        }
    }
    if let (Some(warnings), Some(errors)) = (&settings.warnings, &settings.errors) {
        let author = if record.source.version < 12 {
            "dev"
        } else {
            "author"
        };
        for (key, warning, error) in [
            (author, warnings.author, errors.author),
            ("deprecated", warnings.deprecated, errors.deprecated),
            ("experimental", warnings.experimental, errors.experimental),
            (
                "installAbsoluteDestination",
                warnings.install_absolute_destination,
                errors.install_absolute_destination,
            ),
            (
                "nonTargetDirective",
                warnings.non_target_directive,
                errors.non_target_directive,
            ),
            ("policy", warnings.policy, errors.policy),
            ("strict", warnings.strict, errors.strict),
            (
                "uninitialized",
                warnings.uninitialized,
                errors.uninitialized,
            ),
            ("unusedCli", warnings.unused_cli, errors.unused_cli),
        ] {
            if warning == Some(false) && error == Some(true) {
                return Err(record.invalid(&format!(
                    "\"errors.{key}\" is enabled while \"warnings.{key}\" is disabled"
                )));
            }
        }
    }
    Ok(())
}

fn workflow_config_mismatch(
    kind: PresetKind,
    step: &BString,
    associated: &BString,
    first: &BString,
) -> PresetsError {
    let kind = kind.title();
    PresetsError::new(format!(
        "{kind} workflow step \"{step}\" uses configure preset \"{associated}\", but workflow configure preset is \"{first}\""
    ))
}
