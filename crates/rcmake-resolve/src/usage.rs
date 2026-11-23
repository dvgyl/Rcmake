//! Transitive usage requirements (ADR-007 item 6).
//!
//! Compile usage follows `INTERFACE_LINK_LIBRARIES` but skips `$<LINK_ONLY:...>` items.
//! Link usage includes them. Compile-usage deduplication (encounter order and `SYSTEM`
//! classification) stays separate from link-line ordering.

use rcmake_base::BString;
use rcmake_genex::{EvalContext, EvalFlags, UsageTraversal};
use rcmake_model::{BT, ProjectModel, TargetId, UsageField};

use crate::ResolveError;

/// Memoization key of one transitive evaluation (ADR-006 item 4). `head` is only part
/// of the key when the cached value is head-sensitive.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UsageKey {
    /// Target whose interface is walked.
    pub target: TargetId,
    /// Property.
    pub field: UsageField,
    /// Configuration name.
    pub config: BString,
    /// Compile language, if any.
    pub language: Option<BString>,
    /// Consuming target, when the value depends on it.
    pub head: Option<TargetId>,
    /// Walk kind.
    pub traversal: UsageTraversal,
}

/// Result of a transitive walk: entries in encounter order plus dependency flags.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageValue {
    /// Evaluated entries.
    pub entries: Vec<BT<BString>>,
    /// What the value depends on.
    pub flags: EvalFlags,
}

/// Collect `field` for `target` and its transitive interface in `ctx`.
pub fn collect(
    _model: &ProjectModel,
    _target: TargetId,
    _field: UsageField,
    _ctx: &EvalContext,
) -> Result<UsageValue, ResolveError> {
    Err(ResolveError::Unimplemented("usage requirement collection"))
}
