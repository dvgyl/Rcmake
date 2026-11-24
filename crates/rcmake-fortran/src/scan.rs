use std::collections::BTreeSet;
use std::fmt;

use rcmake_base::{BString, Limits};

use crate::{SourceForm, source::identifier};

const MAX_DEPTH: usize = 64;
const MAX_BRANCH_STATES: usize = 32;

const INTRINSIC_MODULES: &[&[u8]] = &[
    b"iso_c_binding",
    b"iso_fortran_env",
    b"ieee_exceptions",
    b"ieee_arithmetic",
    b"ieee_features",
    b"omp_lib",
    b"omp_lib_kinds",
    b"openacc",
    b"openacc_kinds",
];

/// A source file and its scan settings.
#[derive(Clone, Copy, Debug)]
pub struct ScanInput<'a> {
    /// Source file identity for include resolution.
    pub path: &'a [u8],
    /// Source file bytes.
    pub text: &'a [u8],
    /// Source form for this file and its includes.
    pub form: SourceForm,
    /// Treat unqualified uses of standard intrinsic modules as intrinsic.
    ///
    /// Set this to false to match CMake's qualifier-only scan.
    pub implicit_intrinsics: bool,
    /// Track explicitly intrinsic uses as dependencies when building intrinsic modules.
    pub building_intrinsic_modules: bool,
}

impl<'a> ScanInput<'a> {
    /// Construct scan settings with the conventional intrinsic module filter enabled.
    pub const fn new(path: &'a [u8], text: &'a [u8], form: SourceForm) -> Self {
        Self {
            path,
            text,
            form,
            implicit_intrinsics: true,
            building_intrinsic_modules: false,
        }
    }
}

/// Logical dependencies found in a source file and its includes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FortranSourceInfo {
    /// Lowercase module names and ancestor@child submodule names defined here.
    pub provides: BTreeSet<BString>,
    /// Lowercase module and submodule names needed to compile this source.
    pub requires: BTreeSet<BString>,
    /// Lowercase module names used as intrinsics.
    pub intrinsic: BTreeSet<BString>,
    /// Include names as written, with string escapes decoded.
    pub includes: BTreeSet<BString>,
    /// Resolved include paths suitable for build prerequisites.
    pub resolved_includes: BTreeSet<BString>,
}

/// Supplies include contents from memory. The caller controls the search order.
pub trait IncludeResolver {
    /// Return the resolved path and bytes for a directive, if available.
    ///
    /// The same file must have the same resolved path when reached by an alias.
    fn read(&mut self, from: &[u8], name: &[u8], quoted: bool) -> Option<(BString, BString)>;
}

/// A scan exceeded a resource limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanError {
    /// The combined source and include bytes exceeded the read budget.
    SourceBytes,
    /// The include nesting limit was reached.
    IncludeDepth,
    /// The conditional nesting or continuation state limit was reached.
    ConditionalLimit,
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SourceBytes => "Fortran source bytes exceed the scan limit.",
            Self::IncludeDepth => "Fortran include depth exceeds the scan limit.",
            Self::ConditionalLimit => "Fortran conditional state exceeds the scan limit.",
        })
    }
}

impl std::error::Error for ScanError {}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Include {
    name: BString,
    quoted: bool,
}

#[derive(Default)]
struct FileScan {
    info: FortranSourceInfo,
    includes: BTreeSet<Include>,
}

