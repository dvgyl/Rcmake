use std::collections::BTreeMap;

use rcmake_base::BString;

use crate::{JsonError, ReadOptions, Value};

const MAX_DEPTH: usize = 1000;
const EXPECT_VALUE: &str = "Syntax error: value, object or array expected.";
const EXPECT_KEY: &str = "Missing '}' or object member name";
const EXPECT_COLON: &str = "Missing ':' after object member name";
const EXPECT_ARRAY_END: &str = "Missing ',' or ']' in array declaration";
const EXPECT_OBJECT_END: &str = "Missing ',' or '}' in object declaration";

/// Parse one JSON value from a byte slice. A leading UTF-8 byte order mark is optional.
///
/// # Errors
///
/// Returns the location and message of the first syntax error. Also rejects nesting
/// beyond 1000 containers.
pub fn parse(src: &[u8], options: &ReadOptions) -> Result<Value, JsonError> {
    let src = src.strip_prefix(b"\xef\xbb\xbf").unwrap_or(src);
    Parser {
        src,
        pos: 0,
        options,
    }
    .document()
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    options: &'a ReadOptions,
}

enum Frame {
    Array(Vec<Value>),
    Object(BTreeMap<BString, Value>, BString),
}

impl Parser<'_> {
    fn document(mut self) -> Result<Value, JsonError> {
        let mut frames = Vec::new();
        loop {
            self.skip_trivia()?;
            let start = self.pos;
            let token = self.peek();
            if frames.len() == MAX_DEPTH && matches!(token, Some(b'[' | b'{')) {
                return Err(self.error(start, "Exceeded the JSON nesting limit of 1000."));
            }
            let mut value = match token {
                Some(b'[') => {
                    self.pos += 1;
                    self.skip_trivia()?;
                    let items = Vec::new();
                    if !self.eat(b']') {
                        frames.push(Frame::Array(items));
                        continue;
                    }
                    Value::Array(items)
                }
                Some(b'{') => {
                    self.pos += 1;
                    self.skip_trivia()?;
                    let members = BTreeMap::new();
                    if !self.eat(b'}') {
                        let key = self.object_key(&members)?;
                        frames.push(Frame::Object(members, key));
                        continue;
                    }
                    Value::Object(members)
                }
                Some(b'"') => Value::String(self.string(EXPECT_VALUE)?),
                Some(b'-' | b'0'..=b'9') => self.number()?,
                Some(b'n') if self.eat_bytes(b"null") => Value::Null,
                Some(b't') if self.eat_bytes(b"true") => Value::Bool(true),
                Some(b'f') if self.eat_bytes(b"false") => Value::Bool(false),
                _ => return Err(self.error(start, EXPECT_VALUE)),
            };

            loop {
                self.skip_trivia()?;
                match frames.pop() {
                    Some(Frame::Array(mut items)) => {
                        items.push(value);
                        if self.eat(b']') {
                            value = Value::Array(items);
                        } else {
                            self.require(b',', EXPECT_ARRAY_END)?;
                            frames.push(Frame::Array(items));
                            break;
                        }
                    }
                    Some(Frame::Object(mut members, key)) => {
                        members.insert(key, value);
                        if self.eat(b'}') {
                            value = Value::Object(members);
                        } else {
                            self.require(b',', EXPECT_OBJECT_END)?;
                            let key = self.object_key(&members)?;
                            frames.push(Frame::Object(members, key));
                            break;
                        }
                    }
                    None if self.peek().is_none() => return Ok(value),
                    None => {
                        return Err(self.error(self.pos, "Extra non-whitespace after JSON value."));
                    }
                }
            }
        }
    }

    fn object_key(&mut self, members: &BTreeMap<BString, Value>) -> Result<BString, JsonError> {
        self.skip_trivia()?;
        let start = self.pos;
        let key = self.string(EXPECT_KEY)?;
        if !self.options.dup_keys_last_wins && members.contains_key(&key) {
            let name = String::from_utf8_lossy(key.as_slice());
            return Err(self.error(start, format!("Duplicate key: '{name}'")));
        }
        self.skip_trivia()?;
        self.require(b':', EXPECT_COLON)?;
        Ok(key)
    }

    fn string(&mut self, token_error: &str) -> Result<BString, JsonError> {
        let start = self.pos;
        self.require(b'"', token_error)?;
        while let Some(byte) = self.peek() {
            self.pos += 1;
            match byte {
                b'"' => {
                    let content = self.src.get(start + 1..self.pos - 1).unwrap_or_default();
                    return decode_string(content).map_err(|message| self.error(start, message));
                }
                b'\\' if self.peek().is_some() => self.pos += 1,
                _ => {}
            }
        }
        Err(self.error(start, token_error))
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        let negative = self.eat(b'-');
        let integer_valid = if self.eat(b'0') {
            !self.take_digits()
        } else {
            self.take_digits()
        };

        let fraction = self.eat(b'.');
        let fraction_valid = !fraction || self.take_digits();
        let exponent = self.eat(b'e') || self.eat(b'E');
        let exponent_valid = if exponent {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            self.take_digits()
        } else {
            true
        };

        let token = self.src.get(start..self.pos).unwrap_or_default();
        let text = std::str::from_utf8(token).unwrap_or_default();
        let invalid = || self.error(start, format!("'{text}' is not a number."));
        if !integer_valid || !fraction_valid || !exponent_valid {
            return Err(invalid());
        }

        if !fraction && !exponent {
            if let Ok(value) = text.parse::<i64>() {
                return Ok(Value::Int(value));
            }
            if !negative && let Ok(value) = text.parse::<u64>() {
                return Ok(Value::UInt(value));
            }
        }
        text.parse::<f64>().map(Value::Real).map_err(|_| invalid())
    }

    fn take_digits(&mut self) -> bool {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos != start
    }

    fn skip_trivia(&mut self) -> Result<(), JsonError> {
        loop {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.pos += 1;
            }
            if !self.options.allow_comments {
                return Ok(());
            }
            let start = self.pos;
            if self.eat_bytes(b"//") {
                while matches!(self.peek(), Some(byte) if !matches!(byte, b'\r' | b'\n')) {
                    self.pos += 1;
                }
            } else if self.eat_bytes(b"/*") {
                while !self.eat_bytes(b"*/") {
                    if self.peek().is_none() {
                        return Err(self.error(start, "Unterminated block comment."));
                    }
                    self.pos += 1;
                }
            } else {
                return Ok(());
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() != Some(byte) {
            return false;
        }
        self.pos += 1;
        true
    }

    fn eat_bytes(&mut self, bytes: &[u8]) -> bool {
        if !self
            .src
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(bytes))
        {
            return false;
        }
        self.pos += bytes.len();
        true
    }

    fn require(&mut self, byte: u8, message: &str) -> Result<(), JsonError> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.error(self.pos, message))
        }
    }

    fn error(&self, pos: usize, message: impl Into<String>) -> JsonError {
        let mut line = 1_u32;
        let mut column = 1_u32;
        let mut previous = None;
        for &byte in self.src.iter().take(pos) {
            match byte {
                b'\n' if previous == Some(b'\r') => {}
                b'\n' | b'\r' => {
                    line = line.saturating_add(1);
                    column = 1;
                }
                _ => column = column.saturating_add(1),
            }
            previous = Some(byte);
        }
        JsonError {
            line,
            column,
            message: message.into(),
        }
    }
}

