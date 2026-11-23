//! Evaluation of `CPackConfig.cmake` in a script-mode interpreter with the full
//! `rcmake_commands::register_all` registry.

use std::collections::BTreeMap;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_host::Host;

use crate::{CPackOptions, CpackError};

/// The `CPACK_*` variables of a configuration file, after the command-line overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpackConfig {
    /// Variables by name.
    pub vars: BTreeMap<BString, BString>,
}

/// Run the configuration file that `options` selects and apply the overrides.
///
/// # Errors
///
/// Returns [`CpackError::Unsupported`] until configuration loading is implemented.
pub fn load(host: &Arc<dyn Host>, options: &CPackOptions) -> Result<CpackConfig, CpackError> {
    let _ = (host, options);
    Err(CpackError::Unsupported("CPack configuration loading"))
}
