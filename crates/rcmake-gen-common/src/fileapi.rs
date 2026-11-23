//! File API v1 replies (`codemodel` 2.12, `cache` 2.0, `toolchains` 1.1, `cmakeFiles` 1.1, `configureLog` 1.0).

/// Answer the File API queries under `<build_dir>/.cmake/api/v1/query`.
pub fn write_replies(
    _result: &rcmake_resolve::GenerationResult,
    _host: &dyn rcmake_host::Host,
    _build_dir: &std::path::Path,
) -> Result<(), crate::GenError> {
    Err(crate::GenError::Unimplemented("File API replies"))
}
