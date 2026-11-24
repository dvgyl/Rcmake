//! Source maps, persistent call stacks, CMake 4.5.1 policy & diagnostic state,
//! byte-exact terminal rendering, and SARIF 2.1.0 emission.

#![forbid(unsafe_code)]

pub mod annotate;
pub mod backtrace;
pub mod category;
#[allow(missing_docs)]
pub mod generated_policies;
pub mod policy;
pub mod render;
pub mod source;
pub mod suggest;

use std::collections::BTreeMap;

use rcmake_base::{BString, CMAKE_VERSION_STR};
use rcmake_json::Value;

pub use backtrace::{Backtrace, Frame, FrameKind};
pub use category::{DiagAction, DiagCategory, DiagState};
pub use policy::{PolicyError, PolicyId, PolicyInfo, PolicyMap, PolicyStatus};
pub use render::{RenderOptions, format_paragraphs, render};
pub use source::{SourceFile, SourceId, SourceMap};

/// How a diagnostic renders on stderr/stdout (separate from execution [`Effect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    /// Fatal or send error (`CMake Error`).
    Error,
    /// Internal bug (`CMake Internal Error`).
    InternalError,
    /// Standard warning (`CMake Warning`).
    Warning,
    /// Author / developer warning (`CMake Warning (dev)` or `(author)`).
    AuthorWarning,
    /// Author / developer error (`CMake Error (dev)` or `(author)`).
    AuthorError,
    /// Deprecation warning (`CMake Deprecation Warning`).
    DeprecationWarning,
    /// Deprecation error (`CMake Deprecation Error`).
    DeprecationError,
    /// Informational/log message.
    Log,
}

/// Execution control effect of an issued diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Informational or warning only; execution continues and succeeds.
    None,
    /// Record an error and prevent generation, but continue processing (`SEND_ERROR`).
    SetError,
    /// Stop processing immediately (`FATAL_ERROR`).
    Fatal,
}

/// Source location attached to a diagnostic.
#[derive(Clone, Debug)]
pub enum Location {
    /// No source location (e.g., CLI argument error: `CMake Error: ...`).
    None,
    /// A single file and 1-based line without a command name.
    FileLine {
        /// File path bytes.
        file: std::sync::Arc<[u8]>,
        /// 1-based line number (`0` renders `in <file>`).
        line: u32,
    },
    /// Full persistent call stack.
    Backtrace(Backtrace),
}

/// A structured CMake diagnostic ready for terminal and SARIF rendering.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    /// Render classification.
    pub kind: MessageKind,
    /// Execution effect.
    pub effect: Effect,
    /// Optional CMake 4.4+/4.5 diagnostic category.
    pub category: Option<DiagCategory>,
    /// Optional associated policy ID (`CMPnnnn`).
    pub policy: Option<PolicyId>,
    /// Source location or call stack.
    pub location: Location,
    /// Raw, unwrapped message body.
    pub body: BString,
}

/// Top-level diagnostic configuration derived from cache, presets, and CLI flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiagOptions {
    /// Initial diagnostic action table across all categories.
    pub initial_state: DiagState,
    /// Whether to record SARIF 2.1.0 entries.
    pub sarif: bool,
}

/// Accumulates error/fatal flags and optional SARIF 2.1.0 results without touching `Host`.
#[derive(Debug)]
pub struct DiagEngine {
    options: DiagOptions,
    error_occurred: bool,
    fatal_occurred: bool,
    sarif_results: Vec<Value>,
}

impl DiagEngine {
    /// Create a new diagnostic engine with the given options.
    #[must_use]
    pub fn new(options: DiagOptions) -> Self {
        Self {
            options,
            error_occurred: false,
            fatal_occurred: false,
            sarif_results: Vec::new(),
        }
    }

