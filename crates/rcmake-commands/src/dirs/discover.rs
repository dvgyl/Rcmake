//! `discover_tests()` (ADR-011): record a [`rcmake_model::TestKind::Discover`] entry
//! that ctest expands at test time.

use rcmake_eval::{CmdError, CmdResult, Cx};
use rcmake_expr::ExpandedArg;

pub(super) fn discover_tests(_: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    Err(CmdError::Status("requires build-system support.".into()))
}
