//! Path policy and lookup keys for the overlay.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use rcmake_base::BString;

use super::{HostError, HostResult, SyntheticError, path};

const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_COMPONENTS: usize = 256;

/// Root and separator rules used by the overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayPathStyle {
    /// Use a slash root and forward-slash separators.
    Posix,
    /// Require a drive or UNC share. Accept both separator spellings.
    Windows,
    /// Accept slash, drive, and UNC roots, as [`crate::PureHost`] does.
    Virtual,
}

/// Root and name lookup rules for an overlay.
///
/// Use the case rule of the filesystem that the base host exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayPathPolicy {
    /// Rules for roots and separators.
    pub style: OverlayPathStyle,
    /// Whether names that differ only in case refer to one entry.
    pub case_sensitive: bool,
}

impl OverlayPathPolicy {
    pub(super) fn name_key(self, name: &[u8]) -> Vec<u8> {
        if self.case_sensitive {
            return name.to_vec();
        }
        // Keep invalid UTF-8 bytes distinct and fold the valid text between them.
        let mut key = Vec::with_capacity(name.len());
        for chunk in name.utf8_chunks() {
            for ch in chunk.valid().chars().flat_map(char::to_lowercase) {
                key.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            }
            key.extend(chunk.invalid().iter().map(u8::to_ascii_lowercase));
        }
        key
    }

    fn windows_root(mut bytes: &[u8]) -> Option<(Vec<u8>, &[u8])> {
        let sep = |b: u8| separator(b, true);
        let mut unc_prefix = false;
        if let [a, b, b'?', c, rest @ ..] = bytes
            && sep(*a)
            && sep(*b)
            && sep(*c)
        {
            bytes = rest;
            if let [u, n, c, s, rest @ ..] = bytes
                && [*u, *n, *c].eq_ignore_ascii_case(b"UNC")
                && sep(*s)
            {
                bytes = rest;
                unc_prefix = true;
            }
        }
        if !unc_prefix
            && let [letter, b':', slash, rest @ ..] = bytes
            && letter.is_ascii_alphabetic()
            && sep(*slash)
        {
            return Some((vec![letter.to_ascii_uppercase(), b':', b'/'], rest));
        }
        let remote = if unc_prefix {
            bytes
        } else if let [a, b, rest @ ..] = bytes
            && sep(*a)
            && sep(*b)
        {
            rest
        } else {
            return None;
        };
        let mut parts = remote.splitn(3, |&b| sep(b));
        let server = parts.next()?;
        let share = parts.next()?;
        if [server, share].into_iter().any(|name| {
            name.is_empty() || matches!(name, b"." | b".." | b"?") || name.contains(&b':')
        }) {
            return None;
        }
        let root = [b"//", server, b"/", share, b"/"].concat();
        Some((root, parts.next().unwrap_or_default()))
    }

    pub(super) fn parse<'a>(self, input: &'a [u8], windows_parent: bool) -> Parsed<'a> {
        if self.style != OverlayPathStyle::Posix
            && let Some((root, rest)) = Self::windows_root(input)
        {
            return Parsed {
                root: Some(root),
                rest,
                windows: true,
            };
        }
        let windows = self.style == OverlayPathStyle::Windows
            || (self.style == OverlayPathStyle::Virtual
                && (windows_parent || cfg!(windows) || input.starts_with(b"\\")));
        let root = (self.style != OverlayPathStyle::Windows
            && input.first().is_some_and(|&b| separator(b, windows)))
        .then(|| b"/".to_vec());
        Parsed {
            root,
            rest: input,
            windows,
        }
    }

    pub(super) fn require_absolute(self, p: &Path) -> HostResult<()> {
        self.with_absolute(p, |_| Ok(()))
    }

    pub(super) fn with_absolute<T>(
        self,
        p: &Path,
        operation: impl FnOnce(Parsed<'_>) -> HostResult<T>,
    ) -> HostResult<T> {
        validate_length(p)?;
        let bytes = path::from_native(p);
        let parsed = self.parse(&bytes, false);
        if parsed.root.is_none() {
            return Err(HostError::not_absolute(p.to_path_buf()));
        }
        operation(parsed)
    }
}

