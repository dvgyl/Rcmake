use std::ops::Range;

use rcmake_syntax::{ArgKind, Argument, CommandInvocation};

use crate::position::Lines;

// The strict parser still supplies diagnostics. This scanner keeps source spans
// and open invocations available when the document has a syntax error.
pub(crate) fn recover(text: &str, lines: &Lines) -> (Vec<CommandInvocation>, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut cursor = usize::from(text.starts_with('\u{feff}')) * 3;
    let mut commands = Vec::new();
    let mut unfinished = Vec::new();
    let mut current: Option<CommandInvocation> = None;
    let mut nested = 0usize;
    let mut first_on_line = true;

    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b'\n' => {
                first_on_line = true;
                cursor += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                cursor += 1;
                continue;
            }
            b'#' => {
                cursor = comment_end(bytes, cursor);
                first_on_line = false;
                continue;
            }
            _ => {}
        }
        let line = lines.line_at(cursor);
        let line_number = u32::try_from(line + 1).unwrap_or(u32::MAX);
        if (current.is_none() || (first_on_line && nested == 0))
            && let Some((name_end, open)) = command_start(bytes, cursor)
            && current.as_ref().is_none_or(|command| {
                let indent = cursor - lines.start(line).unwrap_or_default();
                let opening_indent = command.span.start
                    - lines
                        .start(command.line.saturating_sub(1) as usize)
                        .unwrap_or_default();
                indent <= opening_indent
                    || bytes.get(cursor..name_end).is_some_and(is_block_boundary)
            })
        {
            if let Some(mut prior) = current.take() {
                prior.span.end = cursor;
                prior.close_line = line_number;
                unfinished.push(prior.span.start);
                commands.push(prior);
            }
            let name = bytes.get(cursor..name_end).unwrap_or_default();
            current = Some(CommandInvocation {
                name: name.into(),
                lower: name.to_ascii_lowercase().into(),
                line: line_number,
                close_line: line_number,
                span: cursor..open + 1,
                args: Vec::new(),
            });
            cursor = open + 1;
            first_on_line = false;
            continue;
        }
        first_on_line = false;
        let start = cursor;
        let (end, content, kind) = match byte {
            b'(' => {
                nested += usize::from(current.is_some());
                (start + 1, start..start + 1, ArgKind::Unquoted)
            }
            b')' if nested == 0 => {
                cursor += 1;
                if let Some(mut command) = current.take() {
                    command.span.end = cursor;
                    command.close_line = line_number;
                    commands.push(command);
                }
                continue;
            }
            b')' => {
                nested -= 1;
                (start + 1, start..start + 1, ArgKind::Unquoted)
            }
            b'"' => {
                let (end, closed) = quoted_end(bytes, start);
                (end, start + 1..end - usize::from(closed), ArgKind::Quoted)
            }
            b'[' if let Some((end, content)) = bracket(bytes, start) => {
                (end, content, ArgKind::Bracket)
            }
            _ => {
                let end = unquoted_end(bytes, start);
                (end, start..end, ArgKind::Unquoted)
            }
        };
        cursor = end;
        if let Some(command) = current.as_mut() {
            command.args.push(Argument {
                kind,
                text: bytes.get(content).unwrap_or_default().into(),
                span: start..end,
                line: line_number,
            });
        }
    }
    if let Some(mut command) = current {
        command.span.end = bytes.len();
        command.close_line = u32::try_from(lines.line_at(bytes.len()) + 1).unwrap_or(u32::MAX);
        unfinished.push(command.span.start);
        commands.push(command);
    }
    (commands, unfinished)
}

fn is_block_boundary(name: &[u8]) -> bool {
    [
        b"else".as_slice(),
        b"elseif",
        b"endif",
        b"endforeach",
        b"endwhile",
        b"endfunction",
        b"endmacro",
        b"endblock",
    ]
    .iter()
    .any(|boundary| name.eq_ignore_ascii_case(boundary))
}

fn command_start(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let tail = bytes.get(start..)?;
    let first = *tail.first()?;
    if first != b'_' && !first.is_ascii_alphabetic() {
        return None;
    }
    let name_len = tail
        .iter()
        .take_while(|&&byte| byte == b'_' || byte.is_ascii_alphanumeric())
        .count();
    let after_name = tail.get(name_len..)?;
    let space = after_name
        .iter()
        .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
        .count();
    (after_name.get(space) == Some(&b'(')).then_some((start + name_len, start + name_len + space))
}

pub(crate) fn comment_end(bytes: &[u8], start: usize) -> usize {
    if let Some((end, _)) = bracket(bytes, start + 1) {
        return end;
    }
    let text = bytes.get(start..).unwrap_or_default();
    start
        + text
            .iter()
            .position(|&byte| matches!(byte, b'\r' | b'\n'))
            .unwrap_or(text.len())
}

pub(crate) fn quoted_end(bytes: &[u8], start: usize) -> (usize, bool) {
    let mut cursor = start + 1;
    while let Some(&byte) = bytes.get(cursor) {
        cursor += 1;
        if byte == b'"' {
            return (cursor, true);
        }
        if byte == b'\\' && cursor < bytes.len() {
            cursor += 1;
        }
    }
    (cursor, false)
}

pub(crate) fn bracket(bytes: &[u8], start: usize) -> Option<(usize, Range<usize>)> {
    let tail = bytes.get(start + 1..)?;
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let equals = tail.iter().take_while(|&&byte| byte == b'=').count();
    if tail.get(equals) != Some(&b'[') {
        return None;
    }
    let mut content = start + equals + 2;
    content += match bytes.get(content..) {
        Some([b'\r', b'\n', ..]) => 2,
        Some([b'\n', ..]) => 1,
        _ => 0,
    };
    let mut cursor = content;
    while let Some(&byte) = bytes.get(cursor) {
        if byte == b']' {
            let tail = bytes.get(cursor + 1..).unwrap_or_default();
            let closing_equals = tail.iter().take_while(|&&byte| byte == b'=').count();
            if closing_equals == equals && tail.get(closing_equals) == Some(&b']') {
                return Some((cursor + closing_equals + 2, content..cursor));
            }
            cursor += closing_equals;
        }
        cursor += 1;
    }
    Some((bytes.len(), content..bytes.len()))
}

fn unquoted_end(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start;
    while let Some(&byte) = bytes.get(cursor) {
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'#' => break,
            b'\\'
                if bytes
                    .get(cursor + 1)
                    .is_some_and(|b| !matches!(b, b'\r' | b'\n')) =>
            {
                cursor += 1;
            }
            b'$' if bytes.get(cursor + 1) == Some(&b'(') => {
                let name = bytes.get(cursor + 2..).unwrap_or_default();
                let len = name
                    .iter()
                    .take_while(|&&byte| byte == b'_' || byte.is_ascii_alphanumeric())
                    .count();
                if name.get(len) == Some(&b')') {
                    cursor += len + 2;
                }
            }
            // A quote in a legacy unquoted argument can include spaces, but
            // cannot include a newline, comment delimiter, or parenthesis.
            b'"' => {
                let (end, closed) = quoted_end(bytes, cursor);
                if closed
                    && !bytes
                        .get(cursor + 1..end - 1)
                        .unwrap_or_default()
                        .iter()
                        .any(|byte| matches!(byte, b'\r' | b'\n' | b'#' | b'(' | b')'))
                {
                    cursor = end;
                    continue;
                }
                break;
            }
            _ => {}
        }
        cursor += 1;
    }
    cursor.max(start + 1)
}
