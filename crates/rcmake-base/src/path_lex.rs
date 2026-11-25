//! Pure lexical `cmake_path` operations shared by `cmake_path()` and `$<PATH:...>`.
//!
//! All operations work on byte strings. They do not access the filesystem.

use bstr::BString;

#[derive(Clone, Copy)]
/// Lexical view of the root name, root directory, and relative part of a path.
pub struct Lexical<'a> {
    raw: &'a [u8],
    root_name_len: usize,
    relative_start: usize,
    windows: bool,
}

impl<'a> Lexical<'a> {
    /// Split `raw` into root name, root directory, and relative part.
    pub fn new(raw: &'a [u8]) -> Self {
        Self::with_dialect(raw, cfg!(windows))
    }

    /// Split a forward-slash path using Windows drive and UNC rules.
    pub fn windows(raw: &'a [u8]) -> Self {
        Self::with_dialect(raw, true)
    }

    fn with_dialect(raw: &'a [u8], windows: bool) -> Self {
        let root_name_len = root_name_len(raw, windows);
        let relative_start = root_name_len
            + raw
                .get(root_name_len..)
                .unwrap_or_default()
                .iter()
                .take_while(|&&b| b == b'/')
                .count();
        Self {
            raw,
            root_name_len,
            relative_start,
            windows,
        }
    }

    /// Root name (`C:` or `//server` on Windows; always empty on POSIX).
    pub fn root_name(self) -> &'a [u8] {
        self.raw.get(..self.root_name_len).unwrap_or_default()
    }

    /// Root directory (`/` when the path has one after the root name).
    pub fn root_directory(self) -> &'static [u8] {
        if self.relative_start > self.root_name_len {
            b"/"
        } else {
            b""
        }
    }

    /// Root name followed by root directory.
    pub fn root_path(self) -> BString {
        [self.root_name(), self.root_directory()].concat().into()
    }

    /// `true` for an absolute path (`cmake_path(IS_ABSOLUTE)`).
    pub fn absolute(self) -> bool {
        !self.root_directory().is_empty() && (!self.windows || !self.root_name().is_empty())
    }

    /// Path text after the root path.
    pub fn relative_part(self) -> &'a [u8] {
        self.raw.get(self.relative_start..).unwrap_or_default()
    }

    /// Last component of the relative part (empty after a trailing `/`).
    pub fn filename(self) -> &'a [u8] {
        self.relative_part()
            .rsplit(|&b| b == b'/')
            .next()
            .unwrap_or_default()
    }

    /// Path text with the filename removed (`REMOVE_FILENAME`).
    pub fn without_filename(self) -> &'a [u8] {
        self.raw
            .get(..self.raw.len() - self.filename().len())
            .unwrap_or_default()
    }

    /// Parent path (`GET PARENT_PATH`).
    pub fn parent(self) -> &'a [u8] {
        if self.relative_part().is_empty() {
            return self.raw;
        }
        let prefix = self.without_filename();
        let len = prefix.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        prefix
            .get(..len.max(self.relative_start))
            .unwrap_or_default()
    }

    /// Split the filename into stem and extension. With `last_only`, the extension starts
    /// at the last dot; otherwise at the first dot after the first byte.
    pub fn stem_extension(self, last_only: bool) -> (&'a [u8], &'a [u8]) {
        let name = self.filename();
        if matches!(name, b"." | b"..") {
            return (name, b"");
        }
        let mut dots = name.iter().enumerate().skip(1).filter(|(_, b)| **b == b'.');
        let dot = if last_only {
            dots.next_back()
        } else {
            dots.next()
        };
        name.split_at(dot.map_or(name.len(), |(i, _)| i))
    }

    /// Non-empty components of the relative part.
    pub fn components(self) -> impl Iterator<Item = &'a [u8]> {
        self.relative_part()
            .split(|&b| b == b'/')
            .filter(|part| !part.is_empty())
    }

    fn trailing(self) -> bool {
        !self.relative_part().is_empty() && self.raw.ends_with(b"/")
    }

    fn same_root(self, other: Self) -> bool {
        self.root_name() == other.root_name() && self.root_directory() == other.root_directory()
    }

    /// Lexical path equality (`cmake_path(COMPARE ... EQUAL ...)`).
    pub fn equal(self, other: Self) -> bool {
        self.same_root(other)
            && self.components().eq(other.components())
            && self.trailing() == other.trailing()
    }

    /// `true` if `self` is a lexical prefix of `other` (`cmake_path(IS_PREFIX)`).
    pub fn prefix(self, other: Self) -> bool {
        if !self.same_root(other) {
            return false;
        }
        let mut target = other.components();
        self.components().all(|part| target.next() == Some(part))
            && (!self.trailing() || target.next().is_some() || other.trailing())
    }

    /// Lexically normal form (`NORMAL_PATH`): removes `.`, resolves `..`, collapses
    /// separators.
    pub fn normalize(self) -> BString {
        if self.raw.is_empty() {
            return BString::default();
        }
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut trailing = self.trailing();
        for part in self.components() {
            trailing = match part {
                b"." => true,
                b".." if parts.last().is_some_and(|previous| *previous != b"..") => {
                    parts.pop();
                    true
                }
                b".." if !self.root_directory().is_empty() => true,
                _ => {
                    parts.push(part);
                    false
                }
            };
        }
        let mut output = self.root_path();
        output.extend_from_slice(&parts.join(&b'/'));
        if output.is_empty() {
            output.push(b'.');
        } else if !parts.is_empty()
            && parts.last() != Some(&b"..".as_slice())
            && (trailing || self.trailing())
        {
            output.push(b'/');
        }
        output
    }

    /// Resolve a path against a base and normalize the result.
    pub fn absolute_from(base: &[u8], path: &[u8]) -> BString {
        Lexical::new(&Lexical::new(base).append(path)).normalize()
    }

    /// Append `input` with `cmake_path(APPEND)` semantics.
    pub fn append(self, input: &[u8]) -> BString {
        let rhs = Lexical::with_dialect(input, self.windows);
        if rhs.absolute() || (!rhs.root_name().is_empty() && self.root_name() != rhs.root_name()) {
            return input.into();
        }
        if !rhs.root_directory().is_empty() {
            return [
                self.root_name(),
                input.get(rhs.root_name_len..).unwrap_or_default(),
            ]
            .concat()
            .into();
        }
        let separator: &[u8] = if !self.filename().is_empty() {
            b"/"
        } else {
            b""
        };
        [self.raw, separator, rhs.relative_part()].concat().into()
    }

    /// Relative path from `base` to `self` (`RELATIVE_PATH`). `None` when the roots differ.
    pub fn relative(self, base: Self) -> Option<BString> {
        self.relative_with_case(base, false)
    }

    /// Compute a relative path, optionally folding ASCII case in path components.
    pub fn relative_with_case(self, base: Self, ignore_case: bool) -> Option<BString> {
        let equal = |a: &[u8], b: &[u8]| a == b || (ignore_case && a.eq_ignore_ascii_case(b));
        if !equal(self.root_name(), base.root_name())
            || self.root_directory() != base.root_directory()
        {
            return None;
        }
        let mut target = self.components().peekable();
        let mut base_parts = base.components().peekable();
        while target
            .peek()
            .zip(base_parts.peek())
            .is_some_and(|(a, b)| equal(a, b))
        {
            target.next();
            base_parts.next();
        }
        let mut parents = 0_i64;
        for part in base_parts {
            match part {
                b"." => {}
                b".." => parents -= 1,
                _ => parents += 1,
            }
        }
        let parents = usize::try_from(parents).ok()?;
        let mut parts = vec![b"..".as_slice(); parents];
        let has_target = target.peek().is_some();
        parts.extend(target);
        if parts.is_empty() {
            return Some(BString::from("."));
        }
        if self.trailing() && has_target {
            parts.push(b"");
        }
        Some(parts.join(&b'/').into())
    }
}