pub(super) fn validate_length(p: &Path) -> HostResult<()> {
    let bytes = p.as_os_str().as_encoded_bytes();
    if bytes.len() > MAX_PATH_BYTES {
        return Err(budget_error(p));
    }
    if bytes.contains(&0) {
        return Err(SyntheticError::InvalidInput.at(Some(p)));
    }
    Ok(())
}

pub(super) fn budget_error(p: &Path) -> HostError {
    let mut error = SyntheticError::FileTooLarge.at(Some(p));
    error.os_message = "The overlay size limit was reached".into();
    error
}

fn separator(byte: u8, windows: bool) -> bool {
    byte == b'/' || (windows && byte == b'\\')
}

pub(super) struct Parsed<'a> {
    pub(super) root: Option<Vec<u8>>,
    pub(super) rest: &'a [u8],
    pub(super) windows: bool,
}

impl Parsed<'_> {
    fn names(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.rest
            .split(|&byte| separator(byte, self.windows))
            .filter(|name| !name.is_empty())
    }

    pub(super) fn endpoint(&self, p: &Path) -> HostResult<(&[u8], bool)> {
        let trailing = self
            .rest
            .last()
            .is_some_and(|&b| separator(b, self.windows));
        match self.names().next_back() {
            None => Err(SyntheticError::ResourceBusy.at(Some(p))),
            Some(b"." | b"..") => Err(SyntheticError::InvalidInput.at(Some(p))),
            Some(name) => Ok((name, trailing)),
        }
    }

    pub(super) fn parts(&self, create: bool, p: &Path) -> HostResult<VecDeque<Part>> {
        let mut parts = VecDeque::new();
        let dot = self.names().next_back() == Some(b".");
        for name in self.names() {
            if parts.len() >= MAX_COMPONENTS {
                return Err(budget_error(p));
            }
            match name {
                b"." => {}
                b".." => parts.push_back(Part::Parent),
                _ => parts.push_back(Part::Name(name.to_vec(), create)),
            }
        }
        if dot
            || self
                .rest
                .last()
                .is_some_and(|&b| separator(b, self.windows))
        {
            parts.push_back(Part::Directory);
        }
        Ok(parts)
    }
}

pub(super) enum Part {
    Name(Vec<u8>, bool),
    Parent,
    Directory,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Key(pub(super) Vec<u8>);

impl Key {
    pub(super) fn root(policy: OverlayPathPolicy, root: &[u8]) -> Self {
        let mut bytes = policy.name_key(root);
        bytes.push(0);
        Self(bytes)
    }

    pub(super) fn child(&self, name: &[u8]) -> Self {
        let mut bytes = self.0.clone();
        bytes.extend_from_slice(name);
        bytes.push(0);
        Self(bytes)
    }

    pub(super) fn is_child_of(&self, parent: &Self) -> bool {
        self.0
            .strip_prefix(parent.0.as_slice())
            .is_some_and(|rest| {
                rest.strip_suffix(b"\0")
                    .is_some_and(|name| !name.contains(&0))
            })
    }

    pub(super) fn is_under(&self, parent: &Self) -> bool {
        self.0.starts_with(&parent.0)
    }

    pub(super) fn rebased(&self, from: &Self, to: &Self) -> Self {
        let suffix = self.0.strip_prefix(from.0.as_slice()).unwrap_or_default();
        Self([to.0.as_slice(), suffix].concat())
    }
}

pub(super) fn child_path(dir: &Path, name: &[u8]) -> PathBuf {
    let mut bytes = path::from_native(dir);
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name);
    path::to_native(&bytes)
}

pub(super) fn display_name(p: &Path) -> BString {
    let bytes = path::from_native(p);
    bytes
        .rsplit(|b| *b == b'/')
        .next()
        .unwrap_or_default()
        .into()
}
