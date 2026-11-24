//! Deferred target property requests from `cmake_language(PRINT_PROPERTIES)`.

use rcmake_base::BString;
use rcmake_diag::Backtrace;

use crate::DirId;

/// One target property request to display during generation.
#[derive(Clone, Debug)]
pub struct PropertyPrint {
    /// Directory of the call.
    pub dir: DirId,
    /// Backtrace of the call.
    pub backtrace: Backtrace,
    /// Target names in the requested order.
    pub targets: Vec<BString>,
    /// Named properties in order, or all properties when absent.
    pub names: Option<Vec<BString>>,
    /// Regular expression for property names.
    pub name_filter: Option<BString>,
    /// Regular expression for property values.
    pub value_filter: Option<BString>,
    /// Include reachable link dependencies.
    pub follow: bool,
    /// Use the banner format of the legacy module.
    pub legacy: bool,
}