    /// Issue a diagnostic, updating error/fatal state and SARIF log, and returning
    /// the rendered terminal bytes (or `None` if suppressed).
    pub fn issue(
        &mut self,
        diag: &Diagnostic,
        state: &DiagState,
        render_opts: &RenderOptions,
    ) -> Option<BString> {
        let effective = match diag.category {
            Some(cat) => match state.get(cat) {
                DiagAction::Ignore => return None,
                DiagAction::Warn => Effect::None,
                DiagAction::SendError => Effect::SetError,
                DiagAction::FatalError => Effect::Fatal,
            },
            None => diag.effect,
        };

        match effective {
            Effect::None => {}
            Effect::SetError => self.error_occurred = true,
            Effect::Fatal => {
                self.error_occurred = true;
                self.fatal_occurred = true;
            }
        }

        if self.options.sarif {
            self.record_sarif(diag, effective);
        }

        Some(render(diag, render_opts))
    }

    /// Returns `true` if any `SetError` or `Fatal` diagnostic was issued.
    #[must_use]
    pub fn error_occurred(&self) -> bool {
        self.error_occurred
    }

    /// Returns `true` if a `Fatal` diagnostic was issued.
    #[must_use]
    pub fn fatal_occurred(&self) -> bool {
        self.fatal_occurred
    }

    /// Export recorded diagnostics as a SARIF 2.1.0 JSON document.
    #[must_use]
    pub fn sarif_json(&self) -> Value {
        let mut driver = BTreeMap::new();
        driver.insert(BString::from("name"), Value::string("CMake"));
        driver.insert(BString::from("version"), Value::string(CMAKE_VERSION_STR));

        let mut tool = BTreeMap::new();
        tool.insert(BString::from("driver"), Value::Object(driver));

        let mut run = BTreeMap::new();
        run.insert(BString::from("tool"), Value::Object(tool));
        run.insert(
            BString::from("results"),
            Value::Array(self.sarif_results.clone()),
        );

        let mut root = BTreeMap::new();
        root.insert(
            BString::from("$schema"),
            Value::string("https://docs.oasis-open.org/sarif/sarif/v2.1.0/errata01/os/schemas/sarif-schema-2.1.0.json"),
        );
        root.insert(BString::from("version"), Value::string("2.1.0"));
        root.insert(
            BString::from("runs"),
            Value::Array(vec![Value::Object(run)]),
        );
        Value::Object(root)
    }

    fn record_sarif(&mut self, diag: &Diagnostic, effective: Effect) {
        let level = match effective {
            Effect::None => "warning",
            Effect::SetError | Effect::Fatal => "error",
        };
        let rule_id = match diag.category {
            Some(cat) => cat.sarif_rule_id(),
            None => match diag.kind {
                MessageKind::Error | MessageKind::InternalError => "CMake.FatalError",
                MessageKind::Warning => "CMake.Warning",
                MessageKind::AuthorWarning | MessageKind::AuthorError => "CMake.Author",
                MessageKind::DeprecationWarning | MessageKind::DeprecationError => {
                    "CMake.Deprecated"
                }
                MessageKind::Log => "CMake.Note",
            },
        };

        let mut msg = BTreeMap::new();
        msg.insert(BString::from("text"), Value::String(diag.body.clone()));

        let mut entry = BTreeMap::new();
        entry.insert(BString::from("level"), Value::string(level));
        entry.insert(BString::from("message"), Value::Object(msg));
        entry.insert(BString::from("ruleId"), Value::string(rule_id));

        self.sarif_results.push(Value::Object(entry));
    }
}

/// Summary for the CMP0224 fixture repeat policy.
pub const CMP0224_SUMMARY: &str =
    "Test fixtures use AROUND_EACH_REPEAT mode by default with ctest --repeat.";

/// Activation value for the experimental build database export.
pub const EXPORT_BUILD_DATABASE_ACTIVATION: &[u8] = b"70ef007e-b743-492d-9407-e35eeac03a40";

/// Description of the experimental build database export.
pub const EXPORT_BUILD_DATABASE_DESCRIPTION: &str =
    "CMake's support for exporting build databases is experimental.";
