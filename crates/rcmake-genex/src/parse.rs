//! Lexer and parser for CMake 4.5.1 generator expressions (`$<...>`).

use std::{collections::HashMap, sync::Arc};

use rcmake_base::BString;

use crate::{GenexAst, GenexError, GenexExpr, GenexNode, MAX_NESTING};

/// Reuse parsed generator expressions within a resolution session.
#[derive(Default)]
pub struct ParseCache {
    entries: HashMap<BString, Arc<GenexAst>>,
    key_bytes: usize,
}

impl ParseCache {
    /// Return the parsed expression. Keep successful parses of common sizes.
    pub fn parse(&mut self, input: &[u8]) -> Result<Arc<GenexAst>, GenexError> {
        const MAX_ENTRIES: usize = 4_096;
        const MAX_KEY_BYTES: usize = 1024 * 1024;

        if let Some(ast) = self.entries.get(input) {
            return Ok(Arc::clone(ast));
        }
        let ast = Arc::new(parse(input)?);
        if self.entries.len() < MAX_ENTRIES && input.len() <= MAX_KEY_BYTES - self.key_bytes {
            self.key_bytes += input.len();
            self.entries.insert(input.into(), Arc::clone(&ast));
        }
        Ok(ast)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token<'a> {
    Begin,
    End,
    Colon,
    Comma,
    Text(&'a [u8]),
}

fn lex(input: &[u8]) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while let Some(&byte) = input.get(i) {
        let token = match byte {
            b'$' if input.get(i + 1) == Some(&b'<') => Some((Token::Begin, 2)),
            b'>' => Some((Token::End, 1)),
            b':' => Some((Token::Colon, 1)),
            b',' => Some((Token::Comma, 1)),
            _ => None,
        };
        match token {
            Some((token, len)) => {
                if start < i {
                    tokens.push(Token::Text(input.get(start..i).unwrap_or_default()));
                }
                tokens.push(token);
                i += len;
                start = i;
            }
            None => i += 1,
        }
    }
    if start < input.len() {
        tokens.push(Token::Text(input.get(start..).unwrap_or_default()));
    }
    tokens
}

fn token_text(token: Token<'_>) -> &[u8] {
    match token {
        Token::Begin => b"$<",
        Token::End => b">",
        Token::Colon => b":",
        Token::Comma => b",",
        Token::Text(text) => text,
    }
}

fn push_text(nodes: &mut Vec<GenexNode>, text: &[u8]) {
    if let Some(GenexNode::Text(last)) = nodes.last_mut() {
        last.extend_from_slice(text);
    } else {
        nodes.push(GenexNode::Text(BString::from(text)));
    }
}

fn extend_nodes(nodes: &mut Vec<GenexNode>, more: Vec<GenexNode>) {
    for node in more {
        match node {
            GenexNode::Text(text) => push_text(nodes, &text),
            expr @ GenexNode::Expr(_) => nodes.push(expr),
        }
    }
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token<'_>> {
        self.tokens.get(self.pos).copied()
    }

