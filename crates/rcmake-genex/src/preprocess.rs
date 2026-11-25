//! Configure-time helpers (`cmGeneratorExpression::Find`, `Preprocess`).

use rcmake_base::BString;

use crate::parse::render;
use crate::{GenexAst, GenexExpr, GenexNode};

/// Which interface `strip` keeps (`cmGeneratorExpression::PreprocessContext`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PreprocessMode {
    /// Keep `$<BUILD_INTERFACE:...>` content, drop `$<INSTALL_INTERFACE:...>`.
    Build,
    /// Keep `$<INSTALL_INTERFACE:...>` content, drop `$<BUILD_INTERFACE:...>`.
    Install,
}

/// Byte offset of the first `$<` in `input` (`cmGeneratorExpression::Find`).
#[must_use]
pub fn find_genex(input: &[u8]) -> Option<usize> {
    input.windows(2).position(|w| w == b"$<")
}

/// Remove every complete `$<...>` expression from `input`, without parsing operators
/// (`string(GENEX_STRIP)`, `list(TRANSFORM GENEX_STRIP)`). Text after an unterminated
/// `$<` is kept.
#[must_use]
pub fn genex_strip(input: &[u8]) -> BString {
    let mut out = Vec::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = find_genex(rest) {
        out.extend_from_slice(rest.get(..pos).unwrap_or_default());
        let body = rest.get(pos + 2..).unwrap_or_default();
        let mut depth = 1_usize;
        let mut i = 0;
        let mut end = None;
        while let Some(&byte) = body.get(i) {
            if byte == b'$' && body.get(i + 1) == Some(&b'<') {
                depth += 1;
                i += 2;
                continue;
            }
            if byte == b'>' {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            i += 1;
        }
        match end {
            Some(end) => rest = body.get(end + 1..).unwrap_or_default(),
            None => {
                out.extend_from_slice(rest.get(pos..).unwrap_or_default());
                rest = b"";
            }
        }
    }
    out.extend_from_slice(rest);
    out.into()
}

/// Keep the content of the selected interface and discard the other interfaces.
/// Preserve expressions inside the selected content.
#[must_use]
pub fn strip(ast: &GenexAst, mode: PreprocessMode) -> GenexAst {
    GenexAst {
        nodes: strip_nodes(&ast.nodes, mode),
    }
}

fn strip_nodes(nodes: &[GenexNode], mode: PreprocessMode) -> Vec<GenexNode> {
    let mut out = Vec::new();
    for node in nodes {
        match node {
            GenexNode::Text(_) => out.push(node.clone()),
            GenexNode::Expr(expr) => {
                let name = render(&expr.identifier);
                let keep = match (name.as_slice(), mode) {
                    (b"BUILD_INTERFACE", PreprocessMode::Build)
                    | (b"INSTALL_INTERFACE", PreprocessMode::Install) => Some(true),
                    (b"BUILD_INTERFACE", PreprocessMode::Install)
                    | (b"INSTALL_INTERFACE", PreprocessMode::Build)
                    | (b"BUILD_LOCAL_INTERFACE", _)
                        if expr.params.is_some() =>
                    {
                        Some(false)
                    }
                    _ => None,
                };
                match (keep, &expr.params) {
                    (Some(true), Some(params)) => {
                        for (i, param) in params.iter().enumerate() {
                            if i > 0 {
                                out.push(GenexNode::Text(",".into()));
                            }
                            out.extend(param.iter().cloned());
                        }
                    }
                    (Some(_), _) => {}
                    (None, _) => out.push(GenexNode::Expr(GenexExpr {
                        identifier: strip_nodes(&expr.identifier, mode),
                        params: expr
                            .params
                            .as_ref()
                            .map(|ps| ps.iter().map(|p| strip_nodes(p, mode)).collect()),
                    })),
                }
            }
        }
    }
    out
}

/// Replace each `$<INSTALL_PREFIX>` in `input` with `replacement` (for example
/// `${_IMPORT_PREFIX}` in generated export files).
#[must_use]
pub fn rewrite_install_prefix(input: &[u8], replacement: &[u8]) -> BString {
    const NEEDLE: &[u8] = b"$<INSTALL_PREFIX>";
    let mut out = Vec::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.windows(NEEDLE.len()).position(|w| w == NEEDLE) {
        out.extend_from_slice(rest.get(..pos).unwrap_or_default());
        out.extend_from_slice(replacement);
        rest = rest.get(pos + NEEDLE.len()..).unwrap_or_default();
    }
    out.extend_from_slice(rest);
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn genex_strip_drops_complete_expressions() {
        assert_eq!(genex_strip(b"a$<X:$<Y>>b$<Z>c"), "abc");
        assert_eq!(genex_strip(b"a$<open"), "a$<open");
        assert_eq!(genex_strip(b"a>b"), "a>b");
        assert_eq!(find_genex(b"ab$<c>"), Some(2));
    }

    #[test]
    fn strip_selects_interface() {
        let ast = parse(b"$<BUILD_INTERFACE:/src/inc>;$<INSTALL_INTERFACE:inc>;$<CONFIG>").unwrap();
        let build = render(&strip(&ast, PreprocessMode::Build).nodes);
        let install = render(&strip(&ast, PreprocessMode::Install).nodes);
        assert_eq!(build, "/src/inc;;$<CONFIG>");
        assert_eq!(install, ";inc;$<CONFIG>");
        assert_eq!(
            rewrite_install_prefix(b"$<INSTALL_PREFIX>/include", b"${_IMPORT_PREFIX}"),
            "${_IMPORT_PREFIX}/include"
        );
    }
}
