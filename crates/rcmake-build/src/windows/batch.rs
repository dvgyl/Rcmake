// SPDX-License-Identifier: MIT OR Apache-2.0

use rcmake_base::BString;
use rcmake_host::HostError;

pub(super) fn render(
    rule: &[u8],
    mut prepare: impl FnMut(&[BString]) -> Result<(), HostError>,
) -> Result<Vec<u8>, HostError> {
    let mut result = Vec::new();
    let mut words = Vec::new();
    let mut word = BString::default();
    let mut active = false;
    let mut quote = None;
    let mut bytes = rule.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match (quote, byte) {
            (_, 0) | (Some(_), b'\r' | b'\n') => return Err(invalid_rule()),
            (Some(delimiter), byte) if delimiter == byte => quote = None,
            (Some(b'\''), _) => word.push(byte),
            (None, b'\'' | b'"') => {
                quote = Some(byte);
                active = true;
            }
            (_, b'\\') if bytes.peek() == Some(&b'\'') && quote.is_none() => {
                bytes.next();
                word.push(b'\'');
                active = true;
            }
            (_, b'\\') => {
                let mut count = 1;
                while bytes.next_if_eq(&b'\\').is_some() {
                    count += 1;
                }
                if quote == Some(b'"') && bytes.next_if_eq(&b'"').is_some() {
                    word.extend(std::iter::repeat_n(b'\\', count / 2));
                    if count % 2 == 0 {
                        quote = None;
                    } else {
                        word.push(b'"');
                    }
                } else if quote.is_none() && count == 1 && bytes.next_if_eq(&b'"').is_some() {
                    word.push(b'"');
                } else {
                    word.extend(std::iter::repeat_n(b'\\', count));
                }
                active = true;
            }
            (None, b' ' | b'\t') => flush(&mut result, &mut words, &mut word, &mut active),
            (None, b'&' | b'|' | b'<' | b'>' | b'(' | b')' | b'\r' | b'\n') => {
                let mut operator = Vec::new();
                if matches!(byte, b'<' | b'>')
                    && active
                    && !word.is_empty()
                    && word.iter().all(u8::is_ascii_digit)
                {
                    operator.extend_from_slice(&word);
                    word.clear();
                    active = false;
                }
                operator.push(byte);
                match byte {
                    b'<' | b'>' if bytes.next_if_eq(&b'&').is_some() => {
                        operator.push(b'&');
                        while let Some(digit) = bytes.next_if(u8::is_ascii_digit) {
                            operator.push(digit);
                        }
                    }
                    b'&' | b'|' | b'>' if bytes.next_if_eq(&byte).is_some() => operator.push(byte),
                    b'\r' if bytes.next_if_eq(&b'\n').is_some() => operator.push(b'\n'),
                    _ => {}
                }
                flush(&mut result, &mut words, &mut word, &mut active);
                if !matches!(byte, b'<' | b'>') {
                    prepare(words.get(1..).unwrap_or_default())?;
                    words.clear();
                }
                separator(&mut result);
                result.extend_from_slice(&operator);
            }
            _ => {
                word.push(byte);
                active = true;
            }
        }
    }
    if quote.is_some() {
        return Err(invalid_rule());
    }
    flush(&mut result, &mut words, &mut word, &mut active);
    prepare(words.get(1..).unwrap_or_default())?;
    Ok(result)
}

fn invalid_rule() -> HostError {
    HostError::unsupported("The Windows build rule contains an invalid argument.")
}

fn flush(result: &mut Vec<u8>, words: &mut Vec<BString>, word: &mut BString, active: &mut bool) {
    if *active {
        separator(result);
        quote_argument(result, word);
        words.push(std::mem::take(word));
        *active = false;
    }
}

fn separator(result: &mut Vec<u8>) {
    if result
        .last()
        .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
    {
        result.push(b' ');
    }
}

fn quote_argument(result: &mut Vec<u8>, word: &[u8]) {
    let quoted = word.is_empty()
        || word
            .iter()
            .any(|byte| byte.is_ascii_whitespace() || b"\"&|<>()^".contains(byte));
    if quoted {
        result.push(b'"');
    }
    let mut backslashes = 0;
    for &byte in word {
        if byte == b'\\' {
            backslashes += 1;
            continue;
        }
        if byte == b'"' {
            // End the quoted segment so cmd.exe can escape the literal quote.
            // The child sees one argument made of adjacent quoted segments.
            result.extend(std::iter::repeat_n(b'\\', backslashes * 2));
            result.extend_from_slice(b"\"\\^\"\"");
        } else {
            result.extend(std::iter::repeat_n(b'\\', backslashes));
            if byte == b'%' {
                result.push(b'%');
            }
            result.push(byte);
        }
        backslashes = 0;
    }
    result.extend(std::iter::repeat_n(
        b'\\',
        if quoted { backslashes * 2 } else { backslashes },
    ));
    if quoted {
        result.push(b'"');
    }
}
