//! Read and write Instrumentation API v1 query files.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use rcmake_base::BString;
use rcmake_host::{Host, path};
use rcmake_json::{ReadOptions, Value, parse};

use crate::files::{absolute, api_dir, json_files, publish, read_bytes};
use crate::{
    DATA_VERSION, DataVersion, Hook, InstrumentError, InstrumentOption, Object, Query, invalid,
    strings, version_value,
};

fn integer(value: &Value) -> Option<u64> {
    match value {
        Value::Int(n) => u64::try_from(*n).ok(),
        Value::UInt(n) => Some(*n),
        _ => None,
    }
}

fn version(value: &Value) -> Result<DataVersion, InstrumentError> {
    let (major, minor) = match value {
        Value::Object(fields) => {
            let major = fields.get(b"major".as_slice()).and_then(integer);
            let minor = match fields.get(b"minor".as_slice()) {
                Some(value) => integer(value),
                None => Some(0),
            };
            (major, minor)
        }
        _ => (integer(value), Some(0)),
    };
    match (major, minor) {
        (Some(1), Some(minor)) if minor <= DATA_VERSION.minor => {
            Ok(DataVersion { major: 1, minor })
        }
        (Some(_), Some(_)) => Err(InstrumentError::UnsupportedVersion),
        _ => Err(invalid(
            "The instrumentation query version must contain integers.",
        )),
    }
}

fn entries<T>(
    root: &Object,
    key: &str,
    convert: impl Fn(&BString) -> Option<T>,
) -> Result<Vec<T>, InstrumentError> {
    match root.get(key.as_bytes()) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(name) => convert(name).ok_or_else(|| {
                    invalid(format!("Unknown instrumentation query {key} entry: {name}"))
                }),
                _ => Err(invalid(format!(
                    "Instrumentation query {key} entries must be strings."
                ))),
            })
            .collect(),
        _ => Err(invalid(format!(
            "Instrumentation query {key} must be an array."
        ))),
    }
}

/// Parse one query file.
///
/// # Errors
///
/// Returns an error for invalid JSON, invalid fields, or an unsupported data version.
pub fn parse_query(text: &[u8]) -> Result<Query, InstrumentError> {
    let Value::Object(root) =
        parse(text, &ReadOptions::default()).map_err(|error| invalid(error.to_string()))?
    else {
        return Err(invalid("An instrumentation query must be an object."));
    };
    let version = version(
        root.get(b"version".as_slice())
            .ok_or_else(|| invalid("The instrumentation query has no version."))?,
    )?;
    Ok(Query {
        version,
        callbacks: entries(&root, "callbacks", |name| Some(name.clone()))?,
        hooks: entries(&root, "hooks", |name| Hook::from_name(name))?,
        options: entries(&root, "options", |name| InstrumentOption::from_name(name))?,
    })
}

fn user_config(host: &dyn Host) -> Option<PathBuf> {
    let env = |name: &[u8]| {
        host.env_get(name)
            .filter(|value| !value.is_empty())
            .map(|value| path::to_native(&value))
    };
    env(b"CMAKE_CONFIG_DIR").or_else(|| match host.system().sysname.as_slice() {
        b"Windows" | b"Windows_NT" => env(b"LOCALAPPDATA").map(|dir| dir.join("CMake")),
        b"Darwin" | b"macOS" => env(b"XDG_CONFIG_HOME")
            .or_else(|| env(b"HOME").map(|dir| dir.join("Library/Application Support")))
            .map(|dir| dir.join("CMake")),
        _ => env(b"XDG_CONFIG_HOME")
            .or_else(|| env(b"HOME").map(|dir| dir.join(".config")))
            .map(|dir| dir.join("cmake")),
    })
}

/// Read user, project, and generated queries in that order. Ignore queries with
/// unsupported data versions.
///
/// # Errors
///
/// Returns an error if a query cannot be read or contains invalid fields.
pub fn read_queries(host: &dyn Host, build_dir: &Path) -> Result<Vec<Query>, InstrumentError> {
    let project = api_dir(&absolute(host, build_dir)).join("query");
    let mut dirs = Vec::new();
    if let Some(config) = user_config(host) {
        let config = absolute(host, &config);
        dirs.push(config.join("instrumentation/v1/query"));
        if host
            .env_get(b"CMAKE_CONFIG_DIR")
            .is_some_and(|value| !value.is_empty())
        {
            dirs.push(config.join("cmake/instrumentation/v1/query"));
        }
    }
    dirs.extend([project.clone(), project.join("generated")]);

    let mut queries = Vec::new();
    for dir in dirs {
        for name in json_files(host, &dir)? {
            let file = dir.join(path::to_native(&name));
            match parse_query(&read_bytes(host, &file)?) {
                Ok(query) => queries.push(query),
                Err(InstrumentError::UnsupportedVersion) => {}
                Err(error) => return Err(invalid(format!("{}: {error}", file.display()))),
            }
        }
    }
    Ok(queries)
}

fn generated_dir(host: &dyn Host, build_dir: &Path) -> PathBuf {
    api_dir(&absolute(host, build_dir)).join("query/generated")
}

/// Remove the previous configure step's generated queries.
///
/// # Errors
///
/// Returns an error if the host cannot remove the generated query directory.
pub fn clear_generated_queries(host: &dyn Host, build_dir: &Path) -> Result<(), InstrumentError> {
    match host.remove_dir_all(&generated_dir(host, build_dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Write the next numbered query in `query/generated/`.
///
/// # Errors
///
/// Returns an error if the version is unsupported or a host operation fails.
pub fn write_generated_query(
    host: &dyn Host,
    build_dir: &Path,
    query: &Query,
) -> Result<(), InstrumentError> {
    let data_version = version_value(query.version);
    version(&data_version)?;
    let dir = generated_dir(host, build_dir);
    let value = Value::Object(Object::from([
        ("version".into(), data_version),
        ("callbacks".into(), strings(query.callbacks.iter().cloned())),
        (
            "hooks".into(),
            strings(
                Hook::ALL
                    .into_iter()
                    .filter(|hook| query.hooks.contains(hook))
                    .map(Hook::as_str),
            ),
        ),
        (
            "options".into(),
            strings(
                InstrumentOption::ALL
                    .into_iter()
                    .filter(|opt| query.options.contains(opt))
                    .map(InstrumentOption::as_str),
            ),
        ),
    ]));
    publish(host, &dir, &value, |n| format!("query-{n}.json"))?;
    Ok(())
}
