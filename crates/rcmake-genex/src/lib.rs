//! Generator expression (`$<...>`) engine for Rcmake (ADR-006).
//!
//! This crate depends only on `rcmake-base`, `rcmake-diag`, and `rcmake-regex`. It
//! reads targets through the [`GenexHost`] trait, which `rcmake-resolve` implements.
//! This breaks the recursion between `$<TARGET_PROPERTY:...>` and transitive usage
//! resolution without a crate cycle.
//!
#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_diag::{Backtrace, Diagnostic, Effect, Frame, Location, MessageKind, RenderOptions};

mod eval;
mod parse;
mod preprocess;

pub use eval::{
    GenexStep, GenexTrace, SkippedOperand, evaluate, evaluate_diagnostics, evaluate_traced,
    evaluate_with_target_lookups,
};
pub use parse::{ParseCache, parse, render};
pub use preprocess::{PreprocessMode, find_genex, genex_strip, rewrite_install_prefix, strip};

/// Maximum nesting depth of `$<...>` that [`parse`] accepts.
///
/// The parser recurses once for each level between the stack guard checks of the
/// evaluator, so the limit matches the 256-level limit of variable expansion.
pub const MAX_NESTING: usize = 256;

/// A build configuration name (`$<CONFIG>`). The empty name means "no configuration".
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ConfigId(pub BString);

impl ConfigId {
    /// `true` if `name` names this configuration (`$<CONFIG:cfg>` compares
    /// case-insensitively).
    #[must_use]
    pub fn matches(&self, name: &[u8]) -> bool {
        self.0.eq_ignore_ascii_case(name)
    }
}

/// Opaque target handle that a [`GenexHost`] hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetHandle(pub u32);

/// What the evaluation result depends on. `rcmake-resolve` adds the flagged parts of
/// the context to its memoization key (ADR-006 item 4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EvalFlags {
    /// The result depends on the consuming (`head`) target.
    pub head_sensitive: bool,
    /// The result depends on the current target or other evaluation context.
    pub context_sensitive: bool,
    /// The result depends on the link language (`$<LINK_LANGUAGE>`).
    pub link_language_sensitive: bool,
}

impl EvalFlags {
    /// Union of two flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            head_sensitive: self.head_sensitive || other.head_sensitive,
            context_sensitive: self.context_sensitive || other.context_sensitive,
            link_language_sensitive: self.link_language_sensitive || other.link_language_sensitive,
        }
    }
}

/// Which usage requirements a transitive walk follows (ADR-007 item 6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum UsageTraversal {
    /// Compile usage: skips `$<LINK_ONLY:...>` items.
    #[default]
    Compile,
    /// Link usage: includes `$<LINK_ONLY:...>` items.
    Link,
}

/// Where an expression comes from, for diagnostics.
#[derive(Clone, Debug, Default)]
pub struct EvalSite {
    /// Backtrace of the command or property that holds the expression.
    pub backtrace: Backtrace,
    /// Property name when the expression is a property value.
    pub property: Option<BString>,
}

/// Evaluation context for generator expressions.
#[derive(Clone, Debug, Default)]
pub struct EvalContext {
    /// Configuration being generated.
    pub config: ConfigId,
    /// Consuming target (`head`), if any.
    pub head: Option<TargetHandle>,
    /// Target whose property is being evaluated, if any.
    pub current: Option<TargetHandle>,
    /// Compile language (`$<COMPILE_LANGUAGE>`), if known.
    pub language: Option<BString>,
    /// Link language (`$<LINK_LANGUAGE>`), if known.
    pub link_language: Option<BString>,
    /// The current evaluation supplies inputs to the device linker.
    pub device_link: bool,
    /// `Ninja Multi-Config` command configuration (`$<COMMAND_CONFIG:...>`).
    pub command_config: Option<ConfigId>,
    /// `Ninja Multi-Config` output configuration (`$<OUTPUT_CONFIG:...>`).
    pub output_config: Option<ConfigId>,
    /// Usage walk kind for `$<TARGET_PROPERTY:...>` on `INTERFACE_*` properties.
    pub traversal: UsageTraversal,
    /// Suppress diagnostics during evaluation.
    pub quiet: bool,
    /// Origin of the expression.
    pub site: EvalSite,
}

