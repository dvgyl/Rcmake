//! CPS 0.15.0 JSON writer for `install(PACKAGE_INFO)` and `export(PACKAGE_INFO)`.

use std::collections::BTreeMap;

use rcmake_base::BString;
use rcmake_json::{Value, write_styled};

use crate::{
    CPS_VERSION, CpsComponent, CpsConfigSupplement, CpsError, CpsPackage, CpsRequirement,
    CpsTargetFileSet, CpsUsage, validate_version,
};

type Object = BTreeMap<BString, Value>;

/// Write a root or appendix file with CMake's JSON formatting.
/// If both `cps_path` and `prefix` are absent, use `@prefix@` for `cps_path`.
///
/// # Errors
///
/// Returns an error if the package name is empty or a simple version is invalid.
pub fn write_package(package: &CpsPackage) -> Result<BString, CpsError> {
    write_package_at(package, package.cps_path.as_ref(), package.prefix.as_ref())
}

/// Write a build-tree root or appendix file. The `cps_path` is `@prefix@`.
/// Supply configuration data on each component.
///
/// # Errors
///
/// Returns an error if the package name is empty or a simple version is invalid.
pub fn write_build_package(package: &CpsPackage) -> Result<BString, CpsError> {
    write_package_at(package, None, None)
}

fn write_package_at(
    package: &CpsPackage,
    cps_path: Option<&BString>,
    prefix: Option<&BString>,
) -> Result<BString, CpsError> {
    validate_version(package)?;
    let mut root = named_object(&package.name)?;
    root.insert("cps_version".into(), Value::string(CPS_VERSION));
    string(&mut root, "cps_path", cps_path);
    string(&mut root, "prefix", prefix);
    if !root.contains_key(b"cps_path".as_slice()) && !root.contains_key(b"prefix".as_slice()) {
        root.insert("cps_path".into(), Value::string("@prefix@"));
    }
    if package
        .version
        .as_ref()
        .is_some_and(|value| !value.is_empty())
    {
        string(&mut root, "version", package.version.as_ref());
        string(&mut root, "compat_version", package.compat_version.as_ref());
        string(&mut root, "version_schema", package.version_schema.as_ref());
    }
    string(&mut root, "description", package.description.as_ref());
    string(&mut root, "website", package.website.as_ref());
    string(&mut root, "license", package.license.as_ref());
    string(
        &mut root,
        "default_license",
        package.default_license.as_ref(),
    );
    list(&mut root, "default_components", &package.default_components);
    list(&mut root, "configurations", &package.configurations);
    if !package.requires.is_empty() {
        root.insert("requires".into(), object(&package.requires, requirement));
    }
    root.insert("components".into(), object(&package.components, component));
    Ok(write_styled(&Value::Object(root), "  "))
}

/// Write a configuration supplement. The configuration keeps its original case.
/// An empty configuration uses `noconfig`.
///
/// # Errors
///
/// Returns an error if the package name is empty.
pub fn write_supplement(supplement: &CpsConfigSupplement) -> Result<BString, CpsError> {
    let mut root = named_object(&supplement.name)?;
    let config = if supplement.configuration.is_empty() {
        Value::string("noconfig")
    } else {
        Value::string(supplement.configuration.clone())
    };
    root.insert("configuration".into(), config);
    root.insert("components".into(), object(&supplement.components, usage));
    Ok(write_styled(&Value::Object(root), "  "))
}

fn named_object(name: &BString) -> Result<Object, CpsError> {
    if name.is_empty() {
        return Err(CpsError::Invalid("A CPS package must have a name.".into()));
    }
    Ok(Object::from([("name".into(), Value::string(name.clone()))]))
}

fn string(out: &mut Object, key: &str, value: Option<&BString>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        out.insert(key.into(), Value::string(value.clone()));
    }
}

fn array(values: &[BString]) -> Value {
    Value::Array(values.iter().cloned().map(Value::String).collect())
}

fn list(out: &mut Object, key: &str, values: &[BString]) {
    if !values.is_empty() {
        out.insert(key.into(), array(values));
    }
}

fn object<T>(values: &BTreeMap<BString, T>, convert: impl Fn(&T) -> Value) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(name, value)| (name.clone(), convert(value)))
            .collect(),
    )
}

fn language_lists(out: &mut Object, key: &str, values: &BTreeMap<BString, Vec<BString>>) {
    if let Some((language, items)) = values.first_key_value() {
        if values.len() == 1 && language.as_slice() == b"*" {
            list(out, key, items);
        } else {
            out.insert(key.into(), object(values, |items| array(items)));
        }
    }
}

fn extension(key: &str, value: Value) -> Value {
    Value::Object(Object::from([(
        "cmake".into(),
        Value::Object(Object::from([(key.into(), value)])),
    )]))
}

fn requirement(requirement: &CpsRequirement) -> Value {
    let mut out = Object::new();
    list(&mut out, "components", &requirement.components);
    list(&mut out, "hints", &requirement.hints);
    string(&mut out, "version", requirement.version.as_ref());
    if !requirement.domains.is_empty() {
        out.insert(
            "extensions".into(),
            extension("domains@v1", array(&requirement.domains)),
        );
    }
    Value::Object(out)
}

fn component(component: &CpsComponent) -> Value {
    let mut out = usage_fields(&component.usage);
    out.insert("type".into(), Value::string(component.ty.as_bytes()));
    string(&mut out, "license", component.license.as_ref());
    if !component.configurations.is_empty() {
        out.insert(
            "configurations".into(),
            object(&component.configurations, usage),
        );
    }
    Value::Object(out)
}

fn file_set(file_set: &CpsTargetFileSet) -> Value {
    let mut out = Object::from([
        ("type".into(), Value::string(file_set.ty.clone())),
        ("root".into(), Value::string(file_set.root.clone())),
        ("files".into(), array(&file_set.files)),
    ]);
    if let Some(name) = &file_set.name {
        out.insert(
            "extensions".into(),
            extension("name@v1", Value::string(name.clone())),
        );
    }
    Value::Object(out)
}

fn usage(value: &CpsUsage) -> Value {
    Value::Object(usage_fields(value))
}

fn usage_fields(usage: &CpsUsage) -> Object {
    let mut out = Object::new();
    string(&mut out, "location", usage.location.as_ref());
    string(&mut out, "link_location", usage.link_location.as_ref());
    string(&mut out, "link_name", usage.link_name.as_ref());
    language_lists(&mut out, "includes", &usage.includes);
    if !usage.definitions.is_empty() {
        out.insert(
            "definitions".into(),
            object(&usage.definitions, |definitions| {
                object(definitions, |value| {
                    value.clone().map_or(Value::Null, Value::String)
                })
            }),
        );
    }
    list(&mut out, "compile_features", &usage.compile_features);
    language_lists(&mut out, "compile_flags", &usage.compile_flags);
    list(&mut out, "compile_requires", &usage.compile_requires);
    list(&mut out, "link_features", &usage.link_features);
    list(&mut out, "link_directories", &usage.link_directories);
    list(&mut out, "link_flags", &usage.link_flags);
    list(&mut out, "link_languages", &usage.link_languages);
    list(&mut out, "link_libraries", &usage.link_libraries);
    list(&mut out, "link_requires", &usage.link_requires);
    list(&mut out, "dyld_requires", &usage.dyld_requires);
    list(&mut out, "requires", &usage.requires);
    if !usage.file_sets.is_empty() {
        out.insert(
            "file_sets".into(),
            Value::Array(usage.file_sets.iter().map(file_set).collect()),
        );
    }
    out
}
