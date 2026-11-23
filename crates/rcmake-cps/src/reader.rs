//! Read CPS documents and merge their appendices and configuration supplements.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use rcmake_base::BString;
use rcmake_json::{ReadOptions, Value, parse};

use crate::{
    CpsComponent, CpsComponentType, CpsConfigSupplement, CpsError, CpsPackage, CpsRequirement,
    CpsTargetFileSet, CpsUsage, paths::Paths, validate_version,
};

type Object = BTreeMap<BString, Value>;

const MAX_DOCUMENT_SIZE: usize = 16 * 1024 * 1024;

/// Read a root file or a self-contained appendix. Resolve paths from the CPS
/// file location and the declared `cps_path` or `prefix`. The file system is
/// not accessed.
///
/// # Errors
///
/// Returns an error for invalid JSON, missing required data, an unsupported CPS
/// version, an invalid simple version, or a prefix that cannot be determined.
/// The maximum document size is 16 MiB.
pub fn read_package(text: &[u8], path: &[u8]) -> Result<CpsPackage, CpsError> {
    with_path(path, || {
        let mut package = package_from_json(&document(text)?, true)?;
        let paths = Paths::new(&package, path)?;
        resolve_package(&mut package, &paths);
        Ok(package)
    })
}

/// Read a configuration supplement. Its paths stay as written until
/// [`merge_supplement`] resolves them with the owning package's prefix.
///
/// # Errors
///
/// Returns an error for invalid JSON, missing required data, or an unsupported
/// CPS version. The maximum document size is 16 MiB.
pub fn read_supplement(text: &[u8], path: &[u8]) -> Result<CpsConfigSupplement, CpsError> {
    with_path(path, || {
        let root = document(text)?;
        let root = as_object(&root, "document")?;
        Ok(CpsConfigSupplement {
            name: nonempty_string(root, "name")?,
            cps_version: schema_version(root, false)?,
            configuration: nonempty_string(root, "configuration")?,
            components: read_map(required_object(root, "components")?, read_usage)?,
        })
    })
}

/// Read an appendix and add its components and dependencies to the package.
/// The appendix file must be in the package directory. It inherits the prefix
/// and can supply a default license for its own components.
///
/// # Errors
///
/// Returns an error for invalid input, a different package name, a duplicate
/// component, or conflicting version or domain requirements for a dependency.
/// On error the package stays unchanged.
pub fn merge_appendix(package: &mut CpsPackage, text: &[u8], path: &[u8]) -> Result<(), CpsError> {
    with_path(path, || {
        let paths = Paths::new(package, path)?;
        let mut appendix = package_from_json(&document(text)?, false)?;
        check_name(package, &appendix.name)?;
        for name in appendix.components.keys() {
            if package.components.contains_key(name) {
                return Err(invalid(format!(
                    "Duplicate component {name:?} in appendix."
                )));
            }
        }
        for (name, dependency) in &appendix.requires {
            if let Some(existing) = package.requires.get(name)
                && (existing.version.is_some()
                    && dependency.version.is_some()
                    && existing.version != dependency.version
                    || existing.domains != dependency.domains)
            {
                return Err(invalid(format!(
                    "Conflicting requirements for {name:?} in appendix."
                )));
            }
        }

        resolve_package(&mut appendix, &paths);
        if let Some(license) = appendix.default_license {
            for component in appendix.components.values_mut() {
                if component.license.is_none() {
                    component.license = Some(license.clone());
                }
            }
        }
        package.components.append(&mut appendix.components);
        for (name, dependency) in appendix.requires {
            match package.requires.entry(name) {
                Entry::Vacant(entry) => {
                    entry.insert(dependency);
                }
                Entry::Occupied(mut entry) => {
                    let existing = entry.get_mut();
                    extend_unique(&mut existing.components, dependency.components);
                    extend_unique(&mut existing.hints, dependency.hints);
                    if existing.version.is_none() {
                        existing.version = dependency.version;
                    }
                }
            }
        }
        Ok(())
    })
}

