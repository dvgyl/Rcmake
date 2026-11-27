//! `CMakeFiles/rcmake/find-trace.v1.json`: persisted `find_package()` records of one
//! native configure, with the identity and stamps that decide whether they are current.

use rcmake_base::BString;
use rcmake_eval::FindTraceStore;
use rcmake_eval::find_trace::{
    CallFrame, FindCandidate, FindMode, FindOutcome, FindPackageTrace, FindRequestSummary,
    FindSelection, Rejection, SearchRoot,
};
use rcmake_json::{ParseBudget, ReadOptions, Value};

/// Schema name of the persisted file.
pub const FIND_TRACE_SCHEMA: &str = "rcmake-find-trace-v1";
/// Path of the persisted file relative to the build directory.
pub const FIND_TRACE_PATH: &str = "CMakeFiles/rcmake/find-trace.v1.json";
/// Largest file that the reader accepts.
pub const FIND_TRACE_MAX_BYTES: usize = 32 << 20;
const WRITE_BUDGET: usize = 16 << 20;

/// Size and modification time of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// Absolute path.
    pub path: BString,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

/// Contents of the persisted file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindTraceDocument {
    /// Tool that wrote the file.
    pub producer: BString,
    /// Absolute build directory.
    pub build: BString,
    /// Absolute source directory.
    pub source: BString,
    /// `CMakeCache.txt` stamp after the configure saved the cache.
    pub cache: Option<FileStamp>,
    /// Stamps of the package files that calls selected.
    pub inputs: Vec<FileStamp>,
    /// The configure ran to its end. A `false` value means later calls are unknown.
    pub completed: bool,
    /// Recorded calls.
    pub store: FindTraceStore,
}

/// Whether a saved document describes the current build tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Identity and all available stamps match.
    Current,
    /// The document is evidence from a past run.
    Past(&'static str),
}

impl FindTraceDocument {
    /// Compare the identity and stamps with the current tree.
    ///
    /// `stamp` returns the current stamp of a path, or `None` when it cannot be read.
    pub fn freshness(
        &self,
        build: &[u8],
        source: &[u8],
        mut stamp: impl FnMut(&[u8]) -> Option<FileStamp>,
    ) -> Freshness {
        if self.build != build {
            return Freshness::Past("The trace belongs to another build directory.");
        }
        if self.source != source {
            return Freshness::Past("The trace belongs to another source directory.");
        }
        let Some(cache) = &self.cache else {
            return Freshness::Past("The trace has no cache stamp.");
        };
        if stamp(&cache.path).as_ref() != Some(cache) {
            return Freshness::Past("CMakeCache.txt changed after the trace was written.");
        }
        if self
            .inputs
            .iter()
            .any(|input| stamp(&input.path).as_ref() != Some(input))
        {
            return Freshness::Past("A package file changed after the trace was written.");
        }
        Freshness::Current
    }

