use rcmake_base::{BString, hash::HashAlgo, list_ops};
use rcmake_regex::{MatchOptions, Regex, expand_replacement};

use super::{Call, Evaluator, Value};

pub(super) fn convert(op: &[u8], value: &[u8]) -> BString {
    match op {
        b"LOWER_CASE" | b"TOLOWER" => value.to_ascii_lowercase().into(),
        b"UPPER_CASE" | b"TOUPPER" => value.to_ascii_uppercase().into(),
        _ => {
            let mut identifier = BString::default();
            if value.first().is_some_and(u8::is_ascii_digit) {
                identifier.push(b'_');
            }
            for &byte in value {
                identifier.push(if byte.is_ascii_alphanumeric() {
                    byte
                } else {
                    b'_'
                });
            }
            identifier
        }
    }
}

impl Evaluator<'_> {
    pub(super) fn string(&mut self, call: Call<'_>) -> Value {
        let args = self.arguments(call)?;
        let [op, rest @ ..] = args.as_slice() else {
            return Err(call.arity(1));
        };
        match (op.as_slice(), rest) {
            (b"LENGTH", [value]) => Ok(value.len().to_string().into()),
            (b"SUBSTRING", [value, start, count]) => substring(call, value, start, count),
            (b"FIND", [value, needle]) => Ok(find(value, needle, false)),
            (b"FIND", [value, from, needle])
                if matches!(from.as_slice(), b"FROM:BEGIN" | b"FROM:END") =>
            {
                Ok(find(value, needle, from == "FROM:END"))
            }
            (b"MATCH", [value, pattern]) => self.regex_output(call, value, pattern, None, false),
            (b"MATCH", [value, seek, pattern])
                if matches!(seek.as_slice(), b"SEEK:ONCE" | b"SEEK:ALL") =>
            {
                self.regex_output(call, value, pattern, None, seek == "SEEK:ALL")
            }
            (b"REPLACE", [value, needle, replacement]) => {
                self.replace_literal(value, needle, replacement)
            }
            (b"REPLACE", [mode, value, needle, replacement]) if mode == "STRING" => {
                self.replace_literal(value, needle, replacement)
            }
            (b"REPLACE", [mode, value, pattern, replacement]) if mode == "REGEX" => {
                self.regex_output(call, value, pattern, Some(replacement), true)
            }
            (b"APPEND" | b"PREPEND" | b"JOIN", [value, items @ ..]) if !items.is_empty() => {
                self.concatenate(op, value, items)
            }
            (b"TOLOWER" | b"TOUPPER" | b"MAKE_C_IDENTIFIER", [value]) => Ok(convert(op, value)),
            (b"STRIP", [mode, value]) if mode == "SPACES" => {
                Ok(list_ops::strip_spaces(value).into())
            }
            (b"QUOTE", [mode, value]) if mode == "REGEX" => self.quote_regex(value),
            (b"HEX", [value]) => self.hex(value),
            (b"HASH", [value, algorithm]) => {
                let algo = algorithm
                    .strip_prefix(b"ALGORITHM:")
                    .and_then(HashAlgo::parse)
                    .ok_or_else(|| call.error("Invalid hash algorithm."))?;
                Ok(algo.hex_digest(value).into())
            }
            (b"ASCII", [_, ..]) => rest
                .iter()
                .map(|value| {
                    let code = list_ops::parse_index(value);
                    code.and_then(|code| u8::try_from(code).ok())
                        .filter(|&code| code != 0)
                        .ok_or_else(|| {
                            call.error(format!("Character with code {value} does not exist."))
                        })
                })
                .collect(),
            (b"UUID", args) => self.uuid(call, args),
            (b"RANDOM", args) => self.random(call, args),
            (b"TIMESTAMP", args) => self.timestamp(call, args),
            _ => Err(call.error(format!("Invalid arguments for $<STRING:{op}>."))),
        }
    }

    fn concatenate(&self, op: &[u8], value: &[u8], items: &[BString]) -> Value {
        let mut out = BString::default();
        if op == b"APPEND" {
            self.append(&mut out, value)?;
        }
        for (index, item) in items.iter().enumerate() {
            if index != 0 && op == b"JOIN" {
                self.append(&mut out, value)?;
            }
            self.append(&mut out, item)?;
        }
        if op == b"PREPEND" {
            self.append(&mut out, value)?;
        }
        Ok(out)
    }

    fn replace_literal(&mut self, value: &[u8], needle: &[u8], replacement: &[u8]) -> Value {
        if needle.is_empty() {
            return Ok(value.into());
        }
        let mut out = BString::default();
        let mut rest = value;
        while let Some(index) = rest.windows(needle.len()).position(|part| part == needle) {
            let (before, after) = rest.split_at(index);
            self.append(&mut out, before)?;
            self.append(&mut out, replacement)?;
            rest = after.get(needle.len()..).unwrap_or_default();
        }
        self.append(&mut out, rest)?;
        Ok(out)
    }

    pub(super) fn regex(&self, call: Call<'_>, pattern: &[u8]) -> Result<Regex, crate::GenexError> {
        Regex::new(pattern).map_err(|_| {
            call.error(format!(
                "Regular expression \"{}\" cannot compile.",
                BString::from(pattern)
            ))
        })
    }

    pub(super) fn regex_matches(
        &mut self,
        call: Call<'_>,
        regex: &Regex,
        value: &[u8],
    ) -> Result<bool, crate::GenexError> {
        regex
            .find_at(value, 0, MatchOptions::default(), &mut self.regex_steps)
            .map(|caps| caps.is_some())
            .map_err(|err| call.error(err.to_string()))
    }

    pub(super) fn regex_output(
        &mut self,
        call: Call<'_>,
        value: &[u8],
        pattern: &[u8],
        replacement: Option<&[u8]>,
        all: bool,
    ) -> Value {
        let regex = self.regex(call, pattern)?;
        let options = MatchOptions {
            bol_at_start: false,
            allow_empty_at_start: true,
        };
        let mut out = BString::default();
        let mut start = 0;
        let mut copied = 0;
        let mut matched = false;
        while start <= value.len() {
            let found = regex
                .find_at(value, start, options, &mut self.regex_steps)
                .map_err(|err| call.error(err.to_string()))?;
            let Some(caps) = found else {
                break;
            };
            let Some((begin, end)) = caps.groups.first().copied().flatten() else {
                break;
            };
            if let Some(replacement) = replacement {
                self.append(&mut out, value.get(copied..begin).unwrap_or_default())?;
                let text = expand_replacement(replacement, value, &caps, regex.group_count())
                    .map_err(|error| call.error(error.message))?;
                self.append(&mut out, &text)?;
                copied = end;
            } else {
                if matched {
                    self.append(&mut out, b";")?;
                }
                self.append(&mut out, value.get(begin..end).unwrap_or_default())?;
            }
            matched = true;
            if !all {
                break;
            }
            start = if end == begin { end + 1 } else { end };
        }
        if replacement.is_some() {
            self.append(&mut out, value.get(copied..).unwrap_or_default())?;
        }
        Ok(out)
    }

    fn quote_regex(&self, value: &[u8]) -> Value {
        let mut out = BString::default();
        for &byte in value {
            if b"\\.^$|()[]*+?".contains(&byte) {
                self.append(&mut out, b"\\")?;
            }
            self.append(&mut out, &[byte])?;
        }
        Ok(out)
    }

    fn hex(&self, value: &[u8]) -> Value {
        let mut out = BString::default();
        for &byte in value {
            self.append(&mut out, format!("{byte:02x}").as_bytes())?;
        }
        Ok(out)
    }

    fn uuid(&self, call: Call<'_>, args: &[BString]) -> Value {
        let mut namespace = None;
        let mut kind = None;
        let mut name = b"".as_slice();
        let mut upper = false;
        for arg in args {
            if let Some(value) = arg.strip_prefix(b"NAMESPACE:") {
                namespace = uuid_namespace(value);
            } else if let Some(value) = arg.strip_prefix(b"TYPE:") {
                kind = HashAlgo::parse(value);
            } else if let Some(value) = arg.strip_prefix(b"NAME:") {
                name = value;
            } else if arg == "CASE:UPPER" {
                upper = true;
            } else if arg == "CASE:LOWER" {
                upper = false;
            } else {
                return Err(call.error("Invalid UUID option."));
            }
        }
        let mut input = namespace.ok_or_else(|| call.error("Invalid UUID namespace."))?;
        self.append(&mut input, name)?;
        let (hash, version) = match kind {
            Some(HashAlgo::Md5) => (HashAlgo::Md5, b'3'),
            Some(HashAlgo::Sha1) => (HashAlgo::Sha1, b'5'),
            _ => return Err(call.error("The UUID type must be MD5 or SHA1.")),
        };
        let digest = hash.hex_digest(&input);
        let mut output = BString::default();
        for (index, mut byte) in digest.bytes().take(32).enumerate() {
            match index {
                8 | 20 => output.push(b'-'),
                12 => {
                    output.push(b'-');
                    byte = version;
                }
                16 => {
                    output.push(b'-');
                    let variant = hex_digit(byte).unwrap_or_default() & 3;
                    byte = if variant < 2 {
                        b'8' + variant
                    } else {
                        b'a' + variant - 2
                    };
                }
                _ => {}
            }
            output.push(if upper {
                byte.to_ascii_uppercase()
            } else {
                byte
            });
        }
        Ok(output)
    }

    fn random(&mut self, call: Call<'_>, args: &[BString]) -> Value {
        let mut count = 5;
        let mut alphabet =
            b"qwertyuiopasdfghjklzxcvbnmQWERTYUIOPASDFGHJKLZXCVBNM0123456789".as_slice();
        let mut seed = None;
        for arg in args {
            if let Some(value) = arg.strip_prefix(b"LENGTH:") {
                count = list_ops::parse_index(value)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n > 0 && n <= self.limits.max_value_bytes)
                    .ok_or_else(|| call.error("Invalid random string length."))?;
            } else if let Some(value) = arg.strip_prefix(b"ALPHABET:") {
                alphabet = value;
            } else if let Some(value) = arg.strip_prefix(b"RANDOM_SEED:") {
                seed = Some(
                    list_ops::parse_index(value)
                        .ok_or_else(|| call.error("Invalid random seed."))?
                        .cast_unsigned(),
                );
            } else {
                return Err(call.error("Invalid random string option."));
            }
        }
        if alphabet.is_empty() || !alphabet.is_ascii() {
            return Err(call.error("The random string alphabet must contain ASCII characters."));
        }
        self.flags.context_sensitive |= seed.is_none();
        let mut state = seed
            .or_else(|| self.host.random_seed())
            .ok_or_else(|| call.error("The evaluation context has no random seed."))?
            as u32;
        let mut out = BString::default();
        for _ in 0..count {
            #[cfg(not(windows))]
            let (sample, range) = {
                let previous = if state == 0 { 123_459_876 } else { state };
                state = ((u64::from(previous) * 16_807) % 2_147_483_647) as u32;
                (state, 2_147_483_648)
            };
            #[cfg(windows)]
            let (sample, range) = {
                state = state.wrapping_mul(214_013).wrapping_add(2_531_011);
                ((state >> 16) & 0x7fff, 32_768)
            };
            let index = (u64::from(sample) * alphabet.len() as u64 / range) as usize;
            if let Some(&byte) = alphabet.get(index) {
                out.push(byte);
            }
        }
        Ok(out)
    }

    fn timestamp(&mut self, call: Call<'_>, args: &[BString]) -> Value {
        self.flags.context_sensitive = true;
        let (mut format, mut utc) = (None, false);
        for arg in args {
            if arg == "UTC" && !utc {
                utc = true;
            } else if format.is_none() {
                format = Some(arg.as_slice());
            } else {
                return Err(call.error("Invalid timestamp option."));
            }
        }
        self.host
            .timestamp(format, utc)
            .ok_or_else(|| call.error("The evaluation context has no timestamp."))
    }
}