/// A transitive property value as seen by a consumer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InterfaceView {
    /// Evaluated list value.
    pub value: BString,
    /// What the value depends on.
    pub flags: EvalFlags,
}

/// Target artifact that `$<TARGET_FILE...>` and related operators name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactKind {
    /// `TARGET_FILE`.
    File,
    /// `TARGET_LINKER_FILE`.
    LinkerFile,
    /// `TARGET_LINKER_LIBRARY_FILE`.
    LinkerLibraryFile,
    /// `TARGET_LINKER_IMPORT_FILE`.
    LinkerImportFile,
    /// `TARGET_SONAME_FILE`.
    SonameFile,
    /// `TARGET_SONAME_IMPORT_FILE`.
    SonameImportFile,
    /// `TARGET_PDB_FILE`.
    PdbFile,
    /// `TARGET_BUNDLE_DIR`.
    BundleDir,
    /// `TARGET_BUNDLE_CONTENT_DIR`.
    BundleContentDir,
    /// `TARGET_IMPORT_FILE`.
    ImportFile,
}

/// Part of an artifact path (`_DIR`, `_NAME`, `_BASE_NAME`, `_PREFIX`, `_SUFFIX`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ArtifactPart {
    /// The full path.
    #[default]
    Full,
    /// `_DIR`.
    Dir,
    /// `_NAME`.
    Name,
    /// `_BASE_NAME`.
    BaseName,
    /// `_PREFIX`.
    Prefix,
    /// `_SUFFIX`.
    Suffix,
}

/// A property of a custom rule, read by `$<RULE_PROPERTY:rule,prop>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleProperty {
    /// The `NAME` property of the rule, for diagnostics.
    pub rule_name: BString,
    /// The raw value of the property. The value can contain generator
    /// expressions.
    pub value: Option<BString>,
}

/// Target and build-system data that evaluation needs (implemented by
/// `rcmake-resolve`).
pub trait GenexHost {
    /// Target named `name` as seen from the evaluation's directory.
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle>;

    /// Find a target in the lookup scope of the evaluation context.
    fn find_target_in_context(&self, name: &[u8], _ctx: &EvalContext) -> Option<TargetHandle> {
        self.find_target(name)
    }

    /// Record a generator expression read of a target's `SOURCES` property.
    fn read_target_sources(&mut self, _target: TargetHandle) {}

    /// Name of `target`.
    fn target_name(&self, target: TargetHandle) -> BString;

    /// Read a property that belongs to an alias name.
    fn alias_property(
        &self,
        _name: &[u8],
        _property: &[u8],
        _ctx: &EvalContext,
    ) -> Option<BString> {
        None
    }

    /// Read a source property in the selected directory.
    fn source_property(
        &self,
        _source: &[u8],
        _scope: Option<(&[u8], &[u8])>,
        _property: &[u8],
        _ctx: &EvalContext,
    ) -> Result<BString, BString> {
        Ok(BString::default())
    }

    /// Check whether a source is registered in the selected directory.
    fn source_exists(
        &self,
        _source: &[u8],
        _scope: Option<(&[u8], &[u8])>,
        _ctx: &EvalContext,
    ) -> Result<bool, BString> {
        Ok(false)
    }

    /// Check whether the target has a file set with this name.
    fn file_set_exists(&self, _target: TargetHandle, _name: &[u8]) -> bool {
        false
    }

    /// Read a raw property from an existing file set.
    fn file_set_property(
        &self,
        _target: TargetHandle,
        _name: &[u8],
        _property: &[u8],
    ) -> Option<BString> {
        None
    }

    /// Read a global property.
    fn global_property(&self, _name: &[u8]) -> Option<BString> {
        None
    }

