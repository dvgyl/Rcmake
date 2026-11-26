use std::cmp::Ordering;
use std::collections::BTreeMap;

use rcmake_base::{BString, ByteSlice as _};

use super::cat;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Strictness {
    Strict,
    #[default]
    Permissive,
    BestEffort,
}

pub(super) enum InvalidFile {
    Syntax,
    Resolution,
}

#[derive(Default)]
pub(super) struct Package(BTreeMap<BString, BString>);

impl Package {
    pub(super) fn field(&self, key: &[u8]) -> &[u8] {
        self.0.get(key).map_or(b"", |value| value.as_slice())
    }

    pub(super) fn parse(
        content: &[u8],
        mut variables: BTreeMap<BString, BString>,
        mode: Strictness,
        limit: usize,
    ) -> Result<Self, InvalidFile> {
        let mut package = Self::default();
        let mut logical = Vec::new();
        let mut remaining = limit;
        for line in content.split(|&byte| byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(part) = line.strip_suffix(b"\\") {
                logical.extend_from_slice(part);
                continue;
            }
            logical.extend_from_slice(line);
            let line = without_comment(&logical).trim_ascii();
            if !line.is_empty() {
                match line.iter().position(|byte| matches!(byte, b':' | b'=')) {
                    Some(index) => {
                        let (key, rest) = line.split_at(index);
                        let key = key.trim_ascii_end();
                        let Some((&separator, raw_value)) = rest.split_first() else {
                            return Err(InvalidFile::Syntax);
                        };
                        let value = expand(raw_value, &variables, mode, remaining)?;
                        remaining = remaining.saturating_sub(value.len());
                        if separator == b'=' {
                            if variables.insert(key.into(), value).is_some()
                                && mode == Strictness::Strict
                            {
                                return Err(InvalidFile::Resolution);
                            }
                        } else {
                            package.insert(key, value, mode, limit)?;
                        }
                    }
                    None if mode != Strictness::BestEffort => return Err(InvalidFile::Syntax),
                    None => {}
                }
            }
            logical.clear();
        }
        if mode != Strictness::BestEffort {
            if !logical.is_empty() {
                return Err(InvalidFile::Syntax);
            }
            if [b"Name".as_slice(), b"Description", b"Version"]
                .iter()
                .any(|key| !package.0.contains_key(*key))
            {
                return Err(InvalidFile::Resolution);
            }
        }
        if mode == Strictness::Strict
            && [
                (b"Cflags".as_slice(), b"CFlags".as_slice()),
                (b"Cflags.private", b"CFlags.private"),
            ]
            .into_iter()
            .any(|(primary, alternate)| {
                package.0.contains_key(primary) && package.0.contains_key(alternate)
            })
        {
            return Err(InvalidFile::Resolution);
        }
        Ok(package)
    }

    fn insert(
        &mut self,
        key: &[u8],
        value: BString,
        mode: Strictness,
        limit: usize,
    ) -> Result<(), InvalidFile> {
        if let Some(previous) = self.0.get_mut(key) {
            if mode == Strictness::Strict || previous.len() + value.len() + 1 > limit {
                return Err(InvalidFile::Resolution);
            }
            if !previous.is_empty() && !value.is_empty() {
                previous.push(b' ');
            }
            previous.extend_from_slice(&value);
        } else {
            self.0.insert(key.into(), value);
        }
        Ok(())
    }
}

fn without_comment(line: &[u8]) -> &[u8] {
    let mut escaped = false;
    for (index, &byte) in line.iter().enumerate() {
        if byte == b'#' && !escaped {
            return line.split_at(index).0;
        }
        escaped = byte == b'\\' && !escaped;
    }
    line
}

fn expand(
    mut value: &[u8],
    variables: &BTreeMap<BString, BString>,
    mode: Strictness,
    limit: usize,
) -> Result<BString, InvalidFile> {
    let mut expanded = BString::default();
    while let Some((before, after)) = value.split_once_str(b"${") {
        let Some((key, rest)) = after.split_once_str(b"}") else {
            if mode != Strictness::BestEffort {
                return Err(InvalidFile::Syntax);
            }
            break;
        };
        let replacement = match variables.get(key) {
            Some(value) => value.as_slice(),
            None if mode == Strictness::Strict => return Err(InvalidFile::Resolution),
            None => b"",
        };
        if expanded.len() + before.len() + replacement.len() > limit {
            return Err(InvalidFile::Resolution);
        }
        expanded.extend_from_slice(before);
        expanded.extend_from_slice(replacement);
        value = rest;
    }
    if expanded.len() + value.len() > limit {
        return Err(InvalidFile::Resolution);
    }
    expanded.extend_from_slice(value);
    Ok(expanded.trim_ascii().into())
}

