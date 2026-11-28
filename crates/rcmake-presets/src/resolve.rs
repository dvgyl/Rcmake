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
    /// Get workflow definitions in declaration order, including unavailable workflows.
    pub fn workflow_definitions(&self) -> impl Iterator<Item = (&PresetSummary, WorkflowSettings)> {
        self.records
            .get(&PresetKind::Workflow)
            .into_iter()
            .flat_map(|records| records.values())
            .filter_map(|record| match record.read(&mut schema::literal).ok()? {
                PresetSettings::Workflow(workflow) => Some((&record.summary, workflow)),
                _ => None,
            })
    }

    /// List presets in declaration order, excluding hidden presets.
    pub fn list_defined(&self, kind: PresetKind) -> impl Iterator<Item = &PresetSummary> {
        self.records
            .get(&kind)
            .into_iter()
            .flat_map(|records| records.values())
            .filter(|record| !record.hidden)
            .map(|record| &record.summary)
    }

    /// List all presets in declaration order, including hidden presets.
    pub fn list_all(&self, kind: PresetKind) -> impl Iterator<Item = &PresetSummary> {
        self.records
            .get(&kind)
            .into_iter()
            .flat_map(|records| records.values())
            .map(|record| &record.summary)
    }

    /// Inspect a preset without rejecting hidden or disabled presets.
    ///
    /// Returns `Ok(None)` if no preset of `kind` with `name` exists.
    /// Evaluates inheritance, macros, environment variables, and conditions against `env`.
    pub fn inspect(
        &self,
        kind: PresetKind,
        name: &[u8],
        env: &EnvSnapshot,
    ) -> Result<Option<crate::PresetInspection>, PresetsError> {
        let Some(record) = self.get(kind, name) else {
            return Ok(None);
        };
        let (mut preset, mut enabled) = self.expand_for_inspect(record, env)?;
        if let Some(associated) = preset
            .configure_preset
            .as_ref()
            .filter(|name| !name.is_empty())
            && let Some(cfg_record) = self.get(PresetKind::Configure, associated.as_slice())
        {
            let (cfg_resolved, cfg_enabled) = self.expand_for_inspect(cfg_record, env)?;
            preset.binary_dir = cfg_resolved.binary_dir;
            if !cfg_enabled {
                enabled = false;
            }
        }
        if let PresetSettings::Workflow(workflow) = &preset.settings {
            for step in &workflow.steps {
                if let Some(step_record) = self.get(step.kind, step.name.as_slice()) {
                    let (_, step_enabled) = self.expand_for_inspect(step_record, env)?;
                    if !step_enabled {
                        enabled = false;
                    }
                }
            }
        }
        Ok(Some(crate::PresetInspection {
            preset,
            hidden: record.hidden,
            enabled,
            condition: record
                .condition
                .as_ref()
                .map(|cond| cond.1.as_ref().clone()),
            inherits: record.inherits.clone(),
            source_file: record.source.path.clone(),
        }))
    }

    fn expand_for_inspect(
        &self,
        record: &Record,
        env: &EnvSnapshot,
    ) -> Result<(ResolvedPreset, bool), PresetsError> {
        self.expand_record(record, env).map_err(|error| {
            if expand::is_vendor_error(&error) {
                PresetsError::new(format!(
                    "Could not evaluate {} preset \"{}\": Invalid macro expansion",
                    record.summary.kind.label(),
                    record.summary.name
                ))
            } else {
                error
            }
        })
    }

    /// Get the JSON condition of a preset, including a condition it inherits.
    #[must_use]
    pub fn preset_condition(&self, kind: PresetKind, name: &[u8]) -> Option<&rcmake_json::Value> {
        let record = self.get(kind, name)?;
        Some(&record.condition.as_ref()?.1)
    }

    /// List presets of this kind in declaration order.
    ///
    /// The list contains presets that can be resolved in the supplied environment.
    /// It omits a preset whose configure preset has a false condition, although
    /// [`Presets::resolve`] accepts that preset.
    #[must_use]
    pub fn list_visible(&self, kind: PresetKind, env: &EnvSnapshot) -> Vec<PresetSummary> {
        self.records
            .get(&kind)
            .into_iter()
            .flat_map(|records| records.values())
            .filter(|record| {
                self.resolve(kind, record.summary.name.as_slice(), env)
                    .is_ok_and(|preset| {
                        preset.configure_preset.is_none_or(|configure| {
                            self.resolve_definition(PresetKind::Configure, &configure, env)
                                .is_ok()
                        })
                    })
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
        let mut result = self.resolve_definition(kind, name, env)?;
        if let Some(associated) = &result.configure_preset {
            result.binary_dir = self
                .resolve_ignoring_condition(PresetKind::Configure, associated.as_slice(), env)?
                .binary_dir;
        }
        if let PresetSettings::Workflow(workflow) = &result.settings {
            for step in &workflow.steps {
                self.resolve(step.kind, step.name.as_slice(), env)?;
            }
        }
        Ok(result)
    }

    /// Resolve the fields and condition of one preset.
    ///
    /// The caller checks the availability of its configure preset or workflow steps.
    pub fn resolve_definition(
        &self,
        kind: PresetKind,
        name: &[u8],
        env: &EnvSnapshot,
    ) -> Result<ResolvedPreset, PresetsError> {
        let (result, enabled) = self.resolve_record(kind, name, env)?;
        if !enabled {
            return Err(PresetsError::new(format!(
                "Cannot use disabled {} preset in {}: \"{}\"",
                kind.label(),
                path::from_native_cmake(&self.source_dir),
                result.name
            )));
        }
        Ok(result)
    }

    /// Resolve the fields of one preset without rejecting a false condition.
    ///
    /// A build, test, or package preset uses this rule for its configure preset.
    /// A missing or hidden preset, an invalid macro, and an unsupported vendor
    /// macro in the preset or its condition still cause an error.
    pub fn resolve_ignoring_condition(
        &self,
        kind: PresetKind,
        name: &[u8],
        env: &EnvSnapshot,
    ) -> Result<ResolvedPreset, PresetsError> {
        Ok(self.resolve_record(kind, name, env)?.0)
    }

    fn resolve_record(
        &self,
        kind: PresetKind,
        name: &[u8],
        env: &EnvSnapshot,
    ) -> Result<(ResolvedPreset, bool), PresetsError> {
        let record = self.get(kind, name).ok_or_else(|| {
            PresetsError::new(format!(
                "No such {} preset in {}: \"{}\"",
                kind.label(),
                path::from_native_cmake(&self.source_dir),
                String::from_utf8_lossy(name)
            ))
        })?;
        if record.hidden {
            return Err(PresetsError::new(format!(
                "Cannot use hidden {} preset in {}: \"{}\"",
                kind.label(),
                path::from_native_cmake(&self.source_dir),
                record.summary.name
            )));
        }
        self.expand_record(record, env).map_err(|error| {
            if expand::is_vendor_error(&error) {
                PresetsError::new(format!(
                    "Could not evaluate {} preset \"{}\": Invalid macro expansion",
                    kind.label(),
                    record.summary.name
                ))
            } else {
                error
            }
        })
    }

    pub(crate) fn validate_expansions(&self, env: &EnvSnapshot) -> Result<(), PresetsError> {
        for records in self.records.values() {
            for record in records.values() {
                if let Err(error) = self.expand_record(record, env)
                    && !expand::is_vendor_error(&error)
                {
                    return Err(error);
                }
            }
        }
        Ok(())
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
                        self.configure_reference(record, &name, reachable)?;
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
                if associated != &first.name {
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

    fn expand_record(
        &self,
        record: &Record,
        env: &EnvSnapshot,
    ) -> Result<(ResolvedPreset, bool), PresetsError> {
        let kind = record.summary.kind;
        if kind == PresetKind::Workflow {
            return Ok((
                result(
                    record,
                    None,
                    None,
                    None,
                    None,
                    BTreeMap::new(),
                    record.read(&mut schema::literal)?,
                ),
                true,
            ));
        }

        let mut variables = expand::environment_fields(record)?;
        let configure_name = if kind == PresetKind::Configure {
            record.summary.name.clone()
        } else {
            optional_configure_name(record)?.unwrap_or_default()
        };
        let config = if kind == PresetKind::Configure {
            Some(record)
        } else if record.hidden {
            self.get(PresetKind::Configure, configure_name.as_slice())
        } else {
            Some(self.configure_reference(record, &configure_name, &self.reachable)?)
        };
        let inherit_environment = if kind == PresetKind::Configure {
            None
        } else {
            let inherit = record
                .fields
                .get(b"inheritConfigureEnvironment".as_slice())
                .map(|field| bool::decode(field, &mut schema::literal))
                .transpose()?
                .unwrap_or(true);
            if inherit && let Some(config) = config {
                for (name, field) in expand::environment_fields(config)? {
                    variables.entry(name).or_insert(field);
                }
            }
            Some(inherit)
        };
        let generator = config
            .and_then(|config| config.fields.get(b"generator".as_slice()))
            .map(schema::literal)
            .transpose()?;

        let macro_generator = if record.hidden {
            None
        } else {
            self.macro_generator(&record.summary.name)?
        };
        let mut macros = Macros::new(self, env, Some(record), variables);
        macros.configure_name = Some(&configure_name);
        macros.generator = macro_generator.as_ref();
        let environment = macros
            .environment()
            .map_err(|error| expansion_error(record, error, Some("Invalid macro expansion")))?;
        let enabled = record
            .condition
            .as_ref()
            .map(|condition| {
                condition
                    .0
                    .evaluate(&mut |field| macros.expand(field))
                    .map_err(|error| expansion_error(record, error, Some("Invalid condition")))
            })
            .transpose()?
            .unwrap_or(true);
        let mut settings = record
            .read(&mut |field| macros.expand(field))
            .map_err(|error| expansion_error(record, error, None))?;
        let binary_dir = if let PresetSettings::Configure(configure) = &mut settings {
            self.source_relative(&mut configure.binary_dir);
            self.source_relative(&mut configure.install_dir);
            configure.binary_dir.clone()
        } else {
            None
        };
        Ok((
            result(
                record,
                (kind != PresetKind::Configure).then_some(configure_name),
                inherit_environment,
                generator,
                binary_dir,
                environment,
                settings,
            ),
            enabled,
        ))
    }

    // `${generator}` looks up presets by name only. A build or test preset with this
    // name selects its configure preset. Otherwise the configure preset with this
    // name supplies the generator.
    fn macro_generator(&self, name: &[u8]) -> Result<Option<BString>, PresetsError> {
        let mut configure = BString::from(name);
        for kind in [PresetKind::Build, PresetKind::Test] {
            if let Some(record) = self.get(kind, name) {
                configure = optional_configure_name(record)?.unwrap_or_default();
                break;
            }
        }
        self.get(PresetKind::Configure, configure.as_slice())
            .and_then(|config| config.fields.get(b"generator".as_slice()))
            .map(schema::literal)
            .transpose()
    }

    fn source_relative(&self, value: &mut Option<BString>) {
        if let Some(text) = value
            && !text.is_empty()
        {
            let native = path::to_native(text.as_slice());
            let resolved = if native.has_root() {
                graph::normalize(&native)
            } else {
                graph::normalize(&self.source_dir.join(native))
            };
            *text = path::from_native_cmake(&resolved);
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
    optional_configure_name(record)?.ok_or_else(|| missing_configure(record))
}

fn optional_configure_name(record: &Record) -> Result<Option<BString>, PresetsError> {
    record
        .fields
        .get(b"configurePreset".as_slice())
        .map(schema::literal)
        .transpose()
        .map(|name| name.filter(|name| !name.is_empty()))
}

fn expansion_error(record: &Record, error: PresetsError, context: Option<&str>) -> PresetsError {
    if expand::is_vendor_error(&error)
        || (context.is_none() && error.message != "Invalid macro expansion")
    {
        return error;
    }
    let detail = format!("Invalid macro expansion in \"{}\"", record.summary.name);
    match context {
        Some(context) => PresetsError::new(format!("{}\n{detail}", record.invalid(context))),
        None => PresetsError::new(detail),
    }
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
