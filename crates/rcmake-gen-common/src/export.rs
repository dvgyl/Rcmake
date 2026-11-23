//! Export files for `install(EXPORT)` and `export()`.

/// Render the main export file of `plan`.
pub fn render(_plan: &rcmake_resolve::ExportPlan) -> Result<rcmake_base::BString, crate::GenError> {
    Err(crate::GenError::Unimplemented("export files"))
}
