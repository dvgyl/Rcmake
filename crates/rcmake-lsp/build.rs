//! Compress the generated CMake 4.5.1 LSP catalog into a compact zero-dependency binary blob.

#![forbid(unsafe_code)]
#![allow(
    clippy::disallowed_methods,
    clippy::exit,
    clippy::print_stderr,
    clippy::print_stdout
)]

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

const TABLES: [&str; 5] = [
    "COMMANDS",
    "VARIABLES",
    "POLICIES",
    "TARGET_PROPERTIES",
    "MODULES",
];

fn main() {
    println!("cargo:rerun-if-changed=src/generated_catalog.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let source = match fs::read_to_string("src/generated_catalog.rs") {
        Ok(text) => text,
        Err(err) => {
            eprintln!("failed to read src/generated_catalog.rs: {err}");
            std::process::exit(1);
        }
    };

    let raw = match serialize_catalog(&source) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("failed to parse src/generated_catalog.rs: {err}");
            std::process::exit(1);
        }
    };

    let compressed = compress_lz(&raw);
    let out_dir = match env::var_os("OUT_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            eprintln!("OUT_DIR is not set");
            std::process::exit(1);
        }
    };
    let out_path = out_dir.join("catalog.lz");
    if let Err(err) = fs::write(&out_path, compressed) {
        eprintln!("failed to write {}: {err}", out_path.display());
        std::process::exit(1);
    }
}

struct ParsedEntry {
    fields: [String; 4],
    flags: u8,
}

fn serialize_catalog(source: &str) -> Result<Vec<u8>, String> {
    let mut parsed_tables: Vec<Vec<ParsedEntry>> = Vec::with_capacity(TABLES.len());
    for table in TABLES {
        let entries = parse_table(source, table)?;
        parsed_tables.push(entries);
    }

    let mut out = Vec::with_capacity(320 * 1024);
    for entries in &parsed_tables {
        let count = u32::try_from(entries.len())
            .map_err(|_| format!("table exceeds u32 length: {}", entries.len()))?;
        out.extend_from_slice(&count.to_le_bytes());
    }
    for entries in &parsed_tables {
        for entry in entries {
            out.push(entry.flags);
            for field in &entry.fields {
                let bytes = field.as_bytes();
                let len = u16::try_from(bytes.len())
                    .map_err(|_| format!("field exceeds u16 length: {}", bytes.len()))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(bytes);
            }
        }
    }
    Ok(out)
}

fn parse_table(source: &str, table: &str) -> Result<Vec<ParsedEntry>, String> {
    let marker = format!("pub(super) static {table}: &[Entry] = &[");
    let start = source
        .find(&marker)
        .ok_or_else(|| format!("missing table {table}"))?
        + marker.len();
    let rest = source
        .get(start..)
        .ok_or_else(|| format!("invalid slice for table {table}"))?;
    let mut entries = Vec::new();
    let mut cursor = rest;

    loop {
        cursor = cursor.trim_start();
        if cursor.starts_with("];") {
            return Ok(entries);
        }
        let after_entry = cursor
            .strip_prefix("Entry(")
            .ok_or_else(|| format!("expected Entry( in {table}"))?;
        let (f0, c) = parse_rust_string(after_entry)?;
        let c = expect_comma(c, table)?;
        let (f1, c) = parse_rust_string(c)?;
        let c = expect_comma(c, table)?;
        let (f2, c) = parse_rust_string(c)?;
        let c = expect_comma(c, table)?;
        let (f3, c) = parse_rust_string(c)?;
        let c = expect_comma(c, table)?;
        let c = c.trim_start();
        let digits_len = c.bytes().take_while(u8::is_ascii_digit).count();
        let digits = c
            .get(..digits_len)
            .ok_or_else(|| format!("missing flags in {table}"))?;
        let flags = digits
            .parse::<u8>()
            .map_err(|err| format!("invalid flags '{digits}' in {table}: {err}"))?;
        let c = c
            .get(digits_len..)
            .ok_or_else(|| format!("truncated entry in {table}"))?
            .trim_start();
        let c = c
            .strip_prefix("),")
            .ok_or_else(|| format!("expected ), after Entry in {table}"))?;
        entries.push(ParsedEntry {
            fields: [f0, f1, f2, f3],
            flags,
        });
        cursor = c;
    }
}

fn expect_comma<'a>(input: &'a str, table: &str) -> Result<&'a str, String> {
    input
        .trim_start()
        .strip_prefix(',')
        .map(str::trim_start)
        .ok_or_else(|| format!("expected ',' in {table}"))
}

