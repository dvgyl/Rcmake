use crate::{MAX_CAPTURE_SLOTS, RegexError};

const BYTECODE_LIMIT: usize = 65_535;
// Use the CMake instruction size to check the compiled pattern limit.
const HEADER_BYTES: usize = 3;

pub(crate) struct Expression<'a> {
    pub(crate) branches: Vec<Vec<Piece<'a>>>,
}

pub(crate) struct Piece<'a> {
    pub(crate) atom: Atom<'a>,
    pub(crate) repeat: Repeat,
}

pub(crate) enum Atom<'a> {
    Literal(&'a [u8]),
    Class(ByteClass),
    Any,
    Start,
    End,
    Group(usize, Expression<'a>),
}

#[derive(Debug, Default)]
pub(crate) struct ByteClass {
    bits: [u64; 4],
    negated: bool,
}

impl ByteClass {
    fn insert(&mut self, byte: u8) {
        if let Some(word) = self.bits.get_mut(usize::from(byte / 64)) {
            *word |= 1 << (byte % 64);
        }
    }

    pub(crate) fn matches(&self, byte: u8) -> bool {
        let present = self
            .bits
            .get(usize::from(byte / 64))
            .is_some_and(|word| word & (1 << (byte % 64)) != 0);
        present != self.negated
    }
}

pub(crate) enum Repeat {
    Once,
    Optional,
    ZeroOrMore,
    OneOrMore,
}

pub(crate) fn parse(pattern: &[u8]) -> Result<(Expression<'_>, usize), RegexError> {
    let mut parser = Parser {
        input: pattern,
        groups: 0,
        // The program marker and end instruction occur once.
        size: 1 + HEADER_BYTES,
    };
    let (expression, _) = parser.expression()?;
    if !parser.input.is_empty() {
        return Err(error("Unmatched parentheses."));
    }
    Ok((expression, parser.groups))
}

struct Parser<'a> {
    input: &'a [u8],
    groups: usize,
    size: usize,
}

impl<'a> Parser<'a> {
    fn expression(&mut self) -> Result<(Expression<'a>, bool), RegexError> {
        let mut branches = Vec::new();
        // Each branch must consume at least one byte to allow * or +.
        let mut has_width = true;
        loop {
            self.charge(HEADER_BYTES)?;
            let mut branch = Vec::new();
            let mut branch_has_width = false;
            while self.peek().is_some_and(|byte| !matches!(byte, b'|' | b')')) {
                let (piece, piece_has_width) = self.piece()?;
                branch_has_width |= piece_has_width;
                branch.push(piece);
            }
            if branch.is_empty() {
                self.charge(HEADER_BYTES)?;
            }
            has_width &= branch_has_width;
            branches.push(branch);
            if self.peek() != Some(b'|') {
                return Ok((Expression { branches }, has_width));
            }
            self.take();
        }
    }

    fn piece(&mut self) -> Result<(Piece<'a>, bool), RegexError> {
        let (atom, has_width) = self.atom()?;
        let Some(quantifier) = self.peek().filter(|&byte| is_quantifier(byte)) else {
            return Ok((
                Piece {
                    atom,
                    repeat: Repeat::Once,
                },
                has_width,
            ));
        };
        self.take();
        if quantifier != b'?' && !has_width {
            return Err(RegexError {
                message: "RegularExpression::compile() : *+ operand could be empty.".to_owned(),
            });
        }
        if self.peek().is_some_and(is_quantifier) {
            return Err(error("Nested *?+."));
        }

        let simple = matches!(atom, Atom::Literal(bytes) if bytes.len() == 1)
            || matches!(atom, Atom::Class(_) | Atom::Any);
        let repeat_bytes = match quantifier {
            b'?' => 3 * HEADER_BYTES,
            _ if simple => HEADER_BYTES,
            _ => 4 * HEADER_BYTES,
        };
        self.charge(repeat_bytes)?;

        let repeat = match quantifier {
            b'?' => Repeat::Optional,
            b'*' => Repeat::ZeroOrMore,
            _ => Repeat::OneOrMore,
        };
        Ok((Piece { atom, repeat }, quantifier == b'+' && has_width))
    }

