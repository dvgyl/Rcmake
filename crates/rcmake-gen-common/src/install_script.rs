//! `cmake_install.cmake` scripts for `cmake --install`.

/// Render the `cmake_install.cmake` script of the directory with binary directory `binary_dir`.
pub fn render(
    _manifest: &rcmake_resolve::InstallManifest,
    _binary_dir: &[u8],
) -> Result<rcmake_base::BString, crate::GenError> {
    Err(crate::GenError::Unimplemented("cmake_install.cmake"))
}
