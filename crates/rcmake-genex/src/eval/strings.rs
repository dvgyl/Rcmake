use rcmake_base::{BString, hash::HashAlgo, list_ops};
use rcmake_regex::{MatchOptions, Regex, expand_replacement};

use super::{Call, Evaluator, Value, operations::operation_arity};

pub(super) enum RegexSearch<'a> {
    First,
    All,
    ListReplace {
        bol_at_start: bool,
        pattern: &'a [u8],
    },
}

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
            return Err(call.error("$<STRING> expression requires at least one parameter."));
        };
        let (minimum, maximum) = match op.as_slice() {
            b"" => return Err(call.error("$<STRING> expression requires at least one parameter.")),
            b"LENGTH" | b"TOLOWER" | b"TOUPPER" | b"HEX" | b"MAKE_C_IDENTIFIER" => (1, Some(1)),
            b"SUBSTRING" => (3, Some(3)),
            b"STRIP" | b"QUOTE" | b"HASH" => (2, Some(2)),
            b"FIND" | b"MATCH" => (2, Some(3)),
            b"REPLACE" => (3, Some(4)),
            b"UUID" => (2, Some(4)),
            b"RANDOM" => (0, Some(3)),
            b"TIMESTAMP" => (0, Some(2)),
            b"JOIN" | b"APPEND" | b"PREPEND" => (2, None),
            b"ASCII" => (1, None),
            _ => return Err(call.error(format!("{op}: invalid option."))),
        };
        if maximum == Some(minimum) && rest.len() != minimum {
            return Err(operation_arity(call, op, None, "exactly", minimum));
        }
        if rest.len() < minimum {
            return Err(operation_arity(call, op, None, "at least", minimum));
        }
        if let Some(maximum) = maximum.filter(|&maximum| rest.len() > maximum) {
            return Err(operation_arity(call, op, None, "at most", maximum));
        }
        match (op.as_slice(), rest) {
            (b"LENGTH", [value]) => Ok(value.len().to_string().into()),
            (b"SUBSTRING", [value, start, count]) => substring(call, value, start, count),
            (b"FIND", [value, needle]) => Ok(find(value, needle, false)),
            (b"FIND", [value, first, second]) => {
                let (needle, reverse) =
                    option_argument(call, first, second, "FROM", "BEGIN", "END")?;
                Ok(find(value, needle, reverse))
            }
            (b"MATCH", [value, pattern]) => self.string_regex(call, value, pattern, None, false),
            (b"MATCH", [value, first, second]) => {
                let (pattern, all) = option_argument(call, first, second, "SEEK", "ONCE", "ALL")?;
                self.string_regex(call, value, pattern, None, all)
            }
            (b"REPLACE", [value, needle, replacement]) => {
                self.replace_literal(value, needle, replacement)
            }
            (b"REPLACE", [mode, value, needle, replacement]) if mode == "STRING" => {
                self.replace_literal(value, needle, replacement)
            }
            (b"REPLACE", [mode, value, pattern, replacement]) if mode == "REGEX" => {
                self.string_regex(call, value, pattern, Some(replacement), true)
            }
            (b"REPLACE", [mode, ..]) => Err(call.error(format!(
                "'{mode}' is unexpected.  'STRING' or 'REGEX' expected."
            ))),
            (b"APPEND" | b"PREPEND" | b"JOIN", [value, items @ ..]) if !items.is_empty() => {
                self.concatenate(op, value, items)
            }
            (b"TOLOWER" | b"TOUPPER" | b"MAKE_C_IDENTIFIER", [value]) => Ok(convert(op, value)),
            (b"STRIP", [mode, value]) if mode == "SPACES" => {
                Ok(list_ops::strip_spaces(value).into())
            }
            (b"STRIP", [mode, _]) => {
                Err(call.error(format!("'{mode}' is unexpected.  'SPACES' expected.")))
            }
            (b"QUOTE", [mode, value]) if mode == "REGEX" => self.quote_regex(value),
            (b"QUOTE", [mode, _]) => {
                Err(call.error(format!("'{mode}' is unexpected.  'REGEX' expected.")))
            }
            (b"HEX", [value]) => self.hex(value),
            (b"HASH", [value, algorithm]) => {
                let name = algorithm.strip_prefix(b"ALGORITHM:").ok_or_else(|| {
                    call.error(format!(
                        "{algorithm}: invalid parameter. Option 'ALGORITHM:' expected."
                    ))
                })?;
                if name.is_empty() {
                    return Err(call.error("Missing value for 'ALGORITHM:' option."));
                }
                let algo = HashAlgo::parse(name).ok_or_else(|| {
                    call.error(format!("{}: invalid hash algorithm.", BString::from(name)))
                })?;
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

    fn string_regex(
        &mut self,
        call: Call<'_>,
        value: &[u8],
        pattern: &[u8],
        replacement: Option<&[u8]>,
        all: bool,
    ) -> Value {
        let regex = Regex::new(pattern).map_err(|_| {
            call.error(format!(
                "Failed to compile regex \"{}\"",
                BString::from(pattern)
            ))
        })?;
        let search = if all {
            RegexSearch::All
        } else {
            RegexSearch::First
        };
        self.regex_output_compiled(call, &regex, value, replacement, search)
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

    pub(super) fn regex_output_compiled(
        &mut self,
        call: Call<'_>,
        regex: &Regex,
        value: &[u8],
        replacement: Option<&[u8]>,
        search: RegexSearch<'_>,
    ) -> Value {
        let mut options = MatchOptions {
            bol_at_start: matches!(
                search,
                RegexSearch::ListReplace {
                    bol_at_start: true,
                    ..
                }
            ),
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
                    .map_err(|error| match search {
                        RegexSearch::ListReplace { pattern, .. }
                            if error.message.contains("contains an out-of-range escape for group") =>
                        {
                            call.error(format!(
                                "sub-command TRANSFORM, action REPLACE: replace expression \"{}\" contains an out-of-range escape for regex \"{}\".",
                                BString::from(replacement),
                                BString::from(pattern),
                            ))
                        }
                        _ => call.error(error.message),
                    })?;
                self.append(&mut out, &text)?;
                copied = end;
            } else {
                if matched {
                    self.append(&mut out, b";")?;
                }
                self.append(&mut out, value.get(begin..end).unwrap_or_default())?;
            }
            matched = true;
            if matches!(search, RegexSearch::First) || begin == value.len() {
                break;
            }
            if matches!(search, RegexSearch::ListReplace { .. }) {
                start = end;
                options.allow_empty_at_start = begin != end;
            } else {
                start = if end == begin { end + 1 } else { end };
            }
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
                if value.is_empty() {
                    return Err(call.error("Invalid value for 'NAMESPACE:' option."));
                }
                namespace = Some(value);
            } else if let Some(value) = arg.strip_prefix(b"TYPE:") {
                kind = Some(match value {
                    b"MD5" => HashAlgo::Md5,
                    b"SHA1" => HashAlgo::Sha1,
                    _ => {
                        return Err(call.error(
                            "Invalid value for 'TYPE:' option.  'MD5' or 'SHA1' expected.",
                        ));
                    }
                });
            } else if let Some(value) = arg.strip_prefix(b"NAME:") {
                name = value;
            } else if arg == "CASE:UPPER" {
                upper = true;
            } else if arg == "CASE:LOWER" {
                upper = false;
            } else if arg.starts_with(b"CASE:") {
                return Err(
                    call.error("Invalid value for 'CASE:' option.  'UPPER' or 'LOWER' expected.")
                );
            } else {
                return Err(call.error(format!("{arg}: invalid parameter.")));
            }
        }
        let namespace =
            namespace.ok_or_else(|| call.error("Required option 'NAMESPACE:' is missing."))?;
        let hash = kind.ok_or_else(|| call.error("Required option 'TYPE:' is missing."))?;
        let version = if hash == HashAlgo::Md5 { b'3' } else { b'5' };
        let mut input =
            uuid_namespace(namespace).ok_or_else(|| call.error("Invalid UUID namespace."))?;
        self.append(&mut input, name)?;
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
        let default_alphabet =
            b"qwertyuiopasdfghjklzxcvbnmQWERTYUIOPASDFGHJKLZXCVBNM0123456789".as_slice();
        let mut alphabet = default_alphabet;
        let mut seed = None;
        for arg in args {
            if let Some(value) = arg.strip_prefix(b"LENGTH:") {
                count = list_ops::parse_index(value)
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n > 0 && n <= self.limits.max_value_bytes)
                    .ok_or_else(|| {
                        call.error(format!(
                            "{arg}: invalid numeric value for 'LENGTH:' option."
                        ))
                    })?;
            } else if let Some(value) = arg.strip_prefix(b"ALPHABET:") {
                alphabet = value;
            } else if let Some(value) = arg.strip_prefix(b"RANDOM_SEED:") {
                seed = Some(
                    list_ops::parse_index(value)
                        .ok_or_else(|| {
                            call.error(format!(
                                "{arg}: invalid numeric value for 'RANDOM_SEED:' option."
                            ))
                        })?
                        .cast_unsigned(),
                );
            } else {
                return Err(call.error(format!("{arg}: invalid parameter.")));
            }
        }
        if alphabet.is_empty() {
            alphabet = default_alphabet;
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
                return Err(call.error("'UTC' option is expected."));
            }
        }
        self.host
            .timestamp(format, utc)
            .ok_or_else(|| call.error("The evaluation context has no timestamp."))
    }
}

