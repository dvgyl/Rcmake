//! Byte-oriented preset macro expansion.

use std::collections::{BTreeMap, BTreeSet};

use rcmake_base::BString;
use rcmake_host::path;
use rcmake_json::Value;

use crate::graph::Record;
use crate::schema::{self, Field, Fields};
use crate::{EnvSnapshot, PresetKind, Presets, PresetsError};

const MAX_EXPANSION: usize = 8 * 1024 * 1024;
const VENDOR_MACRO: &str = "Preset contains an unsupported vendor macro";

pub(crate) fn is_vendor_error(error: &PresetsError) -> bool {
    error.message == VENDOR_MACRO
}

pub(crate) struct Macros<'a> {
    presets: &'a Presets,
    env: &'a EnvSnapshot,
    preset: Option<&'a Record>,
    pub(crate) configure_name: Option<&'a BString>,
    pub(crate) generator: Option<&'a BString>,
    pub(crate) variables: Fields,
    expanded: BTreeMap<BString, Option<BString>>,
    visiting: BTreeSet<BString>,
    remaining_bytes: usize,
}

impl<'a> Macros<'a> {
    pub(crate) fn new(
        presets: &'a Presets,
        env: &'a EnvSnapshot,
        preset: Option<&'a Record>,
        variables: Fields,
    ) -> Self {
        Self {
            presets,
            env,
            preset,
            configure_name: None,
            generator: None,
            variables,
            expanded: BTreeMap::new(),
            visiting: BTreeSet::new(),
            remaining_bytes: MAX_EXPANSION,
        }
    }

    pub(crate) fn expand(&mut self, field: &Field) -> Result<BString, PresetsError> {
        let input = schema::text(field)?.as_slice();
        let mut result = Vec::new();
        let mut rest = input;
        while let Some(dollar) = rest.iter().position(|byte| *byte == b'$') {
            let (before, after) = rest.split_at(dollar);
            append(&mut result, before)?;
            let Some(after) = after.strip_prefix(b"$") else {
                break;
            };
            let candidate = [
                (b"{".as_slice(), Namespace::Builtin),
                (b"env{".as_slice(), Namespace::Environment),
                (b"penv{".as_slice(), Namespace::Parent),
                (b"vendor{".as_slice(), Namespace::Vendor),
            ]
            .iter()
            .find_map(|(prefix, namespace)| {
                after.strip_prefix(*prefix).map(|name| (*namespace, name))
            });
            let Some((namespace, name)) = candidate else {
                append(&mut result, b"$")?;
                rest = after;
                continue;
            };
            let end = name
                .iter()
                .position(|byte| *byte == b'}')
                .ok_or_else(invalid)?;
            let (name, tail) = name.split_at(end);
            rest = tail.strip_prefix(b"}").unwrap_or_default();
            let value = self.value(namespace, name, field)?;
            append(&mut result, value.as_slice())?;
        }
        append(&mut result, rest)?;
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(result.len())
            .ok_or_else(invalid)?;
        Ok(BString::from(result))
    }

    fn value(
        &mut self,
        namespace: Namespace,
        name: &[u8],
        field: &Field,
    ) -> Result<BString, PresetsError> {
        let version = self
            .preset
            .map_or(field.source.version, |preset| preset.source.version);
        if self.preset.is_none()
            && !matches!(namespace, Namespace::Parent)
            && !(matches!(namespace, Namespace::Builtin) && version >= 9)
        {
            return Err(invalid());
        }
        match namespace {
            Namespace::Builtin => self.builtin(name, field, version),
            Namespace::Environment if !name.is_empty() => {
                let value = self.variable(name)?;
                Ok(value.unwrap_or_else(|| self.parent(name)))
            }
            Namespace::Parent if !name.is_empty() => Ok(self.parent(name)),
            Namespace::Vendor => Err(PresetsError::new(VENDOR_MACRO)),
            _ => Err(invalid()),
        }
    }

