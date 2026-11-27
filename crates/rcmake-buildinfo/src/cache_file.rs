//! Lenient read-only `CMakeCache.txt` reader.

use std::collections::BTreeMap;

use rcmake_base::BString;

use crate::snapshot::CacheRecord;

/// Parse cache entries. Malformed lines are skipped. `-ADVANCED` and other property
/// entries are kept as ordinary entries with their full names.
#[must_use]
pub fn parse(data: &[u8]) -> BTreeMap<BString, CacheRecord> {
    let mut entries = BTreeMap::new();
    let mut help = BString::default();
    for line in data.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(comment) = line.strip_prefix(b"//") {
            if let Some(rest) = comment.strip_prefix(b"\\n") {
                help.push(b'\n');
                help.extend_from_slice(rest);
            } else {
                help.extend_from_slice(comment);
            }
            continue;
        }
        let description = std::mem::take(&mut help);
        if line.starts_with(b"#") || line.trim_ascii().is_empty() {
            continue;
        }
        if let Some((name, ty, value)) = entry(line) {
            entries.insert(
                name,
                CacheRecord {
                    ty,
                    value,
                    help: description,
                },
            );
        }
    }
    entries
}

fn entry(line: &[u8]) -> Option<(BString, BString, BString)> {
    let (name, rest) = if let Some(quoted) = line.strip_prefix(b"\"") {
        let end = quoted.iter().position(|byte| *byte == b'"')?;
        (quoted.get(..end)?, quoted.get(end + 1..)?)
    } else {
        let end = line.iter().position(|byte| *byte == b':')?;
        (line.get(..end)?, line.get(end..)?)
    };
    let rest = rest.strip_prefix(b":")?;
    let equals = rest.iter().position(|byte| *byte == b'=')?;
    let ty = rest.get(..equals)?;
    let mut value = rest.get(equals + 1..)?;
    if value.len() >= 2 && value.first() == Some(&b'\'') && value.last() == Some(&b'\'') {
        value = value.get(1..value.len() - 1)?;
    }
    (!name.is_empty()).then(|| (name.into(), ty.into(), value.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_types_values_help_and_quoted_names() {
        let data = b"# comment\n//Build type\n//\\nsecond line\nCMAKE_BUILD_TYPE:STRING=Release\n\"odd:name\":BOOL=ON\nV:STRING='  spaced  '\nbroken line\nFoo_DIR:PATH=Foo_DIR-NOTFOUND\r\n";
        let entries = parse(data);
        let build = &entries[b"CMAKE_BUILD_TYPE".as_slice()];
        assert_eq!(build.value, "Release");
        assert_eq!(build.ty, "STRING");
        assert_eq!(build.help, "Build type\nsecond line");
        assert_eq!(entries[b"odd:name".as_slice()].value, "ON");
        assert_eq!(entries[b"V".as_slice()].value, "  spaced  ");
        assert_eq!(entries[b"Foo_DIR".as_slice()].value, "Foo_DIR-NOTFOUND");
        assert_eq!(entries.len(), 4);
    }
}