fn decode_string(mut content: &[u8]) -> Result<BString, &'static str> {
    let mut decoded = Vec::with_capacity(content.len());
    while let Some((&byte, rest)) = content.split_first() {
        content = rest;
        let byte = match byte {
            b'\\' => {
                let Some((&escape, rest)) = content.split_first() else {
                    return Err("Empty escape sequence in string");
                };
                content = rest;
                match escape {
                    b'"' | b'\\' | b'/' => escape,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' => {
                        let character = decode_unicode(&mut content)?;
                        let mut buffer = [0; 4];
                        decoded.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                        continue;
                    }
                    _ => return Err("Bad escape sequence in string"),
                }
            }
            _ => byte,
        };
        decoded.push(byte);
    }
    Ok(BString::from(decoded))
}

fn decode_unicode(content: &mut &[u8]) -> Result<char, &'static str> {
    let lead = read_hex_quad(content)?;
    let point = if (0xd800..=0xdbff).contains(&lead) {
        if content.len() < 6 {
            return Err("additional six characters expected to parse unicode surrogate pair.");
        }
        *content = content.strip_prefix(b"\\u").ok_or(
            "expecting another \\u token to begin the second half of a unicode surrogate pair",
        )?;
        let trail = read_hex_quad(content)?;
        if !(0xdc00..=0xdfff).contains(&trail) {
            return Err("Invalid low surrogate in unicode escape sequence.");
        }
        0x10000 + ((lead - 0xd800) << 10) + (trail - 0xdc00)
    } else {
        lead
    };
    char::from_u32(point).ok_or("Invalid unicode surrogate in string.")
}

fn read_hex_quad(content: &mut &[u8]) -> Result<u32, &'static str> {
    let (digits, rest) = content
        .split_at_checked(4)
        .ok_or("Bad unicode escape sequence in string: four digits expected.")?;
    *content = rest;
    digits.iter().try_fold(0, |value, &byte| {
        let digit = char::from(byte)
            .to_digit(16)
            .ok_or("Bad unicode escape sequence in string: hexadecimal digit expected.")?;
        Ok((value << 4) | digit)
    })
}
