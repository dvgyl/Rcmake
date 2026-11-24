use std::{fmt::Write as _, sync::Mutex};

use jiff::{Timestamp, fmt::strtime, tz::TimeZone};
use rcmake_base::{BString, ByteSlice, hash::HashAlgo};
use rcmake_eval::{CmdResult, Cx};
use rcmake_expr::{
    ExpandEnv,
    configure::{ConfigureOptions, configure as expand_template},
};

use super::{append, check_size, decimal_prefix, set, status};

pub(super) fn ascii(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [codes @ .., out] = args else {
        return Err(status("No output variable specified"));
    };
    if codes.is_empty() {
        return Err(status("No output variable specified"));
    }
    check_size(cx, codes.len())?;
    let mut result = Vec::with_capacity(codes.len());
    for code in codes {
        let value = u8::try_from(decimal_prefix(code) as i32)
            .ok()
            .filter(|&value| value != 0)
            .ok_or_else(|| {
                status(format!(
                    "Character with code {} does not exist.",
                    code.to_str_lossy()
                ))
            })?;
        result.push(value);
    }
    set(cx, out, result)
}

struct TemplateEnv<'a, 'b> {
    cx: &'a mut Cx<'b>,
    line: u32,
}

impl ExpandEnv for TemplateEnv<'_, '_> {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.cx.var(name)
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.cx.cache().get(name).map(|entry| entry.value.clone())
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.cx.host().env_get(name)
    }

    fn current_line(&self) -> u32 {
        self.line
    }
}

pub(super) fn configure(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [input, out, flags @ ..] = args else {
        return Err(status(if args.is_empty() {
            "No input string specified."
        } else {
            "No output variable specified."
        }));
    };
    let mut options = ConfigureOptions::default();
    for flag in flags {
        match *flag {
            b"@ONLY" => options.at_only = true,
            b"ESCAPE_QUOTES" => options.escape_quotes = true,
            _ => {
                return Err(status(format!(
                    "Unrecognized argument \"{}\"",
                    flag.to_str_lossy()
                )));
            }
        }
    }
    let mut result = Vec::new();
    // The shared file expander always emits a newline and strips CR at the end of a line.
    let mut env = TemplateEnv {
        line: cx.current_line(),
        cx,
    };
    for line in input.split_inclusive(|&byte| byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let trimmed = body.strip_suffix(b"\r").unwrap_or(body);
        let expanded =
            expand_template(trimmed, &mut env, options).map_err(|error| status(error.message))?;
        append(
            env.cx,
            &mut result,
            expanded.strip_suffix(b"\n").unwrap_or(&expanded),
        )?;
        append(
            env.cx,
            &mut result,
            line.get(trimmed.len()..).unwrap_or_default(),
        )?;
        env.line = env.line.saturating_add(1);
    }
    set(cx, out, result)
}

// CMake uses the process-wide C random number generator. This is the sequence
// from the pinned macOS oracle, including the special transition from state zero.
static RANDOM_STATE: Mutex<Option<u32>> = Mutex::new(None);

pub(super) fn random(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    const ALPHABET: &[u8] = b"qwertyuiopasdfghjklzxcvbnmQWERTYUIOPASDFGHJKLZXCVBNM0123456789";
    let missing = || status("sub-command RANDOM requires at least one argument.");
    let [options @ .., out] = args else {
        return Err(missing());
    };
    let mut length = 5;
    let mut alphabet = ALPHABET;
    let mut seed = None;
    let mut options = options.iter().copied();
    while let Some(option) = options.next() {
        match option {
            b"LENGTH" => length = decimal_prefix(options.next().ok_or_else(missing)?) as i32,
            b"ALPHABET" => alphabet = options.next().ok_or_else(missing)?,
            b"RANDOM_SEED" => {
                seed = Some(decimal_prefix(options.next().ok_or_else(missing)?) as u32)
            }
            _ => {}
        }
    }
    if length <= 0 {
        return Err(status("sub-command RANDOM: Invoked with bad length.."));
    }
    let length = length as usize;
    check_size(cx, length)?;
    if alphabet.is_empty() {
        alphabet = ALPHABET;
    }
    let mut state = RANDOM_STATE
        .lock()
        .map_err(|_| status("could not access random state."))?;
    if let Some(seed) = seed {
        *state = Some(seed);
    }
    let state = state.get_or_insert_with(|| {
        let now = cx.host().now();
        (now.secs as u32).rotate_left(11) ^ now.nanos
    });
    let mut result = Vec::with_capacity(length);
    for _ in 0..length {
        let previous = if *state == 0 { 123_459_876 } else { *state };
        *state = ((u64::from(previous) * 16_807) % 2_147_483_647) as u32;
        let index = (u64::from(*state) * alphabet.len() as u64 / 2_147_483_648) as usize;
        if let Some(&byte) = alphabet.get(index) {
            result.push(byte);
        }
    }
    set(cx, out, result)
}

