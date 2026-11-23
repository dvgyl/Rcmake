//! Staging: install `CPACK_INSTALLED_DIRECTORIES`, `CPACK_INSTALL_CMAKE_PROJECTS`, and
//! `CPACK_INSTALL_SCRIPTS` into the staging directory.

use std::path::Path;

use rcmake_host::Host;

use crate::CpackError;
use crate::config::CpackConfig;

/// Install the projects of `config` into `staging_dir`.
///
/// # Errors
///
/// Returns [`CpackError::Unsupported`] until staging is implemented.
pub fn stage(host: &dyn Host, config: &CpackConfig, staging_dir: &Path) -> Result<(), CpackError> {
    let _ = (host, config, staging_dir);
    Err(CpackError::Unsupported("CPack staging"))
}
