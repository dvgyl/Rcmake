//! Read dependency evidence from compilers.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rcmake_host::{RunOutput, path};
#[cfg(test)]
use rcmake_json::{ReadOptions, Value};

const MAX_TRACE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PATHS: usize = 16_384;

fn insert(paths: &mut BTreeSet<PathBuf>, cwd: &Path, bytes: &[u8]) -> Option<()> {
    if bytes.is_empty() || bytes.contains(&0) || paths.len() >= MAX_PATHS {
        return None;
    }
    let input = path::to_native(bytes);
    if path::from_native(&input).as_slice() != bytes {
        return None;
    }
    paths.insert(cwd.join(input));
    Some(())
}

/// Parse make rules. An empty or malformed dependency list gives no evidence.
pub(crate) fn make_dependencies(bytes: &[u8], cwd: &Path) -> Option<BTreeSet<PathBuf>> {
    if bytes.len() > MAX_TRACE_BYTES {
        return None;
    }
    let mut paths = BTreeSet::new();
    let mut token = Vec::new();
    let mut targets = 0;
    let mut prerequisites = false;
    let mut rest = bytes;
    while let Some((&byte, tail)) = rest.split_first() {
        rest = tail;
        match (byte, rest) {
            (b'\\', [b'\r', b'\n', tail @ ..] | [b'\n', tail @ ..]) => {
                rest = tail;
            }
            (b'\\', [next @ (b' ' | b'\t' | b'#' | b':'), tail @ ..]) => {
                token.push(*next);
                rest = tail;
                continue;
            }
            (b'\\', [b'\\', tail @ ..]) => {
                // A raw UNC prefix uses two backslashes on Windows.
                if cfg!(windows) && token.is_empty() && !tail.starts_with(b"\\") {
                    token.push(b'\\');
                }
                token.push(b'\\');
                rest = tail;
                continue;
            }
            (b'\\', []) => return None,
            (b'$', [b'$', tail @ ..]) => {
                token.push(b'$');
                rest = tail;
                continue;
            }
            (b'$' | b'#' | b'|' | 0, _) => return None,
            (b':', [b'/' | b'\\', ..]) if matches!(token.as_slice(), [letter] if letter.is_ascii_alphabetic()) =>
            {
                token.push(byte);
                continue;
            }
            (b':' | b' ' | b'\t' | b'\r' | b'\n', _) => {}
            _ => {
                token.push(byte);
                continue;
            }
        }
        if !token.is_empty() {
            if prerequisites {
                insert(&mut paths, cwd, &token)?;
            } else {
                targets += 1;
            }
            token.clear();
        }
        if byte == b':' {
            if prerequisites || targets == 0 {
                return None;
            }
            prerequisites = true;
        } else if byte == b'\n' {
            if targets != 0 && !prerequisites {
                return None;
            }
            targets = 0;
            prerequisites = false;
        }
    }
    if !token.is_empty() {
        if !prerequisites {
            return None;
        }
        insert(&mut paths, cwd, &token)?;
    } else if targets != 0 && !prerequisites {
        return None;
    }
    (!paths.is_empty()).then_some(paths)
}

/// Read MSVC source and include paths from a UTF-8 source dependency document.
/// Imported modules and header units require additional evidence.
#[cfg(test)]
pub(crate) fn msvc_dependencies(bytes: &[u8], cwd: &Path) -> Option<BTreeSet<PathBuf>> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    if bytes.len() > MAX_TRACE_BYTES || std::str::from_utf8(bytes).is_err() {
        return None;
    }
    let options = ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: false,
    };
    let Value::Object(document) = rcmake_json::parse(bytes, &options).ok()? else {
        return None;
    };
    let Value::String(version) = document.get(b"Version".as_slice())? else {
        return None;
    };
    if !matches!(version.as_slice(), b"1.0" | b"1.1" | b"1.2") {
        return None;
    }
    let Value::Object(data) = document.get(b"Data".as_slice())? else {
        return None;
    };
    let Value::String(source) = data.get(b"Source".as_slice())? else {
        return None;
    };
    let Value::Array(includes) = data.get(b"Includes".as_slice())? else {
        return None;
    };
    if data.iter().any(|(key, value)| match key.as_slice() {
        b"Source" | b"Includes" => false,
        _ => {
            !matches!(value, Value::Array(items) if items.is_empty())
                && !matches!(value, Value::String(bytes) if bytes.is_empty())
        }
    }) {
        return None;
    }
    let mut paths = BTreeSet::new();
    insert(&mut paths, cwd, source)?;
    for include in includes {
        let Value::String(include) = include else {
            return None;
        };
        insert(&mut paths, cwd, include)?;
    }
    Some(paths)
}

/// Read localized include lines after the caller decodes the compiler output.
/// The caller supplies the detected prefix, including its punctuation.
#[cfg(test)]
pub(crate) fn msvc_show_includes(
    bytes: &[u8],
    prefix: &[u8],
    cwd: &Path,
) -> Option<BTreeSet<PathBuf>> {
    if bytes.len() > MAX_TRACE_BYTES || prefix.is_empty() {
        return None;
    }
    let mut paths = BTreeSet::new();
    for line in bytes.split(|&byte| byte == b'\n') {
        if let Some(file) = line.strip_prefix(prefix) {
            insert(&mut paths, cwd, file.trim_ascii())?;
        }
    }
    (!paths.is_empty()).then_some(paths)
}

pub(super) fn missing_input_diagnostic(output: &RunOutput) -> bool {
    const MESSAGES: &[&[u8]] = &[
        b"file not found",
        b"No such file or directory",
        b"cannot find -l",
        b"library not found",
        b"C1083:",
        b"LNK1104:",
    ];
    [&output.stdout, &output.stderr].into_iter().any(|bytes| {
        MESSAGES.iter().any(|message| {
            bytes
                .windows(message.len())
                .any(|window| window == *message)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_multiple_targets_and_continued_dependencies() {
        let cwd = Path::new("build");
        let files = make_dependencies(
            b"a.o C:/b.o: src.c dir/a\\ b.h \\\r\n next.h\ndir/a\\ b.h:\n",
            cwd,
        );
        assert_eq!(
            files,
            Some([cwd.join("src.c"), cwd.join("dir/a b.h"), cwd.join("next.h")].into())
        );
        assert!(make_dependencies(b"a.o: \n", cwd).is_none());
    }

    #[test]
    fn reads_localized_and_json_records() {
        let cwd = Path::new("build");
        let files = msvc_dependencies(br#"{"Version":"1.2","Data":{"Source":"a.c","Includes":["foo bar.h"],"ImportedModules":[]}}"#, cwd);
        assert_eq!(files, Some([cwd.join("a.c"), cwd.join("foo bar.h")].into()));
        let prefix = "Hinweis: Einlesen der Datei:".as_bytes();
        assert_eq!(
            msvc_show_includes(
                "Hinweis: Einlesen der Datei:   foo bar.h\r\n".as_bytes(),
                prefix,
                cwd
            ),
            Some([cwd.join("foo bar.h")].into())
        );
    }
}
