use std::path::Path;

use rcmake_base::{BString, Limits};

use crate::{ArchiveError, MAX_NAME_BYTES, archive_name};

pub(super) struct Patterns<'a> {
    items: Vec<Pattern<'a>>,
    steps_left: u64,
}

struct Pattern<'a> {
    source: &'a BString,
    tokens: Vec<Token>,
    anchored: bool,
    matched: bool,
}

enum Token {
    Literal(u8),
    Any,
    Star,
    Class {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
}

impl<'a> Patterns<'a> {
    pub(super) fn new(patterns: &'a [BString]) -> Result<Self, ArchiveError> {
        let mut bytes_left = MAX_NAME_BYTES;
        let mut items = Vec::new();
        for source in patterns {
            bytes_left = bytes_left
                .checked_sub(source.len() + 1)
                .ok_or_else(|| ArchiveError::new("Archive patterns exceed the size limit"))?;
            let escape_count = source
                .iter()
                .rev()
                .skip(1)
                .take_while(|&&byte| byte == b'\\')
                .count();
            let anchored = source.ends_with(b"$") && escape_count.is_multiple_of(2);
            let pattern = if anchored {
                source.strip_suffix(b"$").unwrap_or(source)
            } else {
                source
            };
            let pattern = pattern
                .split(|&byte| byte == b'/')
                .filter(|part| !part.is_empty() && *part != b".")
                .collect::<Vec<_>>()
                .join(&b'/');
            items.push(Pattern {
                source,
                tokens: tokenize(&pattern),
                anchored,
                matched: false,
            });
        }
        Ok(Self {
            items,
            steps_left: Limits::default().regex_steps,
        })
    }

    pub(super) fn matches(
        &mut self,
        path: &Path,
        directory_slash: bool,
    ) -> Result<bool, ArchiveError> {
        if self.items.is_empty() {
            return Ok(true);
        }
        let mut name = archive_name(path);
        if directory_slash {
            name.push(b'/');
        }
        let mut matched = false;
        for pattern in &mut self.items {
            if pattern.matches(&name, &mut self.steps_left)? {
                pattern.matched = true;
                matched = true;
            }
        }
        Ok(matched)
    }

    pub(super) fn excludes(&mut self, path: &Path, directory: bool) -> Result<bool, ArchiveError> {
        if self.items.is_empty() {
            return Ok(false);
        }
        let mut name = archive_name(path);
        if directory {
            name.push(b'/');
        }
        for pattern in &mut self.items {
            let mut candidate = name.as_slice();
            loop {
                if pattern.matches(candidate, &mut self.steps_left)? {
                    return Ok(true);
                }
                let Some(separator) = candidate.iter().position(|&byte| byte == b'/') else {
                    break;
                };
                let Some(suffix) = candidate
                    .get(separator + 1..)
                    .filter(|suffix| !suffix.is_empty())
                else {
                    break;
                };
                candidate = suffix;
            }
        }
        Ok(false)
    }

    pub(super) fn finish(&self) -> Result<(), ArchiveError> {
        if let Some(pattern) = self.items.iter().find(|item| !item.matched) {
            return Err(ArchiveError::new(format!(
                "{}: Not found in archive",
                pattern.source
            )));
        }
        Ok(())
    }
}

impl Pattern<'_> {
    fn matches(&self, mut name: &[u8], steps_left: &mut u64) -> Result<bool, ArchiveError> {
        let mut tokens = self.tokens.as_slice();
        let mut retry = None;
        loop {
            spend(steps_left, 1)?;
            match (tokens.split_first(), name.split_first()) {
                (None, None) => return Ok(true),
                (None, Some((b'/', rest))) if !self.anchored || rest.is_empty() => return Ok(true),
                (Some((Token::Star, rest)), _) => {
                    retry = Some((rest, name));
                    tokens = rest;
                    continue;
                }
                (Some((token, rest)), Some((&byte, suffix)))
                    if token.matches(byte, steps_left)? =>
                {
                    tokens = rest;
                    name = suffix;
                    continue;
                }
                _ => {}
            }
            let Some((after_star, bytes)) = retry else {
                return Ok(false);
            };
            let Some((_, suffix)) = bytes.split_first() else {
                return Ok(false);
            };
            retry = Some((after_star, suffix));
            tokens = after_star;
            name = suffix;
        }
    }
}

impl Token {
    fn matches(&self, byte: u8, steps_left: &mut u64) -> Result<bool, ArchiveError> {
        Ok(match self {
            Self::Literal(value) => *value == byte,
            Self::Any => true,
            Self::Star => false,
            Self::Class { negated, ranges } => {
                spend(steps_left, ranges.len() as u64)?;
                *negated
                    != ranges
                        .iter()
                        .any(|&(first, last)| (first..=last).contains(&byte))
            }
        })
    }
}

fn spend(steps_left: &mut u64, steps: u64) -> Result<(), ArchiveError> {
    *steps_left = steps_left
        .checked_sub(steps)
        .ok_or_else(|| ArchiveError::new("Archive pattern matching exceeds the step limit"))?;
    Ok(())
}

fn tokenize(mut pattern: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut can_open_class = true;
    while let Some((&byte, rest)) = pattern.split_first() {
        pattern = rest;
        tokens.push(match byte {
            b'*' => Token::Star,
            b'?' => Token::Any,
            b'\\' if !rest.is_empty() => {
                if let Some((&literal, suffix)) = rest.split_first() {
                    pattern = suffix;
                    Token::Literal(literal)
                } else {
                    Token::Literal(byte)
                }
            }
            b'[' if can_open_class => {
                if let Some((class, suffix)) = parse_class(rest) {
                    pattern = suffix;
                    class
                } else {
                    can_open_class = false;
                    Token::Literal(byte)
                }
            }
            _ => Token::Literal(byte),
        });
    }
    tokens
}

fn parse_class(pattern: &[u8]) -> Option<(Token, &[u8])> {
    let negated = matches!(pattern.first(), Some(b'!' | b'^'));
    let members = pattern.get(usize::from(negated)..)?;
    let skip = usize::from(members.starts_with(b"]"));
    let end = skip + members.get(skip..)?.iter().position(|&byte| byte == b']')?;
    let mut members_end = members.get(..end)?;
    let mut ranges = Vec::new();
    while let Some((&first, rest)) = members_end.split_first() {
        if let [b'-', last, suffix @ ..] = rest {
            ranges.push((first, *last));
            members_end = suffix;
        } else {
            ranges.push((first, first));
            members_end = rest;
        }
    }
    Some((Token::Class { negated, ranges }, members.get(end + 1..)?))
}
