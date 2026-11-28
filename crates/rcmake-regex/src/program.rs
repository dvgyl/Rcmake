use rcmake_base::ByteSlice as _;

use crate::parse::{Atom, ByteClass, Expression, Piece, Repeat};
use crate::{Captures, MAX_CAPTURE_SLOTS, MatchOptions, RegexLimit};

type Span = Option<(usize, usize)>;

/// Number of haystack bytes charged as one step during SIMD literal prefilter / reverse scans.
const SCAN_BYTES_PER_STEP: usize = 16;

/// Compiled regex bytecode and sound prefilter metadata.
#[derive(Debug)]
pub(crate) struct Compiled {
    instructions: Box<[Instruction]>,
    anchored_start: bool,
    leading_literal: Option<Box<[u8]>>,
    required_literal: Option<Box<[u8]>>,
}

#[derive(Debug)]
pub(crate) enum Instruction {
    Literal(Box<[u8]>),
    Class(Box<ByteClass>),
    Any,
    /// Greedy `.*` (`min == 0`) or `.+` (`min == 1`) whose next mandatory consuming
    /// instruction (skipping `Open`/`Close`/`Jump`) is `Instruction::Literal` at
    /// `literal_pc`. Jumps to `after_pc` at each candidate position in reverse order.
    GreedyAnyUntilLiteral {
        min: usize,
        after_pc: usize,
        literal_pc: usize,
    },
    /// Greedy `.*` (`min == 0`) or `.+` (`min == 1`) whose next mandatory instruction
    /// (skipping `Open`/`Close`/`Jump`) is `Instruction::End`.
    GreedyAnyToEnd {
        min: usize,
        after_pc: usize,
    },
    Start,
    End,
    Open(usize),
    Close(usize),
    Split(usize),
    Jump(usize),
    Accept,
}

pub(crate) fn compile(expression: Expression<'_>) -> Compiled {
    let anchored_start = is_start_anchored(&expression);
    let leading_literal = if anchored_start {
        None
    } else {
        leading_literal(&expression).map(Box::from)
    };
    let candidate_required = if anchored_start || leading_literal.is_some() {
        None
    } else {
        required_literal(&expression).map(Box::from)
    };

    let mut compiler = Compiler(Vec::new());
    compiler.expression(expression);
    compiler.emit(Instruction::Accept);
    optimize_greedy_any(&mut compiler.0);
    // Only pre-scan backwards for a required literal when the pattern begins with
    // an unbounded greedy `.*`/`.+`, where even offset 0 must reach the end of the
    // haystack. Non-`.*` patterns (such as `a?z`) must try offset 0 first.
    let required_literal = if starts_with_greedy_any(&compiler.0) {
        candidate_required
    } else {
        None
    };
    Compiled {
        instructions: compiler.0.into_boxed_slice(),
        anchored_start,
        leading_literal,
        required_literal,
    }
}

fn starts_with_greedy_any(instructions: &[Instruction]) -> bool {
    let mut pc = 0;
    while let Some(instruction) = instructions.get(pc) {
        match instruction {
            Instruction::Open(_) => pc += 1,
            Instruction::GreedyAnyUntilLiteral { .. } | Instruction::GreedyAnyToEnd { .. } => {
                return true;
            }
            _ => return false,
        }
    }
    false
}

fn is_start_anchored(expression: &Expression<'_>) -> bool {
    !expression.branches.is_empty()
        && expression
            .branches
            .iter()
            .all(|branch| branch_start_anchored(branch))
}

fn branch_start_anchored(branch: &[Piece<'_>]) -> bool {
    let Some(first) = branch.first() else {
        return false;
    };
    if !matches!(first.repeat, Repeat::Once | Repeat::OneOrMore) {
        return false;
    }
    match &first.atom {
        Atom::Start => true,
        Atom::Group(_, inner) => is_start_anchored(inner),
        _ => false,
    }
}

fn leading_literal<'a>(expression: &Expression<'a>) -> Option<&'a [u8]> {
    let [branch] = expression.branches.as_slice() else {
        return None;
    };
    let first = branch.first()?;
    if !matches!(first.repeat, Repeat::Once | Repeat::OneOrMore) {
        return None;
    }
    match &first.atom {
        Atom::Literal(bytes) if !bytes.is_empty() => Some(bytes),
        Atom::Group(_, inner) => leading_literal(inner),
        _ => None,
    }
}

