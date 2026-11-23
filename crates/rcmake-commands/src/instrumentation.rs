//! `cmake_instrumentation()` (ADR-013): record an instrumentation query in
//! [`rcmake_model::GlobalState::instrumentation`].

use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Registry};
use rcmake_expr::ExpandedArg;

pub(crate) fn register(registry: &mut Registry) {
    registry.add(
        "cmake_instrumentation",
        cmake_instrumentation,
        CmdFlags { scriptable: false },
    );
}

fn cmake_instrumentation(_: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    Err(CmdError::Status("is not supported yet.".into()))
}
