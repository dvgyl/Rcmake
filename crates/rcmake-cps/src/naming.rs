//! CPS file names: root `<Pkg>.cps`, appendix `<Pkg>-<appendix>.cps`, and
//! configuration supplement `<Pkg>[-<appendix>]@<lowercase-config>.cps`
//! (`@noconfig` when the configuration is empty).

use rcmake_base::BString;

use crate::CpsError;

/// File name of the root file, or of the appendix file when `appendix` is set.
/// `lower_case` is `LOWER_CASE_FILE`.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until CPS file naming is implemented.
pub fn package_file_name(
    package: &[u8],
    appendix: Option<&[u8]>,
    lower_case: bool,
) -> Result<BString, CpsError> {
    let _ = (package, appendix, lower_case);
    Err(CpsError::Unsupported("CPS package file naming"))
}

/// File name of the configuration supplement for `config`.
///
/// # Errors
///
/// Returns [`CpsError::Unsupported`] until CPS file naming is implemented.
pub fn supplement_file_name(
    package: &[u8],
    appendix: Option<&[u8]>,
    config: &[u8],
    lower_case: bool,
) -> Result<BString, CpsError> {
    let _ = (package, appendix, config, lower_case);
    Err(CpsError::Unsupported(
        "CPS configuration supplement file naming",
    ))
}