pub(super) fn timestamp(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let (out, format, utc) = match args {
        [] => {
            return Err(status(
                "sub-command TIMESTAMP requires at least one argument.",
            ));
        }
        [out] => (*out, b"".as_slice(), false),
        [out, b"UTC", ..] if args.len() <= 3 => (*out, b"".as_slice(), true),
        [out, format] => (*out, *format, false),
        [out, format, b"UTC"] => (*out, *format, true),
        [_, _, option] => {
            return Err(status(format!(
                "TIMESTAMP sub-command does not recognize option {}.",
                option.to_str_lossy()
            )));
        }
        _ => {
            return Err(status(
                "sub-command TIMESTAMP takes at most three arguments.",
            ));
        }
    };
    let format = if format.is_empty() {
        if utc {
            b"%Y-%m-%dT%H:%M:%SZ".as_slice()
        } else {
            b"%Y-%m-%dT%H:%M:%S".as_slice()
        }
    } else {
        format
    };
    let (secs, nanos) = if let Some(epoch) = cx
        .host()
        .env_get(b"SOURCE_DATE_EPOCH")
        .filter(|epoch| !epoch.is_empty())
    {
        let secs = rcmake_base::parse_source_date_epoch(&epoch).ok_or_else(|| {
            crate::exit_with_error(cx, 27, b"Cannot parse SOURCE_DATE_EPOCH as integer")
        })?;
        if secs == -1 {
            return set(cx, out, "");
        }
        (secs, 0)
    } else {
        let now = cx.host().now();
        (now.secs, now.nanos as i32)
    };
    let Ok(time) = Timestamp::new(secs, nanos) else {
        return set(cx, out, "");
    };
    let zone = if utc {
        TimeZone::UTC
    } else {
        cx.host().time_zone()
    };
    let zoned = time.to_zoned(zone);
    let mut output = Vec::new();
    let mut bytes = format.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            append(cx, &mut output, &[byte])?;
            continue;
        }
        match bytes.next() {
            Some(b'%') | None => append(cx, &mut output, b"%")?,
            Some(b'f') => append(cx, &mut output, format!("{:06}", nanos / 1000).as_bytes())?,
            Some(b's') => append(cx, &mut output, secs.to_string().as_bytes())?,
            Some(b'Z') if utc => append(cx, &mut output, b"GMT")?,
            Some(b'Z') => append(
                cx,
                &mut output,
                zoned
                    .time_zone()
                    .to_offset_info(time)
                    .abbreviation()
                    .as_bytes(),
            )?,
            Some(
                spec @ (b'd' | b'H' | b'I' | b'j' | b'm' | b'b' | b'B' | b'M' | b'S' | b'U' | b'V'
                | b'w' | b'a' | b'A' | b'y' | b'Y' | b'z'),
            ) => {
                let Ok(formatted) = strtime::format([b'%', spec], &zoned) else {
                    return set(cx, out, "");
                };
                append(cx, &mut output, formatted.as_bytes())?;
            }
            Some(spec) => append(cx, &mut output, &[b'%', spec])?,
        }
    }
    set(cx, out, output)
}

pub(super) fn uuid(cx: &mut Cx<'_>, args: &[&[u8]]) -> CmdResult {
    let [out, options @ ..] = args else {
        return Err(status("UUID sub-command requires an output variable."));
    };
    let mut namespace = b"".as_slice();
    let mut name = b"".as_slice();
    let mut algorithm = b"MD5".as_slice();
    let mut upper = false;
    let mut iter = options.iter().copied();
    while let Some(option) = iter.next() {
        let missing = || {
            status(format!(
                "UUID sub-command, {} requires a value.",
                option.to_str_lossy()
            ))
        };
        match option {
            b"NAMESPACE" => namespace = iter.next().ok_or_else(missing)?,
            b"NAME" => name = iter.next().ok_or_else(missing)?,
            b"TYPE" => algorithm = iter.next().ok_or_else(missing)?,
            b"UPPER" => upper = true,
            _ => {
                return Err(status(format!(
                    "UUID sub-command does not recognize option {}.",
                    option.to_str_lossy()
                )));
            }
        }
    }

    let (algo, version) = match algorithm {
        b"MD5" => (HashAlgo::Md5, 3),
        b"SHA1" => (HashAlgo::Sha1, 5),
        _ => {
            return Err(status(format!(
                "UUID sub-command, unknown TYPE '{}'.",
                algorithm.to_str_lossy()
            )));
        }
    };
    let mut data = decode_uuid(namespace)
        .ok_or_else(|| status("UUID sub-command, malformed NAMESPACE UUID."))?;
    data.extend_from_slice(name);
    let digest = algo.hex_digest(&data);
    let invalid_digest = || status("UUID sub-command could not read the digest.");
    let mut uuid: Vec<u8> = digest
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .take(16)
        .map(|pair| decode_hex(pair))
        .collect::<Option<_>>()
        .ok_or_else(invalid_digest)?;
    let byte = uuid.get_mut(6).ok_or_else(invalid_digest)?;
    *byte = (*byte & 0x0f) | (version << 4);
    let byte = uuid.get_mut(8).ok_or_else(invalid_digest)?;
    *byte = (*byte & 0x3f) | 0x80;
    let mut result = String::with_capacity(36);
    for (i, byte) in uuid.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            result.push('-');
        }
        let _ = write!(result, "{byte:02x}");
    }
    if upper {
        result.make_ascii_uppercase();
    }
    set(cx, out, result)
}

fn decode_uuid(input: &[u8]) -> Option<Vec<u8>> {
    let mut groups = input.split(|&byte| byte == b'-');
    let mut bytes = Vec::with_capacity(16);
    for length in [8, 4, 4, 4, 12] {
        let group = groups.next().filter(|group| group.len() == length)?;
        for pair in group.as_chunks::<2>().0 {
            bytes.push(decode_hex(pair)?);
        }
    }
    groups.next().is_none().then_some(bytes)
}

fn decode_hex(pair: &[u8]) -> Option<u8> {
    if !pair.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
}
