//! Write Instrumentation API v1 snippet, content, and index files.

use std::path::{Path, PathBuf};

use rcmake_base::{BString, list, truth};
use rcmake_host::{Follow, Host, path};
use rcmake_json::{ReadOptions, Value, parse, write_compact};

use crate::files::{
    absolute, api_dir, command_hash, newest_content, publish_at, read_json, timestamp,
};
use crate::system::{dynamic_information, static_information};
use crate::{
    CustomContent, CustomContentType, DATA_VERSION, IndexFile, InstrumentError, Object, Snippet,
    SnippetRole, invalid, strings, version_value,
};

pub use crate::system::SystemSample;

/// Resource use reported by the platform for a wrapped command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessMetrics {
    /// Peak resident memory size in KiB.
    pub max_rss: u64,
    /// CPU time in user mode in microseconds.
    pub user_time: u64,
    /// CPU time in kernel mode in microseconds.
    pub system_time: u64,
}

impl ProcessMetrics {
    fn json(self) -> Value {
        Value::Object(Object::from([
            ("maxRSS".into(), Value::UInt(self.max_rss)),
            ("userTime".into(), Value::UInt(self.user_time)),
            ("systemTime".into(), Value::UInt(self.system_time)),
        ]))
    }
}

/// Additional fields for a recorded command.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnippetDetails {
    /// Directory relative output paths are resolved against.
    pub output_base: Option<PathBuf>,
    /// Signal that interrupted the command.
    pub interrupt_signal: Option<i32>,
    /// Before and after samples of host resource use.
    pub dynamic: Option<(SystemSample, SystemSample)>,
    /// Metrics when requested. The inner value is absent if the host cannot supply them.
    pub process_metrics: Option<Option<ProcessMetrics>>,
    /// Relative path to the CMake content file.
    pub cmake_content: Option<BString>,
    /// Relative path to a saved compiler trace. The inner value is absent if no trace was produced.
    pub trace_file: Option<Option<BString>>,
    /// Labels of the associated build target.
    pub target_labels: Option<Vec<BString>>,
    /// Whether CTest ran in show-only mode.
    pub show_only: bool,
    /// Original one-based test index for test snippet identity.
    pub test_index: Option<usize>,
}

pub(crate) fn snippet_value(
    snippet: &Snippet,
    details: &SnippetDetails,
    output_sizes: Vec<Value>,
) -> Value {
    let mut fields = Object::from([
        ("version".into(), version_value(DATA_VERSION)),
        ("role".into(), Value::string(snippet.role.as_str())),
        (
            "workingDir".into(),
            Value::String(snippet.working_dir.clone()),
        ),
        (
            "result".into(),
            snippet.result.map_or(Value::Null, Value::Int),
        ),
        ("timeStart".into(), Value::UInt(snippet.time_start)),
        ("duration".into(), Value::UInt(snippet.duration)),
    ]);
    if snippet.role != SnippetRole::Build || !snippet.command.is_empty() {
        fields.insert("command".into(), Value::String(snippet.command.clone()));
    }
    for (key, value) in [
        ("target", &snippet.target),
        ("source", &snippet.source),
        ("language", &snippet.language),
        ("testName", &snippet.test_name),
        ("stdout", &snippet.stdout),
        ("stderr", &snippet.stderr),
        ("cmakeContent", &details.cmake_content),
    ] {
        if let Some(value) = value {
            fields.insert(key.into(), Value::String(value.clone()));
        }
    }
    if let Some(config) = &snippet.config {
        fields.insert("config".into(), Value::String(config.clone()));
    } else if matches!(
        snippet.role,
        SnippetRole::Compile
            | SnippetRole::Link
            | SnippetRole::Custom
            | SnippetRole::Install
            | SnippetRole::Test
    ) {
        fields.insert("config".into(), Value::string(""));
    }
    if !snippet.outputs.is_empty()
        || details.target_labels.is_some()
        || matches!(
            snippet.role,
            SnippetRole::Compile | SnippetRole::Link | SnippetRole::Custom
        )
    {
        fields.insert("outputs".into(), strings(snippet.outputs.iter().cloned()));
        fields.insert("outputSizes".into(), Value::Array(output_sizes));
    }
    if let Some(labels) = &details.target_labels {
        fields.insert("targetLabels".into(), strings(labels.iter().cloned()));
    }
    if snippet.role == SnippetRole::CTest {
        fields.insert("showOnly".into(), Value::Bool(details.show_only));
    }
    if let Some(signal) = details.interrupt_signal {
        fields.insert("interruptSignal".into(), Value::Int(i64::from(signal)));
    }
    if let Some((before, after)) = details.dynamic {
        fields.insert(
            "dynamicSystemInformation".into(),
            dynamic_information(before, after),
        );
    }
    if let Some(metrics) = details.process_metrics {
        fields.insert(
            "processMetrics".into(),
            metrics.map_or(Value::Null, ProcessMetrics::json),
        );
    }
    if let Some(trace) = &details.trace_file {
        fields.insert(
            "traceFile".into(),
            trace.clone().map_or(Value::Null, Value::String),
        );
    }
    Value::Object(fields)
}

