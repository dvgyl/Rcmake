//! Lowering of [`ResolvedTarget`]s into a [`BuildGraph`] (ADR-009 layer 3).
//!
//! Rule templates use CMake's placeholders: `<FLAGS> <DEFINES> <INCLUDES> <OBJECT>
//! <SOURCE> <DEP_FILE> <LINK_FLAGS> <LINK_LIBRARIES> <OBJECTS> <TARGET> <SONAME_FLAG>
//! <TARGET_INSTALLNAME_DIR>`.

use rcmake_base::BString;
use rcmake_genex::ConfigId;
use rcmake_model::ProjectModel;

use crate::{BuildGraph, ResolveError, ResolvedTarget};

/// Expand the `<NAME>` placeholders of a rule template with `lookup`. Unknown
/// placeholders stay as written.
#[must_use]
pub fn expand_rule(template: &[u8], lookup: impl Fn(&[u8]) -> Option<BString>) -> BString {
    let mut out = Vec::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.iter().position(|&b| b == b'<') {
        out.extend_from_slice(rest.get(..open).unwrap_or_default());
        let tail = rest.get(open + 1..).unwrap_or_default();
        let name_len = tail
            .iter()
            .position(|&b| !(b.is_ascii_uppercase() || b == b'_'))
            .unwrap_or(tail.len());
        let name = tail.get(..name_len).unwrap_or_default();
        match (tail.get(name_len), lookup(name)) {
            (Some(b'>'), Some(value)) if !name.is_empty() => {
                out.extend_from_slice(&value);
                rest = tail.get(name_len + 1..).unwrap_or_default();
            }
            _ => {
                out.push(b'<');
                rest = tail;
            }
        }
    }
    out.extend_from_slice(rest);
    out.into()
}

/// Lower the resolved targets of one configuration.
pub fn lower(
    _model: &ProjectModel,
    _targets: &[ResolvedTarget],
    _config: &ConfigId,
) -> Result<BuildGraph, ResolveError> {
    Err(ResolveError::Unimplemented("build graph lowering"))
}

#[cfg(test)]
mod tests {
    use super::expand_rule;

    #[test]
    fn expands_known_placeholders_only() {
        let out = expand_rule(
            b"<CMAKE_C_COMPILER> <FLAGS> -o <OBJECT> -c <SOURCE> <x> <",
            |n| match n {
                b"FLAGS" => Some("-O2".into()),
                b"OBJECT" => Some("a.o".into()),
                b"SOURCE" => Some("a.c".into()),
                _ => None,
            },
        );
        assert_eq!(out, "<CMAKE_C_COMPILER> -O2 -o a.o -c a.c <x> <");
    }
}
