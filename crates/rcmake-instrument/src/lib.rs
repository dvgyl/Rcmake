//! CMake Instrumentation API v1 for Rcmake (ADR-013).
//!
//! A project or a user asks for instrumentation with query files in
//! `<build>/.cmake/instrumentation/v1/query/` (or `cmake_instrumentation()`, which writes
//! `query/generated/`). Rcmake then records one JSON snippet file for each configure,
//! compile, link, test, or other command. At each requested hook, Rcmake collects the
//! snippets into an index file and runs the query callbacks with its path.
//!
//! Modules:
//!
//! - [`query`]: read and merge query files.
//! - [`snippet`]: write snippet and index files.
//! - [`hooks`]: run indexing and callbacks at a hook.
//! - [`launcher`]: wrap a command and record its snippet (`ctest --instrument`).
//! - [`interrupt`]: catch user interrupts (`SIGINT`) around instrumented commands.
//! - [`trace`]: write the Google trace file.

#![forbid(unsafe_code)]

use rcmake_base::BString;

mod build_daemon;
mod files;
mod system;

pub mod hooks;
pub mod interrupt;
pub mod launcher;
pub mod metrics;
pub mod query;
pub mod snippet;
pub mod trace;

/// Environment variable set while `cmake --build` is active so native make-program
/// start-instrumentation hooks (`preBuild` / `postBuild`) do not fire under `cmake --build`.
pub const IN_CMAKE_BUILD_ENV: &[u8] = b"_RCMAKE_INSTRUMENT_IN_CMAKE_BUILD";

/// Return the signal from the internal instrumentation test environment.
#[doc(hidden)]
pub fn injected_interrupt(host: &dyn rcmake_host::Host) -> Option<i32> {
    host.env_get(b"__CMAKE_INSTRUMENTATION_TEST_INTERRUPT")?
        .to_string()
        .parse::<i32>()
        .ok()
        .filter(|&signal| signal > 0)
}

#[cfg(test)]
mod tests;

/// Instrumentation API version (`apiVersion` in `cmake -E capabilities`).
pub const API_VERSION: u64 = 1;

/// Newest data version that Rcmake writes (`dataVersion`).
pub const DATA_VERSION: DataVersion = DataVersion { major: 1, minor: 2 };

/// Error of an instrumentation operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InstrumentError {
    /// The query requests a data version that this implementation ignores.
    #[error("unsupported instrumentation data version")]
    UnsupportedVersion,
    /// A query or argument is not valid. The text is the CMake-style message.
    #[error("{0}")]
    Invalid(BString),
    /// A host operation failed.
    #[error(transparent)]
    Host(#[from] rcmake_host::HostError),
}

/// A data version (`{ "major": 1, "minor": 2 }`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DataVersion {
    /// Major version.
    pub major: u64,
    /// Minor version.
    pub minor: u64,
}

/// When indexing and callbacks run (`hooks` in a query file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Hook {
    /// `postGenerate`.
    PostGenerate,
    /// `preBuild`: `ninja` or `make` starts.
    PreBuild,
    /// `postBuild`: `ninja` or `make` completes.
    PostBuild,
    /// `preCMakeBuild`: `cmake --build` starts.
    PreCMakeBuild,
    /// `postCMakeBuild`: `cmake --build` completes.
    PostCMakeBuild,
    /// `postCMakeInstall`.
    PostCMakeInstall,
    /// `postCMakeWorkflow`.
    PostCMakeWorkflow,
    /// `postCTest`.
    PostCTest,
    /// `prepareForCDash`.
    PrepareForCDash,
    /// `manual`.
    Manual,
}

impl Hook {
    /// All hooks in generated query order.
    pub const ALL: [Self; 10] = [
        Self::PostGenerate,
        Self::PreBuild,
        Self::PostBuild,
        Self::PreCMakeBuild,
        Self::PostCMakeBuild,
        Self::PostCTest,
        Self::PostCMakeInstall,
        Self::PostCMakeWorkflow,
        Self::PrepareForCDash,
        Self::Manual,
    ];

    /// The hook keyword.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PostGenerate => "postGenerate",
            Self::PreBuild => "preBuild",
            Self::PostBuild => "postBuild",
            Self::PreCMakeBuild => "preCMakeBuild",
            Self::PostCMakeBuild => "postCMakeBuild",
            Self::PostCMakeInstall => "postCMakeInstall",
            Self::PostCMakeWorkflow => "postCMakeWorkflow",
            Self::PostCTest => "postCTest",
            Self::PrepareForCDash => "prepareForCDash",
            Self::Manual => "manual",
        }
    }

    /// The hook with keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|hook| hook.as_str().as_bytes() == name)
    }
}

/// Optional data collection (`options` in a query file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstrumentOption {
    /// `staticSystemInformation`.
    StaticSystemInformation,
    /// `dynamicSystemInformation`.
    DynamicSystemInformation,
    /// `captureOutput` (data version 1.1).
    CaptureOutput,
    /// `compileTrace` (data version 1.1).
    CompileTrace,
    /// `processMetrics` (data version 1.2).
    ProcessMetrics,
    /// `cdashSubmit`.
    CdashSubmit,
    /// `cdashVerbose`.
    CdashVerbose,
    /// `trace`.
    Trace,
}