fn root_name_len(raw: &[u8], windows: bool) -> usize {
    if windows {
        if let [drive, b':', ..] = raw
            && drive.is_ascii_alphabetic()
        {
            return 2;
        }
        if let Some(rest) = raw.strip_prefix(b"//")
            && rest.first().is_some_and(|&b| b != b'/')
        {
            return 2 + rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
        }
    }
    0
}

/// Separator of native path lists (`:` on POSIX, `;` on Windows).
pub const NATIVE_LIST_SEPARATOR: u8 = if cfg!(windows) { b';' } else { b':' };

/// Split a native path list at [`NATIVE_LIST_SEPARATOR`].
pub fn native_list(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut remaining = Some((input, 0));
    std::iter::from_fn(move || {
        let (rest, skip) = remaining.take()?;
        let end = rest
            .iter()
            .enumerate()
            .skip(skip)
            .find(|(_, b)| **b == NATIVE_LIST_SEPARATOR);
        if let Some((end, _)) = end {
            let (part, tail) = rest.split_at(end);
            remaining = tail.get(1..).map(|tail| (tail, 1));
            Some(part)
        } else {
            Some(rest)
        }
    })
}

/// Convert a native path to CMake style (forward slashes; strips `\\?\` on Windows).
pub fn cmake_style(input: &[u8]) -> BString {
    if cfg!(windows) {
        let mut output = Vec::new();
        let input = if let Some(verbatim) = input.strip_prefix(br"\\?\") {
            if let Some(unc) = verbatim.strip_prefix(br"UNC\") {
                output.extend_from_slice(b"//");
                unc
            } else {
                verbatim
            }
        } else {
            input
        };
        output.extend(input.iter().map(|&b| if b == b'\\' { b'/' } else { b }));
        output.into()
    } else {
        input.into()
    }
}

/// Convert a CMake-style path to native separators.
pub fn native_style(input: &[u8]) -> BString {
    if cfg!(windows) {
        input
            .iter()
            .map(|&b| if b == b'/' { b'\\' } else { b })
            .collect::<Vec<_>>()
            .into()
    } else {
        input.into()
    }
}

#[test]
fn resolves_and_normalizes_absolute_paths() {
    let root = if cfg!(windows) { "C:" } else { "" };
    let base = format!("{root}/work/src");
    for (path, suffix) in [
        ("../project//file", "/work/project/file"),
        ("/another/./file", "/another/file"),
    ] {
        assert_eq!(
            Lexical::absolute_from(base.as_bytes(), path.as_bytes()),
            format!("{root}{suffix}")
        );
    }
}