    fn atom(&mut self) -> Result<(Atom<'a>, bool), RegexError> {
        match self.peek() {
            Some(b'(') => self.group(),
            Some(b'[') => self.class().map(|class| (Atom::Class(class), true)),
            Some(b'\\') => {
                self.take();
                let Some((byte, rest)) = self.input.split_first() else {
                    return Err(error("Trailing backslash."));
                };
                self.input = rest;
                self.charge(HEADER_BYTES + 2)?;
                Ok((Atom::Literal(std::slice::from_ref(byte)), true))
            }
            Some(b'.' | b'^' | b'$') => {
                let atom = match self.take() {
                    Some(b'^') => Atom::Start,
                    Some(b'$') => Atom::End,
                    _ => Atom::Any,
                };
                self.charge(HEADER_BYTES)?;
                let has_width = matches!(atom, Atom::Any);
                Ok((atom, has_width))
            }
            Some(byte) if is_quantifier(byte) => Err(error("?+* follows nothing.")),
            _ => self.literal().map(|bytes| (Atom::Literal(bytes), true)),
        }
    }

    fn group(&mut self) -> Result<(Atom<'a>, bool), RegexError> {
        self.take();
        self.groups += 1;
        let slot = self.groups;
        if slot >= MAX_CAPTURE_SLOTS {
            return Err(error("Too many parentheses."));
        }
        self.charge(2 * HEADER_BYTES)?;
        let (expression, has_width) = self.expression()?;
        if self.take() != Some(b')') {
            return Err(error("Unmatched parentheses."));
        }
        Ok((Atom::Group(slot, expression), has_width))
    }

    fn class(&mut self) -> Result<ByteClass, RegexError> {
        self.take();
        self.charge(HEADER_BYTES + 1)?;
        let negated = self.peek() == Some(b'^');
        if negated {
            self.take();
        }
        let mut class = ByteClass {
            negated,
            ..ByteClass::default()
        };
        let mut previous = None;
        if let Some(byte @ (b']' | b'-')) = self.peek() {
            self.take();
            self.charge(1)?;
            class.insert(byte);
            previous = Some(byte);
        }
        while let Some(byte) = self.take() {
            if byte == b']' {
                return Ok(class);
            }
            if byte == b'-'
                && let (Some(first), Some(last)) = (previous, self.peek())
                && last != b']'
            {
                self.take();
                if first > last {
                    return Err(error("Invalid range in []."));
                }
                self.charge(usize::from(last - first))?;
                for member in first..last {
                    class.insert(member + 1);
                }
                previous = Some(last);
            } else {
                self.charge(1)?;
                class.insert(byte);
                previous = Some(byte);
            }
        }
        Err(error("Unmatched []."))
    }

    fn literal(&mut self) -> Result<&'a [u8], RegexError> {
        let mut len = self
            .input
            .iter()
            .take(BYTECODE_LIMIT)
            .take_while(|&&byte| !is_meta(byte))
            .count();
        // A quantifier applies only to the last byte of a literal run.
        if len > 1 && self.input.get(len).copied().is_some_and(is_quantifier) {
            len -= 1;
        }
        if len >= BYTECODE_LIMIT {
            return Err(error("Expression too big."));
        }
        self.charge(HEADER_BYTES + 1 + len)?;
        let (literal, rest) = self.input.split_at(len);
        self.input = rest;
        Ok(literal)
    }

    fn peek(&self) -> Option<u8> {
        self.input.first().copied()
    }

    fn take(&mut self) -> Option<u8> {
        let (&byte, rest) = self.input.split_first()?;
        self.input = rest;
        Some(byte)
    }

    fn charge(&mut self, size: usize) -> Result<(), RegexError> {
        self.size += size;
        if self.size >= BYTECODE_LIMIT {
            return Err(error("Expression too big."));
        }
        Ok(())
    }
}

fn error(message: &str) -> RegexError {
    RegexError {
        message: format!("RegularExpression::compile(): {message}"),
    }
}

fn is_quantifier(byte: u8) -> bool {
    matches!(byte, b'*' | b'+' | b'?')
}

fn is_meta(byte: u8) -> bool {
    matches!(byte, b'.' | b'^' | b'$' | b'[' | b'(' | b')' | b'|' | b'\\') || is_quantifier(byte)
}
