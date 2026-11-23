//! Source packages (`cpack --config CPackSourceConfig.cmake`, `package_source`).

use std::path::Path;

use rcmake_host::Host;

use crate::CpackError;
use crate::config::CpackConfig;

/// Copy the source tree into `staging_dir`, without the files that match
/// `CPACK_SOURCE_IGNORE_FILES`.
///
/// # Errors
///
/// Returns [`CpackError::Unsupported`] until source staging is implemented.
pub fn stage_sources(
    host: &dyn Host,
    config: &CpackConfig,
    staging_dir: &Path,
) -> Result<(), CpackError> {
    let _ = (host, config, staging_dir);
    Err(CpackError::Unsupported("CPack source packaging"))
}
