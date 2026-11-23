//! Shared JSON field access and file formatting.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_json::{ReadOptions, Value, parse, write_compact};

use crate::ModulesError;

pub(crate) type Object = BTreeMap<BString, Value>;

pub(crate) fn parse_json(text: &[u8]) -> Result<Value, ModulesError> {
    parse(text, &ReadOptions::default()).map_err(|err| invalid(err.to_string()))
}

pub(crate) fn invalid(message: impl Into<BString>) -> ModulesError {
    ModulesError::Invalid(message.into())
}

pub(crate) fn object(value: &Value) -> Result<&Object, ModulesError> {
    match value {
        Value::Object(fields) => Ok(fields),
        _ => Err(invalid("Expected a JSON object")),
    }
}

pub(crate) fn entries(
    value: Option<&Value>,
) -> Result<impl Iterator<Item = (&BString, &Value)>, ModulesError> {
    let fields = value
        .filter(|v| **v != Value::Null)
        .map(object)
        .transpose()?;
    Ok(fields.into_iter().flatten())
}

pub(crate) fn array(value: Option<&Value>) -> Result<&[Value], ModulesError> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => Err(invalid("Expected a JSON array")),
    }
}

pub(crate) fn string(value: &Value) -> Result<BString, ModulesError> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => Err(invalid("Expected a JSON string")),
    }
}

pub(crate) fn optional_string(value: Option<&Value>) -> Result<Option<BString>, ModulesError> {
    value.filter(|v| **v != Value::Null).map(string).transpose()
}

pub(crate) fn strings(value: Option<&Value>) -> Result<Vec<BString>, ModulesError> {
    array(value)?.iter().map(string).collect()
}

pub(crate) fn boolean(value: Option<&Value>, default: bool) -> Result<bool, ModulesError> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        _ => Err(invalid("Expected a JSON boolean")),
    }
}

pub(crate) fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

pub(crate) fn text(value: &BString) -> Value {
    Value::String(value.clone())
}

pub(crate) fn text_array(values: &[BString]) -> Value {
    Value::Array(values.iter().map(text).collect())
}

pub(crate) fn write_file(value: &Value) -> BString {
    let mut out = BString::default();
    write_value(&mut out, value, 0, false);
    out
}

fn write_value(out: &mut BString, value: &Value, depth: usize, after_key: bool) {
    let (open, close) = match value {
        Value::Array(items) if !items.is_empty() => (b'[', b']'),
        Value::Object(fields) if !fields.is_empty() => (b'{', b'}'),
        _ => {
            out.extend_from_slice(&write_compact(value));
            return;
        }
    };
    if after_key {
        line(out, depth);
    }
    out.push(open);
    match value {
        Value::Object(fields) => {
            for (index, (key, child)) in fields.iter().enumerate() {
                item(out, depth, index);
                out.extend_from_slice(&write_compact(&text(key)));
                out.extend_from_slice(b" : ");
                write_value(out, child, depth + 1, true);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                item(out, depth, index);
                write_value(out, child, depth + 1, false);
            }
        }
        _ => {}
    }
    line(out, depth);
    out.push(close);
}

fn item(out: &mut BString, depth: usize, index: usize) {
    if index != 0 {
        out.push(b',');
    }
    line(out, depth + 1);
}

fn line(out: &mut BString, depth: usize) {
    out.push(b'\n');
    out.extend(std::iter::repeat_n(b'\t', depth));
}
