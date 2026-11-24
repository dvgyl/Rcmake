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

mod source;

use source::{Location, QueryDiagnostics};

fn integer(value: &Value) -> Option<i128> {
    match value {
        Value::Int(n) => Some(i128::from(*n)),
        Value::UInt(n) => Some(i128::from(*n)),
        Value::Real(n) if n.is_finite() && n.fract() == 0.0 => Some(*n as i128),
        _ => None,
    }
}

fn version(value: &Value, errors: &mut QueryDiagnostics<'_>) -> Option<DataVersion> {
    let (major, minor) = match value {
        Value::Object(fields) => {
            let major = version_member(fields, "major", errors);
            let minor = version_member(fields, "minor", errors);
            for key in fields
                .keys()
                .filter(|key| !matches!(key.as_slice(), b"major" | b"minor"))
            {
                errors.add(
                    Location::VersionMember(key),
                    format!("Invalid extra field \"{key}\" in version object"),
                );
            }
            (major?, minor?)
        }
        _ => match integer(value) {
            Some(major) => (major, 0),
            None => {
                errors.add(
                    Location::Field(b"version"),
                    "Version must be an integer or object",
                );
                return None;
            }
        },
    };
    (major == 1 && (0..=i128::from(DATA_VERSION.minor)).contains(&minor)).then(|| DataVersion {
        major: 1,
        minor: u64::try_from(minor).unwrap_or_default(),
    })
}

fn version_member(
    fields: &Object,
    key: &'static str,
    errors: &mut QueryDiagnostics<'_>,
) -> Option<i128> {
    match fields.get(key.as_bytes()) {
        None if key == "minor" => Some(0),
        None => {
            errors.add(
                Location::Field(b"version"),
                "Missing required field \"major\" in version object",
            );
            None
        }
        Some(value) => integer(value).or_else(|| {
            let mut message = format!("\"{key}\" expected an integer");
            if let Some(value) = scalar_text(value) {
                message.push_str(&format!(", got: {value}"));
            }
            errors.add(Location::VersionMember(key.as_bytes()), message);
            None
        }),
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::Object(_) | Value::Array(_) => None,
        Value::String(text) => Some(text.to_string()),
        Value::Null => Some(String::new()),
        _ => Some(String::from_utf8_lossy(&rcmake_json::write_compact(value)).into_owned()),
    }
}

fn entries<T>(
    root: &Object,
    key: &'static [u8],
    errors: &mut QueryDiagnostics<'_>,
    convert: impl Fn(&BString) -> Option<T>,
    invalid_entry: impl Fn(usize, &Value) -> String,
) -> Vec<T> {
    match root.get(key) {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                if let Value::String(name) = value
                    && let Some(entry) = convert(name)
                {
                    return Some(entry);
                }
                errors.add(Location::Item(key, index), invalid_entry(index, value));
                None
            })
            .collect(),
        Some(value) => {
            let location = if matches!(value, Value::Null) {
                Location::Absent
            } else {
                Location::Field(key)
            };
            errors.add(location, "Invalid Array");
            Vec::new()
        }
    }
}

fn invalid_keyword(kind: &str, value: &Value) -> String {
    scalar_text(value).map_or_else(
        || format!("Expected a string for {kind}"),
        |value| format!("Not a valid {kind}: \"{value}\""),
    )
}

fn parse_query_file(text: &[u8], filename: &str) -> Result<Query, InstrumentError> {
    let text = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
    let mut errors = QueryDiagnostics::new(text, filename);
    let root = match parse(text, &ReadOptions::default()) {
        Ok(Value::Object(root)) => root,
        Ok(_) => {
            errors.add(Location::Root, "Invalid root object");
            return Err(errors.into_error());
        }
        Err(error) => {
            errors.add_line(error.line as usize, error.column as usize, &error.message);
            return Err(errors.into_error());
        }
    };
    let version = if let Some(value) = root.get(b"version".as_slice()) {
        version(value, &mut errors)
    } else {
        errors.add(
            Location::Root,
            "Missing required field \"version\" in root object",
        );
        None
    };
    if version.is_none() && errors.is_empty() {
        return Err(InstrumentError::UnsupportedVersion);
    }
    let options = entries(
        &root,
        b"options",
        &mut errors,
        |name| InstrumentOption::from_name(name),
        |_, value| invalid_keyword("option", value),
    );
    let hooks = entries(
        &root,
        b"hooks",
        &mut errors,
        |name| Hook::from_name(name),
        |_, value| invalid_keyword("hook", value),
    );
    let callbacks = entries(
        &root,
        b"callbacks",
        &mut errors,
        |name| Some(name.clone()),
        |index, value| {
            let mut message = format!("\"$vector_item_{index}\" expected a string");
            if let Some(value) = scalar_text(value) {
                message.push_str(&format!(", got: {value}"));
            }
            message
        },
    );
    for key in root.keys().filter(|key| {
        !matches!(
            key.as_slice(),
            b"version" | b"options" | b"hooks" | b"callbacks"
        )
    }) {
        errors.add(
            Location::Field(key),
            format!("Invalid extra field \"{key}\" in root object"),
        );
    }
    match version {
        Some(version) if errors.is_empty() => Ok(Query {
            version,
            options,
            hooks,
            callbacks,
        }),
        _ => Err(errors.into_error()),
    }
}

/// Parse one query file.
///
/// # Errors
///
/// Returns an error for invalid JSON, invalid fields, or an unsupported data version.
pub fn parse_query(text: &[u8]) -> Result<Query, InstrumentError> {
    parse_query_file(text, "")
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
        dirs.push(absolute(host, &config).join("instrumentation/v1/query"));
    }
    dirs.extend([project.clone(), project.join("generated")]);

    let mut queries = Vec::new();
    for dir in dirs {
        let mut errors = Vec::new();
        for name in json_files(host, &dir)? {
            let file = dir.join(path::to_native(&name));
            match parse_query_file(&read_bytes(host, &file)?, &name.to_string()) {
                Ok(mut query) => {
                    if query.options.iter().any(|option| {
                        matches!(
                            option,
                            InstrumentOption::CdashSubmit | InstrumentOption::CdashVerbose
                        )
                    }) {
                        if !query
                            .options
                            .contains(&InstrumentOption::DynamicSystemInformation)
                        {
                            query
                                .options
                                .push(InstrumentOption::DynamicSystemInformation);
                        }
                        if !query.hooks.contains(&Hook::PrepareForCDash) {
                            query.hooks.push(Hook::PrepareForCDash);
                        }
                    }
                    queries.push(query);
                }
                Err(InstrumentError::UnsupportedVersion) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }
        if !errors.is_empty() {
            return Err(invalid(format!(
                "Could not load instrumentation queries from {}:\n{}",
                path::from_native_cmake(&dir),
                errors.join("\n")
            )));
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
    if query.version.major != DATA_VERSION.major || query.version.minor > DATA_VERSION.minor {
        return Err(InstrumentError::UnsupportedVersion);
    }
    let data_version = version_value(query.version);
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
