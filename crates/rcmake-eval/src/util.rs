//! Byte helpers shared by the evaluator (paths, escaping, text replacement, integers).

use rcmake_base::BString;

/// Concatenate byte slices.
pub(crate) fn cat(parts: &[&[u8]]) -> BString {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    BString::from(out)
}

/// Lexically collapse `path` against `base` (`cmSystemTools::CollapseFullPath`).
///
/// Relative paths are joined to `base`. `.` components are removed and `..` removes the
/// previous component (never above the root).
pub(crate) fn collapse_full_path(path: &[u8], base: &[u8]) -> BString {
    let mut joined = Vec::new();
    if path.first() != Some(&b'/') {
        joined.extend_from_slice(base);
        joined.push(b'/');
    }
    joined.extend_from_slice(path);

    let mut parts: Vec<&[u8]> = Vec::new();
    for comp in joined.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = Vec::with_capacity(joined.len());
    for part in &parts {
        out.push(b'/');
        out.extend_from_slice(part);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    BString::from(out)
}

/// Directory part of a path (`cmSystemTools::GetFilenamePath`).
pub(crate) fn filename_path(path: &[u8]) -> BString {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => BString::from("/"),
        Some(pos) => BString::from(path.get(..pos).unwrap_or_default()),
        None => BString::default(),
    }
}

/// Quote a value for display in CMake code (`cmOutputConverter::EscapeForCMake`).
pub(crate) fn escape_for_cmake(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    for &b in value {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'$' => out.extend_from_slice(b"\\$"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            other => out.push(other),
        }
    }
    out.push(b'"');
    out
}

/// Replace every non-overlapping occurrence of `needle` in `hay` with `rep`, scanning
/// left to right (`cmSystemTools::ReplaceString`).
pub(crate) fn replace_all(hay: &[u8], needle: &[u8], rep: &[u8]) -> Vec<u8> {
    if needle.is_empty() || hay.len() < needle.len() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay.get(i..i + needle.len()) == Some(needle) {
            out.extend_from_slice(rep);
            i += needle.len();
        } else if let Some(&b) = hay.get(i) {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// Double every backslash (CMP0219 `NEW` macro argument escaping).
pub(crate) fn escape_backslashes(value: &[u8]) -> BString {
    BString::from(replace_all(value, b"\\", b"\\\\"))
}

/// Error from [`stoi`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntError {
    /// No digits (`std::invalid_argument`).
    Invalid,
    /// Value outside `int` (`std::out_of_range`).
    OutOfRange,
}

/// Parse an integer with `std::stoi` rules: leading whitespace, an optional sign, at
/// least one decimal digit, and trailing bytes ignored.
pub(crate) fn stoi(s: &[u8]) -> Result<i32, IntError> {
    let mut i = 0;
    while s
        .get(i)
        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        i += 1;
    }
    let mut negative = false;
    if let Some(&sign) = s.get(i)
        && (sign == b'+' || sign == b'-')
    {
        negative = sign == b'-';
        i += 1;
    }
    let mut value: i64 = 0;
    let mut digits = 0usize;
    let mut overflow = false;
    while let Some(&b) = s.get(i) {
        if !b.is_ascii_digit() {
            break;
        }
        digits += 1;
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add(i64::from(b - b'0')))
            .unwrap_or_else(|| {
                overflow = true;
                i64::MAX
            });
        i += 1;
    }
    if digits == 0 {
        return Err(IntError::Invalid);
    }
    let signed = if negative { -value } else { value };
    if overflow {
        return Err(IntError::OutOfRange);
    }
    i32::try_from(signed).map_err(|_| IntError::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_and_dirname() {
        assert_eq!(
            collapse_full_path(b"a/./b/../c.cmake", b"/w"),
            "/w/a/c.cmake"
        );
        assert_eq!(collapse_full_path(b"/x/../../y", b"/w"), "/y");
        assert_eq!(collapse_full_path(b".", b"/"), "/");
        assert_eq!(filename_path(b"/w/a.cmake"), "/w");
        assert_eq!(filename_path(b"/a.cmake"), "/");
        assert_eq!(filename_path(b"a.cmake"), "");
    }

    #[test]
    fn escape_and_replace() {
        assert_eq!(escape_for_cmake(b"a\"$\\b"), b"\"a\\\"\\$\\\\b\"".to_vec());
        assert_eq!(replace_all(b"${a}${a}x", b"${a}", b"1"), b"11x".to_vec());
        assert_eq!(replace_all(b"aaa", b"aa", b"b"), b"ba".to_vec());
        assert_eq!(escape_backslashes(b"a\\b"), "a\\\\b");
    }

    #[test]
    fn stoi_matches_std() {
        assert_eq!(stoi(b"  42abc"), Ok(42));
        assert_eq!(stoi(b"-7"), Ok(-7));
        assert_eq!(stoi(b"+3"), Ok(3));
        assert_eq!(stoi(b"abc"), Err(IntError::Invalid));
        assert_eq!(stoi(b"-"), Err(IntError::Invalid));
        assert_eq!(stoi(b"2147483647"), Ok(i32::MAX));
        assert_eq!(stoi(b"-2147483648"), Ok(i32::MIN));
        assert_eq!(stoi(b"2147483648"), Err(IntError::OutOfRange));
        assert_eq!(stoi(b"99999999999999999999999"), Err(IntError::OutOfRange));
    }
}
