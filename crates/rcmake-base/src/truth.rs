//! CMake 4.5.1 boolean truthiness rules.
//!
//! CMake distinguishes three related predicates:
//! - [`is_on`]: `1`, `ON`, `YES`, `TRUE`, `Y` (ASCII case-insensitive)
//! - [`is_off`]: `""`, `0`, `OFF`, `NO`, `FALSE`, `N`, `IGNORE` (ASCII case-insensitive),
//!   plus exact `NOTFOUND` or suffix `-NOTFOUND` (**case-sensitive**)
//! - [`if_constant`]: recognizes [`is_on`], [`is_off`], and finite numeric literals
//!   (non-zero is `Some(true)`, zero is `Some(false)`); returns `None` when an unquoted
//!   `if()` operand should be dereferenced as a variable name.

/// Return `true` iff `v` is one of CMake's explicit true constants (`1`, `ON`, `YES`, `TRUE`, `Y`).
#[must_use]
pub fn is_on(v: &[u8]) -> bool {
    v.eq_ignore_ascii_case(b"1")
        || v.eq_ignore_ascii_case(b"ON")
        || v.eq_ignore_ascii_case(b"YES")
        || v.eq_ignore_ascii_case(b"TRUE")
        || v.eq_ignore_ascii_case(b"Y")
}

/// Return `true` iff `v` is one of CMake's explicit false constants.
///
/// Note that `NOTFOUND` and the `-NOTFOUND` suffix are **case-sensitive** in CMake.
#[must_use]
pub fn is_off(v: &[u8]) -> bool {
    if v.is_empty()
        || v.eq_ignore_ascii_case(b"0")
        || v.eq_ignore_ascii_case(b"OFF")
        || v.eq_ignore_ascii_case(b"NO")
        || v.eq_ignore_ascii_case(b"FALSE")
        || v.eq_ignore_ascii_case(b"N")
        || v.eq_ignore_ascii_case(b"IGNORE")
    {
        return true;
    }
    v == b"NOTFOUND" || v.ends_with(b"-NOTFOUND")
}

/// Check whether an `if()` operand is a boolean or numeric constant.
///
/// Returns `Some(true)` / `Some(false)` for explicit constants or whole-string numeric
/// values (non-zero is true, zero is false), or `None` if the operand is not a constant
/// (in which case an unquoted `if()` operand is looked up once as a variable name, while
/// a quoted operand evaluates to `false`).
#[must_use]
pub fn if_constant(v: &[u8]) -> Option<bool> {
    if is_on(v) {
        return Some(true);
    }
    if is_off(v) {
        return Some(false);
    }
    parse_cmake_number_whole(v).map(|num| num != 0.0)
}

/// Parse a whole byte string as a CMake numeric constant (matching C `strtod` whole-string
/// consumption without leading/trailing garbage).
fn parse_cmake_number_whole(v: &[u8]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let s = core::str::from_utf8(v).ok()?;
    // C `strtod` accepts leading ASCII whitespace, but trailing whitespace is rejected
    // by CMake's `*end == '\0'` check, and non-numeric strings like "nan"/"inf" are
    // handled carefully: match standard decimal float syntax.
    let trimmed = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let first = *bytes.first()?;
    let body = if first == b'+' || first == b'-' {
        bytes.get(1..)?
    } else {
        bytes
    };
    let lead = *body.first()?;
    if !(lead.is_ascii_digit() || lead == b'.') {
        return None;
    }
    let parsed: f64 = trimmed.parse().ok()?;
    if parsed.is_nan() {
        return None;
    }
    Some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truth_constants_and_case_sensitivity() {
        assert!(is_on(b"ON"));
        assert!(is_on(b"true"));
        assert!(is_on(b"y"));
        assert!(is_on(b"1"));
        assert!(!is_on(b"2"));

        assert!(is_off(b""));
        assert!(is_off(b"0"));
        assert!(is_off(b"false"));
        assert!(is_off(b"Ignore"));
        assert!(is_off(b"NOTFOUND"));
        assert!(is_off(b"FOO-NOTFOUND"));
        assert!(!is_off(b"notfound"));
        assert!(!is_off(b"x-notfound"));
        assert!(!is_off(b"0.0"));

        assert_eq!(if_constant(b"0.0"), Some(false));
        assert_eq!(if_constant(b"-0.0"), Some(false));
        assert_eq!(if_constant(b"2"), Some(true));
        assert_eq!(if_constant(b"-3.5"), Some(true));
        assert_eq!(if_constant(b"MY_VAR"), None);
    }
}