    /// One content item: an expression after `$<`, or the current token as text.
    fn content(&mut self, out: &mut Vec<GenexNode>) -> Result<(), GenexError> {
        match self.peek() {
            Some(Token::Begin) => {
                self.pos += 1;
                self.expression(out)
            }
            Some(token) => {
                push_text(out, token_text(token));
                self.pos += 1;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Body of `$<...>` after the `$<` token. Unterminated input recovers as text.
    fn expression(&mut self, out: &mut Vec<GenexNode>) -> Result<(), GenexError> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(GenexError::TooDeep);
        }
        let mut identifier = Vec::new();
        while let Some(token) = self.peek() {
            match token {
                Token::End | Token::Colon => break,
                Token::Comma => {
                    push_text(&mut identifier, b",");
                    self.pos += 1;
                }
                _ => self.content(&mut identifier)?,
            }
        }
        let result = match self.peek() {
            Some(Token::End) => {
                self.pos += 1;
                out.push(GenexNode::Expr(GenexExpr {
                    identifier,
                    params: None,
                }));
                Ok(())
            }
            Some(Token::Colon) => {
                self.pos += 1;
                self.parameters(identifier, out)
            }
            _ => {
                push_text(out, b"$<");
                extend_nodes(out, identifier);
                Ok(())
            }
        };
        self.depth -= 1;
        result
    }

    fn parameters(
        &mut self,
        identifier: Vec<GenexNode>,
        out: &mut Vec<GenexNode>,
    ) -> Result<(), GenexError> {
        let mut params: Vec<Vec<GenexNode>> = vec![Vec::new()];
        while let Some(token) = self.peek() {
            match token {
                Token::End => {
                    self.pos += 1;
                    out.push(GenexNode::Expr(GenexExpr {
                        identifier,
                        params: Some(params),
                    }));
                    return Ok(());
                }
                Token::Comma => {
                    params.push(Vec::new());
                    self.pos += 1;
                }
                _ => {
                    let mut current = Vec::new();
                    self.content(&mut current)?;
                    if let Some(last) = params.last_mut() {
                        extend_nodes(last, current);
                    }
                }
            }
        }
        push_text(out, b"$<");
        extend_nodes(out, identifier);
        push_text(out, b":");
        for (i, param) in params.into_iter().enumerate() {
            if i > 0 {
                push_text(out, b",");
            }
            extend_nodes(out, param);
        }
        Ok(())
    }
}

/// Parse `input` into a [`GenexAst`] (ADR-006 item 3).
///
/// `$<` opens an expression and `>` closes the innermost open one. The first `:` at an
/// expression's level ends the identifier, and each comma at that level starts a new
/// parameter. A `>`, `:`, or `,` outside an expression is text, and an unterminated
/// expression is kept as text. Returns [`GenexError::TooDeep`] when nesting exceeds
/// [`MAX_NESTING`].
pub fn parse(input: &[u8]) -> Result<GenexAst, GenexError> {
    let mut parser = Parser {
        tokens: lex(input),
        pos: 0,
        depth: 0,
    };
    let mut nodes = Vec::new();
    while parser.peek().is_some() {
        parser.content(&mut nodes)?;
    }
    Ok(GenexAst { nodes })
}

/// Render nodes back to source text.
#[must_use]
pub fn render(nodes: &[GenexNode]) -> BString {
    let mut out = BString::default();
    render_into(nodes, &mut out);
    out
}

fn render_into(nodes: &[GenexNode], out: &mut BString) {
    for node in nodes {
        match node {
            GenexNode::Text(text) => out.extend_from_slice(text),
            GenexNode::Expr(expr) => render_expr_into(expr, out),
        }
    }
}

pub(crate) fn render_expr(expr: &GenexExpr) -> BString {
    let mut out = BString::default();
    render_expr_into(expr, &mut out);
    out
}

fn render_expr_into(expr: &GenexExpr, out: &mut BString) {
    out.extend_from_slice(b"$<");
    render_into(&expr.identifier, out);
    if let Some(params) = &expr.params {
        out.push(b':');
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            render_into(param, out);
        }
    }
    out.push(b'>');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> GenexNode {
        GenexNode::Text(s.into())
    }

    #[test]
    fn repeated_input_reuses_the_parsed_tree() -> Result<(), GenexError> {
        let mut cache = ParseCache::default();
        let text = b"$<$<CONFIG:Debug>:-DDEBUG>";
        let first = cache.parse(text)?;
        let second = cache.parse(text)?;
        let other = cache.parse(b"$<$<CONFIG:Release>:-DDEBUG>")?;
        assert!(Arc::ptr_eq(&first, &second));
        assert_ne!(first, other);
        assert_eq!(*second, parse(text)?);
        Ok(())
    }

    #[test]
    fn splits_identifier_and_parameters() {
        let ast = parse(b"x$<UPPER_CASE:a,b:c>y").unwrap();
        assert_eq!(
            ast.nodes,
            vec![
                text("x"),
                GenexNode::Expr(GenexExpr {
                    identifier: vec![text("UPPER_CASE")],
                    params: Some(vec![vec![text("a")], vec![text("b:c")]]),
                }),
                text("y"),
            ]
        );
    }

    #[test]
    fn unmatched_delimiters_recover_as_text() {
        for input in ["a>b:c,d", "$<FOO", "$<FOO:a,b", "$<A:$<B>", "$"] {
            let ast = parse(input.as_bytes()).unwrap();
            assert_eq!(render(&ast.nodes), input, "{input}");
        }
        assert!(parse(b"$<A:$<B>").unwrap().nodes.len() == 2);
        assert!(parse(b"a>b").unwrap().is_plain());
    }

    #[test]
    fn empty_expression_and_nesting() {
        let ast = parse(b"$<>").unwrap();
        assert_eq!(ast.nodes, vec![GenexNode::Expr(GenexExpr::default())]);
        let deep = "$<".repeat(MAX_NESTING + 1);
        assert_eq!(parse(deep.as_bytes()), Err(GenexError::TooDeep));
        let ok = "$<a:".repeat(10) + &">".repeat(10);
        assert_eq!(render(&parse(ok.as_bytes()).unwrap().nodes), ok);
    }
}
