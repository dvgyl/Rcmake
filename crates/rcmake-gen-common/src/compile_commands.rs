//! `compile_commands.json` (`CMAKE_EXPORT_COMPILE_COMMANDS`).

/// Render `compile_commands.json` for one build graph.
pub fn render(
    _graph: &rcmake_resolve::BuildGraph,
    _build_dir: &[u8],
) -> Result<rcmake_base::BString, crate::GenError> {
    Err(crate::GenError::Unimplemented("compile_commands.json"))
}