/// Scan one source and, optionally, its includes. Visit all preprocessor branches.
///
/// A resolver can supply include files from memory. Unresolved include names
/// remain in the result. A resolved file is scanned once, including in a cycle.
pub fn scan(
    input: &ScanInput<'_>,
    mut resolver: Option<&mut dyn IncludeResolver>,
    limits: &Limits,
) -> Result<FortranSourceInfo, ScanError> {
    let mut budget = limits.max_file_read;
    let root = scan_file(input.text, input, &mut budget, limits)?;
    let mut result = root.info;
    let mut seen = BTreeSet::from([BString::from(input.path)]);
    let mut stack = vec![(BString::from(input.path), root.includes.into_iter())];
    while let Some((path, includes)) = stack.last_mut() {
        let Some(include) = includes.next() else {
            stack.pop();
            continue;
        };
        result.includes.insert(include.name.clone());
        let Some((resolved, text)) = resolver
            .as_deref_mut()
            .and_then(|resolver| resolver.read(path, &include.name, include.quoted))
        else {
            continue;
        };
        if resolved.as_slice() != input.path {
            result.resolved_includes.insert(resolved.clone());
        }
        if seen.insert(resolved.clone()) {
            if stack.len() >= MAX_DEPTH.min(limits.max_recursion as usize) {
                return Err(ScanError::IncludeDepth);
            }
            let mut child = scan_file(&text, input, &mut budget, limits)?;
            result.provides.append(&mut child.info.provides);
            result.requires.append(&mut child.info.requires);
            result.intrinsic.append(&mut child.info.intrinsic);
            stack.push((resolved, child.includes.into_iter()));
        }
    }
    Ok(result)
}

#[derive(Clone, Default, Eq, Ord, PartialEq, PartialOrd)]
struct LogicalLine {
    text: Vec<u8>,
    quote: Option<u8>,
}

#[derive(Default)]
struct Conditional {
    start: Vec<LogicalLine>,
    finished: Vec<LogicalLine>,
    has_else: bool,
}

fn scan_file(
    text: &[u8],
    input: &ScanInput<'_>,
    budget: &mut u64,
    limits: &Limits,
) -> Result<FileScan, ScanError> {
    *budget = budget
        .checked_sub(text.len() as u64)
        .ok_or(ScanError::SourceBytes)?;
    let mut output = FileScan::default();
    let mut states = vec![LogicalLine::default()];
    let mut conditions: Vec<Conditional> = Vec::new();
    let mut saved_bytes = 0usize;
    let mut lines = text
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(text)
        .split(|&byte| byte == b'\n');
    while let Some(line) = lines.next() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(directive) = line.trim_ascii_start().strip_prefix(b"#") {
            let directive = splice_directive(directive, &mut lines);
            let (word, rest) = word(&directive);
            match word {
                b"include" => {
                    if let Some(include) = include_name(rest, true) {
                        output.includes.insert(include);
                    }
                }
                b"if" | b"ifdef" | b"ifndef" => {
                    if conditions.len() >= MAX_DEPTH.min(limits.max_recursion as usize)
                        || saved_bytes.saturating_add(state_bytes(&states).saturating_mul(2))
                            > limits.max_value_bytes
                    {
                        return Err(ScanError::ConditionalLimit);
                    }
                    conditions.push(Conditional {
                        start: states.clone(),
                        ..Conditional::default()
                    });
                }
                b"elif" | b"else" => {
                    if let Some(branch) = conditions.last_mut() {
                        if saved_bytes
                            .saturating_add(state_bytes(&states))
                            .saturating_add(state_bytes(&branch.start))
                            > limits.max_value_bytes
                        {
                            return Err(ScanError::ConditionalLimit);
                        }
                        branch.finished.append(&mut states);
                        normalize_states(&mut branch.finished)?;
                        states.clone_from(&branch.start);
                        branch.has_else |= word == b"else";
                    }
                }
                b"endif" => {
                    if let Some(mut branch) = conditions.pop() {
                        states.append(&mut branch.finished);
                        if !branch.has_else {
                            states.append(&mut branch.start);
                        }
                        normalize_states(&mut states)?;
                    }
                }
                _ => {}
            }
            saved_bytes = conditions
                .iter()
                .map(|branch| state_bytes(&branch.start) + state_bytes(&branch.finished))
                .sum();
            if saved_bytes.saturating_add(state_bytes(&states)) > limits.max_value_bytes {
                return Err(ScanError::ConditionalLimit);
            }
            continue;
        }
        if saved_bytes
            .saturating_add(state_bytes(&states))
            .saturating_add(states.len().saturating_mul(line.len() + 1))
            > limits.max_value_bytes
        {
            return Err(ScanError::ConditionalLimit);
        }
        for state in &mut states {
            state.push(line, input, &mut output);
        }
        if states.len() > 1 {
            normalize_states(&mut states)?;
        }
    }
    // Well-formed branches reunite at endif. Preserve pending statements in
    // malformed input too, so a missing endif cannot hide a dependency.
    for branch in conditions {
        states.extend(branch.finished);
        states.extend(branch.start);
        normalize_states(&mut states)?;
    }
    for mut state in states {
        state.flush(input, &mut output);
    }
    Ok(output)
}