fn required_literal<'a>(expression: &Expression<'a>) -> Option<&'a [u8]> {
    let [branch] = expression.branches.as_slice() else {
        return None;
    };
    let mut best: Option<&'a [u8]> = None;
    for piece in branch {
        if !matches!(piece.repeat, Repeat::Once | Repeat::OneOrMore) {
            continue;
        }
        let candidate = match &piece.atom {
            Atom::Literal(bytes) if !bytes.is_empty() => Some(*bytes),
            Atom::Group(_, inner) => required_literal(inner),
            _ => None,
        };
        if let Some(lit) = candidate
            && best.is_none_or(|current| lit.len() > current.len())
        {
            best = Some(lit);
        }
    }
    best
}

enum NextMandatory {
    Literal(usize),
    End,
    Other,
}

fn next_mandatory(instructions: &[Instruction], mut pc: usize) -> NextMandatory {
    let mut visited = 0;
    while visited < instructions.len() {
        visited += 1;
        match instructions.get(pc) {
            Some(Instruction::Open(_) | Instruction::Close(_)) => pc += 1,
            Some(Instruction::Jump(target)) => pc = *target,
            Some(Instruction::Literal(bytes)) if !bytes.is_empty() => {
                return NextMandatory::Literal(pc);
            }
            Some(Instruction::End) => return NextMandatory::End,
            _ => return NextMandatory::Other,
        }
    }
    NextMandatory::Other
}

/// Replace greedy `.*` (`Split(pc+3); Any; Jump(pc)`) and `.+` (`Any; Split(pc+3); Jump(pc)`)
/// entry instructions in place when followed by a mandatory literal or `$` anchor.
///
/// Instruction indices never shift, so all existing `Split`/`Jump` targets remain valid.
fn optimize_greedy_any(instructions: &mut [Instruction]) {
    let len = instructions.len();
    for pc in 0..len {
        let min = match (
            instructions.get(pc),
            instructions.get(pc + 1),
            instructions.get(pc + 2),
        ) {
            (
                Some(Instruction::Split(exit)),
                Some(Instruction::Any),
                Some(Instruction::Jump(back)),
            ) if *exit == pc + 3 && *back == pc => Some(0),
            (
                Some(Instruction::Any),
                Some(Instruction::Split(exit)),
                Some(Instruction::Jump(back)),
            ) if *exit == pc + 3 && *back == pc => Some(1),
            _ => None,
        };
        let Some(min) = min else {
            continue;
        };
        let after_pc = pc + 3;
        match next_mandatory(instructions, after_pc) {
            NextMandatory::Literal(literal_pc) => {
                if let Some(slot) = instructions.get_mut(pc) {
                    *slot = Instruction::GreedyAnyUntilLiteral {
                        min,
                        after_pc,
                        literal_pc,
                    };
                }
            }
            NextMandatory::End => {
                if let Some(slot) = instructions.get_mut(pc) {
                    *slot = Instruction::GreedyAnyToEnd { min, after_pc };
                }
            }
            NextMandatory::Other => {}
        }
    }
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
    compiled: &Compiled,
    hay: &[u8],
    start: usize,
    options: MatchOptions,
    budget: &mut u64,
    scratch: &mut Scratch,
) -> Result<Option<Captures>, RegexLimit> {
    if *budget == 0 {
        return Err(RegexLimit);
    }
    if start > hay.len() {
        return Ok(None);
    }
    let bol = if options.bol_at_start { start } else { 0 };
    if compiled.anchored_start {
        if start > bol {
            charge_step(budget, 1)?;
            return Ok(None);
        }
        let mut vm = Vm {
            program: &compiled.instructions,
            hay,
            bol,
            skip_empty_at: (!options.allow_empty_at_start).then_some(start),
            required_hint: None,
            budget,
            scratch,
        };
        return vm.run(start);
    }

    if let Some(lit) = compiled.leading_literal.as_deref() {
        let mut cursor = start;
        let mut vm = Vm {
            program: &compiled.instructions,
            hay,
            bol,
            skip_empty_at: (!options.allow_empty_at_start).then_some(start),
            required_hint: None,
            budget,
            scratch,
        };
        while let Some(attempt) = find_next_literal(hay, cursor, lit, vm.budget)? {
            if let Some(captures) = vm.run(attempt)? {
                return Ok(Some(captures));
            }
            cursor = attempt.saturating_add(1);
        }
        return Ok(None);
    }

    let (max_attempt, required_hint) = if let Some(lit) = compiled.required_literal.as_deref() {
        match find_last_literal(hay, start, hay.len(), lit, budget)? {
            Some(last_pos) => (last_pos, Some((lit, last_pos))),
            None => return Ok(None),
        }
    } else {
        (hay.len(), None)
    };

    let mut vm = Vm {
        program: &compiled.instructions,
        hay,
        bol,
        skip_empty_at: (!options.allow_empty_at_start).then_some(start),
        required_hint,
        budget,
        scratch,
    };
    for attempt in start..=max_attempt {
        if let Some(captures) = vm.run(attempt)? {
            return Ok(Some(captures));
        }
    }
    Ok(None)
}

