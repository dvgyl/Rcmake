//! Schema field reading and source tracking for inherited values.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_json::Value;

use crate::PresetsError;
use crate::location::{self, Span};

pub(crate) const MAX_DEPTH: usize = 64;
pub(crate) const MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;
pub(crate) type Fields = BTreeMap<BString, Field>;

#[derive(Clone, Debug)]
pub(crate) struct Origin {
    pub(crate) path: PathBuf,
    pub(crate) version: u32,
    pub(crate) text: Option<Arc<[u8]>>,
}

impl Origin {
    pub(crate) fn invalid(
        &self,
        offset: Option<usize>,
        message: impl std::fmt::Display,
    ) -> PresetsError {
        if let (Some(text), Some(offset)) = (&self.text, offset) {
            location::diagnostic(&self.path, text, offset, message)
        } else {
            PresetsError::new(format!("{}: {message}", self.path.display()))
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub(crate) source: Arc<Origin>,
    pub(crate) value: Arc<Value>,
    pub(crate) members: Option<Arc<Fields>>,
    span: Option<Arc<Span>>,
    key: Option<BString>,
    pub(crate) preset_name: Option<BString>,
}

impl Field {
    pub(crate) fn new(source: &Arc<Origin>, value: Value, span: Option<Arc<Span>>) -> Self {
        Self {
            source: Arc::clone(source),
            value: Arc::new(value),
            members: None,
            span,
            key: None,
            preset_name: None,
        }
    }

    pub(crate) fn invalid(&self, message: impl std::fmt::Display) -> PresetsError {
        if self.source.text.is_some() && matches!(self.value.as_ref(), Value::Null) {
            return PresetsError::new(message.to_string());
        }
        self.source.invalid(self.offset(), message)
    }

    pub(crate) fn offset(&self) -> Option<usize> {
        self.span.as_ref().map(|span| span.start)
    }

    fn expected(&self, kind: &str) -> PresetsError {
        if let Some(key) = &self.key {
            let value = match self.value.as_ref() {
                Value::Array(_) | Value::Object(_) => None,
                Value::Null => Some(String::new()),
                Value::String(text) => Some(text.to_string()),
                value => Some(rcmake_json::write_compact(value).to_string()),
            };
            if let Some(value) = value {
                self.invalid(format_args!("\"{key}\" expected {kind}, got: {value}"))
            } else {
                self.invalid(format_args!("\"{key}\" expected {kind}"))
            }
        } else {
            self.invalid(format_args!("Expected {kind}"))
        }
    }
}

pub(crate) type Expand<'a> = dyn FnMut(&Field) -> Result<BString, PresetsError> + 'a;

pub(crate) trait Decode: Sized {
    fn decode(field: &Field, expand: &mut Expand<'_>) -> Result<Self, PresetsError>;
}

pub(crate) struct Reader<'a> {
    fields: Fields,
    source: &'a Arc<Origin>,
    label: &'a str,
    offset: Option<usize>,
    preset_name: Option<&'a BString>,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(
        fields: Fields,
        source: &'a Arc<Origin>,
        label: &'a str,
        offset: Option<usize>,
    ) -> Self {
        Self {
            fields,
            source,
            label,
            offset,
            preset_name: None,
        }
    }

    pub(crate) fn object(field: &'a Field, label: &'a str) -> Result<Self, PresetsError> {
        let members = fields(field).map_err(|_| field.invalid(format_args!("Invalid {label}")))?;
        Ok(Self::new(members, &field.source, label, field.offset()))
    }

    pub(crate) fn with_preset(mut self, name: &'a BString) -> Self {
        self.preset_name = Some(name);
        self
    }

    pub(crate) fn take(
        &mut self,
        key: &str,
        min: u32,
        max: u32,
    ) -> Result<Option<Field>, PresetsError> {
        let mut field = self.fields.remove(key.as_bytes());
        if let Some(field) = &field
            && !(min..=max).contains(&field.source.version)
        {
            if key == "$comment" {
                return Err(field.invalid(format_args!(
                    "Invalid extra field \"{key}\" in {}",
                    self.label
                )));
            }
            let (required, relation) = if field.source.version < min {
                (min, "higher")
            } else {
                (max, "lower")
            };
            let feature = match (self.label, key) {
                (_, "buildPresets" | "testPresets") => "build and test preset".to_owned(),
                (_, "packagePresets") => "package preset".to_owned(),
                (_, "workflowPresets") => "workflow preset".to_owned(),
                (_, "graphviz" | "installDir" | "toolchainFile" | "trace") => {
                    format!("{key} preset")
                }
                ("warnings" | "errors", _) => format!("{}.{key}", self.label),
                _ => key.to_owned(),
            };
            return Err(PresetsError::new(format!(
                "File version must be {required} or {relation} for {feature} support"
            )));
        }
        if let Some(field) = &mut field {
            field.preset_name = self.preset_name.cloned();
        }
        Ok(field)
    }

    pub(crate) fn optional<T: Decode>(
        &mut self,
        key: &str,
        min: u32,
        max: u32,
        expand: &mut Expand<'_>,
    ) -> Result<Option<T>, PresetsError> {
        self.take(key, min, max)?
            .map(|field| T::decode(&field, expand))
            .transpose()
    }

    pub(crate) fn required(&mut self, key: &str) -> Result<Field, PresetsError> {
        self.take(key, 1, 13)?.ok_or_else(|| {
            self.source.invalid(
                self.offset,
                format_args!("Missing required field \"{key}\" in {}", self.label),
            )
        })
    }

    pub(crate) fn into_fields(self) -> Fields {
        self.fields
    }

    pub(crate) fn finish(mut self) -> Result<(), PresetsError> {
        self.take("$comment", 10, 13)?;
        if let Some((key, field)) = self.fields.first_key_value() {
            return Err(field.invalid(format_args!(
                "Invalid extra field \"{key}\" in {}",
                self.label
            )));
        }
        Ok(())
    }
}

pub(crate) fn object(field: &Field) -> Result<&BTreeMap<BString, Value>, PresetsError> {
    match field.value.as_ref() {
        Value::Object(map) => Ok(map),
        _ => Err(field.invalid("Expected an object")),
    }
}

pub(crate) fn fields(field: &Field) -> Result<Fields, PresetsError> {
    if let Some(members) = &field.members {
        return Ok(members.as_ref().clone());
    }
    Ok(object(field)?
        .iter()
        .map(|(key, value)| {
            let span = field
                .span
                .as_ref()
                .and_then(|span| span.member(key.as_slice()));
            let mut member = Field::new(&field.source, value.clone(), span);
            member.key = Some(key.clone());
            (key.clone(), member)
        })
        .collect())
}

pub(crate) fn array(field: &Field) -> Result<impl Iterator<Item = Field> + '_, PresetsError> {
    match field.value.as_ref() {
        Value::Array(values) => Ok(values.iter().enumerate().map(|(index, value)| {
            let span = field.span.as_ref().and_then(|span| span.item(index));
            Field::new(&field.source, value.clone(), span)
        })),
        _ => Err(field.invalid("Expected an array")),
    }
}

