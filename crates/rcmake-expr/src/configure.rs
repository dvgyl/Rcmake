use rcmake_base::{BString, ByteSlice, limits::Limits, truth::is_off};

use crate::{
    ExpandEnv, ExpandError,
    expand::{Mode, error, expand},
};

/// Options for `configure_file` substitution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfigureOptions {
    /// Expand only `@VAR@` references (`@ONLY`).
    pub at_only: bool,
    /// Escape double quotes in substituted values (`ESCAPE_QUOTES`).
    pub escape_quotes: bool,
}

/// Expand a template. Emit a line feed after each input line.
pub fn configure(
    input: &[u8],
    env: &mut dyn ExpandEnv,
    options: ConfigureOptions,
) -> Result<BString, ExpandError> {
    let mut output = Vec::new();
    let line_number = env.current_line();
    let mode = Mode::Template {
        at_only: options.at_only,
        escape_quotes: options.escape_quotes,
    };
    let limit = Limits::default().max_value_bytes;
    for line in input.split_inclusive(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let directive = find_directive(line, b"cmakedefine")
            .map(|directive| (directive, false))
            .or_else(|| find_directive(line, b"cmakedefine01").map(|directive| (directive, true)));
        let modified;
        let line = if let Some((directive, binary)) = directive {
            modified = configure_directive(line, directive, binary, env);
            modified.as_slice()
        } else {
            line
        };
        let expanded = expand(line, line_number, env, mode)?;
        if output
            .len()
            .saturating_add(expanded.len())
            .saturating_add(1)
            > limit
        {
            return Err(error("Template expansion exceeded the value size limit."));
        }
        output.extend_from_slice(&expanded);
        output.push(b'\n');
    }
    Ok(output.into())
}

struct Directive<'a> {
    marker: &'a [u8],
    name: &'a [u8],
}

fn find_directive<'a>(line: &'a [u8], keyword: &[u8]) -> Option<Directive<'a>> {
    for (hash, _) in line.iter().enumerate().filter(|&(_, &byte)| byte == b'#') {
        let after_hash = line.get(hash + 1..)?;
        let text = trim_horizontal_space(after_hash);
        let Some(tail) = text
            .strip_prefix(keyword)
            .filter(|tail| matches!(tail.first(), Some(b' ' | b'\t')))
        else {
            continue;
        };
        let end = line.len() - tail.len();
        let tail = trim_horizontal_space(tail);
        let name_end = tail
            .iter()
            .take_while(|&&byte| byte.is_ascii_alphanumeric() || byte == b'_')
            .count();
        return Some(Directive {
            marker: line.get(hash..end)?,
            name: tail.get(..name_end)?,
        });
    }
    None
}

fn configure_directive(
    line: &[u8],
    directive: Directive<'_>,
    binary: bool,
    env: &mut dyn ExpandEnv,
) -> Vec<u8> {
    let enabled = env.var(directive.name).is_some_and(|value| !is_off(&value));
    if !binary && !enabled {
        let mut output = b"/* #undef ".to_vec();
        output.extend_from_slice(directive.name);
        output.extend_from_slice(b" */");
        return output;
    }

    let padding = if binary {
        b"cmakedefine01".as_slice()
    } else {
        b"cmakedefine"
    };
    let mut replacement = directive
        .marker
        .strip_suffix(padding)
        .unwrap_or_default()
        .to_vec();
    replacement.extend_from_slice(b"define");
    let mut output = line.replace(directive.marker, replacement);

    if binary {
        output.extend_from_slice(if enabled { b" 1" } else { b" 0" });
    }
    output
}

fn trim_horizontal_space(bytes: &[u8]) -> &[u8] {
    let leading_space = bytes
        .iter()
        .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
        .count();
    bytes.get(leading_space..).unwrap_or_default()
}
