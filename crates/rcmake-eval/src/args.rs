//! Shared keyword argument parsing helpers for CMake built-in commands.

use rcmake_expr::ExpandedArg;

/// Simple cursor over an [`ExpandedArg`] slice for built-in command parsers.
#[derive(Debug)]
pub struct ArgCursor<'a> {
    args: &'a [ExpandedArg],
    pos: usize,
}

impl<'a> ArgCursor<'a> {
    /// Construct a cursor positioned at the beginning of `args`.
    #[must_use]
    pub const fn new(args: &'a [ExpandedArg]) -> Self {
        Self { args, pos: 0 }
    }

    /// Peek the next argument without advancing.
    #[must_use]
    pub fn peek(&self) -> Option<&'a ExpandedArg> {
        self.args.get(self.pos)
    }

    /// Consume and return the next argument.
    pub fn next_arg(&mut self) -> Option<&'a ExpandedArg> {
        let arg = self.args.get(self.pos)?;
        self.pos += 1;
        Some(arg)
    }

    /// Remaining unconsumed arguments.
    #[must_use]
    pub fn rest(&self) -> &'a [ExpandedArg] {
        self.args.get(self.pos..).unwrap_or(&[])
    }
}
