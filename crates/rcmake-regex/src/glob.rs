//! Convert CMake glob patterns to regular expressions.

use rcmake_base::BString;

/// Convert a CMake glob pattern to an anchored CMake regular expression.
///
/// Apply the resulting expression to a single path component for `file(GLOB)`.
/// A backslash outside a class makes the next pattern byte literal.
#[must_use]
pub fn to_regex(glob: &[u8]) -> BString {
    let mut out = vec![b'^'];
    let mut input = glob;
    let mut can_open_class = true;
    while let Some((&byte, rest)) = input.split_first() {
        input = rest;
        match byte {
            b'*' => out.extend_from_slice(b".*"),
            b'?' => out.push(b'.'),
            b'\\' => {
                let literal = if let Some((&literal, rest)) = input.split_first() {
                    input = rest;
                    literal
                } else {
                    byte
                };
                push_literal(&mut out, literal);
            }
            b'[' if can_open_class => {
                if let Some((negated, members, rest)) = split_class(input) {
                    out.push(b'[');
                    if negated {
                        out.push(b'^');
                    }
                    out.extend_from_slice(members);
                    out.push(b']');
                    input = rest;
                } else {
                    // No closing bracket remains for any later class.
                    can_open_class = false;
                    push_literal(&mut out, byte);
                }
            }
            _ => push_literal(&mut out, byte),
        }
    }
    out.push(b'$');
    BString::from(out)
}

fn split_class(input: &[u8]) -> Option<(bool, &[u8], &[u8])> {
    let negated = matches!(input.first(), Some(b'!' | b'^'));
    let members = if negated { input.get(1..)? } else { input };
    let first_close = usize::from(members.first() == Some(&b']'));
    let end = first_close
        + members
            .get(first_close..)?
            .iter()
            .position(|&b| b == b']')?;
    Some((negated, members.get(..end)?, members.get(end + 1..)?))
}

fn push_literal(out: &mut Vec<u8>, byte: u8) {
    if matches!(
        byte,
        b'\\' | b'.' | b'^' | b'$' | b'*' | b'+' | b'?' | b'(' | b')' | b'[' | b'|'
    ) {
        out.push(b'\\');
    }
    out.push(byte);
}