    /// Read a property from a directory. An error means that the directory is unknown.
    fn directory_property(
        &self,
        _directory: &[u8],
        _property: &[u8],
        _ctx: &EvalContext,
    ) -> Result<Option<BString>, BString> {
        Ok(None)
    }

    /// Read a property of a custom rule that is visible in the evaluation
    /// directory. Return `None` if the rule is unknown.
    fn rule_property(&self, _rule: &[u8], _property: &[u8]) -> Option<RuleProperty> {
        None
    }

    /// Return the policy state in the evaluation directory, if known.
    fn directory_policy(&self, _policy: &[u8], _ctx: &EvalContext) -> Option<bool> {
        None
    }

    /// Return the full policy status for a compatibility check.
    fn directory_policy_status(
        &self,
        policy: rcmake_diag::PolicyId,
        ctx: &EvalContext,
    ) -> Option<rcmake_diag::PolicyStatus> {
        self.directory_policy(policy.to_string().as_bytes(), ctx)
            .map(|new| {
                if new {
                    rcmake_diag::PolicyStatus::New
                } else {
                    rcmake_diag::PolicyStatus::Old
                }
            })
    }

    /// Report a policy warning at the expression's call site.
    fn policy_warning(
        &mut self,
        _policy: rcmake_diag::PolicyId,
        _detail: &[u8],
        _ctx: &EvalContext,
    ) {
    }

    /// Raw (unevaluated) value of a non-transitive property.
    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString>;

    /// Value of a scalar property in an expression, with any compatible interface requirements.
    fn scalar_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        _ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        Ok(InterfaceView {
            value: self.raw_property(target, name).unwrap_or_default(),
            flags: EvalFlags::default(),
        })
    }

    /// Evaluated transitive property (`INTERFACE_*` closure) of `target` for the
    /// consumer in `ctx`.
    fn transitive_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError>;

    /// Artifact path part of `target` in configuration `config`.
    fn artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString>;

    /// Get an artifact part, with errors from output property evaluation.
    ///
    /// # Errors
    /// Returns an error if a property that names the artifact cannot be evaluated.
    fn checked_artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Result<Option<BString>, GenexError> {
        Ok(self.artifact(target, kind, part, config))
    }

    /// Targets in the link closure of `target` for the consumer in `ctx`.
    fn link_closure(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError>;

    /// Variable value of the evaluation directory (`CMAKE_<LANG>_COMPILER_ID`, and so
    /// on).
    fn definition(&self, name: &[u8]) -> Option<BString>;

    /// Report whether a language is enabled when that information is available.
    fn language_enabled(&self, _language: &[u8]) -> Option<bool> {
        None
    }

    /// Return the configurations that can match a CONFIG test on an imported target.
    fn imported_config_matches(
        &self,
        _target: TargetHandle,
        _head: Option<TargetHandle>,
        _consumer: &ConfigId,
    ) -> Option<Vec<BString>> {
        None
    }

    /// Return the configurations that can match a CONFIG test on an imported target,
    /// with full evaluation context for policy warnings.
    fn imported_config_matches_ctx(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Option<Vec<BString>> {
        self.imported_config_matches(target, ctx.head, &ctx.config)
    }

    /// Return the policy state at target creation.
    fn target_policy(&self, target: TargetHandle, policy: &[u8]) -> Option<bool> {
        self.raw_property(target, policy)
            .map(|value| value == "NEW")
    }

    /// Return object paths for the target and the optional source filter.
    fn target_objects(
        &self,
        target: TargetHandle,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        let _ = (config, sources);
        self.raw_property(target, b"TARGET_OBJECTS")
    }

    /// Directory that contains the target's intermediate build files.
    fn target_intermediate_directory(
        &self,
        _target: TargetHandle,
        _config: &ConfigId,
    ) -> Option<BString> {
        None
    }

    /// Return the artifact base name without the configuration postfix.
    fn artifact_base_name_without_postfix(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        config: &ConfigId,
    ) -> Option<BString> {
        let mut path = self.artifact(target, kind, ArtifactPart::BaseName, config)?;
        let mut property = config.0.to_ascii_uppercase();
        property.extend_from_slice(b"_POSTFIX");
        if let Some(postfix) = self.raw_property(target, &property)
            && path.ends_with(&postfix)
        {
            let length = path.len() - postfix.len();
            path.truncate(length);
        }
        Some(path)
    }

    /// Return whether the current evaluation is for a device link step.
    fn is_device_link(&self, ctx: &EvalContext) -> bool {
        ctx.device_link
    }

    /// Return the install prefix when this context permits its evaluation.
    fn install_prefix(&self, _ctx: &EvalContext) -> Option<BString> {
        None
    }

    /// Return the timestamp in the requested format and time zone.
    fn timestamp(&self, _format: Option<&[u8]>, _utc: bool) -> Option<BString> {
        None
    }

    /// Return a seed for an unseeded random string expression.
    fn random_seed(&self) -> Option<u64> {
        None
    }

    /// Report a warning from evaluation, for example an invalid configuration name that
    /// CMake accepts for compatibility. The body is the complete message text.
    fn warning(&mut self, _body: BString, _ctx: &EvalContext) {}

    /// Return the execution limits for generator expressions.
    fn limits(&self) -> rcmake_base::Limits {
        rcmake_base::Limits::default()
    }
}

/// A parsed generator expression string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenexAst {
    /// Top-level nodes in order.
    pub nodes: Vec<GenexNode>,
}