/// Add supplement data to the named configuration. Merge appendices first if a
/// supplement refers to their components. Pass the supplement's file path in the
/// package directory. A supplement replaces scalar values and adds list entries
/// after those already in the configuration.
///
/// # Errors
///
/// Returns an error for a different package name, an unknown component, or an
/// unresolved prefix. On error the package stays unchanged.
pub fn merge_supplement(
    package: &mut CpsPackage,
    supplement: CpsConfigSupplement,
    path: &[u8],
) -> Result<(), CpsError> {
    with_path(path, || {
        check_name(package, &supplement.name)?;
        let paths = Paths::new(package, path)?;
        for name in supplement.components.keys() {
            if !package.components.contains_key(name) {
                return Err(invalid(format!(
                    "Unknown component {name:?} in supplement."
                )));
            }
        }
        let configuration = if supplement.configuration.is_empty() {
            BString::from("noconfig")
        } else {
            supplement.configuration
        };
        for (name, mut usage) in supplement.components {
            if let Some(component) = package.components.get_mut(&name) {
                resolve_usage(&mut usage, &paths);
                let key = component
                    .configurations
                    .keys()
                    .find(|name| name.eq_ignore_ascii_case(&configuration))
                    .cloned()
                    .unwrap_or_else(|| configuration.clone());
                merge_usage(component.configurations.entry(key).or_default(), usage);
            }
        }
        Ok(())
    })
}

fn extend_unique(values: &mut Vec<BString>, additions: Vec<BString>) {
    let mut seen: BTreeSet<_> = values.iter().cloned().collect();
    values.extend(
        additions
            .into_iter()
            .filter(|value| seen.insert(value.clone())),
    );
}

fn merge_usage(out: &mut CpsUsage, usage: CpsUsage) {
    let CpsUsage {
        location,
        link_location,
        link_name,
        includes,
        definitions,
        compile_features,
        compile_flags,
        compile_requires,
        link_features,
        link_directories,
        link_flags,
        link_languages,
        link_libraries,
        link_requires,
        dyld_requires,
        requires,
        file_sets,
    } = usage;
    out.location = location.or(out.location.take());
    out.link_location = link_location.or(out.link_location.take());
    out.link_name = link_name.or(out.link_name.take());
    for (language, paths) in includes {
        out.includes.entry(language).or_default().extend(paths);
    }
    for (language, values) in definitions {
        out.definitions.entry(language).or_default().extend(values);
    }
    for (language, flags) in compile_flags {
        out.compile_flags.entry(language).or_default().extend(flags);
    }
    out.compile_features.extend(compile_features);
    out.compile_requires.extend(compile_requires);
    out.link_features.extend(link_features);
    out.link_directories.extend(link_directories);
    out.link_flags.extend(link_flags);
    out.link_languages.extend(link_languages);
    out.link_libraries.extend(link_libraries);
    out.link_requires.extend(link_requires);
    out.dyld_requires.extend(dyld_requires);
    out.requires.extend(requires);
    out.file_sets.extend(file_sets);
}

fn check_name(package: &CpsPackage, name: &BString) -> Result<(), CpsError> {
    if name != &package.name {
        return Err(invalid(format!(
            "Expected package {:?}, but found {name:?}.",
            package.name
        )));
    }
    Ok(())
}

fn with_path<T>(path: &[u8], read: impl FnOnce() -> Result<T, CpsError>) -> Result<T, CpsError> {
    read().map_err(|error| invalid(format!("CPS file {:?}: {error}", BString::from(path))))
}

fn invalid(message: impl Into<BString>) -> CpsError {
    CpsError::Invalid(message.into())
}

fn document(text: &[u8]) -> Result<Value, CpsError> {
    if text.len() > MAX_DOCUMENT_SIZE {
        return Err(invalid("The document exceeds 16 MiB."));
    }
    parse(text, &ReadOptions::default()).map_err(|error| invalid(error.to_string()))
}

fn field<'a>(object: &'a Object, key: &str) -> Option<&'a Value> {
    object
        .get(key.as_bytes())
        .filter(|value| **value != Value::Null)
}

fn as_object<'a>(value: &'a Value, key: &str) -> Result<&'a Object, CpsError> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(invalid(format!("The \"{key}\" value must be an object."))),
    }
}

