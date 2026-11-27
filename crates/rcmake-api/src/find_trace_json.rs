//! `CMakeFiles/rcmake/find-trace.v1.json`: persisted `find_package()` records of one
//! native configure, with the identity and stamps that decide whether they are current.

use std::collections::BTreeSet;

use rcmake_base::BString;
use rcmake_eval::FindTraceStore;
use rcmake_eval::find_trace::{
    CallFrame, FindCandidate, FindMode, FindOutcome, FindPackageTrace, FindRequestSummary,
    FindSelection, Rejection, SearchRoot, text_cost,
};
use rcmake_json::{ParseBudget, ReadOptions, Value};

/// Schema name of the persisted file.
pub const FIND_TRACE_SCHEMA: &str = "rcmake-find-trace-v1";
/// Path of the persisted file relative to the build directory.
pub const FIND_TRACE_PATH: &str = "CMakeFiles/rcmake/find-trace.v1.json";
/// Largest file that the writer produces and the reader accepts.
pub const FIND_TRACE_MAX_BYTES: usize = 16 << 20;
/// Upper bound of the JSON size of the document without its text, stamps, and calls.
const HEADER_COST: u64 = 1024;
/// Upper bound of the JSON size of one stamp without its path.
const STAMP_COST: u64 = 160;

/// State of one input path when the trace was written.
///
/// A file stamp holds the size and modification time. A directory stamp holds the
/// modification time, which changes when an entry is added or removed. An absent
/// stamp records a path that did not exist, for example an empty search location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    /// Absolute path.
    pub path: BString,
    /// The path existed.
    pub exists: bool,
    /// The path was a directory.
    pub directory: bool,
    /// Size in bytes. 0 for an absent path.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch. 0 for an absent path.
    pub mtime_ns: i64,
}

impl FileStamp {
    /// Stamp of a path that does not exist.
    #[must_use]
    pub fn absent(path: impl Into<BString>) -> Self {
        Self {
            path: path.into(),
            exists: false,
            directory: false,
            size: 0,
            mtime_ns: 0,
        }
    }

    fn cost(&self) -> u64 {
        STAMP_COST.saturating_add(text_cost(self.path.len()))
    }
}

/// Value of one search environment variable when the trace was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvStamp {
    /// Variable name.
    pub name: BString,
    /// Value, or `None` for an unset variable.
    pub value: Option<BString>,
}

impl EnvStamp {
    fn cost(&self) -> u64 {
        STAMP_COST
            .saturating_add(text_cost(self.name.len()))
            .saturating_add(
                self.value
                    .as_ref()
                    .map_or(0, |value| text_cost(value.len())),
            )
    }
}

/// Environment variables that every `find_package()` call can search, whatever the
/// package name is. `PATH` gives the `<prefix>/bin` and `<prefix>/sbin` prefixes.
pub const FIND_SEARCH_ENV: [&str; 7] = [
    "CMAKE_PREFIX_PATH",
    "CMAKE_FRAMEWORK_PATH",
    "CMAKE_APPBUNDLE_PATH",
    "CMAKE_PROGRAM_PATH",
    "CMAKE_INCLUDE_PATH",
    "CMAKE_LIBRARY_PATH",
    "PATH",
];

