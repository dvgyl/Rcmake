//! Generator expression (`$<...>`) engine for Rcmake (ADR-006).
//!
//! This crate depends only on `rcmake-base`, `rcmake-diag`, and `rcmake-regex`. It
//! reads targets through the [`GenexHost`] trait, which `rcmake-resolve` implements.
//! This breaks the recursion between `$<TARGET_PROPERTY:...>` and transitive usage
//! resolution without a crate cycle.
//!
#![forbid(unsafe_code)]

use rcmake_base::BString;
use rcmake_diag::Backtrace;

mod eval;
mod parse;
mod preprocess;

pub use eval::evaluate;
pub use parse::parse;
pub use preprocess::{PreprocessMode, find_genex, genex_strip, rewrite_install_prefix, strip};

/// Maximum nesting depth of `$<...>` that [`parse`] accepts.
pub const MAX_NESTING: usize = 1000;

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

/// Evaluation context (`cmGeneratorExpressionContext`).
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
    /// `Ninja Multi-Config` command configuration (`$<COMMAND_CONFIG:...>`).
    pub command_config: Option<ConfigId>,
    /// `Ninja Multi-Config` output configuration (`$<OUTPUT_CONFIG:...>`).
    pub output_config: Option<ConfigId>,
    /// Usage walk kind for `$<TARGET_PROPERTY:...>` on `INTERFACE_*` properties.
    pub traversal: UsageTraversal,
    /// Do not issue diagnostics (`cmGeneratorExpression::Quiet`).
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

/// Target and build-system data that evaluation needs (implemented by
/// `rcmake-resolve`).
pub trait GenexHost {
    /// Target named `name` as seen from the evaluation's directory.
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle>;

    /// Find a target in the lookup scope of the evaluation context.
    fn find_target_in_context(&self, name: &[u8], _ctx: &EvalContext) -> Option<TargetHandle> {
        self.find_target(name)
    }

    /// Name of `target`.
    fn target_name(&self, target: TargetHandle) -> BString;

    /// Raw (unevaluated) value of a non-transitive property.
    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString>;

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

    /// Targets in the link closure of `target` for the consumer in `ctx`.
    fn link_closure(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError>;

    /// Variable value of the evaluation directory (`CMAKE_<LANG>_COMPILER_ID`, and so
    /// on).
    fn definition(&self, name: &[u8]) -> Option<BString>;

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
    fn is_device_link(&self, _ctx: &EvalContext) -> bool {
        false
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

/// Evaluation failure (`cmGeneratorExpressionDAGChecker` and operator errors).
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
    /// Nesting is deeper than [`MAX_NESTING`].
    #[error("generator expression nesting is too deep")]
    TooDeep,
    /// The operator is not implemented yet.
    #[error("generator expression is not supported yet")]
    Unsupported {
        /// The full expression text.
        expression: BString,
    },
}