fn state_bytes(states: &[LogicalLine]) -> usize {
    states.iter().map(|state| state.text.len()).sum()
}

fn normalize_states(states: &mut Vec<LogicalLine>) -> Result<(), ScanError> {
    states.sort_unstable();
    states.dedup();
    if states.len() > MAX_BRANCH_STATES {
        return Err(ScanError::ConditionalLimit);
    }
    Ok(())
}

fn splice_directive<'a>(line: &[u8], lines: &mut impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut directive = line.to_vec();
    while directive.ends_with(b"\\") {
        directive.pop();
        let Some(next) = lines.next() else { break };
        directive.extend_from_slice(next.strip_suffix(b"\r").unwrap_or(next));
    }
    directive
}

impl LogicalLine {
    fn push(&mut self, line: &[u8], input: &ScanInput<'_>, output: &mut FileScan) {
        let trimmed = line.trim_ascii_start();
        if trimmed.is_empty() {
            return;
        }
        match input.form {
            SourceForm::Free => {
                let line = trimmed;
                if line.starts_with(b"!") {
                    return;
                }
                let fragment =
                    if let Some(rest) = line.strip_prefix(b"&").filter(|_| !self.text.is_empty()) {
                        rest
                    } else {
                        if !self.text.is_empty() {
                            self.text.push(b' ');
                        }
                        line
                    };
                let (fragment, quote) = strip_comment(fragment, self.quote);
                let fragment = fragment.trim_ascii_end();
                if let Some(rest) = fragment.strip_suffix(b"&") {
                    self.quote = quote;
                    self.text.extend_from_slice(rest);
                } else {
                    self.text.extend_from_slice(fragment);
                    self.flush(input, output);
                }
            }
            SourceForm::Fixed => {
                if trimmed.starts_with(b"!") && line.len() - trimmed.len() != 5 {
                    return;
                }
                if line
                    .first()
                    .is_some_and(|byte| matches!(byte, b'C' | b'c' | b'*' | b'!'))
                {
                    return;
                }
                let continued = line
                    .get(5)
                    .is_some_and(|byte| !matches!(byte, b' ' | b'0' | b'\t'));
                let fragment = line.get(6..line.len().min(72)).unwrap_or_default();
                if !continued {
                    self.flush(input, output);
                }
                let (fragment, quote) = strip_comment(fragment, self.quote);
                self.quote = quote;
                self.text.extend_from_slice(fragment);
            }
        }
    }

    fn flush(&mut self, input: &ScanInput<'_>, output: &mut FileScan) {
        let mut quote = None;
        let mut start = 0;
        for (index, &byte) in self.text.iter().enumerate() {
            if byte == b';' && quote.is_none() {
                if let Some(statement) = self.text.get(start..index) {
                    scan_statement(statement, input, output);
                }
                start = index + 1;
            }
            update_quote(&mut quote, byte);
        }
        if let Some(statement) = self.text.get(start..) {
            scan_statement(statement, input, output);
        }
        self.quote = None;
        if self.text.capacity() > 4096 {
            self.text = Vec::new();
        } else {
            self.text.clear();
        }
    }
}

fn strip_comment(line: &[u8], mut quote: Option<u8>) -> (&[u8], Option<u8>) {
    for (index, &byte) in line.iter().enumerate() {
        if byte == b'!' && quote.is_none() {
            return (line.get(..index).unwrap_or_default(), quote);
        }
        update_quote(&mut quote, byte);
    }
    (line, quote)
}

fn update_quote(quote: &mut Option<u8>, byte: u8) {
    match *quote {
        Some(delimiter) if delimiter == byte => *quote = None,
        None if matches!(byte, b'\'' | b'"') => *quote = Some(byte),
        _ => {}
    }
}

