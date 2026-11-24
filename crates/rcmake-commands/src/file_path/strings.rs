use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::Cx;
use rcmake_host::{ReadRange, WriteMode, path::from_native};
use rcmake_regex::{MatchOptions, Regex};

use super::{Result, error, set};

#[derive(Default)]
struct Options {
    maximum: usize,
    minimum: usize,
    count: usize,
    input: Option<usize>,
    output: Option<usize>,
    newline: bool,
    no_hex: bool,
    regex: Option<Arc<Regex>>,
    encoding: Option<Encoding>,
}

#[derive(Clone, Copy)]
enum Encoding {
    Ascii,
    Utf8,
    Utf16(bool),
    Utf32(bool),
}

fn options(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter().copied();
    while let Some(option) = args.next() {
        let number = match option {
            b"LENGTH_MAXIMUM" => &mut options.maximum,
            b"LENGTH_MINIMUM" => &mut options.minimum,
            b"LIMIT_COUNT" => &mut options.count,
            b"LIMIT_INPUT" => options.input.insert(0),
            b"LIMIT_OUTPUT" => options.output.insert(0),
            b"NEWLINE_CONSUME" => {
                options.newline = true;
                continue;
            }
            b"NO_HEX_CONVERSION" => {
                options.no_hex = true;
                continue;
            }
            b"REGEX" => {
                let pattern = value(&mut args, option)?;
                options.regex = Some(cx.regex(pattern).map_err(|_| {
                    error(
                        [
                            b"STRINGS option REGEX value \"",
                            pattern,
                            b"\" could not be compiled.",
                        ]
                        .concat(),
                    )
                })?);
                let policy = PolicyId::new(159);
                if cx.policy(policy) == PolicyStatus::Warn
                    && cx.var(b"CMAKE_POLICY_WARNING_CMP0159").map_or_else(
                        || cx.trace_config().is_some(),
                        |value| rcmake_base::truth::is_on(&value),
                    )
                {
                    cx.issue_policy_warning(
                        policy,
                        b"",
                        b"For compatibility, CMake is leaving CMAKE_MATCH_<n> unchanged.",
                    );
                }
                continue;
            }
            b"ENCODING" => {
                let encoding = value(&mut args, option)?;
                options.encoding = Some(match encoding {
                    b"UTF-8" => Encoding::Utf8,
                    b"UTF-16LE" => Encoding::Utf16(true),
                    b"UTF-16BE" => Encoding::Utf16(false),
                    b"UTF-32LE" => Encoding::Utf32(true),
                    b"UTF-32BE" => Encoding::Utf32(false),
                    _ => {
                        return Err(error(
                            [
                                b"STRINGS option ENCODING value \"",
                                encoding,
                                b"\" not recognized.",
                            ]
                            .concat(),
                        ));
                    }
                });
                continue;
            }
            _ => {
                return Err(error(
                    [b"STRINGS given unknown argument \"", option, b"\""].concat(),
                ));
            }
        };
        let input = value(&mut args, option)?;
        *number = std::str::from_utf8(input)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| {
                error(
                    [
                        b"STRINGS option ",
                        option,
                        b" value \"",
                        input,
                        b"\" is not an unsigned integer.",
                    ]
                    .concat(),
                )
            })?;
    }
    Ok(options)
}

fn value<'a>(args: &mut impl Iterator<Item = &'a [u8]>, option: &[u8]) -> Result<&'a [u8]> {
    args.next()
        .ok_or_else(|| error([b"STRINGS option ", option, b" requires a value."].concat()))
}

