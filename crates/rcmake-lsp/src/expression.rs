use std::ops::Range;

use crate::roles::Variable;

const MAX_DEPTH: usize = 256;
const MAX_EXPRESSIONS: usize = 50_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Reference(Variable),
    Generator,
    Target,
}

pub(crate) struct Expression {
    pub(crate) kind: Kind,
    pub(crate) span: Range<usize>,
    pub(crate) name: Range<usize>,
}

struct Frame {
    kind: Kind,
    start: usize,
    name_start: usize,
    first_colon: Option<usize>,
    first_comma: Option<usize>,
    dynamic_name: bool,
    dynamic_operand: bool,
}

impl Frame {
    fn child(&mut self) {
        match self.kind {
            Kind::Reference(_) => self.dynamic_name = true,
            Kind::Generator if self.first_colon.is_none() => self.dynamic_name = true,
            Kind::Generator if self.first_comma.is_none() => self.dynamic_operand = true,
            _ => {}
        }
    }

    fn emit(self, text: &str, end: usize, closed: bool, output: &mut Vec<Expression>) {
        if self.dynamic_name {
            return;
        }
        let name = self.name_start..self.first_colon.unwrap_or(end);
        if self.kind == Kind::Generator {
            if let Some(operation) = text.get(name.clone()).filter(|op| generator_name(op)) {
                output.push(Expression {
                    kind: self.kind,
                    span: self.start..name.end,
                    name,
                });
                if let Some(colon) = self.first_colon
                    && !self.dynamic_operand
                    && generator_target(operation, self.first_comma.is_some())
                {
                    let target = colon + 1..self.first_comma.unwrap_or(end);
                    if text.get(target.clone()).is_some_and(target_name) {
                        output.push(Expression {
                            kind: Kind::Target,
                            span: target.clone(),
                            name: target,
                        });
                    }
                }
            }
        } else if text.get(name.clone()).is_some_and(variable_name) {
            output.push(Expression {
                kind: self.kind,
                span: self.start..end + usize::from(closed),
                name,
            });
        }
    }
}

pub(crate) fn expressions(text: &str) -> Vec<Expression> {
    scan(text, |_| {})
}

#[cfg(feature = "eval")]
pub(crate) fn generator_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let mut found = None;
    scan(text, |span| {
        if span.contains(&offset) {
            found = Some(span);
        }
    });
    found
}

fn scan(text: &str, mut generator: impl FnMut(Range<usize>)) -> Vec<Expression> {
    let mut found = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut cursor = 0;
    let bytes = text.as_bytes();
    while let Some(&byte) = bytes.get(cursor) {
        if found.len() >= MAX_EXPRESSIONS {
            stack.clear();
            break;
        }
        if byte == b'\\' {
            if let Some(frame) = stack.last_mut() {
                frame.child();
            }
            cursor += 2;
            continue;
        }
        if byte == b'$'
            && let Some((kind, length)) = bytes.get(cursor..).and_then(opener)
        {
            if stack.len() >= MAX_DEPTH {
                stack.clear();
                break;
            }
            if let Some(parent) = stack.last_mut() {
                parent.child();
            }
            stack.push(Frame {
                kind,
                start: cursor,
                name_start: cursor + length,
                first_colon: None,
                first_comma: None,
                dynamic_name: false,
                dynamic_operand: false,
            });
            cursor += length;
            continue;
        }
        if let Some(frame) = stack.last_mut() {
            match (frame.kind, byte) {
                (Kind::Reference(_), b'}') | (Kind::Generator, b'>') => {
                    if let Some(frame) = stack.pop() {
                        if frame.kind == Kind::Generator {
                            generator(frame.start..cursor + 1);
                        }
                        frame.emit(text, cursor, true, &mut found);
                    }
                }
                (Kind::Generator, b':') if frame.first_colon.is_none() => {
                    frame.first_colon = Some(cursor);
                }
                (Kind::Generator, b',')
                    if frame.first_colon.is_some() && frame.first_comma.is_none() =>
                {
                    frame.first_comma = Some(cursor);
                }
                _ => {}
            }
        }
        cursor += 1;
    }
    while let Some(frame) = stack.pop() {
        frame.emit(text, text.len(), false, &mut found);
    }
    found.sort_by_key(|expr| expr.span.start);
    found
}

fn opener(text: &[u8]) -> Option<(Kind, usize)> {
    match text {
        [b'$', b'{', ..] => Some((Kind::Reference(Variable::Normal), 2)),
        [b'$', b'E', b'N', b'V', b'{', ..] => Some((Kind::Reference(Variable::Environment), 5)),
        [b'$', b'C', b'A', b'C', b'H', b'E', b'{', ..] => {
            Some((Kind::Reference(Variable::Cache), 7))
        }
        [b'$', b'<', ..] => Some((Kind::Generator, 2)),
        _ => None,
    }
}

fn variable_name(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_/.+-".contains(&byte))
}

pub(crate) fn target_name(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-:".contains(&byte))
}

fn generator_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn generator_target(operation: &str, multiple: bool) -> bool {
    (operation == "TARGET_PROPERTY" && multiple)
        || matches!(
            operation,
            "TARGET_EXISTS"
                | "TARGET_NAME"
                | "TARGET_NAME_IF_EXISTS"
                | "TARGET_OBJECTS"
                | "TARGET_GENEX_EVAL"
                | "TARGET_RUNTIME_DLLS"
                | "TARGET_RUNTIME_DLL_DIRS"
        )
        || [
            "TARGET_FILE",
            "TARGET_LINKER_FILE",
            "TARGET_SONAME_FILE",
            "TARGET_PDB_FILE",
            "TARGET_BUNDLE_",
            "TARGET_IMPORT_FILE",
        ]
        .iter()
        .any(|prefix| operation.starts_with(prefix))
}

pub(crate) fn description(operation: &str) -> Option<&'static str> {
    match operation {
        "TARGET_EXISTS" => Some("Return 1 if the target exists."),
        "TARGET_PROPERTY" => Some("Read a property of a target."),
        "TARGET_OBJECTS" => Some("Return the object files of the target."),
        "TARGET_FILE" => Some("Return the full path to the target artifact."),
        "CONFIG" => Some("Get the build configuration or compare it with a configuration name."),
        "BUILD_INTERFACE" => Some("Use the enclosed value in the build tree."),
        "INSTALL_INTERFACE" => Some("Use the enclosed value for an installed target."),
        _ => None,
    }
}