    fn builtin(&self, name: &[u8], field: &Field, version: u32) -> Result<BString, PresetsError> {
        let source = self.presets.source_dir.as_path();
        match name {
            b"sourceDir" => Ok(path::from_native(source)),
            b"sourceParentDir" => Ok(source
                .parent()
                .map_or_else(BString::default, path::from_native)),
            b"sourceDirName" => Ok(source
                .file_name()
                .map_or_else(BString::default, |name| path::from_native(name.as_ref()))),
            b"presetName" => self
                .preset
                .map(|preset| preset.summary.name.clone())
                .ok_or_else(invalid),
            b"configurePresetName" if version >= 13 => {
                self.configure_name.cloned().ok_or_else(invalid)
            }
            b"generator" if self.preset.is_some() => {
                Ok(self.generator.cloned().unwrap_or_default())
            }
            b"hostSystemName" if version >= 3 => Ok(self.presets.host_system.clone()),
            b"fileDir" if version >= 4 || field.source.version >= 12 => {
                let origin = match self.preset {
                    Some(preset) if field.source.version < 12 => &preset.source,
                    _ => &field.source,
                };
                Ok(origin
                    .path
                    .parent()
                    .map_or_else(BString::default, path::from_native))
            }
            b"dollar" => Ok(BString::from("$")),
            b"pathListSep" if version >= 5 => {
                Ok(BString::from(if cfg!(windows) { ";" } else { ":" }))
            }
            _ => Err(invalid()),
        }
    }

    fn parent(&self, name: &[u8]) -> BString {
        if cfg!(windows) {
            return self
                .env
                .vars
                .iter()
                .find_map(|(key, value)| {
                    key.as_slice()
                        .eq_ignore_ascii_case(name)
                        .then(|| value.clone())
                })
                .unwrap_or_default();
        }
        self.env.vars.get(name).cloned().unwrap_or_default()
    }

    fn variable(&mut self, key: &[u8]) -> Result<Option<BString>, PresetsError> {
        if let Some(value) = self.expanded.get(key) {
            return Ok(value.clone());
        }
        let Some(field) = self.variables.get(key).cloned() else {
            return Ok(None);
        };
        if matches!(field.value.as_ref(), Value::Null) {
            return Ok(None);
        }
        schema::depth(self.visiting.len(), "Preset environment expansion")?;
        if !self.visiting.insert(BString::from(key)) {
            return Err(invalid());
        }
        let value = self.expand(&field);
        self.visiting.remove(key);
        let value = value?;
        self.expanded
            .insert(BString::from(key), Some(value.clone()));
        Ok(Some(value))
    }

    pub(crate) fn environment(
        &mut self,
    ) -> Result<BTreeMap<BString, Option<BString>>, PresetsError> {
        let keys: Vec<_> = self.variables.keys().cloned().collect();
        keys.into_iter()
            .map(|key| Ok((key.clone(), self.variable(key.as_slice())?)))
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Namespace {
    Builtin,
    Environment,
    Parent,
    Vendor,
}

fn append(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), PresetsError> {
    if output.len().saturating_add(bytes.len()) > MAX_EXPANSION {
        return Err(invalid());
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn invalid() -> PresetsError {
    PresetsError::new("Invalid macro expansion")
}

pub(crate) fn include_path(
    presets: &Presets,
    field: &Field,
    env: &EnvSnapshot,
) -> Result<BString, PresetsError> {
    if field.source.version < 7 {
        return schema::literal(field);
    }
    Macros::new(presets, env, None, Fields::new())
        .expand(field)
        .map_err(|_| field.invalid("Invalid \"include\" field"))
}

pub(crate) fn environment_fields(preset: &Record) -> Result<Fields, PresetsError> {
    if preset.summary.kind == PresetKind::Workflow {
        return Ok(Fields::new());
    }
    preset
        .fields
        .get(b"environment".as_slice())
        .map(schema::fields)
        .transpose()
        .map(Option::unwrap_or_default)
}
