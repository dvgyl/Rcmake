//! CMake 4.5.1 arbitrary-precision version parsing and comparison.
//!
//! CMake's `VERSION_LESS` / `VERSION_GREATER` / `VERSION_EQUAL` operators compare
//! arbitrarily many dot-separated decimal components of arbitrary digit length
//! (ignoring leading zeros and treating missing trailing components as zero).

use core::cmp::Ordering;
use core::fmt;

/// A parsed CMake dotted numeric version (used for `cmake_minimum_required`, policies, etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    components: Vec<u32>,
}

impl Version {
    /// Construct a version from `(major, minor, patch)`.
    #[must_use]
    pub fn from_tuple(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            components: vec![u32::from(major), u32::from(minor), u32::from(patch)],
        }
    }

    /// Parse a strict `major[.minor[.patch[.tweak]]]` version string (1 to 4 components).
    #[must_use]
    pub fn parse_strict(s: &[u8]) -> Option<Self> {
        let text = core::str::from_utf8(s).ok()?;
        if text.is_empty() {
            return None;
        }
        let mut components = Vec::new();
        for part in text.split('.') {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            components.push(part.parse::<u32>().ok()?);
        }
        if components.len() > 4 {
            return None;
        }
        Some(Self { components })
    }

    /// Return the component at `index`, defaulting to `0` if omitted.
    #[must_use]
    pub fn component(&self, index: usize) -> u32 {
        self.components.get(index).copied().unwrap_or(0)
    }

    /// Major version component.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.component(0)
    }

    /// Minor version component.
    #[must_use]
    pub fn minor(&self) -> u32 {
        self.component(1)
    }

    /// Patch version component.
    #[must_use]
    pub fn patch(&self) -> u32 {
        self.component(2)
    }

    /// Tweak version component.
    #[must_use]
    pub fn tweak(&self) -> u32 {
        self.component(3)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.components.len().max(other.components.len());
        for i in 0..len {
            let ord = self.component(i).cmp(&other.component(i));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.components.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

/// Compare two version byte strings using CMake 4.5.1's `VERSION_*` semantics:
/// arbitrary component count, arbitrary-length decimal integers, ignoring leading
/// zeros, stopping each side at the first non-digit/non-dot character, and treating
/// missing trailing components as zero.
#[must_use]
pub fn compare(a: &[u8], b: &[u8]) -> Ordering {
    let mut ia = VersionComponents::new(a);
    let mut ib = VersionComponents::new(b);
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (Some(ca), Some(cb)) => {
                let ord = cmp_decimal_digits(ca, cb);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), None) => {
                if !is_all_zero(ca) {
                    return Ordering::Greater;
                }
            }
            (None, Some(cb)) => {
                if !is_all_zero(cb) {
                    return Ordering::Less;
                }
            }
        }
    }
}

struct VersionComponents<'a> {
    rest: &'a [u8],
    done: bool,
}

impl<'a> VersionComponents<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            rest: input,
            done: false,
        }
    }
}

impl<'a> Iterator for VersionComponents<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.rest.is_empty() {
            return None;
        }
        let mut end = 0;
        while let Some(&b) = self.rest.get(end) {
            if b.is_ascii_digit() {
                end += 1;
            } else {
                break;
            }
        }
        if end == 0 {
            self.done = true;
            return None;
        }
        let digits = self.rest.get(..end)?;
        if self.rest.get(end) == Some(&b'.') {
            self.rest = self.rest.get(end + 1..).unwrap_or(&[]);
        } else {
            self.rest = &[];
            self.done = true;
        }
        Some(digits)
    }
}

fn strip_leading_zeros(digits: &[u8]) -> &[u8] {
    let mut i = 0;
    while let Some(&b) = digits.get(i) {
        if b == b'0' {
            i += 1;
        } else {
            break;
        }
    }
    digits.get(i..).unwrap_or(&[])
}

fn is_all_zero(digits: &[u8]) -> bool {
    strip_leading_zeros(digits).is_empty()
}

fn cmp_decimal_digits(a: &[u8], b: &[u8]) -> Ordering {
    let sa = strip_leading_zeros(a);
    let sb = strip_leading_zeros(b);
    sa.len().cmp(&sb.len()).then_with(|| sa.cmp(sb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_length_and_many_components() {
        assert_eq!(
            compare(
                b"1.2.3.4.5.999999999999999999999999",
                b"1.2.3.4.5.999999999999999999999998"
            ),
            Ordering::Greater
        );
        assert_eq!(compare(b"1.2", b"1.2.0.0"), Ordering::Equal);
        assert_eq!(compare(b"01.002", b"1.2"), Ordering::Equal);
        assert_eq!(compare(b"3.5-rc1", b"3.5.0"), Ordering::Equal);
        assert_eq!(compare(b"4.5.1", b"4.5.0"), Ordering::Greater);
    }
}
