//! `CTestTestfile.cmake` scripts for `ctest`.

/// Render the `CTestTestfile.cmake` of directory `dir`.
pub fn render(
    _model: &rcmake_model::ProjectModel,
    _dir: rcmake_model::DirId,
) -> Result<rcmake_base::BString, crate::GenError> {
    Err(crate::GenError::Unimplemented("CTestTestfile.cmake"))
}