fn charge_step(budget: &mut u64, steps: u64) -> Result<(), RegexLimit> {
    *budget = budget.checked_sub(steps).ok_or(RegexLimit)?;
    Ok(())
}

fn max_scan_bytes(budget: u64) -> usize {
    usize::try_from(budget.saturating_mul(SCAN_BYTES_PER_STEP as u64)).unwrap_or(usize::MAX)
}

fn charge_scan(budget: &mut u64, scanned: usize, charge_zero: bool) -> Result<(), RegexLimit> {
    let steps = u64::try_from(scanned / SCAN_BYTES_PER_STEP).unwrap_or(u64::MAX);
    let steps = if charge_zero { steps.max(1) } else { steps };
    if steps != 0 {
        charge_step(budget, steps)?;
    }
    Ok(())
}

/// Bounded forward search for `needle` in `hay[from..]`, charging scan work to `budget`.
fn find_next_literal(
    hay: &[u8],
    from: usize,
    needle: &[u8],
    budget: &mut u64,
) -> Result<Option<usize>, RegexLimit> {
    let Some(tail) = hay.get(from..) else {
        return Ok(None);
    };
    if tail.len() < needle.len() {
        charge_step(budget, 1)?;
        return Ok(None);
    }
    let cap = max_scan_bytes(*budget);
    if cap == 0 {
        return Err(RegexLimit);
    }
    let window_len = cap.min(tail.len());
    let window = tail.get(..window_len).unwrap_or(tail);
    if let Some(rel) = window.find(needle) {
        charge_scan(budget, rel, false)?;
        return Ok(Some(from + rel));
    }
    if window_len < tail.len() {
        *budget = 0;
        return Err(RegexLimit);
    }
    charge_scan(budget, tail.len(), true)?;
    Ok(None)
}

/// Bounded reverse search for `needle` in `hay[search_start..search_end]`, charging scan work
/// to `budget`.
fn find_last_literal(
    hay: &[u8],
    search_start: usize,
    search_end: usize,
    needle: &[u8],
    budget: &mut u64,
) -> Result<Option<usize>, RegexLimit> {
    if search_end < search_start.saturating_add(needle.len()) {
        charge_step(budget, 1)?;
        return Ok(None);
    }
    let cap = max_scan_bytes(*budget);
    if cap == 0 {
        return Err(RegexLimit);
    }
    let window_start = search_end.saturating_sub(cap).max(search_start);
    let Some(window) = hay.get(window_start..search_end) else {
        return Ok(None);
    };
    if let Some(rel) = window.rfind(needle) {
        let found = window_start + rel;
        let scanned = search_end.saturating_sub(found);
        charge_scan(budget, scanned, true)?;
        return Ok(Some(found));
    }
    if window_start > search_start {
        *budget = 0;
        return Err(RegexLimit);
    }
    charge_scan(budget, search_end.saturating_sub(search_start), true)?;
    Ok(None)
}

#[derive(Default)]
pub(crate) struct Scratch {
    choices: Vec<Choice>,
    changes: Vec<(usize, Span)>,
}