fn required_object<'a>(object: &'a Object, key: &str) -> Result<&'a Object, CpsError> {
    field(object, key)
        .ok_or_else(|| invalid(format!("Missing \"{key}\" object.")))
        .and_then(|value| as_object(value, key))
}

fn opt_string(object: &Object, key: &str) -> Result<Option<BString>, CpsError> {
    match field(object, key) {
        Some(Value::String(value)) => Ok(Some(value.clone())),
        None => Ok(None),
        _ => Err(invalid(format!("The \"{key}\" value must be a string."))),
    }
}

fn nonempty_string(object: &Object, key: &str) -> Result<BString, CpsError> {
    opt_string(object, key)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(format!("The \"{key}\" value must be a nonempty string.")))
}

fn as_list(value: &Value, key: &str) -> Result<Vec<BString>, CpsError> {
    let Value::Array(items) = value else {
        return Err(invalid(format!("The \"{key}\" value must be an array.")));
    };
    items
        .iter()
        .map(|item| match item {
            Value::String(item) => Ok(item.clone()),
            _ => Err(invalid(format!(
                "The \"{key}\" array must contain strings."
            ))),
        })
        .collect()
}

fn list(object: &Object, key: &str) -> Result<Vec<BString>, CpsError> {
    field(object, key).map_or_else(|| Ok(Vec::new()), |value| as_list(value, key))
}

fn read_map<T>(
    object: &Object,
    read: impl Fn(&Value) -> Result<T, CpsError>,
) -> Result<BTreeMap<BString, T>, CpsError> {
    object
        .iter()
        .map(|(key, value)| Ok((key.clone(), read(value)?)))
        .collect()
}

fn optional_map<T>(
    object: &Object,
    key: &str,
    read: impl Fn(&Value) -> Result<T, CpsError>,
) -> Result<BTreeMap<BString, T>, CpsError> {
    field(object, key).map_or_else(
        || Ok(BTreeMap::new()),
        |value| read_map(as_object(value, key)?, read),
    )
}

fn schema_version(root: &Object, required: bool) -> Result<BString, CpsError> {
    let version = match opt_string(root, "cps_version")? {
        Some(version) => version,
        None if !required => return Ok(BString::default()),
        None => return Err(invalid("Missing \"cps_version\".")),
    };
    let mut parts = version.split(|byte| *byte == b'.');
    let number = |part: &[u8]| {
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        part.iter()
            .try_fold(0_u8, |n, byte| n.checked_mul(10)?.checked_add(byte - b'0'))
    };
    if parts.next().and_then(number) != Some(0)
        || !matches!(parts.next().and_then(number), Some(13..=15))
        || !parts.all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
    {
        return Err(invalid(format!(
            "Unsupported cps_version {version:?}. Use a version from 0.13 to below 0.16."
        )));
    }
    Ok(version)
}

fn package_from_json(value: &Value, root_file: bool) -> Result<CpsPackage, CpsError> {
    let root = as_object(value, "document")?;
    let package = CpsPackage {
        name: nonempty_string(root, "name")?,
        cps_version: schema_version(root, root_file)?,
        version: opt_string(root, "version")?,
        compat_version: opt_string(root, "compat_version")?,
        version_schema: opt_string(root, "version_schema")?,
        description: opt_string(root, "description")?,
        website: opt_string(root, "website")?,
        license: opt_string(root, "license")?,
        default_license: opt_string(root, "default_license")?,
        cps_path: opt_string(root, "cps_path")?,
        prefix: opt_string(root, "prefix")?,
        configurations: list(root, "configurations")?,
        default_components: list(root, "default_components")?,
        requires: optional_map(root, "requires", read_requirement)?,
        components: read_map(required_object(root, "components")?, read_component)?,
    };
    validate_version(&package)?;
    Ok(package)
}

fn cmake_extension(object: &Object) -> Option<&Object> {
    let Value::Object(extensions) = field(object, "extensions")? else {
        return None;
    };
    let Value::Object(cmake) = field(extensions, "cmake")? else {
        return None;
    };
    Some(cmake)
}

