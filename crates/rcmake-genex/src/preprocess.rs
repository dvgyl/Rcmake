//! Configure-time generator expression detection, stripping, and prefix rewriting.

use std::borrow::Cow;

use rcmake_base::{BString, ByteSlice, path_lex::Lexical};

use crate::parse::render;
use crate::{GenexAst, GenexExpr, GenexNode};

/// Which interface (`BUILD_INTERFACE` or `INSTALL_INTERFACE`) [`strip`] keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PreprocessMode {
    /// Keep `$<BUILD_INTERFACE:...>` content, drop `$<INSTALL_INTERFACE:...>`.
    Build,
    /// Keep `$<INSTALL_INTERFACE:...>` content, drop `$<BUILD_INTERFACE:...>`.
    Install,
}

/// Byte offset of the first `$<` in `input`.
#[must_use]
pub fn find_genex(input: &[u8]) -> Option<usize> {
    input.find(b"$<")
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
        nodes: strip_nodes(&ast.nodes, mode, b""),
    }
}

impl GenexAst {
    /// Select the install interface and prefix relative paths in its payload.
    #[must_use]
    pub fn strip_install_paths(&self, prefix: &[u8]) -> Self {
        Self {
            nodes: strip_nodes(&self.nodes, PreprocessMode::Install, prefix),
        }
    }
}

fn strip_nodes(nodes: &[GenexNode], mode: PreprocessMode, prefix: &[u8]) -> Vec<GenexNode> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            GenexNode::Text(_) => out.push(node.clone()),
            GenexNode::Expr(expr) => {
                let name: Cow<'_, [u8]> = match expr.identifier.as_slice() {
                    [GenexNode::Text(text)] => Cow::Borrowed(text.as_slice()),
                    _ => Cow::Owned(render(&expr.identifier).into()),
                };
                let keep = match (name.as_ref(), mode) {
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
                        let mut payload = Vec::new();
                        for (i, param) in params.iter().enumerate() {
                            if i > 0 {
                                payload.push(GenexNode::Text(",".into()));
                            }
                            payload.extend(param.iter().cloned());
                        }
                        if prefix.is_empty() {
                            out.extend(payload);
                        } else {
                            prefix_items(payload, prefix, &mut out);
                        }
                    }
                    (Some(_), _) => {}
                    (None, _) => out.push(GenexNode::Expr(GenexExpr {
                        identifier: strip_nodes(&expr.identifier, mode, prefix),
                        params: expr
                            .params
                            .as_ref()
                            .map(|ps| ps.iter().map(|p| strip_nodes(p, mode, prefix)).collect()),
                    })),
                }
            }
        }
    }
    out
}

fn prefix_items(payload: Vec<GenexNode>, prefix: &[u8], out: &mut Vec<GenexNode>) {
    let mut start = true;
    let mut escaped = false;
    for node in payload {
        if let GenexNode::Text(text) = node {
            let mut offset = 0;
            for (i, &byte) in text.iter().enumerate() {
                if byte == b';' && !escaped {
                    push_path_text(
                        text.get(offset..=i).unwrap_or_default(),
                        prefix,
                        &mut start,
                        out,
                    );
                    start = true;
                    offset = i + 1;
                }
                escaped = byte == b'\\' && !escaped;
            }
            push_path_text(
                text.get(offset..).unwrap_or_default(),
                prefix,
                &mut start,
                out,
            );
        } else {
            out.push(node);
            start = false;
            escaped = false;
        }
    }
}

fn push_path_text(text: &[u8], prefix: &[u8], start: &mut bool, out: &mut Vec<GenexNode>) {
    if text.is_empty() {
        return;
    }
    if *start && text != b";" && !Lexical::new(text).absolute() {
        out.push(GenexNode::Text(prefix.into()));
    }
    out.push(GenexNode::Text(text.into()));
    *start = false;
}

/// Replace each `$<INSTALL_PREFIX>` in `input` with `replacement` (for example
/// `${_IMPORT_PREFIX}` in generated export files).
#[must_use]
pub fn rewrite_install_prefix(input: &[u8], replacement: &[u8]) -> BString {
    const NEEDLE: &[u8] = b"$<INSTALL_PREFIX>";
    let Some(first) = input.find(NEEDLE) else {
        return input.into();
    };
    let mut out = Vec::with_capacity(input.len());
    out.extend_from_slice(input.get(..first).unwrap_or_default());
    out.extend_from_slice(replacement);
    let mut rest = input.get(first + NEEDLE.len()..).unwrap_or_default();
    while let Some(pos) = rest.find(NEEDLE) {
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

    #[test]
    fn strip_preserves_nested_interface_inside_kept_wrapper() {
        let nested_install = parse(b"$<BUILD_INTERFACE:$<INSTALL_INTERFACE:inner>outer>").unwrap();
        assert_eq!(
            render(&strip(&nested_install, PreprocessMode::Build).nodes),
            "$<INSTALL_INTERFACE:inner>outer"
        );
        let nested_build = parse(b"$<INSTALL_INTERFACE:$<BUILD_INTERFACE:inner>outer>").unwrap();
        assert_eq!(
            render(&strip(&nested_build, PreprocessMode::Install).nodes),
            "$<BUILD_INTERFACE:inner>outer"
        );
        let dropped_wrapper = parse(b"$<INSTALL_INTERFACE:$<BUILD_INTERFACE:inner>outer>").unwrap();
        assert_eq!(
            render(&strip(&dropped_wrapper, PreprocessMode::Build).nodes),
            ""
        );
    }
}

#[cfg(test)]
#[test]
fn prefixes_install_paths_before_joining_adjacent_text() -> Result<(), crate::GenexError> {
    let ast = crate::parse(
        b"$<BUILD_INTERFACE:src>$<INSTALL_INTERFACE:include>/api;$<INSTALL_INTERFACE:a;/opt/b;$<INSTALL_PREFIX>/c;d$<CONFIG>>;bare",
    )?;
    assert_eq!(
        render(&ast.strip_install_paths(b"${_IMPORT_PREFIX}/").nodes),
        "${_IMPORT_PREFIX}/include/api;${_IMPORT_PREFIX}/a;/opt/b;$<INSTALL_PREFIX>/c;${_IMPORT_PREFIX}/d$<CONFIG>;bare"
    );
    Ok(())
}