    /// Serialize the document. Drop calls from the end to stay within the write budget.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut document = self.clone();
        loop {
            let bytes = rcmake_json::write_compact(&document.to_value());
            if bytes.len() <= WRITE_BUDGET || document.store.calls.is_empty() {
                return bytes.into();
            }
            let keep = document.store.calls.len() / 2;
            let dropped = document.store.calls.len() - keep;
            document.store.calls.truncate(keep);
            document.store.omitted_calls = document
                .store
                .omitted_calls
                .saturating_add(u32::try_from(dropped).unwrap_or(u32::MAX));
        }
    }

    /// Parse a saved document. Return `None` for a missing, oversized, or foreign file.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let budget = ParseBudget {
            max_depth: 32,
            ..ParseBudget::artifact(FIND_TRACE_MAX_BYTES)
        };
        let value = rcmake_json::parse_bounded(bytes, &ReadOptions::default(), &budget).ok()?;
        if value.get("schema")?.as_str()? != FIND_TRACE_SCHEMA {
            return None;
        }
        let store = FindTraceStore {
            calls: value
                .get("calls")?
                .as_array()?
                .iter()
                .map(call_from)
                .collect::<Option<_>>()?,
            omitted_calls: u32::try_from(value.get("omitted_calls")?.as_u64()?).ok()?,
            bytes: 0,
        };
        Some(Self {
            producer: text(value.get("producer")?)?,
            build: text(value.get("build")?)?,
            source: text(value.get("source")?)?,
            cache: value.get("cache").and_then(stamp_from),
            inputs: value
                .get("inputs")?
                .as_array()?
                .iter()
                .map(stamp_from)
                .collect::<Option<_>>()?,
            completed: value.get("completed")?.as_bool()?,
            store,
        })
    }

    fn to_value(&self) -> Value {
        Value::object_from([
            ("schema", Value::string(FIND_TRACE_SCHEMA)),
            ("producer", Value::String(self.producer.clone())),
            ("build", Value::String(self.build.clone())),
            ("source", Value::String(self.source.clone())),
            (
                "cache",
                self.cache.as_ref().map_or(Value::Null, stamp_value),
            ),
            (
                "inputs",
                Value::array_from(self.inputs.iter().map(stamp_value)),
            ),
            ("completed", Value::Bool(self.completed)),
            (
                "omitted_calls",
                Value::UInt(u64::from(self.store.omitted_calls)),
            ),
            (
                "calls",
                Value::array_from(self.store.calls.iter().map(call_value)),
            ),
        ])
    }
}

fn stamp_value(stamp: &FileStamp) -> Value {
    Value::object_from([
        ("path", Value::String(stamp.path.clone())),
        ("size", Value::UInt(stamp.size)),
        ("mtime_ns", Value::Int(stamp.mtime_ns)),
    ])
}

fn stamp_from(value: &Value) -> Option<FileStamp> {
    Some(FileStamp {
        path: text(value.get("path")?)?,
        size: value.get("size")?.as_u64()?,
        mtime_ns: value.get("mtime_ns")?.as_i64()?,
    })
}

fn text(value: &Value) -> Option<BString> {
    value.as_bytes().map(BString::from)
}

fn opt_text(value: Option<&BString>) -> Value {
    value.map_or(Value::Null, |value| Value::String(value.clone()))
}

fn list(values: &[BString]) -> Value {
    Value::array_from(values.iter().map(|value| Value::String(value.clone())))
}

fn list_from(value: &Value) -> Option<Vec<BString>> {
    value.as_array()?.iter().map(text).collect()
}

fn opt_from(value: Option<&Value>) -> Option<Option<BString>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => text(value).map(Some),
    }
}

/// Parse the stable name of a search mode.
#[must_use]
pub(crate) fn mode_from(label: &str) -> Option<FindMode> {
    Some(match label {
        "auto" => FindMode::Auto,
        "module" => FindMode::Module,
        "config" => FindMode::Config,
        "cps" => FindMode::Cps,
        "provider" => FindMode::Provider,
        _ => return None,
    })
}

fn rejection_from(label: &str) -> Option<Rejection> {
    Some(match label {
        "no_file" => Rejection::NoFile,
        "version_mismatch" => Rejection::VersionMismatch,
        "version_unsuitable" => Rejection::VersionUnsuitable,
        "components_missing" => Rejection::ComponentsMissing,
        "ignored" => Rejection::Ignored,
        "not_found" => Rejection::NotFound,
        "error" => Rejection::Error,
        _ => return None,
    })
}

fn outcome_from(label: &str) -> Option<FindOutcome> {
    Some(match label {
        "found" => FindOutcome::Found,
        "not_found" => FindOutcome::NotFound,
        "disabled" => FindOutcome::Disabled,
        "error" => FindOutcome::Error,
        _ => return None,
    })
}