pub(super) fn run(cx: &mut Cx<'_>, path: &Path, out: &[u8], args: &[&[u8]]) -> Result {
    let opts = options(cx, args)?;
    let cap = cx.limits().max_file_read;
    let range = if opts.no_hex
        && let Some(input) = opts.input.filter(|&input| (input as u64) < cap)
    {
        ReadRange::Slice {
            offset: 0,
            len: Some((input as u64).saturating_add(4)),
        }
    } else {
        ReadRange::All { limit: cap }
    };
    let bytes = cx.host().read(path, range).map_err(|_| {
        error(
            [
                b"STRINGS file \"".as_slice(),
                &from_native(path),
                b"\" cannot be read.",
            ]
            .concat(),
        )
    })?;
    let converted = if opts.no_hex {
        None
    } else {
        decode_records(&bytes).filter(|decoded| {
            let output = cx
                .current_binary_dir()
                .join("CMakeFiles/FileCommandStringsBinaryFile");
            let output = cx.resolve_path(&from_native(&output));
            cx.host()
                .write(&output, decoded, WriteMode::Truncate)
                .is_ok()
        })
    };
    let bytes = converted.as_deref().unwrap_or(&bytes);
    let mut output = StringList::default();
    let mut line = BString::default();
    let mut budget = cx.limits().regex_steps;
    for ch in decode(bytes, &opts) {
        let finished = match ch {
            Some('\r') => continue,
            Some(ch @ (' '..='~' | '\t')) => {
                line.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                false
            }
            Some('\n') if opts.newline => {
                line.push(b'\n');
                false
            }
            Some(ch) if !ch.is_ascii() => {
                line.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                false
            }
            Some('\n') => true,
            _ => !line.is_empty(),
        };
        if line.len() > cx.limits().max_value_bytes {
            return Err(error("STRINGS exceeds the variable size limit"));
        }
        if finished || (opts.maximum != 0 && line.len() >= opts.maximum) {
            if output.append(cx, &opts, &line, &mut budget)? {
                return set(cx, out, output.value);
            }
            line.clear();
        }
    }
    if !line.is_empty() {
        output.append(cx, &opts, &line, &mut budget)?;
    }
    set(cx, out, output.value)
}

#[derive(Default)]
struct StringList {
    value: BString,
    count: usize,
    raw_size: usize,
}

impl StringList {
    fn append(
        &mut self,
        cx: &mut Cx<'_>,
        opts: &Options,
        line: &[u8],
        budget: &mut u64,
    ) -> Result<bool> {
        if line.len() < opts.minimum {
            return Ok(false);
        }
        if let Some(regex) = &opts.regex {
            let Some(captures) = regex
                .find_at(line, 0, MatchOptions::default(), budget)
                .map_err(|e| error(e.to_string()))?
            else {
                return Ok(false);
            };
            if cx.policy(PolicyId::new(159)) == PolicyStatus::New {
                cx.set_match_vars(Some((&captures, line)));
            }
        }
        self.raw_size = self.raw_size.saturating_add(line.len()).saturating_add(1);
        if opts.output.is_some_and(|limit| self.raw_size >= limit) {
            return Ok(true);
        }
        let extra = line.iter().filter(|&&byte| byte == b';').count();
        let size = self
            .value
            .len()
            .saturating_add(usize::from(self.count != 0))
            .saturating_add(line.len())
            .saturating_add(extra);
        if size > cx.limits().max_value_bytes {
            return Err(error("STRINGS exceeds the variable size limit"));
        }
        if self.count != 0 {
            self.value.push(b';');
        }
        for &byte in line {
            if byte == b';' {
                self.value.push(b'\\');
            }
            self.value.push(byte);
        }
        self.count += 1;
        Ok(opts.count != 0 && self.count >= opts.count)
    }
}