/// Write a snippet in `data_dir` and return its absolute path.
///
/// # Errors
///
/// Returns an error if a host operation fails.
pub fn write_snippet(
    host: &dyn Host,
    data_dir: &Path,
    snippet: &Snippet,
) -> Result<PathBuf, InstrumentError> {
    let data = absolute(host, data_dir);
    let cmake_content = match snippet.role {
        SnippetRole::Configure | SnippetRole::Test => None,
        _ => newest_content(host, &data)?,
    };
    let details = SnippetDetails {
        cmake_content,
        ..SnippetDetails::default()
    };
    write_snippet_with_details(host, &data, snippet, &details)
}

/// Write a snippet with optional system samples and metadata.
///
/// # Errors
///
/// Returns an error if a host operation fails.
pub fn write_snippet_with_details(
    host: &dyn Host,
    data_dir: &Path,
    snippet: &Snippet,
    details: &SnippetDetails,
) -> Result<PathBuf, InstrumentError> {
    let base = details
        .output_base
        .clone()
        .unwrap_or_else(|| path::to_native(&snippet.working_dir));
    let output_sizes = snippet
        .outputs
        .iter()
        .map(|output| {
            let path = absolute(host, &base.join(path::to_native(output)));
            Value::UInt(
                host.metadata(&path, Follow::Yes)
                    .map_or(0, |metadata| metadata.len),
            )
        })
        .collect();
    let hash = command_hash(host, &snippet.command, details.test_index);
    let stem = format!("{}-{hash}-{}", snippet.role.as_str(), timestamp(host));
    publish_at(
        host,
        &absolute(host, data_dir),
        &stem,
        &snippet_value(snippet, details, output_sizes),
    )
}

/// A CMake target in a content file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContentTarget {
    /// Target name.
    pub name: BString,
    /// CMake target type, such as `EXECUTABLE`.
    pub kind: BString,
    /// Labels assigned to the target.
    pub labels: Vec<BString>,
}

fn custom_value(entry: &CustomContent) -> Result<Value, InstrumentError> {
    Ok(match entry.ty {
        CustomContentType::String => Value::String(entry.content.clone()),
        CustomContentType::Bool => Value::Bool(!truth::is_off(&entry.content)),
        CustomContentType::List => strings(list::split(&entry.content, list::Empty::Drop)),
        CustomContentType::Json => {
            parse(&entry.content, &ReadOptions::default()).map_err(|error| {
                invalid(format!(
                    "Invalid JSON for custom content '{}': {error}",
                    entry.name
                ))
            })?
        }
    })
}

/// Write project and custom content in `data/content/` and return its absolute path.
///
/// # Errors
///
/// Returns an error for invalid custom JSON or a failed host operation.
pub fn write_content(
    host: &dyn Host,
    build_dir: &Path,
    project: &[u8],
    entries: &[CustomContent],
    targets: &[ContentTarget],
    include_static: bool,
) -> Result<PathBuf, InstrumentError> {
    let custom = entries
        .iter()
        .map(|entry| Ok((entry.name.clone(), custom_value(entry)?)))
        .collect::<Result<Object, InstrumentError>>()?;
    let targets = targets
        .iter()
        .map(|target| {
            (
                target.name.clone(),
                Value::Object(Object::from([
                    ("type".into(), Value::String(target.kind.clone())),
                    ("labels".into(), strings(target.labels.iter().cloned())),
                ])),
            )
        })
        .collect();
    let mut value = Object::from([
        ("version".into(), version_value(DATA_VERSION)),
        ("project".into(), Value::string(project)),
        ("custom".into(), Value::Object(custom)),
        ("targets".into(), Value::Object(targets)),
    ]);
    if include_static {
        value.insert("staticSystemInformation".into(), static_information(host));
    }
    let data = api_dir(&absolute(host, build_dir)).join("data");
    let value = Value::Object(value);
    if let Some(previous) = newest_content(host, &data)? {
        let previous = data.join(path::to_native(&previous));
        if read_json(host, &previous).is_ok_and(|old| write_compact(&old) == write_compact(&value))
        {
            return Ok(previous);
        }
    }
    publish_at(
        host,
        &data.join("content"),
        &format!("cmake-{}", timestamp(host)),
        &value,
    )
}

pub(crate) fn index_value(host: &dyn Host, index: &IndexFile, include_static: bool) -> Value {
    let mut value = Object::from([
        ("version".into(), version_value(DATA_VERSION)),
        ("buildDir".into(), Value::String(index.build_dir.clone())),
        ("dataDir".into(), Value::String(index.data_dir.clone())),
        ("hook".into(), Value::String(index.hook.clone())),
        ("snippets".into(), strings(index.snippets.iter().cloned())),
    ]);
    if let Some(trace) = &index.trace {
        value.insert("trace".into(), Value::String(trace.clone()));
    }
    if include_static {
        value.insert("staticSystemInformation".into(), static_information(host));
    }
    Value::Object(value)
}

/// Write an index in `data_dir/index/` and return its absolute path.
///
/// # Errors
///
/// Returns an error if a host operation fails.
pub fn write_index(
    host: &dyn Host,
    data_dir: &Path,
    index: &IndexFile,
) -> Result<PathBuf, InstrumentError> {
    publish_at(
        host,
        &absolute(host, data_dir).join("index"),
        &format!("index-{}", timestamp(host)),
        &index_value(host, index, false),
    )
}