fn option_argument<'a>(
    call: Call<'_>,
    first: &'a BString,
    second: &'a BString,
    key: &str,
    default: &str,
    alternate: &str,
) -> Result<(&'a BString, bool), crate::GenexError> {
    let prefix = format!("{key}:");
    let mut flag = None;
    let mut argument = first;
    for item in [first, second] {
        if let Some(option) = item.strip_prefix(prefix.as_bytes()) {
            flag = Some(if option == default.as_bytes() {
                false
            } else if option == alternate.as_bytes() {
                true
            } else {
                return Err(call.error(format!(
                    "Invalid value for '{prefix}' option.  '{default}' or '{alternate}' expected."
                )));
            });
        } else {
            argument = item;
        }
    }
    flag.map(|flag| (argument, flag))
        .ok_or_else(|| call.error(format!("Expected option '{prefix}' is missing.")))
}

fn substring(call: Call<'_>, value: &[u8], start: &[u8], length: &[u8]) -> Value {
    let numeric = |value: &[u8]| {
        list_ops::parse_index(value).ok_or_else(|| {
            call.error(format!(
                "index: \"{}\" is not a valid index",
                BString::from(value)
            ))
        })
    };
    let start_index = usize::try_from(numeric(start)?)
        .ok()
        .filter(|&n| n <= value.len())
        .ok_or_else(|| {
            call.error(format!(
                "begin index: {} is out of range 0 - {}",
                BString::from(start),
                value.len()
            ))
        })?;
    let remaining = value.get(start_index..).unwrap_or_default();
    let count = match numeric(length)? {
        -1 => remaining.len(),
        n if n >= 0 => usize::try_from(n)
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