impl InstrumentOption {
    /// All options in generated query order.
    pub const ALL: [Self; 8] = [
        Self::StaticSystemInformation,
        Self::DynamicSystemInformation,
        Self::CaptureOutput,
        Self::ProcessMetrics,
        Self::CompileTrace,
        Self::CdashSubmit,
        Self::CdashVerbose,
        Self::Trace,
    ];

    /// The option keyword.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaticSystemInformation => "staticSystemInformation",
            Self::DynamicSystemInformation => "dynamicSystemInformation",
            Self::CaptureOutput => "captureOutput",
            Self::CompileTrace => "compileTrace",
            Self::ProcessMetrics => "processMetrics",
            Self::CdashSubmit => "cdashSubmit",
            Self::CdashVerbose => "cdashVerbose",
            Self::Trace => "trace",
        }
    }

    /// The option with keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|option| option.as_str().as_bytes() == name)
    }
}

/// Type of a `CUSTOM_CONTENT` entry of `cmake_instrumentation()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CustomContentType {
    /// `STRING`.
    String,
    /// `BOOL`.
    Bool,
    /// `LIST`.
    List,
    /// `JSON`.
    Json,
}

impl CustomContentType {
    /// The type with keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"STRING" => Some(Self::String),
            b"BOOL" => Some(Self::Bool),
            b"LIST" => Some(Self::List),
            b"JSON" => Some(Self::Json),
            _ => None,
        }
    }
}

/// A `CUSTOM_CONTENT <name> <type> <content>` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomContent {
    /// Content name.
    pub name: BString,
    /// Content type.
    pub ty: CustomContentType,
    /// Raw content.
    pub content: BString,
}

/// One query file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// `version`: requested data version.
    pub version: DataVersion,
    /// `callbacks`: command lines that get the index file path as last argument.
    pub callbacks: Vec<BString>,
    /// `hooks`.
    pub hooks: Vec<Hook>,
    /// `options`.
    pub options: Vec<InstrumentOption>,
}

/// Role of a snippet (`role` in a snippet file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnippetRole {
    /// `configure`.
    Configure,
    /// `generate`.
    Generate,
    /// `compile`.
    Compile,
    /// `link`.
    Link,
    /// `custom`.
    Custom,
    /// `build`.
    Build,
    /// `cmakeBuild`.
    CMakeBuild,
    /// `cmakeInstall`.
    CMakeInstall,
    /// `install`.
    Install,
    /// `ctest`.
    CTest,
    /// `test`.
    Test,
    /// `workflow`.
    Workflow,
}

impl SnippetRole {
    /// The role with keyword `name`.
    #[must_use]
    pub fn from_name(name: &[u8]) -> Option<Self> {
        [
            Self::Configure,
            Self::Generate,
            Self::Compile,
            Self::Link,
            Self::Custom,
            Self::Build,
            Self::CMakeBuild,
            Self::CMakeInstall,
            Self::Install,
            Self::CTest,
            Self::Test,
            Self::Workflow,
        ]
        .into_iter()
        .find(|role| role.as_str().as_bytes() == name)
    }

    /// The role keyword.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configure => "configure",
            Self::Generate => "generate",
            Self::Compile => "compile",
            Self::Link => "link",
            Self::Custom => "custom",
            Self::Build => "build",
            Self::CMakeBuild => "cmakeBuild",
            Self::CMakeInstall => "cmakeInstall",
            Self::Install => "install",
            Self::CTest => "ctest",
            Self::Test => "test",
            Self::Workflow => "workflow",
        }
    }
}

/// One recorded command (a v1 snippet file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    /// `role`.
    pub role: SnippetRole,
    /// `command`: the command line.
    pub command: BString,
    /// `workingDir`.
    pub working_dir: BString,
    /// `result`: exit code (`None` when the command did not complete).
    pub result: Option<i64>,
    /// `timeStart`: start time in milliseconds since the Unix epoch.
    pub time_start: u64,
    /// `duration`: run time in milliseconds.
    pub duration: u64,
    /// `target`.
    pub target: Option<BString>,
    /// `outputs`.
    pub outputs: Vec<BString>,
    /// `source`.
    pub source: Option<BString>,
    /// `language`.
    pub language: Option<BString>,
    /// `testName`.
    pub test_name: Option<BString>,
    /// `config`.
    pub config: Option<BString>,
    /// `stdout` (`captureOutput`).
    pub stdout: Option<BString>,
    /// `stderr` (`captureOutput`).
    pub stderr: Option<BString>,
}

/// A v1 index file that callbacks get.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexFile {
    /// `buildDir`.
    pub build_dir: BString,
    /// `dataDir`.
    pub data_dir: BString,
    /// `hook` (`manual` when no hook triggered the indexing).
    pub hook: BString,
    /// `snippets`: snippet file paths relative to `dataDir`.
    pub snippets: Vec<BString>,
    /// `trace`: trace file path relative to `dataDir`.
    pub trace: Option<BString>,
}

fn invalid(message: impl Into<BString>) -> InstrumentError {
    InstrumentError::Invalid(message.into())
}

type Object = std::collections::BTreeMap<BString, rcmake_json::Value>;

fn version_value(version: DataVersion) -> rcmake_json::Value {
    use rcmake_json::Value;
    Value::Object(Object::from([
        ("major".into(), Value::UInt(version.major)),
        ("minor".into(), Value::UInt(version.minor)),
    ]))
}

fn strings(values: impl IntoIterator<Item = impl Into<BString>>) -> rcmake_json::Value {
    rcmake_json::Value::Array(values.into_iter().map(rcmake_json::Value::string).collect())
}
