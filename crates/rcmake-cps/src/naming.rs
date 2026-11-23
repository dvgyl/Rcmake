//! File and directory names for CPS packages.

use rcmake_base::BString;

use crate::CpsError;

fn file_part(name: &[u8]) -> bool {
    !name.is_empty()
        && name != b"."
        && name != b".."
        && !name.iter().any(|byte| b"/\\:@\0".contains(byte))
}

fn package_name(package: &[u8], lower_case: bool) -> Result<Vec<u8>, CpsError> {
    if !file_part(package)
        || !package
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(byte))
    {
        return Err(CpsError::Invalid("Invalid CPS package name.".into()));
    }
    Ok(if lower_case {
        package.to_ascii_lowercase()
    } else {
        package.to_vec()
    })
}

fn file_stem(
    package: &[u8],
    appendix: Option<&[u8]>,
    lower_case: bool,
) -> Result<Vec<u8>, CpsError> {
    let mut stem = package_name(package, lower_case)?;
    if let Some(appendix) = appendix.filter(|name| !name.is_empty()) {
        if !file_part(appendix) {
            return Err(CpsError::Invalid("Invalid CPS appendix name.".into()));
        }
        stem.push(b'-');
        stem.extend_from_slice(appendix);
    }
    Ok(stem)
}

/// Get the root or appendix file name. With `LOWER_CASE_FILE`, only the
/// package name changes to lowercase.
///
/// # Errors
///
/// Returns an error if a name is invalid or contains a path separator.
pub fn package_file_name(
    package: &[u8],
    appendix: Option<&[u8]>,
    lower_case: bool,
) -> Result<BString, CpsError> {
    let mut name = file_stem(package, appendix, lower_case)?;
    name.extend_from_slice(b".cps");
    Ok(name.into())
}

/// Get the supplement file name. The configuration suffix uses lowercase.
/// An empty configuration uses `noconfig`.
///
/// # Errors
///
/// Returns an error if a name is invalid or contains a path separator.
pub fn supplement_file_name(
    package: &[u8],
    appendix: Option<&[u8]>,
    config: &[u8],
    lower_case: bool,
) -> Result<BString, CpsError> {
    let mut name = file_stem(package, appendix, lower_case)?;
    name.push(b'@');
    if config.is_empty() {
        name.extend_from_slice(b"noconfig");
    } else if file_part(config) {
        name.extend(config.iter().map(u8::to_ascii_lowercase));
    } else {
        return Err(CpsError::Invalid("Invalid CPS configuration name.".into()));
    }
    name.extend_from_slice(b".cps");
    Ok(name.into())
}

/// Get the default install directory for the target platform. On Windows this
/// is `cps/<package>`. On other platforms it is `<libdir>/cps/<package>`.
///
/// # Errors
///
/// Returns an error if the package name is invalid.
pub fn install_directory(
    package: &[u8],
    libdir: &[u8],
    windows: bool,
    lower_case: bool,
) -> Result<BString, CpsError> {
    package_directory(if windows { b"" } else { libdir }, package, lower_case)
}

/// Get the default export directory `<build_dir>/cps/<package>`.
///
/// # Errors
///
/// Returns an error if the package name is invalid.
pub fn build_directory(
    package: &[u8],
    build_dir: &[u8],
    lower_case: bool,
) -> Result<BString, CpsError> {
    package_directory(build_dir, package, lower_case)
}

fn package_directory(root: &[u8], package: &[u8], lower_case: bool) -> Result<BString, CpsError> {
    let mut directory = root.to_vec();
    if !root.is_empty() && !root.ends_with(b"/") && !root.ends_with(b"\\") {
        directory.push(b'/');
    }
    directory.extend_from_slice(b"cps/");
    directory.extend(package_name(package, lower_case)?);
    Ok(directory.into())
}