/// JSON form of one call, shared by the persisted file and `explain --find --json`.
#[must_use]
pub(crate) fn call_value(call: &FindPackageTrace) -> Value {
    let request = &call.request;
    Value::object_from([
        ("name", Value::String(call.name.clone())),
        (
            "call",
            Value::array_from(call.call.iter().map(|frame| {
                Value::object_from([
                    ("file", Value::String(frame.file.clone())),
                    ("line", Value::UInt(u64::from(frame.line))),
                    ("command", opt_text(frame.command.as_ref())),
                ])
            })),
        ),
        (
            "request",
            Value::object_from([
                (
                    "mode",
                    request
                        .mode
                        .map_or(Value::Null, |mode| Value::string(mode.label())),
                ),
                ("version", opt_text(request.version.as_ref())),
                ("exact", Value::Bool(request.exact)),
                ("components", list(&request.components)),
                ("optional_components", list(&request.optional_components)),
                ("required", Value::Bool(request.required)),
                ("quiet", Value::Bool(request.quiet)),
                ("names", list(&request.names)),
                ("config_files", list(&request.config_files)),
                ("dir_var", Value::String(request.dir_var.clone())),
                (
                    "dir_value_before",
                    opt_text(request.dir_value_before.as_ref()),
                ),
            ]),
        ),
        (
            "roots",
            Value::array_from(call.roots.iter().map(|root| {
                Value::object_from([
                    ("category", Value::String(root.category.clone())),
                    ("enabled", Value::Bool(root.enabled)),
                ])
            })),
        ),
        ("root_path_mode", Value::String(call.root_path_mode.clone())),
        (
            "candidates",
            Value::array_from(call.candidates.iter().map(|candidate| {
                Value::object_from([
                    ("path", Value::String(candidate.path.clone())),
                    ("mode", Value::string(candidate.mode.label())),
                    ("reason", Value::string(candidate.reason.label())),
                    ("found_version", opt_text(candidate.found_version.as_ref())),
                    ("missing_components", list(&candidate.missing_components)),
                    ("detail", opt_text(candidate.detail.as_ref())),
                ])
            })),
        ),
        (
            "candidates_omitted",
            Value::UInt(u64::from(call.candidates_omitted)),
        ),
        ("fields_cut", Value::UInt(u64::from(call.fields_cut))),
        (
            "selected",
            call.selected.as_ref().map_or(Value::Null, |selected| {
                Value::object_from([
                    ("file", Value::String(selected.file.clone())),
                    ("mode", Value::string(selected.mode.label())),
                    ("version", opt_text(selected.version.as_ref())),
                ])
            }),
        ),
        ("outcome", Value::string(call.outcome.label())),
        ("version", opt_text(call.version.as_ref())),
    ])
}