/// Return the search environment variables of a run that searched for `packages`.
///
/// The set holds [`FIND_SEARCH_ENV`] and, for each package `<Pkg>`, `<Pkg>_ROOT`,
/// `<PKG>_ROOT` (policy `CMP0144`), and `<Pkg>_DIR`.
#[must_use]
pub fn find_search_env_names<'a>(
    packages: impl IntoIterator<Item = &'a [u8]>,
) -> BTreeSet<BString> {
    let mut names: BTreeSet<BString> = FIND_SEARCH_ENV
        .iter()
        .map(|name| BString::from(*name))
        .collect();
    for package in packages {
        for (base, suffix) in [
            (package.to_vec(), b"_ROOT".as_slice()),
            (package.to_ascii_uppercase(), b"_ROOT"),
            (package.to_vec(), b"_DIR"),
        ] {
            let mut name = base;
            name.extend_from_slice(suffix);
            names.insert(name.into());
        }
    }
    names
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
    /// Stamps of the listfiles, the selected package files, and the searched
    /// locations of the run.
    pub inputs: Vec<FileStamp>,
    /// `inputs` holds every input. A `false` value means freshness cannot be proved.
    pub inputs_complete: bool,
    /// Search environment variables of the run. `None` means freshness cannot be
    /// proved, for example in a file from an earlier version.
    pub environment: Option<Vec<EnvStamp>>,
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
    /// `cache` is the path of the `CMakeCache.txt` of the build directory. A cache
    /// stamp for another path does not match. `stamp` returns the current stamp of a
    /// path, or `None` when the path cannot be checked. `env` returns the current
    /// value of an environment variable, or `None` when it is unset.
    pub fn freshness(
        &self,
        build: &[u8],
        source: &[u8],
        cache: &[u8],
        mut stamp: impl FnMut(&[u8]) -> Option<FileStamp>,
        mut env: impl FnMut(&[u8]) -> Option<BString>,
    ) -> Freshness {
        if self.build != build {
            return Freshness::Past("The trace belongs to another build directory.");
        }
        if self.source != source {
            return Freshness::Past("The trace belongs to another source directory.");
        }
        let Some(recorded) = &self.cache else {
            return Freshness::Past("The trace has no cache stamp.");
        };
        if recorded.path != cache {
            return Freshness::Past("The trace names another cache file.");
        }
        if stamp(cache).as_ref() != Some(recorded) || !recorded.exists {
            return Freshness::Past("CMakeCache.txt changed after the trace was written.");
        }
        if !self.inputs_complete {
            return Freshness::Past("The trace does not record every input of the run.");
        }
        if self
            .inputs
            .iter()
            .any(|input| stamp(&input.path).as_ref() != Some(input))
        {
            return Freshness::Past(
                "A listfile, package file, or search location changed after the trace was written.",
            );
        }
        let Some(environment) = &self.environment else {
            return Freshness::Past("The trace does not record the search environment.");
        };
        if environment
            .iter()
            .any(|variable| env(&variable.name) != variable.value)
        {
            return Freshness::Past(
                "A search environment variable, for example CMAKE_PREFIX_PATH, changed after the trace was written.",
            );
        }
        Freshness::Current
    }

    /// Upper bound of the serialized size of everything except the calls.
    fn header_cost(&self) -> u64 {
        let stamps = self
            .cache
            .iter()
            .chain(&self.inputs)
            .fold(0_u64, |total, stamp| total.saturating_add(stamp.cost()));
        let environment = self
            .environment
            .iter()
            .flatten()
            .fold(0_u64, |total, variable| {
                total.saturating_add(variable.cost())
            });
        HEADER_COST
            .saturating_add(environment)
            .saturating_add(text_cost(self.producer.len()))
            .saturating_add(text_cost(self.build.len()))
            .saturating_add(text_cost(self.source.len()))
            .saturating_add(stamps)
    }

    /// Serialize the document within [`FIND_TRACE_MAX_BYTES`].
    ///
    /// The size check uses upper bounds and runs before any JSON is built. Calls past
    /// the limit are dropped from the end and counted as omitted. When the stamps
    /// alone do not fit, the stamps and the environment are dropped and the inputs are
    /// marked incomplete.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let max = u64::try_from(FIND_TRACE_MAX_BYTES).unwrap_or(u64::MAX);
        let mut header = self.header_cost();
        let mut inputs: &[FileStamp] = &self.inputs;
        let mut inputs_complete = self.inputs_complete;
        let mut environment = self.environment.as_deref();
        if header > max {
            inputs = &[];
            inputs_complete = false;
            environment = None;
            header = HEADER_COST
                .saturating_add(text_cost(self.producer.len()))
                .saturating_add(text_cost(self.build.len()))
                .saturating_add(text_cost(self.source.len()))
                .saturating_add(self.cache.as_ref().map_or(0, FileStamp::cost));
        }
        let mut total = header;
        let mut keep = 0;
        for call in &self.store.calls {
            let next = total.saturating_add(call.estimated_bytes());
            if next > max {
                break;
            }
            total = next;
            keep += 1;
        }
        let calls = self.store.calls.get(..keep).unwrap_or_default();
        let dropped = u32::try_from(self.store.calls.len() - keep).unwrap_or(u32::MAX);
        let value = Value::object_from([
            ("schema", Value::string(FIND_TRACE_SCHEMA)),
            ("producer", Value::String(self.producer.clone())),
            ("build", Value::String(self.build.clone())),
            ("source", Value::String(self.source.clone())),
            (
                "cache",
                self.cache.as_ref().map_or(Value::Null, stamp_value),
            ),
            ("inputs", Value::array_from(inputs.iter().map(stamp_value))),
            ("inputs_complete", Value::Bool(inputs_complete)),
            (
                "environment",
                environment.map_or(Value::Null, |variables| {
                    Value::array_from(variables.iter().map(|variable| {
                        Value::object_from([
                            ("name", Value::String(variable.name.clone())),
                            ("value", opt_text(variable.value.as_ref())),
                        ])
                    }))
                }),
            ),
            ("completed", Value::Bool(self.completed)),
            (
                "calls_started",
                Value::UInt(u64::from(self.store.calls_started)),
            ),
            (
                "omitted_calls",
                Value::UInt(u64::from(self.store.omitted_calls.saturating_add(dropped))),
            ),
            ("calls", Value::array_from(calls.iter().map(call_value))),
        ]);
        rcmake_json::write_compact(&value).into()
    }

    /// Parse a saved document. Return `None` for an oversized, foreign, or invalid file.
    ///
    /// The calls must have increasing ordinals within `calls_started`, and the kept
    /// and omitted calls must not exceed the started calls.
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
            calls_started: u32::try_from(value.get("calls_started")?.as_u64()?).ok()?,
            omitted_calls: u32::try_from(value.get("omitted_calls")?.as_u64()?).ok()?,
            bytes: 0,
        };
        let ordered = store.calls.windows(2).all(|pair| {
            pair.first().map(|call| call.ordinal) < pair.get(1).map(|call| call.ordinal)
        });
        let in_range = store
            .calls
            .iter()
            .all(|call| (1..=store.calls_started).contains(&call.ordinal));
        let kept = u32::try_from(store.calls.len()).ok()?;
        if !ordered || !in_range || kept.checked_add(store.omitted_calls)? > store.calls_started {
            return None;
        }
        Some(Self {
            producer: text(value.get("producer")?)?,
            build: text(value.get("build")?)?,
            source: text(value.get("source")?)?,
            cache: match value.get("cache")? {
                Value::Null => None,
                cache => Some(stamp_from(cache)?),
            },
            inputs: value
                .get("inputs")?
                .as_array()?
                .iter()
                .map(stamp_from)
                .collect::<Option<_>>()?,
            inputs_complete: value.get("inputs_complete")?.as_bool()?,
            environment: match value.get("environment") {
                None | Some(Value::Null) => None,
                Some(variables) => Some(
                    variables
                        .as_array()?
                        .iter()
                        .map(|variable| {
                            Some(EnvStamp {
                                name: text(variable.get("name")?)?,
                                value: opt_from(variable.get("value"))?,
                            })
                        })
                        .collect::<Option<_>>()?,
                ),
            },
            completed: value.get("completed")?.as_bool()?,
            store,
        })
    }
}

