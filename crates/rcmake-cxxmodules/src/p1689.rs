//! P1689r5 `.ddi` reader and writer.

use rcmake_base::BString;
use rcmake_json::Value;

use crate::json::{
    array, boolean, invalid, object, optional_string, parse_json, strings, text, text_array,
    write_file,
};
use crate::{LookupMethod, ModuleProvide, ModuleRequire, ModulesError, P1689File, P1689Rule};

/// Parse one P1689r5 source rule. Use `path` in diagnostics.
///
/// # Errors
/// Returns an error for invalid fields or an unsupported version.
pub fn parse_ddi(input: &[u8], path: &[u8]) -> Result<P1689File, ModulesError> {
    parse(input).map_err(|error| {
        invalid(format!(
            "-E cmake_ninja_dyndep failed to parse {}: {error}",
            String::from_utf8_lossy(path)
        ))
    })
}

fn parse(input: &[u8]) -> Result<P1689File, ModulesError> {
    let document = parse_json(input)?;
    let root = object(&document)?;
    let version = match root.get(b"version".as_slice()) {
        Some(Value::Int(0) | Value::UInt(0)) => 0,
        Some(Value::Int(1) | Value::UInt(1)) => 1,
        Some(Value::Int(version)) => return Err(invalid(format!("version {version}"))),
        Some(Value::UInt(version)) => return Err(invalid(format!("version {version}"))),
        _ => return Err(invalid("Expected an integer version")),
    };
    let [rule] = array(root.get(b"rules".as_slice()))? else {
        return Err(invalid("expected 1 source entry"));
    };
    let rule = object(rule)?;
    let primary_output = optional_string(rule.get(b"primary-output".as_slice()))?;
    let work_directory = optional_string(rule.get(b"work-directory".as_slice()))?;
    let outputs = strings(rule.get(b"outputs".as_slice()))?;
    let mut provides = Vec::new();
    for entry in array(rule.get(b"provides".as_slice()))? {
        let (module, fields) = read_module(entry)?;
        validate_logical_name(&module.logical_name, LookupMethod::ByName)?;
        provides.push(ModuleProvide {
            logical_name: module.logical_name,
            source_path: module.source_path,
            compiled_module_path: module.compiled_module_path,
            unique_on_source_path: module.unique_on_source_path,
            is_interface: boolean(fields.get(b"is-interface".as_slice()), true)?,
        });
    }
    let mut requires = Vec::new();
    for entry in array(rule.get(b"requires".as_slice()))? {
        let (mut module, fields) = read_module(entry)?;
        if let Some(method) = optional_string(fields.get(b"lookup-method".as_slice()))? {
            module.lookup_method = LookupMethod::from_name(&method)
                .ok_or_else(|| invalid(format!("Invalid lookup-method: {method}")))?;
        }
        validate_logical_name(&module.logical_name, module.lookup_method)?;
        requires.push(module);
    }
    Ok(P1689File {
        version,
        revision: 0,
        rules: vec![P1689Rule {
            primary_output,
            work_directory,
            outputs,
            provides,
            requires,
        }],
    })
}

pub(crate) fn validate_logical_name(name: &[u8], method: LookupMethod) -> Result<(), ModulesError> {
    let valid = match method {
        LookupMethod::ByName => name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':')),
        LookupMethod::IncludeAngle | LookupMethod::IncludeQuote => !name
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n' | b'"')),
    };
    if name.is_empty() || !valid {
        return Err(invalid("Invalid C++ module logical-name"));
    }
    Ok(())
}

fn read_module(entry: &Value) -> Result<(ModuleRequire, &crate::json::Object), ModulesError> {
    let fields = object(entry)?;
    let logical_name = optional_string(fields.get(b"logical-name".as_slice()))?
        .ok_or_else(|| invalid("Missing logical-name"))?;
    let source_path = optional_string(fields.get(b"source-path".as_slice()))?;
    let unique_on_source_path = boolean(fields.get(b"unique-on-source-path".as_slice()), false)?;
    if unique_on_source_path && source_path.is_none() {
        return Err(invalid("unique-on-source-path requires source-path"));
    }
    Ok((
        ModuleRequire {
            logical_name,
            source_path,
            compiled_module_path: optional_string(fields.get(b"compiled-module-path".as_slice()))?,
            unique_on_source_path,
            lookup_method: LookupMethod::ByName,
        },
        fields,
    ))
}

/// Write a P1689r5 scan result.
///
/// # Errors
/// Returns an error for an invalid version, rule count, or module entry.
pub fn write_ddi(ddi: &P1689File) -> Result<BString, ModulesError> {
    let [rule] = ddi.rules.as_slice() else {
        return Err(invalid("expected 1 source entry"));
    };
    if ddi.version > 1 {
        return Err(invalid(format!("version {}", ddi.version)));
    }
    let mut fields = crate::json::Object::new();
    for (key, value) in [
        ("primary-output", &rule.primary_output),
        ("work-directory", &rule.work_directory),
    ] {
        if let Some(value) = value {
            fields.insert(key.into(), text(value));
        }
    }
    if !rule.outputs.is_empty() {
        fields.insert("outputs".into(), text_array(&rule.outputs));
    }
    let mut provides = Vec::new();
    for module in &rule.provides {
        validate_logical_name(&module.logical_name, LookupMethod::ByName)?;
        let mut fields = module_fields(
            &module.logical_name,
            &module.source_path,
            &module.compiled_module_path,
            module.unique_on_source_path,
        )?;
        fields.insert("is-interface".into(), Value::Bool(module.is_interface));
        provides.push(Value::Object(fields));
    }
    let mut requires = Vec::new();
    for module in &rule.requires {
        validate_logical_name(&module.logical_name, module.lookup_method)?;
        let mut fields = module_fields(
            &module.logical_name,
            &module.source_path,
            &module.compiled_module_path,
            module.unique_on_source_path,
        )?;
        fields.insert(
            "lookup-method".into(),
            Value::string(module.lookup_method.as_str()),
        );
        requires.push(Value::Object(fields));
    }
    fields.insert("provides".into(), Value::Array(provides));
    fields.insert("requires".into(), Value::Array(requires));
    Ok(write_file(&crate::json::record([
        ("version", Value::UInt(ddi.version)),
        ("revision", Value::UInt(ddi.revision)),
        ("rules", Value::Array(vec![Value::Object(fields)])),
    ])))
}

fn module_fields(
    name: &BString,
    source: &Option<BString>,
    compiled: &Option<BString>,
    unique: bool,
) -> Result<crate::json::Object, ModulesError> {
    if unique && source.is_none() {
        return Err(invalid("unique-on-source-path requires source-path"));
    }
    let mut fields = crate::json::Object::from([(BString::from("logical-name"), text(name))]);
    for (key, value) in [("source-path", source), ("compiled-module-path", compiled)] {
        if let Some(value) = value {
            fields.insert(key.into(), text(value));
        }
    }
    if unique {
        fields.insert("unique-on-source-path".into(), Value::Bool(true));
    }
    Ok(fields)
}
