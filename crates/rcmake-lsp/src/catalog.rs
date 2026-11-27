use std::sync::OnceLock;

pub(crate) const DOCS: &str = "https://cmake.org/cmake/help/v4.5";
pub(crate) const DEPRECATED: usize = 2;
pub(crate) const READONLY: usize = 4;

const COMPRESSED_CATALOG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/catalog.lz"));
/// Hard upper bound on decompressed catalog size (actual payload is ~423 KiB).
const MAX_DECOMPRESSED_CATALOG_BYTES: usize = 8 * 1024 * 1024;
/// Minimum serialized bytes per raw catalog entry (`flags: u8` + four `u16` string lengths).
const MIN_RAW_ENTRY_BYTES: usize = 1 + 4 * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry(
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) &'static str,
    pub(crate) usize,
);

#[derive(Clone, Copy)]
struct Span {
    start: u32,
    end: u32,
}

#[derive(Clone, Copy)]
struct RawEntry {
    name: Span,
    description: Span,
    signature: Span,
    stem: Span,
    flags: u8,
}

struct CatalogData {
    strings: String,
    tables: [Vec<RawEntry>; 5],
}

impl CatalogData {
    fn slice(&self, span: Span) -> &str {
        self.strings
            .get(span.start as usize..span.end as usize)
            .unwrap_or_default()
    }

    fn entry(&'static self, raw: RawEntry) -> Entry {
        Entry(
            self.slice(raw.name),
            self.slice(raw.description),
            self.slice(raw.signature),
            self.slice(raw.stem),
            usize::from(raw.flags),
        )
    }

    fn decode(compressed: &[u8]) -> Option<Self> {
        let raw = decompress_lz(compressed)?;
        let mut cursor = 0usize;
        let mut counts = [0usize; 5];
        for slot in &mut counts {
            *slot = usize::try_from(read_u32_le(&raw, &mut cursor)?).ok()?;
        }
        let remaining_bytes = raw.len().saturating_sub(cursor);
        let total_entries = counts
            .iter()
            .try_fold(0usize, |acc, &c| acc.checked_add(c))?;
        if total_entries > remaining_bytes / MIN_RAW_ENTRY_BYTES {
            return None;
        }
        let mut strings = String::with_capacity(remaining_bytes);
        let mut tables: [Vec<RawEntry>; 5] = Default::default();
        for (table_idx, count) in counts.into_iter().enumerate() {
            if count > raw.len().saturating_sub(cursor) / MIN_RAW_ENTRY_BYTES {
                return None;
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let flags = *raw.get(cursor)?;
                cursor += 1;
                let name = read_string_span(&raw, &mut cursor, &mut strings)?;
                let description = read_string_span(&raw, &mut cursor, &mut strings)?;
                let signature = read_string_span(&raw, &mut cursor, &mut strings)?;
                let stem = read_string_span(&raw, &mut cursor, &mut strings)?;
                entries.push(RawEntry {
                    name,
                    description,
                    signature,
                    stem,
                    flags,
                });
            }
            if let Some(slot) = tables.get_mut(table_idx) {
                *slot = entries;
            }
        }
        Some(Self { strings, tables })
    }
}

fn read_u32_le(data: &[u8], cursor: &mut usize) -> Option<u32> {
    let bytes = data.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    let arr: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_le_bytes(arr))
}

fn read_u16_le(data: &[u8], cursor: &mut usize) -> Option<u16> {
    let bytes = data.get(*cursor..*cursor + 2)?;
    *cursor += 2;
    let arr: [u8; 2] = bytes.try_into().ok()?;
    Some(u16::from_le_bytes(arr))
}

fn read_string_span(data: &[u8], cursor: &mut usize, arena: &mut String) -> Option<Span> {
    let len = usize::from(read_u16_le(data, cursor)?);
    let slice = data.get(*cursor..*cursor + len)?;
    *cursor += len;
    let text = std::str::from_utf8(slice).ok()?;
    let start = u32::try_from(arena.len()).ok()?;
    arena.push_str(text);
    let end = u32::try_from(arena.len()).ok()?;
    Some(Span { start, end })
}

