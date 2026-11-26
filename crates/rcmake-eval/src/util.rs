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
    let path = rcmake_base::path_lex::cmake_style(path);
    let joined = if path.starts_with(b"/") || drive_root(&path) {
        path
    } else {
        let mut joined = rcmake_base::path_lex::cmake_style(base);
        if !joined.ends_with(b"/") {
            joined.push(b'/');
        }
        joined.extend_from_slice(&path);
        joined
    };

    let root_len = root_len(&joined);
    let (root, relative) = joined.as_slice().split_at(root_len);
    let mut parts = Vec::new();
    for component in relative.split(|&byte| byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut out = BString::from(root);
    if out.is_empty() || (!parts.is_empty() && !out.ends_with(b"/")) {
        out.push(b'/');
    }
    out.extend_from_slice(&parts.join(&b'/'));
    out
}

fn drive_root(path: &[u8]) -> bool {
    cfg!(windows) && matches!(path, [letter, b':', b'/', ..] if letter.is_ascii_alphabetic())
}

fn root_len(path: &[u8]) -> usize {
    if drive_root(path) {
        return 3;
    }
    if cfg!(windows)
        && let Some(remote) = path.strip_prefix(b"//")
    {
        let mut parts = remote.splitn(3, |&byte| byte == b'/');
        if let (Some(server), Some(share)) = (parts.next(), parts.next())
            && !server.is_empty()
            && !share.is_empty()
        {
            return 3 + server.len() + share.len();
        }
    }
    usize::from(path.starts_with(b"/"))
}

/// Directory part of a path (`cmSystemTools::GetFilenamePath`).
pub(crate) fn filename_path(path: &[u8]) -> BString {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => BString::from("/"),
        Some(pos) if pos < root_len(path) => {
            BString::from(path.get(..root_len(path)).unwrap_or_default())
        }
        Some(pos) => BString::from(path.get(..pos).unwrap_or_default()),
        None => BString::default(),
    }
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

/// [`replace_all`] if the result has at most `max` bytes. Return `None` before any copy
/// if the result would be larger.
pub(crate) fn replace_all_bounded(
    hay: &[u8],
    needle: &[u8],
    rep: &[u8],
    max: usize,
) -> Option<Vec<u8>> {
    let count = count_matches(hay, needle);
    let len = hay
        .len()
        .checked_sub(count.checked_mul(needle.len())?)?
        .checked_add(count.checked_mul(rep.len())?)?;
    if len > max {
        return None;
    }
    if count == 0 {
        return Some(hay.to_vec());
    }
    Some(replace_all(hay, needle, rep))
}

/// Count the matches that [`replace_all`] replaces: left to right, without overlap.
fn count_matches(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i < hay.len() {
        if hay.get(i..i + needle.len()) == Some(needle) {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
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

    #[cfg(windows)]
    #[test]
    fn collapse_keeps_drive_and_unc_roots() {
        assert_eq!(
            collapse_full_path(br"C:\one\two\..\..\..\file", b"D:/work"),
            "C:/file"
        );
        assert_eq!(collapse_full_path(b"../file", br"C:\work"), "C:/file");
        assert_eq!(
            collapse_full_path(br"\\server\share\one\..\..\file", b"D:/work"),
            "//server/share/file"
        );
        assert_eq!(
            collapse_full_path(br"\\?\UNC\server\share\one", b"D:/work"),
            "//server/share/one"
        );
        assert_eq!(filename_path(b"C:/file"), "C:/");
    }

    #[test]
    fn replace_and_escape_backslashes() {
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