fn parse_rust_string(input: &str) -> Result<(String, &str), String> {
    let input = input.trim_start();
    let rest = input
        .strip_prefix('"')
        .ok_or_else(|| "expected opening '\"'".to_owned())?;
    let mut chars = rest.char_indices();
    let mut out = String::new();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                let tail = rest
                    .get(idx + 1..)
                    .ok_or_else(|| "invalid closing quote offset".to_owned())?;
                return Ok((out, tail));
            }
            '\\' => {
                let (_, esc) = chars
                    .next()
                    .ok_or_else(|| "unterminated escape sequence".to_owned())?;
                match esc {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '\'' => out.push('\''),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    'u' => {
                        let (_, brace) = chars
                            .next()
                            .ok_or_else(|| "unterminated \\u escape".to_owned())?;
                        if brace != '{' {
                            return Err("expected '{' after \\u".to_owned());
                        }
                        let mut hex = String::new();
                        let mut closed = false;
                        for (_, h) in chars.by_ref() {
                            if h == '}' {
                                closed = true;
                                break;
                            }
                            hex.push(h);
                        }
                        if !closed {
                            return Err("unterminated \\u{...} escape".to_owned());
                        }
                        let code = u32::from_str_radix(&hex, 16)
                            .map_err(|err| format!("invalid unicode escape '{hex}': {err}"))?;
                        let decoded = char::from_u32(code)
                            .ok_or_else(|| format!("invalid unicode scalar 0x{code:x}"))?;
                        out.push(decoded);
                    }
                    other => return Err(format!("unsupported escape '\\{other}'")),
                }
            }
            other => out.push(other),
        }
    }
    Err("unterminated string literal".to_owned())
}

/// Compress `data` using a compact 15-bit LZSS stream preceded by the 4-byte LE uncompressed length.
///
/// Format:
/// - `u32` LE uncompressed length
/// - Repeating blocks of 1 flag byte + up to 8 tokens (LSB-first):
///   - flag bit 0: 1 literal byte
///   - flag bit 1: 2-byte LE token `tok`:
///     - if `(tok & 0x8000) == 0`: short match, `dist = (tok >> 4) + 1` (1..=2048),
///       `len = (tok & 0x0f) + 3` (3..=17)
///     - if `(tok & 0x8000) != 0`: long match, `dist = (tok & 0x7fff) + 1` (1..=32768),
///       followed by 1 byte `extra`, `len = extra + 3` (3..=258)
fn compress_lz(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let total_len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&total_len.to_le_bytes());

    let mut head: HashMap<[u8; 3], Vec<usize>> = HashMap::with_capacity(16384);
    let mut i = 0usize;
    let n = data.len();

    while i < n {
        let flag_pos = out.len();
        out.push(0);
        let mut flags = 0u8;

        for bit in 0..8u8 {
            if i >= n {
                break;
            }
            let mut best_len = 0usize;
            let mut best_dist = 0usize;
            if let Some(key_slice) = data.get(i..i + 3) {
                let key = [
                    key_slice.first().copied().unwrap_or(0),
                    key_slice.get(1).copied().unwrap_or(0),
                    key_slice.get(2).copied().unwrap_or(0),
                ];
                if let Some(prevs) = head.get(&key) {
                    let start = prevs.len().saturating_sub(32);
                    if let Some(window) = prevs.get(start..) {
                        for &p in window.iter().rev() {
                            let dist = i.saturating_sub(p);
                            if dist == 0 || dist > 32768 {
                                break;
                            }
                            let max_len = 258.min(n.saturating_sub(i));
                            let mut len = 3usize;
                            while len < max_len && data.get(p + len) == data.get(i + len) {
                                len += 1;
                            }
                            if len > best_len && (len >= 4 || dist <= 2048) {
                                best_len = len;
                                best_dist = dist;
                                if len == 258 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if best_len >= 4 || (best_len == 3 && best_dist <= 2048) {
                flags |= 1u8 << bit;
                if best_dist <= 2048 && best_len <= 17 {
                    let tok = (((best_dist - 1) as u16) << 4) | ((best_len - 3) as u16);
                    out.extend_from_slice(&tok.to_le_bytes());
                } else {
                    let tok = 0x8000u16 | ((best_dist - 1) as u16);
                    out.extend_from_slice(&tok.to_le_bytes());
                    out.push((best_len - 3) as u8);
                }
                for k in 0..best_len {
                    insert_key(&mut head, data, i + k);
                }
                i += best_len;
            } else {
                if let Some(&byte) = data.get(i) {
                    out.push(byte);
                }
                insert_key(&mut head, data, i);
                i += 1;
            }
        }
        if let Some(slot) = out.get_mut(flag_pos) {
            *slot = flags;
        }
    }
    out
}

fn insert_key(head: &mut HashMap<[u8; 3], Vec<usize>>, data: &[u8], pos: usize) {
    if let Some(key_slice) = data.get(pos..pos + 3) {
        let key = [
            key_slice.first().copied().unwrap_or(0),
            key_slice.get(1).copied().unwrap_or(0),
            key_slice.get(2).copied().unwrap_or(0),
        ];
        head.entry(key).or_default().push(pos);
    }
}
