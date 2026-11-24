//! Resource entries from the Qt resource collection fallback reader.

use std::fmt;

use crate::BString;

/// Maximum size of a resource collection file in bytes.
pub const MAX_RESOURCE_FILE_BYTES: u64 = 16 * 1024 * 1024;

const MAX_ENTRIES: usize = 262_144;

/// The resource collection exceeds the input, entry count, or output size limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimit;

impl fmt::Display for ResourceLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The resource collection exceeds the size limit")
    }
}

impl std::error::Error for ResourceLimit {}

/// Read resource path tokens in CMake's fallback order.
///
/// The fallback reads the bytes between each `<file...>` opening tag and the next tag.
/// It preserves whitespace, entity spelling, and repeated entries. Resolve the returned
/// tokens relative to the directory that contains the resource collection.
///
/// # Errors
///
/// Returns [`ResourceLimit`] if the input or expanded list exceeds the size limits.
pub fn parse(contents: &[u8]) -> Result<Vec<BString>, ResourceLimit> {
    if contents.len() as u64 > MAX_RESOURCE_FILE_BYTES {
        return Err(ResourceLimit);
    }
    let contents = contents.split(|&byte| byte == 0).next().unwrap_or_default();
    let mut entries = Vec::new();
    let mut output_size = 0_u64;
    let mut cursor = 0;
    let mut remaining = contents;
    while let Some(start) = remaining.windows(5).position(|part| part == b"<file") {
        let absolute_start = contents.len() - remaining.len() + start;
        let matched = remaining.get(start..).unwrap_or_default();
        let after_prefix = matched.strip_prefix(b"<file").unwrap_or_default();
        let end = after_prefix
            .iter()
            .position(|&byte| byte == b'<')
            .map_or(matched.len(), |index| index + 5);
        if end == 5 {
            remaining = after_prefix;
            continue;
        }
        let tag = matched.get(..end).unwrap_or_default();
        let path = tag.splitn(2, |&byte| byte == b'>').nth(1).unwrap_or(tag);

        // The reference advances by the match length from the search start. A tag
        // can therefore be reported more than once when preceded by other XML.
        let copies = 1 + (absolute_start - cursor) / end;
        if entries.len() + copies > MAX_ENTRIES {
            return Err(ResourceLimit);
        }
        output_size += path.len() as u64 * copies as u64;
        if output_size > MAX_RESOURCE_FILE_BYTES {
            return Err(ResourceLimit);
        }
        entries.extend(std::iter::repeat_n(BString::from(path), copies));
        cursor += end * copies;
        remaining = contents.get(cursor..).unwrap_or_default();
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_reference_order_and_duplicate_entries() {
        let source = b"<RCC><qresource><file>asset.txt</file></qresource></RCC>";
        assert_eq!(
            parse(source),
            Ok(vec!["asset.txt".into(), "asset.txt".into()])
        );
    }

    #[test]
    fn preserves_lexical_paths_and_alias_independence() {
        let source = b"<file alias='alias'> dir/a&amp;b.txt </file><file>assets/c</file>";
        assert_eq!(
            parse(source),
            Ok(vec![" dir/a&amp;b.txt ".into(), "assets/c".into()])
        );
        assert_eq!(parse(b"<file"), Ok(Vec::new()));
    }

    #[test]
    fn rejects_expansion_beyond_the_entry_limit() {
        let mut source = vec![b' '; MAX_ENTRIES * 6];
        source.extend_from_slice(b"<file>");
        assert_eq!(parse(&source), Err(ResourceLimit));
    }
}