impl GenexAst {
    /// `true` if the input has no `$<...>` expression.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.nodes.iter().all(|n| matches!(n, GenexNode::Text(_)))
    }
}

/// One node of a [`GenexAst`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenexNode {
    /// Literal text, including recovered unmatched delimiters.
    Text(BString),
    /// A complete `$<...>` expression.
    Expr(GenexExpr),
}

/// A complete `$<identifier[:params]>` expression.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenexExpr {
    /// Operator name content (can hold nested expressions).
    pub identifier: Vec<GenexNode>,
    /// Parameters split at every top-level comma. `None` when there is no `:`. The
    /// evaluator joins extra parameters back with `,` for operators that accept
    /// arbitrary content (`$<UPPER_CASE:a,b:c>`).
    pub params: Option<Vec<Vec<GenexNode>>>,
}

/// Generator expression evaluation failure, including cycle and operator errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GenexError {
    /// Operator error with CMake's message text, for example `$<BOOL> expression
    /// requires exactly one parameter.`.
    #[error("{message}")]
    Message {
        /// The full expression text.
        expression: BString,
        /// CMake's message text.
        message: BString,
    },
    /// `Self reference on target "<t>".` from the cycle checker.
    #[error("Self reference on target \"{target}\".")]
    SelfReference {
        /// Target name.
        target: BString,
    },
    /// An indirect generator expression cycle.
    #[error("Dependency loop found.")]
    DependencyLoop {
        /// Expression where the loop was detected.
        expression: BString,
        /// Active expressions from the most recent to the least recent.
        steps: Vec<BString>,
    },
    /// An output property needs its own value to compute the artifact path.
    #[error("Target '{target}' {property} depends on itself.")]
    ArtifactCycle {
        /// Target name.
        target: BString,
        /// Output property class.
        property: &'static str,
    },
    /// `$<TARGET_RUNTIME_DLLS>` needs the link closure of a target that is still being
    /// computed. CMake reports this at the call that created the target.
    #[error(
        "The SOURCES of \"{target}\" use a generator expression that depends on the SOURCES themselves."
    )]
    SourcesCycle {
        /// Name of the target whose sources depend on themselves.
        target: BString,
    },
    /// Generation error reported by an operator without expression text.
    #[error("{message}")]
    Generation {
        /// CMake's message text.
        message: BString,
    },
    /// A generator expression failure with its originating call stack.
    #[error("{0}")]
    Located(Box<GenexDiagnostic>),
    /// Nesting is deeper than [`MAX_NESTING`].
    #[error("generator expression nesting is too deep")]
    TooDeep,
}