fn read_requirement(value: &Value) -> Result<CpsRequirement, CpsError> {
    if matches!(value, Value::Null) {
        return Ok(CpsRequirement::default());
    }
    let object = as_object(value, "requires entry")?;
    Ok(CpsRequirement {
        components: list(object, "components")?,
        version: opt_string(object, "version")?,
        hints: list(object, "hints")?,
        domains: cmake_extension(object)
            .map_or_else(|| Ok(Vec::new()), |cmake| list(cmake, "domains@v1"))?,
    })
}

fn read_component(value: &Value) -> Result<CpsComponent, CpsError> {
    let object = as_object(value, "component")?;
    Ok(CpsComponent {
        ty: CpsComponentType::from_name(&nonempty_string(object, "type")?),
        usage: usage_fields(object)?,
        license: opt_string(object, "license")?,
        configurations: optional_map(object, "configurations", read_usage)?,
    })
}

fn language_lists(object: &Object, key: &str) -> Result<BTreeMap<BString, Vec<BString>>, CpsError> {
    if let Some(value @ Value::Array(_)) = field(object, key) {
        Ok(BTreeMap::from([("*".into(), as_list(value, key)?)]))
    } else {
        optional_map(object, key, |value| as_list(value, key))
    }
}

fn definitions(value: &Value) -> Result<BTreeMap<BString, Option<BString>>, CpsError> {
    read_map(
        as_object(value, "language definitions")?,
        |value| match value {
            Value::Null => Ok(None),
            Value::String(value) => Ok(Some(value.clone())),
            _ => Err(invalid("A definition value must be a string or null.")),
        },
    )
}

fn file_sets(object: &Object) -> Result<Vec<CpsTargetFileSet>, CpsError> {
    let Some(value) = field(object, "file_sets") else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(invalid("The \"file_sets\" value must be an array."));
    };
    items
        .iter()
        .map(|item| {
            let item = as_object(item, "file set")?;
            Ok(CpsTargetFileSet {
                ty: nonempty_string(item, "type")?,
                root: nonempty_string(item, "root")?,
                files: list(item, "files")?,
                name: cmake_extension(item)
                    .map_or(Ok(None), |cmake| opt_string(cmake, "name@v1"))?,
            })
        })
        .collect()
}

fn read_usage(value: &Value) -> Result<CpsUsage, CpsError> {
    usage_fields(as_object(value, "configuration data")?)
}

fn usage_fields(object: &Object) -> Result<CpsUsage, CpsError> {
    Ok(CpsUsage {
        location: opt_string(object, "location")?,
        link_location: opt_string(object, "link_location")?,
        link_name: opt_string(object, "link_name")?,
        includes: language_lists(object, "includes")?,
        definitions: optional_map(object, "definitions", definitions)?,
        compile_features: list(object, "compile_features")?,
        compile_flags: language_lists(object, "compile_flags")?,
        compile_requires: list(object, "compile_requires")?,
        link_features: list(object, "link_features")?,
        link_directories: list(object, "link_directories")?,
        link_flags: list(object, "link_flags")?,
        link_languages: list(object, "link_languages")?,
        link_libraries: list(object, "link_libraries")?,
        link_requires: list(object, "link_requires")?,
        dyld_requires: list(object, "dyld_requires")?,
        requires: list(object, "requires")?,
        file_sets: file_sets(object)?,
    })
}

fn resolve_package(package: &mut CpsPackage, paths: &Paths) {
    for component in package.components.values_mut() {
        resolve_usage(&mut component.usage, paths);
        for usage in component.configurations.values_mut() {
            resolve_usage(usage, paths);
        }
    }
    for requirement in package.requires.values_mut() {
        for hint in &mut requirement.hints {
            paths.resolve(hint);
        }
    }
}

fn resolve_usage(usage: &mut CpsUsage, paths: &Paths) {
    for path in usage
        .location
        .iter_mut()
        .chain(usage.link_location.iter_mut())
        .chain(usage.includes.values_mut().flatten())
        .chain(&mut usage.link_directories)
    {
        paths.resolve(path);
    }
    for library in &mut usage.link_libraries {
        paths.substitute(library);
    }
    for file_set in &mut usage.file_sets {
        paths.resolve(&mut file_set.root);
        for file in &mut file_set.files {
            paths.substitute(file);
        }
    }
}