fn call_from(value: &Value) -> Option<FindPackageTrace> {
    let request = value.get("request")?;
    Some(FindPackageTrace {
        name: text(value.get("name")?)?,
        call: value
            .get("call")?
            .as_array()?
            .iter()
            .map(|frame| {
                Some(CallFrame {
                    file: text(frame.get("file")?)?,
                    line: u32::try_from(frame.get("line")?.as_u64()?).ok()?,
                    command: opt_from(frame.get("command"))?,
                })
            })
            .collect::<Option<_>>()?,
        request: FindRequestSummary {
            mode: match request.get("mode") {
                None | Some(Value::Null) => None,
                Some(mode) => Some(mode_from(mode.as_str()?)?),
            },
            version: opt_from(request.get("version"))?,
            exact: request.get("exact")?.as_bool()?,
            components: list_from(request.get("components")?)?,
            optional_components: list_from(request.get("optional_components")?)?,
            required: request.get("required")?.as_bool()?,
            quiet: request.get("quiet")?.as_bool()?,
            names: list_from(request.get("names")?)?,
            config_files: list_from(request.get("config_files")?)?,
            dir_var: text(request.get("dir_var")?)?,
            dir_value_before: opt_from(request.get("dir_value_before"))?,
        },
        roots: value
            .get("roots")?
            .as_array()?
            .iter()
            .map(|root| {
                Some(SearchRoot {
                    category: text(root.get("category")?)?,
                    enabled: root.get("enabled")?.as_bool()?,
                })
            })
            .collect::<Option<_>>()?,
        root_path_mode: text(value.get("root_path_mode")?)?,
        candidates: value
            .get("candidates")?
            .as_array()?
            .iter()
            .map(|candidate| {
                Some(FindCandidate {
                    path: text(candidate.get("path")?)?,
                    mode: mode_from(candidate.get("mode")?.as_str()?)?,
                    reason: rejection_from(candidate.get("reason")?.as_str()?)?,
                    found_version: opt_from(candidate.get("found_version"))?,
                    missing_components: list_from(candidate.get("missing_components")?)?,
                    detail: opt_from(candidate.get("detail"))?,
                })
            })
            .collect::<Option<_>>()?,
        candidates_omitted: u32::try_from(value.get("candidates_omitted")?.as_u64()?).ok()?,
        fields_cut: u32::try_from(value.get("fields_cut")?.as_u64()?).ok()?,
        selected: match value.get("selected") {
            None | Some(Value::Null) => None,
            Some(selected) => Some(FindSelection {
                file: text(selected.get("file")?)?,
                mode: mode_from(selected.get("mode")?.as_str()?)?,
                version: opt_from(selected.get("version"))?,
            }),
        },
        outcome: outcome_from(value.get("outcome")?.as_str()?)?,
        version: opt_from(value.get("version"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> FindTraceDocument {
        let call = FindPackageTrace {
            name: "Boost".into(),
            call: vec![CallFrame {
                file: "/src/CMakeLists.txt".into(),
                line: 12,
                command: Some("find_package".into()),
            }],
            request: FindRequestSummary {
                mode: Some(FindMode::Auto),
                version: Some("1.84".into()),
                dir_var: "Boost_DIR".into(),
                ..FindRequestSummary::default()
            },
            roots: vec![SearchRoot {
                category: "CMAKE_PATH".into(),
                enabled: true,
            }],
            root_path_mode: "BOTH".into(),
            candidates: vec![FindCandidate {
                path: "/opt/BoostConfig.cmake".into(),
                mode: FindMode::Config,
                reason: Rejection::VersionMismatch,
                found_version: Some("1.83.0".into()),
                missing_components: Vec::new(),
                detail: None,
            }],
            candidates_omitted: 3,
            fields_cut: 0,
            selected: None,
            outcome: FindOutcome::NotFound,
            version: None,
        };
        FindTraceDocument {
            producer: "rcmake 0.1.0".into(),
            build: "/b".into(),
            source: "/s".into(),
            cache: Some(FileStamp {
                path: "/b/CMakeCache.txt".into(),
                size: 10,
                mtime_ns: 5,
            }),
            inputs: Vec::new(),
            completed: false,
            store: FindTraceStore {
                calls: vec![call],
                omitted_calls: 2,
                bytes: 0,
            },
        }
    }

    #[test]
    fn round_trips_and_checks_identity_and_stamps() {
        let document = document();
        let parsed = FindTraceDocument::parse(&document.to_bytes()).expect("parse");
        assert_eq!(parsed, document);
        let current = |_: &[u8]| document.cache.clone();
        assert_eq!(parsed.freshness(b"/b", b"/s", current), Freshness::Current);
        assert!(matches!(
            parsed.freshness(b"/other", b"/s", current),
            Freshness::Past(_)
        ));
        let changed = |_: &[u8]| {
            Some(FileStamp {
                size: 11,
                ..document.cache.clone().expect("cache")
            })
        };
        assert!(matches!(
            parsed.freshness(b"/b", b"/s", changed),
            Freshness::Past(_)
        ));
    }

    #[test]
    fn rejects_foreign_schema_and_deep_files() {
        assert!(FindTraceDocument::parse(br#"{"schema":"other"}"#).is_none());
        let deep = "[".repeat(40) + &"]".repeat(40);
        assert!(FindTraceDocument::parse(deep.as_bytes()).is_none());
    }
}