/// Source location and message of a generator expression failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenexDiagnostic {
    frames: Vec<Frame>,
    body: BString,
    steps: Vec<BString>,
    source_root: BString,
}

impl std::fmt::Display for GenexDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backtrace = self
            .frames
            .iter()
            .rev()
            .fold(Backtrace::default(), |backtrace, frame| {
                backtrace.push(frame.clone())
            });
        let render = |body: BString| {
            if backtrace.is_empty() {
                let mut output = BString::from("CMake Error:\n");
                output.extend_from_slice(&rcmake_diag::format_paragraphs(
                    &body,
                    2,
                    RenderOptions::default().wrap_width,
                ));
                output.extend_from_slice(b"\n\n");
                return output;
            }
            rcmake_diag::render(
                &Diagnostic {
                    annotations: None,
                    kind: MessageKind::Error,
                    effect: Effect::SetError,
                    category: None,
                    policy: None,
                    location: Location::Backtrace(backtrace.clone()),
                    body,
                },
                &RenderOptions {
                    relative_base: Some(self.source_root.clone()),
                    ..RenderOptions::default()
                },
            )
        };
        f.write_str(&String::from_utf8_lossy(&render(self.body.clone())))?;
        for (index, step) in self.steps.iter().enumerate() {
            f.write_str(&String::from_utf8_lossy(&render(
                format!("Loop step {}\n  {step}\n", index + 1).into(),
            )))?;
        }
        Ok(())
    }
}

/// Return the frames of a backtrace inside the directory of its first frame.
///
/// A CMake call stack ends at the `CMakeLists.txt` file of the directory.
/// It does not show the `add_subdirectory` call of the parent directory.
fn directory_frames(backtrace: &Backtrace) -> Vec<Frame> {
    backtrace
        .frames()
        .filter(|frame| frame.line != 0 || frame.name.is_some())
        .take_while(|frame| {
            !frame
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(b"add_subdirectory"))
        })
        .cloned()
        .collect()
}

impl GenexError {
    /// Add source context to an error that does not already have it.
    #[must_use]
    pub fn at(self, backtrace: &Backtrace, source_root: &[u8]) -> Self {
        if backtrace.is_empty() || matches!(self, Self::Located(_)) {
            return self;
        }
        self.with_site(backtrace, source_root)
    }

    /// Report the error at the expression's call site.
    #[must_use]
    pub fn at_call_site(self, backtrace: &Backtrace, source_root: &[u8]) -> Self {
        match self {
            Self::Located(mut diagnostic) if !backtrace.is_empty() => {
                diagnostic.frames = directory_frames(backtrace);
                diagnostic.source_root = source_root.into();
                Self::Located(diagnostic)
            }
            error => error.at(backtrace, source_root),
        }
    }

    /// Format a diagnostic that has no source location.
    #[must_use]
    pub fn without_location(self) -> Self {
        if matches!(self, Self::Located(_)) {
            return self;
        }
        self.with_site(&Backtrace::default(), b"")
    }

    fn with_site(self, backtrace: &Backtrace, source_root: &[u8]) -> Self {
        let body = match &self {
            Self::Message {
                expression,
                message,
            } if !expression.is_empty() => {
                format!("Error evaluating generator expression:\n  {expression}\n{message}").into()
            }
            Self::DependencyLoop { expression, .. } => format!(
                "Error evaluating generator expression:\n  {expression}\nDependency loop found."
            )
            .into(),
            _ => self.to_string().into(),
        };
        let steps = match self {
            Self::DependencyLoop { steps, .. } => steps,
            _ => Vec::new(),
        };
        Self::Located(Box::new(GenexDiagnostic {
            frames: directory_frames(backtrace),
            body,
            steps,
            source_root: source_root.into(),
        }))
    }
}