pub(crate) fn strings(
    field: &Field,
    expand: &mut Expand<'_>,
) -> Result<Vec<BString>, PresetsError> {
    array(field)?.map(|item| expand(&item)).collect()
}

pub(crate) fn string_or_array(
    field: &Field,
    expand: &mut Expand<'_>,
) -> Result<Vec<BString>, PresetsError> {
    match field.value.as_ref() {
        Value::String(_) => Ok(vec![expand(field)?]),
        _ => strings(field, expand),
    }
}

pub(crate) fn text(field: &Field) -> Result<&BString, PresetsError> {
    match field.value.as_ref() {
        Value::String(text) => Ok(text),
        _ => Err(field.expected("a string")),
    }
}

pub(crate) fn literal(field: &Field) -> Result<BString, PresetsError> {
    text(field).cloned()
}

impl Decode for BString {
    fn decode(field: &Field, _: &mut Expand<'_>) -> Result<Self, PresetsError> {
        literal(field)
    }
}

impl Decode for bool {
    fn decode(field: &Field, _: &mut Expand<'_>) -> Result<Self, PresetsError> {
        match field.value.as_ref() {
            Value::Bool(value) => Ok(*value),
            _ => Err(field.expected("a bool")),
        }
    }
}

impl Decode for i32 {
    fn decode(field: &Field, _: &mut Expand<'_>) -> Result<Self, PresetsError> {
        match field.value.as_ref() {
            Value::Int(value) => i32::try_from(*value).ok(),
            Value::UInt(value) => i32::try_from(*value).ok(),
            _ => None,
        }
        .ok_or_else(|| field.invalid("Expected an integer"))
    }
}

impl Decode for u32 {
    fn decode(field: &Field, _: &mut Expand<'_>) -> Result<Self, PresetsError> {
        match field.value.as_ref() {
            Value::Int(value) => u32::try_from(*value).ok(),
            Value::UInt(value) => u32::try_from(*value).ok(),
            Value::Real(value)
                if value.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(value) =>
            {
                Some(*value as u32)
            }
            _ => None,
        }
        .ok_or_else(|| field.invalid("Expected a non-negative integer"))
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(field: &Field, expand: &mut Expand<'_>) -> Result<Self, PresetsError> {
        array(field)?.map(|item| T::decode(&item, expand)).collect()
    }
}

pub(crate) fn choice<T: Copy>(field: &Field, choices: &[(&str, T)]) -> Result<T, PresetsError> {
    let value = text(field)?;
    choices
        .iter()
        .find_map(|(name, choice)| (name.as_bytes() == value.as_slice()).then_some(*choice))
        .ok_or_else(|| field.invalid(format_args!("Invalid value \"{value}\"")))
}

pub(crate) fn depth(depth: usize, label: &str) -> Result<(), PresetsError> {
    if depth >= MAX_DEPTH {
        return Err(PresetsError::new(format!(
            "{label} exceeds the depth limit"
        )));
    }
    Ok(())
}

pub(crate) fn check_tree(root: &Value) -> Result<(), PresetsError> {
    let mut pending = vec![(root, 0)];
    while let Some((value, level)) = pending.pop() {
        depth(level, "Presets JSON")?;
        match value {
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, level + 1)))
            }
            Value::Array(values) => pending.extend(values.iter().map(|value| (value, level + 1))),
            _ => {}
        }
    }
    Ok(())
}