fn stamp_value(stamp: &FileStamp) -> Value {
    Value::object_from([
        ("path", Value::String(stamp.path.clone())),
        ("exists", Value::Bool(stamp.exists)),
        ("directory", Value::Bool(stamp.directory)),
        ("size", Value::UInt(stamp.size)),
        ("mtime_ns", Value::Int(stamp.mtime_ns)),
    ])
}

fn stamp_from(value: &Value) -> Option<FileStamp> {
    Some(FileStamp {
        path: text(value.get("path")?)?,
        exists: value.get("exists")?.as_bool()?,
        directory: value.get("directory")?.as_bool()?,
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
        ("ordinal", Value::UInt(u64::from(call.ordinal))),
        (
            "package_call",
            call.package_call
                .map_or(Value::Null, |index| Value::UInt(u64::from(index))),
        ),
        ("items_omitted", Value::UInt(u64::from(call.items_omitted))),
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
        ordinal: u32::try_from(value.get("ordinal")?.as_u64()?).ok()?,
        package_call: match value.get("package_call")? {
            Value::Null => None,
            index => Some(
                u32::try_from(index.as_u64()?)
                    .ok()
                    .filter(|index| *index > 0)?,
            ),
        },
        items_omitted: u32::try_from(value.get("items_omitted")?.as_u64()?).ok()?,
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

    fn call(ordinal: u32) -> FindPackageTrace {
        FindPackageTrace {
            ordinal,
            package_call: Some(ordinal),
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
            items_omitted: 1,
            selected: None,
            outcome: FindOutcome::NotFound,
            version: None,
        }
    }

    fn stamp(path: &str, size: u64) -> FileStamp {
        FileStamp {
            path: path.into(),
            exists: true,
            directory: false,
            size,
            mtime_ns: 5,
        }
    }

    fn document() -> FindTraceDocument {
        FindTraceDocument {
            producer: "rcmake 0.1.0".into(),
            build: "/b".into(),
            source: "/s".into(),
            cache: Some(stamp("/b/CMakeCache.txt", 10)),
            inputs: vec![
                stamp("/s/CMakeLists.txt", 40),
                FileStamp::absent("/opt/empty/lib/cmake/Boost"),
            ],
            inputs_complete: true,
            environment: Some(vec![
                EnvStamp {
                    name: "CMAKE_PREFIX_PATH".into(),
                    value: None,
                },
                EnvStamp {
                    name: "Boost_ROOT".into(),
                    value: Some("/opt/boost".into()),
                },
            ]),
            completed: false,
            store: FindTraceStore {
                calls: vec![call(2)],
                calls_started: 4,
                omitted_calls: 2,
                bytes: 0,
            },
        }
    }

    fn current(document: &FindTraceDocument) -> impl FnMut(&[u8]) -> Option<FileStamp> {
        let stamps: Vec<FileStamp> = document
            .cache
            .iter()
            .chain(&document.inputs)
            .cloned()
            .collect();
        move |path: &[u8]| stamps.iter().find(|stamp| stamp.path == path).cloned()
    }

    fn env(document: &FindTraceDocument) -> impl FnMut(&[u8]) -> Option<BString> {
        let variables = document.environment.clone().unwrap_or_default();
        move |name: &[u8]| {
            variables
                .iter()
                .find(|variable| variable.name == name)
                .and_then(|variable| variable.value.clone())
        }
    }

    #[test]
    fn search_environment_changes_make_the_trace_past() {
        let document = document();
        let parsed = FindTraceDocument::parse(&document.to_bytes()).expect("parse");
        assert_eq!(parsed.environment, document.environment);
        let cache = b"/b/CMakeCache.txt";
        let mut base = env(&document);
        let prefix_set = |name: &[u8]| {
            if name == b"CMAKE_PREFIX_PATH" {
                Some(BString::from("/opt/new"))
            } else {
                base(name)
            }
        };
        assert!(matches!(
            parsed.freshness(b"/b", b"/s", cache, current(&document), prefix_set),
            Freshness::Past(_)
        ));
        let root_unset = |_: &[u8]| None;
        assert!(matches!(
            parsed.freshness(b"/b", b"/s", cache, current(&document), root_unset),
            Freshness::Past(_)
        ));
        let mut legacy = document.clone();
        legacy.environment = None;
        let reparsed = FindTraceDocument::parse(&legacy.to_bytes()).expect("parse");
        assert!(matches!(
            reparsed.freshness(b"/b", b"/s", cache, current(&document), env(&document)),
            Freshness::Past(_)
        ));
    }

    #[test]
    fn search_env_names_cover_the_package_roots() {
        let names = find_search_env_names([b"Boost".as_slice()]);
        for name in [
            "CMAKE_PREFIX_PATH",
            "PATH",
            "Boost_ROOT",
            "BOOST_ROOT",
            "Boost_DIR",
        ] {
            assert!(names.contains(&BString::from(name)), "{name}");
        }
    }

    #[test]
    fn round_trips_and_checks_identity_and_stamps() {
        let document = document();
        let parsed = FindTraceDocument::parse(&document.to_bytes()).expect("parse");
        assert_eq!(parsed, document);
        let cache = b"/b/CMakeCache.txt";
        assert_eq!(
            parsed.freshness(b"/b", b"/s", cache, current(&document), env(&document)),
            Freshness::Current
        );
        assert!(matches!(
            parsed.freshness(b"/other", b"/s", cache, current(&document), env(&document)),
            Freshness::Past(_)
        ));
        assert!(matches!(
            parsed.freshness(
                b"/b",
                b"/s",
                b"/elsewhere/CMakeCache.txt",
                current(&document),
                env(&document)
            ),
            Freshness::Past(_)
        ));
        let mut changed = current(&document);
        let listfile_edit = |path: &[u8]| {
            if path == b"/s/CMakeLists.txt" {
                Some(stamp("/s/CMakeLists.txt", 41))
            } else {
                changed(path)
            }
        };
        assert!(matches!(
            parsed.freshness(b"/b", b"/s", cache, listfile_edit, env(&document)),
            Freshness::Past(_)
        ));
        let mut base = current(&document);
        let new_package = |path: &[u8]| {
            if path == b"/opt/empty/lib/cmake/Boost" {
                Some(FileStamp {
                    directory: true,
                    ..stamp("/opt/empty/lib/cmake/Boost", 0)
                })
            } else {
                base(path)
            }
        };
        assert!(matches!(
            parsed.freshness(b"/b", b"/s", cache, new_package, env(&document)),
            Freshness::Past(_)
        ));
        let mut incomplete = document.clone();
        incomplete.inputs_complete = false;
        assert!(matches!(
            incomplete.freshness(b"/b", b"/s", cache, current(&document), env(&document)),
            Freshness::Past(_)
        ));
    }

    #[test]
    fn writer_stays_within_the_shared_limit_before_building_json() {
        let mut document = document();
        let huge = BString::from(vec![b'\x01'; 1 << 20]);
        let mut big = call(1);
        big.candidates[0].detail = Some(huge);
        let count = FIND_TRACE_MAX_BYTES / (6 << 20) + 2;
        document.store.calls = (1..=u32::try_from(count).unwrap_or(4))
            .map(|ordinal| FindPackageTrace {
                ordinal,
                ..big.clone()
            })
            .collect();
        document.store.calls_started = 20;
        document.store.omitted_calls = 0;
        let bytes = document.to_bytes();
        assert!(bytes.len() <= FIND_TRACE_MAX_BYTES);
        let parsed = FindTraceDocument::parse(&bytes).expect("parse");
        assert!(parsed.store.calls.len() < count);
        assert_eq!(
            parsed.store.calls.len() + parsed.store.omitted_calls as usize,
            count
        );
    }

    #[test]
    fn rejects_foreign_schema_deep_files_and_invalid_ordinals() {
        assert!(FindTraceDocument::parse(br#"{"schema":"other"}"#).is_none());
        let deep = "[".repeat(40) + &"]".repeat(40);
        assert!(FindTraceDocument::parse(deep.as_bytes()).is_none());
        let mut document = document();
        document.store.calls = vec![call(3), call(2)];
        assert!(FindTraceDocument::parse(&document.to_bytes()).is_none());
        document.store.calls = vec![call(9)];
        assert!(FindTraceDocument::parse(&document.to_bytes()).is_none());
        let mut bad = call(1);
        bad.outcome = FindOutcome::Found;
        let text = String::from_utf8_lossy(&{
            let mut document = self::document();
            document.store.calls = vec![bad];
            document.to_bytes()
        })
        .replace("\"found\"", "\"maybe\"");
        assert!(FindTraceDocument::parse(text.as_bytes()).is_none());
    }
}