impl Scratch {
    /// Keep thread-local scratch capacity bounded after unusually large backtracks.
    pub(crate) fn trim(&mut self) {
        self.choices.clear();
        self.changes.clear();
        if self.choices.capacity() > 4096 {
            self.choices.shrink_to(1024);
        }
        if self.changes.capacity() > 4096 {
            self.changes.shrink_to(1024);
        }
    }
}

enum Choice {
    Branch {
        pc: usize,
        pos: usize,
        changes: usize,
    },
    AnyUntilLiteral {
        after_pc: usize,
        literal_pc: usize,
        search_start: usize,
        search_end: usize,
        changes: usize,
    },
}

struct Vm<'a> {
    program: &'a [Instruction],
    hay: &'a [u8],
    bol: usize,
    skip_empty_at: Option<usize>,
    required_hint: Option<(&'a [u8], usize)>,
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
                Instruction::GreedyAnyUntilLiteral {
                    min,
                    after_pc,
                    literal_pc,
                } => {
                    if let Some(Instruction::Literal(lit)) = self.program.get(literal_pc)
                        && let Some(search_start) = pos.checked_add(min)
                    {
                        let search_end = match self.required_hint {
                            Some((hint_lit, last_pos)) if hint_lit == lit.as_ref() => {
                                last_pos.saturating_add(lit.len()).min(self.hay.len())
                            }
                            _ => self.hay.len(),
                        };
                        if let Some(found) =
                            find_last_literal(self.hay, search_start, search_end, lit, self.budget)?
                        {
                            let next_end = found.saturating_add(lit.len().saturating_sub(1));
                            if found > search_start
                                && next_end >= search_start.saturating_add(lit.len())
                            {
                                self.scratch.choices.push(Choice::AnyUntilLiteral {
                                    after_pc,
                                    literal_pc,
                                    search_start,
                                    search_end: next_end,
                                    changes: self.scratch.changes.len(),
                                });
                            }
                            pc = after_pc;
                            pos = found;
                            continue;
                        }
                    }
                }
                Instruction::GreedyAnyToEnd { min, after_pc } => {
                    if let Some(min_end) = pos.checked_add(min)
                        && min_end <= self.hay.len()
                    {
                        let skipped = self.hay.len().saturating_sub(pos);
                        charge_scan(self.budget, skipped, false)?;
                        pc = after_pc;
                        pos = self.hay.len();
                        continue;
                    }
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
                    self.scratch.choices.push(Choice::Branch {
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
            if !self.backtrack(&mut groups, &mut pc, &mut pos)? {
                return Ok(None);
            }
        }
        Ok(None)
    }

    fn backtrack(
        &mut self,
        groups: &mut [Span; MAX_CAPTURE_SLOTS],
        pc: &mut usize,
        pos: &mut usize,
    ) -> Result<bool, RegexLimit> {
        while let Some(choice) = self.scratch.choices.pop() {
            match choice {
                Choice::Branch {
                    pc: next_pc,
                    pos: next_pos,
                    changes,
                } => {
                    self.rollback(groups, changes);
                    *pc = next_pc;
                    *pos = next_pos;
                    return Ok(true);
                }
                Choice::AnyUntilLiteral {
                    after_pc,
                    literal_pc,
                    search_start,
                    search_end,
                    changes,
                } => {
                    self.rollback(groups, changes);
                    let Some(Instruction::Literal(lit)) = self.program.get(literal_pc) else {
                        continue;
                    };
                    if let Some(found) =
                        find_last_literal(self.hay, search_start, search_end, lit, self.budget)?
                    {
                        let next_end = found.saturating_add(lit.len().saturating_sub(1));
                        if found > search_start
                            && next_end >= search_start.saturating_add(lit.len())
                        {
                            self.scratch.choices.push(Choice::AnyUntilLiteral {
                                after_pc,
                                literal_pc,
                                search_start,
                                search_end: next_end,
                                changes,
                            });
                        }
                        *pc = after_pc;
                        *pos = found;
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn rollback(&mut self, groups: &mut [Span; MAX_CAPTURE_SLOTS], changes: usize) {
        for (slot, old) in self.scratch.changes.drain(changes..).rev() {
            if let Some(capture) = groups.get_mut(slot) {
                *capture = old;
            }
        }
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
        charge_step(self.budget, 1)
    }
}
