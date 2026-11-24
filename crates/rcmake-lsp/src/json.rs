use rcmake_json::Value;

pub(crate) fn object<'a>(fields: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

pub(crate) fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(fields) => fields.get(key.as_bytes()),
        _ => None,
    }
}

pub(crate) fn string(value: &Value) -> Option<&str> {
    match value {
        Value::String(bytes) => std::str::from_utf8(bytes).ok(),
        _ => None,
    }
}

pub(crate) fn array(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Array(values) => Some(values),
        _ => None,
    }
}

pub(crate) fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(value) => Some(*value),
        Value::UInt(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

pub(crate) fn uint(value: usize) -> Value {
    Value::UInt(value as u64)
}
