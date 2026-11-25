// SPDX-License-Identifier: MIT OR Apache-2.0

//! Source text that can read files outside the compiler dependency list.

/// Return whether the text has no construct that hides a file input or
/// makes the compile result depend on time.
pub(super) fn source_hazards(source: &[u8]) -> bool {
    let compact = source
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'\\')
        .collect::<Vec<_>>();
    const DISALLOWED: &[&[u8]] = &[
        b"#embed",
        b"__has_embed",
        b"#pragmaGCCdependency",
        b"#pragmainclude_alias",
        b"#pragmaclangmodule",
        b"#pragmacomment(lib",
        b".incbin",
        b".linker",
        b"__DATE__",
        b"__TIME__",
        b"__TIMESTAMP__",
        b"__COUNTER__",
        b"__pragma",
        b"module;",
        b"@import",
        b"../",
        b"#include\"/",
        b"#include</",
        b"\"/",
        b"__has_include(\"/",
        b"__has_include(</",
        b"%:",
        b"??=",
    ];
    let contains = |needle: &[u8]| compact.windows(needle.len()).any(|part| part == needle);
    let external_pragma = (contains(b"pragma") || contains(b"_Pragma"))
        && ([b"dependency".as_slice(), b"include_alias", b"clangmodule"]
            .iter()
            .any(|pattern| contains(pattern))
            || (contains(b"comment") && contains(b"lib")));
    let identifier_separator = |byte: &u8| !byte.is_ascii_alphanumeric() && *byte != b'_';
    let embed_directive = contains(b"#")
        && compact
            .split(identifier_separator)
            .any(|word| word == b"embed");
    let assembly = source
        .split(identifier_separator)
        .any(|word| matches!(word, b"asm" | b"__asm" | b"__asm__"));
    let concealed_assembly = assembly
        && (contains(b"\"\"")
            || contains(b"\"/*")
            || contains(b"\"//")
            || source
                .windows(2)
                .any(|pair| matches!(pair, [b'\\', b'0'..=b'9' | b'x' | b'u' | b'U'])));
    !external_pragma
        && literal_pragma_operators(source)
        && !embed_directive
        && !concealed_assembly
        && !DISALLOWED.iter().any(|needle| contains(needle))
        && !source
            .split(identifier_separator)
            .any(|word| word == b"import")
}

/// Accept `_Pragma` only with one plain string literal operand.
///
/// That form is equal to a `#pragma` line with the same text, so the pragma
/// word checks apply to it. A macro operand can build any pragma text.
fn literal_pragma_operators(source: &[u8]) -> bool {
    const OPERATOR: &[u8] = b"_Pragma";
    let skip_space = |bytes: &[u8]| -> usize {
        bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .unwrap_or(bytes.len())
    };
    let mut rest = source;
    while let Some(start) = rest
        .windows(OPERATOR.len())
        .position(|window| window == OPERATOR)
    {
        let Some(after) = rest.get(start + OPERATOR.len()..) else {
            return false;
        };
        let after = after.get(skip_space(after)..).unwrap_or_default();
        let Some(after) = after.strip_prefix(b"(") else {
            return false;
        };
        let after = after.get(skip_space(after)..).unwrap_or_default();
        let Some(literal) = after.strip_prefix(b"\"") else {
            return false;
        };
        let Some(end) = literal
            .iter()
            .position(|byte| matches!(byte, b'"' | b'\\' | b'\n'))
        else {
            return false;
        };
        if literal.get(end) != Some(&b'"') {
            return false;
        }
        let after = literal.get(end + 1..).unwrap_or_default();
        let after = after.get(skip_space(after)..).unwrap_or_default();
        let Some(after) = after.strip_prefix(b")") else {
            return false;
        };
        rest = after;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::source_hazards;

    #[test]
    fn source_scan_rejects_hidden_file_inputs() {
        assert!(source_hazards(b"asm(\"nop\");"));
        assert!(!source_hazards(b"#/**/embed \"resource\""));
        assert!(!source_hazards(b"#pragma/**/comment(lib, \"resource\")"));
        assert!(!source_hazards(b"asm(\".inc\" \"bin file\");"));
        assert!(!source_hazards(b"asm(\"\\056incbin file\");"));
    }

    #[test]
    fn pragma_operators_need_a_plain_string_literal() {
        assert!(source_hazards(
            b"#define A _Pragma(\"clang abi_ptr_attr set(single)\")\n_Pragma ( \"once\" )"
        ));
        for source in [
            b"#define P(x) _Pragma(#x)".as_slice(),
            b"_Pragma(TEXT)",
            b"_Pragma(\"GCC dependency \\\"x.h\\\"\")",
            b"_Pragma(\"GCC dependency x.h\")",
            b"_Pragma(L\"once\")",
            b"_Pragma(\"once\"",
            b"_Pragma",
        ] {
            assert!(
                !source_hazards(source),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }
}
