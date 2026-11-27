//! Bounded unified diff rendering for `rcmake fmt --diff` using [`similar`].

#![forbid(unsafe_code)]

use similar::TextDiff;

/// Default maximum byte budget for rendered unified diff output (4 MiB).
pub const DEFAULT_MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

/// Render a bounded unified diff between `original` and `formatted` using `display_path`.
///
/// Handles arbitrary byte sequences safely via lossy UTF-8 display when needed and
/// truncates cleanly with a notice if `max_bytes` would be exceeded.
#[must_use]
pub fn unified_diff(
    display_path: &str,
    original: &[u8],
    formatted: &[u8],
    max_bytes: usize,
) -> Vec<u8> {
    if original == formatted {
        return Vec::new();
    }
    let orig_text = String::from_utf8_lossy(original);
    let fmt_text = String::from_utf8_lossy(formatted);
    let diff = TextDiff::from_lines(orig_text.as_ref(), fmt_text.as_ref());
    let header_old = format!("a/{display_path}");
    let header_new = format!("b/{display_path}");

    let rendered = diff
        .unified_diff()
        .context_radius(3)
        .header(&header_old, &header_new)
        .to_string();

    if rendered.len() <= max_bytes {
        return rendered.into_bytes();
    }

    let notice = format!("\n... diff truncated after {max_bytes} bytes for {display_path} ...\n");
    let keep = max_bytes.saturating_sub(notice.len());
    let boundary = rendered.floor_char_boundary(keep);
    let mut out = rendered
        .get(..boundary)
        .unwrap_or_default()
        .as_bytes()
        .to_vec();
    out.extend_from_slice(notice.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_diff_renders_and_bounds_output() {
        let diff = unified_diff(
            "CMakeLists.txt",
            b"if(ON)\nset(A 1)\nendif()\n",
            b"if(ON)\n  set(A 1)\nendif()\n",
            DEFAULT_MAX_DIFF_BYTES,
        );
        let text = String::from_utf8(diff).unwrap();
        assert!(text.contains("--- a/CMakeLists.txt"));
        assert!(text.contains("+++ b/CMakeLists.txt"));
        assert!(text.contains("-set(A 1)"));
        assert!(text.contains("+  set(A 1)"));

        let small = unified_diff("CMakeLists.txt", b"a\n", b"b\n", 40);
        let small_text = String::from_utf8(small).unwrap();
        assert!(small_text.contains("diff truncated"));
    }
}