fn decompress_lz(compressed: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let expected = usize::try_from(read_u32_le(compressed, &mut pos)?).ok()?;
    if expected > MAX_DECOMPRESSED_CATALOG_BYTES {
        return None;
    }
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected {
        let flags = *compressed.get(pos)?;
        pos += 1;
        for bit in 0..8u8 {
            if out.len() >= expected {
                break;
            }
            if (flags & (1u8 << bit)) == 0 {
                let byte = *compressed.get(pos)?;
                pos += 1;
                out.push(byte);
            } else {
                let tok = read_u16_le(compressed, &mut pos)?;
                let (dist, len) = if (tok & 0x8000) == 0 {
                    (usize::from(tok >> 4) + 1, usize::from(tok & 0x0f) + 3)
                } else {
                    let extra = *compressed.get(pos)?;
                    pos += 1;
                    (usize::from(tok & 0x7fff) + 1, usize::from(extra) + 3)
                };
                if dist == 0 || dist > out.len() || out.len().checked_add(len)? > expected {
                    return None;
                }
                for _ in 0..len {
                    let src = out.len().checked_sub(dist)?;
                    let byte = *out.get(src)?;
                    out.push(byte);
                }
            }
        }
    }
    (out.len() == expected).then_some(out)
}

fn catalog_data() -> &'static CatalogData {
    static DATA: OnceLock<CatalogData> = OnceLock::new();
    DATA.get_or_init(|| {
        CatalogData::decode(COMPRESSED_CATALOG).unwrap_or_else(|| CatalogData {
            strings: String::new(),
            tables: Default::default(),
        })
    })
}

#[derive(Clone, Copy)]
pub(crate) enum Topic {
    Command,
    Variable,
    Policy,
    TargetProperty,
    Module,
}

impl Topic {
    const fn index(self) -> usize {
        match self {
            Self::Command => 0,
            Self::Variable => 1,
            Self::Policy => 2,
            Self::TargetProperty => 3,
            Self::Module => 4,
        }
    }

    pub(crate) fn entries(self) -> impl ExactSizeIterator<Item = Entry> {
        let data = catalog_data();
        let slice = data.tables.get(self.index()).map_or(&[][..], Vec::as_slice);
        slice.iter().copied().map(move |raw| data.entry(raw))
    }

    pub(crate) fn lookup(self, name: &str) -> Option<Entry> {
        let data = catalog_data();
        let slice = data.tables.get(self.index())?;
        let index = slice
            .binary_search_by_key(&name, |raw| data.slice(raw.name))
            .ok()?;
        slice.get(index).copied().map(|raw| data.entry(raw))
    }

    pub(crate) fn link(self, entry: &Entry) -> String {
        let category = match self {
            Self::Command => "command",
            Self::Variable => "variable",
            Self::Policy => "policy",
            Self::TargetProperty => "prop_tgt",
            Self::Module => "module",
        };
        let name = if entry.3.is_empty() { entry.0 } else { entry.3 };
        format!("{DOCS}/{category}/{name}.html")
    }
}

pub(crate) fn command(name: &str) -> Option<Entry> {
    Topic::Command.lookup(&name.to_ascii_lowercase())
}