fn scan_statement(mut text: &[u8], input: &ScanInput<'_>, output: &mut FileScan) {
    text = text.trim_ascii_start();
    let label = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if let Some(rest) = text.get(label..) {
        text = rest;
    }
    let (command, rest) = word(text);
    match command.to_ascii_lowercase().as_slice() {
        b"module" => {
            let (name, tail) = word(rest);
            let name = name.to_ascii_lowercase();
            if identifier(&name)
                && tail.trim_ascii().is_empty()
                && !matches!(name.as_slice(), b"procedure" | b"subroutine" | b"function")
            {
                output.info.provides.insert(name.into());
            }
        }
        b"submodule" => submodule(rest, &mut output.info),
        b"use" => use_module(rest, input, &mut output.info),
        b"include" => {
            if let Some(include) = include_name(rest, false) {
                output.includes.insert(include);
            }
        }
        _ => {}
    }
}

fn word(text: &[u8]) -> (&[u8], &[u8]) {
    let text = text.trim_ascii_start();
    let end = text
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    let (word, rest) = text.split_at(end);
    (word, rest.trim_ascii_start())
}

fn split_once(text: &[u8], separator: u8) -> Option<(&[u8], &[u8])> {
    let index = text.iter().position(|&byte| byte == separator)?;
    Some((text.get(..index)?, text.get(index + 1..)?))
}

fn submodule(text: &[u8], output: &mut FortranSourceInfo) {
    let Some((parents, rest)) = text
        .strip_prefix(b"(")
        .and_then(|rest| split_once(rest, b')'))
    else {
        return;
    };
    let (child, tail) = word(rest);
    let (root, parent) = split_once(parents, b':')
        .map_or((parents.trim_ascii(), None), |(root, parent)| {
            (root.trim_ascii(), Some(parent.trim_ascii()))
        });
    if !identifier(root)
        || !identifier(child)
        || !tail.trim_ascii().is_empty()
        || parent.is_some_and(|parent| !identifier(parent))
    {
        return;
    }
    let qualified = |name: &[u8]| BString::from([root, b"@", name].concat().to_ascii_lowercase());
    output.provides.insert(qualified(child));
    output
        .requires
        .insert(parent.map_or_else(|| root.to_ascii_lowercase().into(), qualified));
}

fn use_module(mut text: &[u8], input: &ScanInput<'_>, output: &mut FortranSourceInfo) {
    let mut qualified = None;
    if let Some(rest) = text.strip_prefix(b",") {
        let (qualifier, rest) = word(rest);
        qualified = if qualifier.eq_ignore_ascii_case(b"intrinsic") {
            Some(true)
        } else if qualifier.eq_ignore_ascii_case(b"non_intrinsic") {
            Some(false)
        } else {
            return;
        };
        let Some(rest) = rest.strip_prefix(b"::") else {
            return;
        };
        text = rest;
    } else if let Some(rest) = text.strip_prefix(b"::") {
        text = rest;
    }
    let (name, rest) = word(text);
    if !identifier(name) || (!rest.is_empty() && !rest.starts_with(b",")) {
        return;
    }
    let name = name.to_ascii_lowercase();
    let intrinsic = qualified.unwrap_or_else(|| {
        input.implicit_intrinsics && INTRINSIC_MODULES.contains(&name.as_slice())
    });
    if intrinsic {
        output.intrinsic.insert(BString::from(name.clone()));
    }
    if !intrinsic || input.building_intrinsic_modules {
        output.requires.insert(name.into());
    }
}

fn include_name(text: &[u8], cpp: bool) -> Option<Include> {
    let (&open, rest) = text.split_first()?;
    let close = match open {
        b'\'' if !cpp => b'\'',
        b'"' => b'"',
        b'<' if cpp => b'>',
        _ => return None,
    };
    let mut name = BString::default();
    let mut bytes = rest.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            0 => return None,
            byte if byte == close => {
                if !cpp && bytes.next_if_eq(&close).is_some() {
                    name.push(byte);
                    continue;
                }
                return (!name.is_empty() && (cpp || bytes.all(|byte| byte.is_ascii_whitespace())))
                    .then_some(Include {
                        name,
                        quoted: open != b'<',
                    });
            }
            _ => name.push(byte),
        }
    }
    None
}
