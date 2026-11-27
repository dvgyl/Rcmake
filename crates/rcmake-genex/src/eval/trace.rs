//! Step records for [`crate::evaluate_traced`].

use rcmake_base::BString;

use crate::{GenexExpr, GenexNode};

const SOURCE_BYTES: usize = 256;
const VALUE_BYTES: usize = 4096;

/// Step-by-step record of one generator expression evaluation.
///
/// Steps cover the nodes of the evaluated expression text. A value that the host
/// computes, such as a transitive `INTERFACE_*` property, is the result of the step
/// that asked for it. The host can evaluate further expressions in that callback.
/// Those nested evaluations are not traced, and the step has `host` set.
#[derive(Clone, Debug)]
pub struct GenexTrace {
    /// Evaluated `$<...>` nodes in pre-order.
    pub steps: Vec<GenexStep>,
    /// Operands that a short-circuit operator did not evaluate.
    pub skipped: Vec<SkippedOperand>,
    /// Number of steps and skipped operands that the limit kept out of the record.
    pub omitted: u32,
    /// The evaluation reached `$<COMMAND_CONFIG:...>`. The step limit does not apply.
    pub command_config_used: bool,
    /// The evaluation reached `$<OUTPUT_CONFIG:...>`. The step limit does not apply.
    pub output_config_used: bool,
    limit: usize,
}

impl Default for GenexTrace {
    fn default() -> Self {
        Self::with_limit(1024)
    }
}

impl GenexTrace {
    /// Create a trace that keeps at most `limit` steps and `limit` skipped operands.
    ///
    /// A limit of 0 records no steps. The operator flags, such as
    /// [`Self::command_config_used`], are recorded for every limit.
    #[must_use]
    pub const fn with_limit(limit: usize) -> Self {
        Self {
            steps: Vec::new(),
            skipped: Vec::new(),
            omitted: 0,
            command_config_used: false,
            output_config_used: false,
            limit,
        }
    }

    /// Return `true` when the limit kept a step or a skipped operand out of the record.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.omitted > 0
    }

    pub(super) fn enter(
        &mut self,
        parent: Option<u32>,
        depth: u32,
        expr: &GenexExpr,
    ) -> Option<u32> {
        if self.steps.len() >= self.limit {
            self.omitted = self.omitted.saturating_add(1);
            return None;
        }
        let id = u32::try_from(self.steps.len()).ok()?;
        self.steps.push(GenexStep {
            id,
            parent,
            depth,
            source: cut(crate::parse::render_expr(expr), SOURCE_BYTES).0,
            operator: BString::default(),
            result: Ok(BString::default()),
            value_truncated: false,
            host: false,
        });
        Some(id)
    }

    pub(super) fn cross_config(&mut self, name: &[u8]) {
        match name {
            b"COMMAND_CONFIG" => self.command_config_used = true,
            b"OUTPUT_CONFIG" => self.output_config_used = true,
            _ => {}
        }
    }

    pub(super) fn operator(&mut self, id: u32, name: &[u8]) {
        if let Some(step) = self.step_mut(id) {
            step.operator = cut(BString::from(name), SOURCE_BYTES).0;
            step.host = host_operator(name);
        }
    }

    pub(super) fn exit(&mut self, id: u32, result: Result<&BString, String>) {
        if let Some(step) = self.step_mut(id) {
            match result {
                Ok(value) => {
                    let (value, truncated) = cut(value.clone(), VALUE_BYTES);
                    step.result = Ok(value);
                    step.value_truncated = truncated;
                }
                Err(error) => step.result = Err(cut(BString::from(error), VALUE_BYTES).0),
            }
        }
    }

    pub(super) fn skip(&mut self, parent: Option<u32>, index: usize, nodes: &[GenexNode]) {
        if self.skipped.len() >= self.limit {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        self.skipped.push(SkippedOperand {
            parent,
            index: u32::try_from(index).unwrap_or(u32::MAX),
            source: cut(crate::parse::render(nodes), SOURCE_BYTES).0,
            literal: nodes.iter().all(|node| matches!(node, GenexNode::Text(_))),
        });
    }

    fn step_mut(&mut self, id: u32) -> Option<&mut GenexStep> {
        self.steps.get_mut(usize::try_from(id).ok()?)
    }
}

/// One evaluated `$<...>` node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenexStep {
    /// Pre-order index of the step.
    pub id: u32,
    /// Step of the enclosing node, when it was recorded.
    pub parent: Option<u32>,
    /// Nesting depth. The outermost node has depth 1.
    pub depth: u32,
    /// Source text of the node, cut at 256 bytes.
    pub source: BString,
    /// Evaluated operator name, for example `TARGET_PROPERTY`.
    pub operator: BString,
    /// Value after the output budget checks, cut at 4096 bytes, or the error text.
    pub result: Result<BString, BString>,
    /// The stored value is a prefix of the real value.
    pub value_truncated: bool,
    /// The host computed the value and can have evaluated nested expressions.
    pub host: bool,
}

/// Operand that a short-circuit operator such as `$<IF>`, `$<AND>`, or `$<OR>` skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedOperand {
    /// Step of the operator, when it was recorded.
    pub parent: Option<u32>,
    /// Zero-based parameter index in the operator.
    pub index: u32,
    /// Source text of the operand, cut at 256 bytes.
    pub source: BString,
    /// The operand holds only literal text.
    pub literal: bool,
}

fn host_operator(name: &[u8]) -> bool {
    name.starts_with(b"TARGET_") || name.ends_with(b"_PROPERTY") || name.ends_with(b"_EXISTS")
}

fn cut(mut value: BString, limit: usize) -> (BString, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while end > 0 && value.get(end).is_some_and(|byte| byte & 0xc0 == 0x80) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}
