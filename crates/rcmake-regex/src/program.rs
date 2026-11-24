use crate::parse::{Atom, ByteClass, Expression, Piece, Repeat};
use crate::{Captures, MAX_CAPTURE_SLOTS, MatchOptions, RegexLimit};

type Span = Option<(usize, usize)>;

#[derive(Debug)]
pub(crate) enum Instruction {
    Literal(Box<[u8]>),
    Class(Box<ByteClass>),
    Any,
    Start,
    End,
    Open(usize),
    Close(usize),
    Split(usize),
    Jump(usize),
    Accept,
}

pub(crate) fn compile(expression: Expression<'_>) -> Vec<Instruction> {
    let mut compiler = Compiler(Vec::new());
    compiler.expression(expression);
    compiler.emit(Instruction::Accept);
    compiler.0
}

struct Compiler(Vec<Instruction>);

impl Compiler {
    fn expression(&mut self, expression: Expression<'_>) {
        let mut branches = expression.branches.into_iter().peekable();
        let mut exits = Vec::new();
        while let Some(branch) = branches.next() {
            let split = branches
                .peek()
                .is_some()
                .then(|| self.emit(Instruction::Split(0)));
            for piece in branch {
                self.piece(piece);
            }
            if let Some(split) = split {
                exits.push(self.emit(Instruction::Jump(0)));
                self.patch(split, Instruction::Split(self.0.len()));
            }
        }
        for exit in exits {
            self.patch(exit, Instruction::Jump(self.0.len()));
        }
    }

    fn piece(&mut self, piece: Piece<'_>) {
        let start = self.0.len();
        let split = matches!(piece.repeat, Repeat::Optional | Repeat::ZeroOrMore)
            .then(|| self.emit(Instruction::Split(0)));
        self.atom(piece.atom);
        match piece.repeat {
            Repeat::ZeroOrMore => {
                self.emit(Instruction::Jump(start));
            }
            Repeat::OneOrMore => {
                let exit = self.0.len() + 2;
                self.emit(Instruction::Split(exit));
                self.emit(Instruction::Jump(start));
            }
            Repeat::Optional | Repeat::Once => {}
        }
        if let Some(split) = split {
            self.patch(split, Instruction::Split(self.0.len()));
        }
    }

    fn atom(&mut self, atom: Atom<'_>) {
        let instruction = match atom {
            Atom::Literal(bytes) => Instruction::Literal(Box::from(bytes)),
            Atom::Class(class) => Instruction::Class(Box::new(class)),
            Atom::Any => Instruction::Any,
            Atom::Start => Instruction::Start,
            Atom::End => Instruction::End,
            Atom::Group(slot, expression) => {
                self.emit(Instruction::Open(slot));
                self.expression(expression);
                Instruction::Close(slot)
            }
        };
        self.emit(instruction);
    }

    fn emit(&mut self, instruction: Instruction) -> usize {
        let pc = self.0.len();
        self.0.push(instruction);
        pc
    }

    fn patch(&mut self, pc: usize, instruction: Instruction) {
        if let Some(target) = self.0.get_mut(pc) {
            *target = instruction;
        }
    }
}

pub(crate) fn find(
    program: &[Instruction],
    hay: &[u8],
    start: usize,
    options: MatchOptions,
    budget: &mut u64,
    scratch: &mut Scratch,
) -> Result<Option<Captures>, RegexLimit> {
    let mut vm = Vm {
        program,
        hay,
        bol: if options.bol_at_start { start } else { 0 },
        skip_empty_at: (!options.allow_empty_at_start).then_some(start),
        budget,
        scratch,
    };
    for attempt in start..=hay.len() {
        if let Some(captures) = vm.run(attempt)? {
            return Ok(Some(captures));
        }
    }
    Ok(None)
}

#[derive(Default)]
pub(crate) struct Scratch {
    choices: Vec<Choice>,
    changes: Vec<(usize, Span)>,
}

struct Choice {
    pc: usize,
    pos: usize,
    changes: usize,
}

struct Vm<'a> {
    program: &'a [Instruction],
    hay: &'a [u8],
    bol: usize,
    skip_empty_at: Option<usize>,
    budget: &'a mut u64,
    scratch: &'a mut Scratch,
}

impl Vm<'_> {
    fn run(&mut self, start: usize) -> Result<Option<Captures>, RegexLimit> {
        self.scratch.choices.clear();
        self.scratch.changes.clear();
        let mut groups = [None; MAX_CAPTURE_SLOTS];
        let mut pc = 0;
        let mut pos = start;
        while let Some(instruction) = self.program.get(pc) {
            self.step()?;
            pc += 1;
            match *instruction {
                Instruction::Literal(ref bytes) => {
                    if self.literal_matches(pos, bytes)? {
                        pos += bytes.len();
                        continue;
                    }
                }
                Instruction::Class(ref class) => {
                    if self.hay.get(pos).is_some_and(|&byte| class.matches(byte)) {
                        pos += 1;
                        continue;
                    }
                }
                Instruction::Any if pos < self.hay.len() => {
                    pos += 1;
                    continue;
                }
                Instruction::Start if pos == self.bol => continue,
                Instruction::End if pos == self.hay.len() => continue,
                Instruction::Open(slot) => {
                    self.capture(&mut groups, slot, (pos, pos));
                    continue;
                }
                Instruction::Close(slot) => {
                    if let Some(Some((open, _))) = groups.get(slot).copied() {
                        self.capture(&mut groups, slot, (open, pos));
                    }
                    continue;
                }
                Instruction::Split(alternative) => {
                    // Try the next instruction first. Save the other path for backtracking.
                    self.scratch.choices.push(Choice {
                        pc: alternative,
                        pos,
                        changes: self.scratch.changes.len(),
                    });
                    continue;
                }
                Instruction::Jump(target) => {
                    pc = target;
                    continue;
                }
                Instruction::Accept if start != pos || self.skip_empty_at != Some(start) => {
                    if let Some(full_match) = groups.first_mut() {
                        *full_match = Some((start, pos));
                    }
                    return Ok(Some(Captures { groups }));
                }
                Instruction::Any | Instruction::Start | Instruction::End | Instruction::Accept => {}
            }
            let Some(choice) = self.scratch.choices.pop() else {
                return Ok(None);
            };
            for (slot, old) in self.scratch.changes.drain(choice.changes..).rev() {
                if let Some(capture) = groups.get_mut(slot) {
                    *capture = old;
                }
            }
            pc = choice.pc;
            pos = choice.pos;
        }
        Ok(None)
    }

    fn capture(
        &mut self,
        groups: &mut [Span; MAX_CAPTURE_SLOTS],
        slot: usize,
        span: (usize, usize),
    ) {
        if let Some(capture) = groups.get_mut(slot) {
            // Save only changes that a pending alternative can undo.
            if !self.scratch.choices.is_empty() {
                self.scratch.changes.push((slot, *capture));
            }
            *capture = Some(span);
        }
    }

    fn literal_matches(&mut self, pos: usize, bytes: &[u8]) -> Result<bool, RegexLimit> {
        let Some(input) = self.hay.get(pos..).and_then(|rest| rest.get(..bytes.len())) else {
            return Ok(false);
        };
        for (index, (actual, expected)) in input.iter().zip(bytes).enumerate() {
            // The instruction already accounts for the first byte.
            if index != 0 {
                self.step()?;
            }
            if actual != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn step(&mut self) -> Result<(), RegexLimit> {
        *self.budget = self.budget.checked_sub(1).ok_or(RegexLimit)?;
        Ok(())
    }
}