fn decode<'a>(bytes: &'a [u8], opts: &Options) -> Box<dyn Iterator<Item = Option<char>> + 'a> {
    let mut data = bytes;
    let mut encoding = opts.encoding.unwrap_or(Encoding::Ascii);
    for (bom, detected) in [
        (b"\xff\xfe\0\0".as_slice(), Encoding::Utf32(true)),
        (b"\0\0\xfe\xff".as_slice(), Encoding::Utf32(false)),
        (b"\xef\xbb\xbf".as_slice(), Encoding::Utf8),
        (b"\xff\xfe".as_slice(), Encoding::Utf16(true)),
        (b"\xfe\xff".as_slice(), Encoding::Utf16(false)),
    ] {
        if let Some(rest) = data.strip_prefix(bom) {
            data = rest;
            encoding = opts.encoding.unwrap_or(detected);
            break;
        }
    }
    let bom_len = bytes.len() - data.len();
    let limit = opts
        .input
        .map_or(data.len(), |limit| limit.saturating_sub(bom_len))
        .min(data.len());
    match encoding {
        Encoding::Ascii => Box::new(
            data.iter()
                .take(limit)
                .map(|&b| b.is_ascii().then(|| char::from(b))),
        ),
        Encoding::Utf8 => {
            let mut remaining = data;
            Box::new(std::iter::from_fn(move || {
                if data.len() - remaining.len() >= limit {
                    return None;
                }
                let (&first, _) = remaining.split_first()?;
                let width = match first {
                    0..=0x7f => 1,
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 1,
                };
                let decoded = remaining
                    .get(..width)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(|text| text.chars().next());
                let consumed = if decoded.is_some() { width } else { 1 };
                remaining = remaining.get(consumed..).unwrap_or_default();
                Some(decoded)
            }))
        }
        Encoding::Utf16(le) => Box::new(
            data.as_chunks::<2>()
                .0
                .iter()
                .copied()
                .take(limit.div_ceil(2))
                .map(move |word| {
                    let value = if le {
                        u16::from_le_bytes(word)
                    } else {
                        u16::from_be_bytes(word)
                    };
                    char::from_u32(u32::from(value)).filter(char::is_ascii)
                }),
        ),
        Encoding::Utf32(le) => Box::new(
            data.as_chunks::<4>()
                .0
                .iter()
                .copied()
                .take(limit.div_ceil(4))
                .map(move |word| {
                    let value = if le {
                        u32::from_le_bytes(word)
                    } else {
                        u32::from_be_bytes(word)
                    };
                    char::from_u32(value).filter(char::is_ascii)
                }),
        ),
    }
}

fn decode_records(input: &[u8]) -> Option<Vec<u8>> {
    let intel = input.starts_with(b":");
    if !intel && !input.starts_with(b"S0") {
        return None;
    }
    let mut output = Vec::new();
    let mut ended = false;
    for line in input.split(|&b| b == b'\n') {
        let line = line.trim_ascii_end();
        if line.is_empty() {
            continue;
        }
        if ended {
            return None;
        }
        if intel {
            let bytes = unhex(line.strip_prefix(b":")?)?;
            let [count, _, _, kind, payload @ .., _] = bytes.as_slice() else {
                return None;
            };
            if usize::from(*count) != payload.len() || !checksum(&bytes, 0) {
                return None;
            }
            match kind {
                0 => output.extend_from_slice(payload),
                1 if payload.is_empty() => ended = true,
                2..=5 => {}
                _ => return None,
            }
        } else {
            let [b'S', record, hex @ ..] = line else {
                return None;
            };
            let bytes = unhex(hex)?;
            let [count, body @ .., _] = bytes.as_slice() else {
                return None;
            };
            if usize::from(*count) != body.len() + 1 || !checksum(&bytes, 255) {
                return None;
            }
            match record {
                b'0' | b'5' | b'6' => {}
                b'1'..=b'3' => {
                    output.extend_from_slice(body.get(usize::from(record - b'1') + 2..)?)
                }
                b'7'..=b'9' => ended = true,
                _ => return None,
            }
        }
    }
    ended.then_some(output)
}

fn checksum(bytes: &[u8], target: u8) -> bool {
    bytes.iter().copied().fold(0, u8::wrapping_add) == target
}

fn unhex(input: &[u8]) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(2) {
        return None;
    }
    input
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[hi, lo]| {
            let hi = char::from(*hi).to_digit(16)?;
            let lo = char::from(*lo).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}