pub(crate) fn code_span(text: &str) -> String {
    let text = text.replace(['\r', '\n'], " ");
    let fence = "`".repeat(backticks(&text) + 1);
    if text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' '))
    {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

pub(crate) fn code_block(text: &str) -> String {
    let fence = "`".repeat((backticks(text) + 1).max(3));
    format!("{fence}cmake\n{text}\n{fence}")
}

fn backticks(text: &str) -> usize {
    text.split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default()
}

pub(crate) fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if "\\`*[]<>|&".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

pub(crate) const MODULE_COMMANDS: &[(&str, &str, &str, &str)] = &[
    (
        "FetchContent_Declare",
        "FetchContent",
        "FetchContent_Declare(<name> <content-options>...)",
        "Record how to obtain a dependency at configure time.",
    ),
    (
        "FetchContent_MakeAvailable",
        "FetchContent",
        "FetchContent_MakeAvailable(<name>...)",
        "Obtain the declared dependencies and make them available to the project.",
    ),
    (
        "FetchContent_GetProperties",
        "FetchContent",
        "FetchContent_GetProperties(<name> [SOURCE_DIR <var>] [BINARY_DIR <var>] [POPULATED <var>])",
        "Read the source directory, binary directory, or population status of a dependency.",
    ),
    (
        "FetchContent_Populate",
        "FetchContent",
        "FetchContent_Populate(<name> <content-options>...)",
        "Obtain the source files for a dependency.",
    ),
    (
        "ExternalProject_Add",
        "ExternalProject",
        "ExternalProject_Add(<name> <option>...)",
        "Add a target that drives the build of an external project.",
    ),
];

pub(crate) fn signature_override(name: &str) -> Option<&'static str> {
    match name {
        "target_link_libraries" => Some(
            "target_link_libraries(<target> <PRIVATE|PUBLIC|INTERFACE> <item>... [...])\n\
             target_link_libraries(<target> <item>...)",
        ),
        "target_compile_options" => Some(
            "target_compile_options(<target> [BEFORE] <INTERFACE|PUBLIC|PRIVATE> <option>... [...])",
        ),
        "target_include_directories" => Some(
            "target_include_directories(<target> [SYSTEM] [AFTER|BEFORE] <INTERFACE|PUBLIC|PRIVATE> <dir>... [...])",
        ),
        "target_compile_definitions" => Some(
            "target_compile_definitions(<target> <INTERFACE|PUBLIC|PRIVATE> <definition>... [...])",
        ),
        "target_compile_features" => {
            Some("target_compile_features(<target> <INTERFACE|PUBLIC|PRIVATE> <feature>... [...])")
        }
        "target_link_options" => Some(
            "target_link_options(<target> [BEFORE] <INTERFACE|PUBLIC|PRIVATE> <option>... [...])",
        ),
        "target_link_directories" => Some(
            "target_link_directories(<target> [BEFORE|AFTER] <INTERFACE|PUBLIC|PRIVATE> <dir>... [...])",
        ),
        "target_precompile_headers" => Some(
            "target_precompile_headers(<target> <INTERFACE|PUBLIC|PRIVATE> <header>... [...])\n\
             target_precompile_headers(<target> REUSE_FROM <other-target>)",
        ),
        "target_sources" => {
            Some("target_sources(<target> <INTERFACE|PUBLIC|PRIVATE> <source>... [...])")
        }
        "add_library" => Some("add_library(<name> [<type>] [EXCLUDE_FROM_ALL] [<source>...])"),
        "add_executable" => Some("add_executable(<name> [<option>...] [<source>...])"),
        "add_subdirectory" => {
            Some("add_subdirectory(<source-dir> [<binary-dir>] [EXCLUDE_FROM_ALL] [SYSTEM])")
        }
        "include" => Some(
            "include(<file|module> [OPTIONAL] [RESULT_VARIABLE <var>] [NO_POLICY_SCOPE] [NO_DIAGNOSTIC_SCOPE])",
        ),
        "add_dependencies" => Some("add_dependencies(<target> [<target-dependency>...])"),
        "set" => Some("set(<variable> <value>... [PARENT_SCOPE])"),
        "option" => Some("option(<variable> \"<help>\" [<value>])"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, Topic};
    use crate::generated_catalog;

    #[test]
    fn compressed_catalog_matches_generated_tables_exactly() {
        let pairs: [(Topic, &[Entry]); 5] = [
            (Topic::Command, generated_catalog::COMMANDS),
            (Topic::Variable, generated_catalog::VARIABLES),
            (Topic::Policy, generated_catalog::POLICIES),
            (Topic::TargetProperty, generated_catalog::TARGET_PROPERTIES),
            (Topic::Module, generated_catalog::MODULES),
        ];
        for (topic, expected) in pairs {
            let actual: Vec<Entry> = topic.entries().collect();
            assert_eq!(actual.len(), expected.len());
            for (got, want) in actual.iter().zip(expected.iter()) {
                assert_eq!(got, want);
                assert_eq!(topic.lookup(want.0), Some(*want));
            }
        }
    }
}
