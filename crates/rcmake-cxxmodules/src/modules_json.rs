//! `CXXModules.json` reader and writer.

use rcmake_base::BString;
use rcmake_json::Value;

use crate::json::{
    boolean, entries, invalid, object, optional_string, parse_json, record, string, strings, text,
    text_array, write_file,
};
use crate::tdi::write_map;
use crate::{
    CxxModulesJson, LookupMethod, ModuleBmi, ModuleImportError, ModuleReference, ModulesError,
};

/// Parse the module table of a linked target.
///
/// # Errors
/// Returns an error for invalid JSON or invalid field types.
pub fn read_modules_json(input: &[u8]) -> Result<CxxModulesJson, ModulesError> {
    let document = parse_json(input)?;
    let root = object(&document)?;
    let mut result = CxxModulesJson::default();
    for (name, value) in entries(root.get(b"modules".as_slice()))? {
        let module = object(value)?;
        result.modules.insert(
            name.clone(),
            ModuleBmi {
                bmi: optional_string(module.get(b"bmi".as_slice()))?.unwrap_or_default(),
                is_private: boolean(module.get(b"is-private".as_slice()), false)?,
            },
        );
    }
    for (name, value) in entries(root.get(b"references".as_slice()))? {
        let reference = object(value)?;
        let lookup_method = optional_string(reference.get(b"lookup-method".as_slice()))?
            .map_or(Some(LookupMethod::ByName), |name| {
                LookupMethod::from_name(&name)
            })
            .ok_or_else(|| invalid("Invalid lookup-method"))?;
        result.references.insert(
            name.clone(),
            ModuleReference {
                path: optional_string(reference.get(b"path".as_slice()))?.unwrap_or_default(),
                lookup_method,
            },
        );
    }
    for (name, value) in entries(root.get(b"usages".as_slice()))? {
        result.usages.insert(name.clone(), strings(Some(value))?);
    }
    for (name, value) in entries(root.get(b"interface-objects".as_slice()))? {
        result
            .interface_objects
            .insert(name.clone(), string(value)?);
    }
    if let Some(Value::Object(error)) = root.get(b"import-error".as_slice()) {
        result.import_error = Some(ModuleImportError {
            message: optional_string(error.get(b"message".as_slice()))?.unwrap_or_default(),
            modules: strings(error.get(b"modules".as_slice()))?,
        });
    }
    Ok(result)
}

/// Write the module table for dependent targets.
///
/// # Errors
/// The writer currently accepts all values of `CxxModulesJson`.
pub fn write_modules_json(modules: &CxxModulesJson) -> Result<BString, ModulesError> {
    let mut root = object(&record([
        (
            "modules",
            write_map(&modules.modules, |bmi| {
                record([
                    ("bmi", text(&bmi.bmi)),
                    ("is-private", Value::Bool(bmi.is_private)),
                ])
            }),
        ),
        (
            "references",
            write_map(&modules.references, |reference| {
                record([
                    ("path", text(&reference.path)),
                    (
                        "lookup-method",
                        Value::string(reference.lookup_method.as_str()),
                    ),
                ])
            }),
        ),
        (
            "usages",
            write_map(&modules.usages, |names| text_array(names)),
        ),
        (
            "interface-objects",
            write_map(&modules.interface_objects, text),
        ),
    ]))?
    .clone();
    if let Some(error) = &modules.import_error {
        root.insert(
            "import-error".into(),
            record([
                ("message", text(&error.message)),
                ("modules", text_array(&error.modules)),
            ]),
        );
    }
    Ok(write_file(&Value::Object(root)))
}