fn substring(call: Call<'_>, value: &[u8], start: &[u8], length: &[u8]) -> Value {
    let start_index = list_ops::parse_index(start)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n <= value.len())
        .ok_or_else(|| {
            call.error(format!(
                "begin index: {} is out of range 0 - {}",
                BString::from(start),
                value.len()
            ))
        })?;
    let remaining = value.get(start_index..).unwrap_or_default();
    let count = match list_ops::parse_index(length) {
        Some(-1) => remaining.len(),
        Some(n) if n >= 0 => usize::try_from(n)
            .unwrap_or(usize::MAX)
            .min(remaining.len()),
        _ => {
            return Err(call.error(format!(
                "end index: {} should be -1 or greater",
                BString::from(length)
            )));
        }
    };
    Ok(remaining.get(..count).unwrap_or_default().into())
}

fn find(value: &[u8], needle: &[u8], reverse: bool) -> BString {
    let index = if needle.is_empty() {
        Some(if reverse { value.len() } else { 0 })
    } else if reverse {
        value.windows(needle.len()).rposition(|part| part == needle)
    } else {
        value.windows(needle.len()).position(|part| part == needle)
    };
    index.map_or_else(|| "-1".into(), |n| n.to_string().into())
}

fn uuid_namespace(value: &[u8]) -> Option<BString> {
    let mut offset = 0;
    let mut out = BString::default();
    for (part, size) in value.split(|&byte| byte == b'-').zip([8, 4, 4, 4, 12]) {
        if part.len() != size {
            return None;
        }
        for [high, low] in part.as_chunks::<2>().0 {
            out.push(hex_digit(*high)? * 16 + hex_digit(*low)?);
        }
        offset += size;
    }
    (offset == 32 && value.len() == 36).then_some(out)
}

fn hex_digit(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}