pub(super) fn requirements(mut input: &[u8]) -> impl Iterator<Item = (&[u8], BString)> {
    std::iter::from_fn(move || {
        input = trim_start(input, |byte| byte.is_ascii_whitespace() || byte == b',');
        if input.is_empty() {
            return None;
        }
        let name = take_until(&mut input, |byte| {
            byte.is_ascii_whitespace() || matches!(byte, b',' | b'<' | b'>' | b'=' | b'!')
        });
        input = input.trim_ascii_start();
        let operator = take_until(&mut input, |byte| {
            !matches!(byte, b'<' | b'>' | b'=' | b'!')
        });
        let constraint = if operator.is_empty() {
            BString::default()
        } else {
            input = input.trim_ascii_start();
            let version = take_until(&mut input, |byte| {
                byte.is_ascii_whitespace() || byte == b','
            });
            cat(&[operator, version])
        };
        Some((name, constraint))
    })
}

fn take_until<'a>(input: &mut &'a [u8], stop: impl Fn(u8) -> bool) -> &'a [u8] {
    let index = input
        .iter()
        .position(|&byte| stop(byte))
        .unwrap_or(input.len());
    let (value, rest) = input.split_at(index);
    *input = rest;
    value
}

pub(super) fn version_part(constraint: &[u8]) -> (&[u8], &[u8]) {
    let mut version = constraint.trim_ascii_start();
    let op = take_until(&mut version, |byte| {
        !matches!(byte, b'<' | b'>' | b'=' | b'!')
    });
    (op, version.trim_ascii())
}

pub(super) fn matches_version(actual: &[u8], constraint: &[u8], exact: bool) -> bool {
    let (op, expected) = version_part(constraint);
    if exact {
        return actual == expected;
    }
    if constraint.is_empty() {
        return true;
    }
    let order = compare_versions(actual, expected);
    match op {
        b"<" => order.is_lt(),
        b">" => order.is_gt(),
        b"<=" => !order.is_gt(),
        b">=" => !order.is_lt(),
        b"!=" => !order.is_eq(),
        b"" | b"=" => order.is_eq(),
        _ => false,
    }
}

fn compare_versions(mut left: &[u8], mut right: &[u8]) -> Ordering {
    loop {
        left = trim_separators(left);
        right = trim_separators(right);
        match (left, right) {
            ([b'~', a @ ..], [b'~', b @ ..]) => {
                left = a;
                right = b;
                continue;
            }
            ([b'~', ..], _) => return Ordering::Less,
            (_, [b'~', ..]) => return Ordering::Greater,
            ([], _) | (_, []) => return left.len().cmp(&right.len()),
            ([a, ..], [b, ..]) => {
                let numeric = a.is_ascii_digit();
                match numeric.cmp(&b.is_ascii_digit()) {
                    Ordering::Equal => {}
                    order => return order,
                }
                let stop =
                    |byte: u8| !byte.is_ascii_alphanumeric() || byte.is_ascii_digit() != numeric;
                let mut a = take_until(&mut left, stop);
                let mut b = take_until(&mut right, stop);
                if numeric {
                    a = trim_start(a, |byte| byte == b'0');
                    b = trim_start(b, |byte| byte == b'0');
                }
                let order = if numeric {
                    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                } else {
                    a.cmp(b)
                };
                if !order.is_eq() {
                    return order;
                }
            }
        }
    }
}

fn trim_separators(value: &[u8]) -> &[u8] {
    trim_start(value, |byte| !byte.is_ascii_alphanumeric() && byte != b'~')
}

fn trim_start(value: &[u8], skip: impl Fn(u8) -> bool) -> &[u8] {
    let mut rest = value;
    while let [byte, tail @ ..] = rest {
        if !skip(*byte) {
            break;
        }
        rest = tail;
    }
    rest
}
