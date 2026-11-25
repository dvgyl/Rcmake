use std::{cell::Cell, cmp::Ordering};

use rcmake_base::{BString, Limits, list, path_lex::Lexical, truth, version};

use crate::{EvalContext, EvalFlags, GenexAst, GenexError, GenexExpr, GenexHost, GenexNode};

mod context;
mod links;
mod lists;
mod operations;
mod paths;
mod properties;
mod rules;
mod strings;
mod targets;

#[cfg(test)]
mod tests;

type Value = Result<BString, GenexError>;

/// Evaluate `ast` in `ctx`. Return the value and its context dependencies.
pub fn evaluate(
    ast: &GenexAst,
    ctx: &EvalContext,
    host: &mut dyn GenexHost,
) -> Result<(BString, EvalFlags), GenexError> {
    let _root = targets::DAGChecker::root(ctx);
    let mut evaluator = Evaluator::new(host);
    let value = evaluator.nodes(&ast.nodes, ctx)?;
    evaluator.validate_link_feature_output(ctx, &value)?;
    Ok((value, evaluator.flags))
}

/// Evaluate independent top-level expressions and collect their diagnostics.
///
/// An error in one expression does not evaluate any skipped branches of that
/// expression. All expressions share the same evaluation limits.
///
/// # Errors
///
/// Returns the diagnostics in source order when evaluation fails.
pub fn evaluate_diagnostics(
    ast: &GenexAst,
    ctx: &EvalContext,
    host: &mut dyn GenexHost,
) -> Result<(BString, EvalFlags), Vec<GenexError>> {
    let _root = targets::DAGChecker::root(ctx);
    let mut evaluator = Evaluator::new(host);
    let mut output = BString::default();
    let mut errors = Vec::new();
    for node in &ast.nodes {
        if let Err(error) = evaluator.append_node(&mut output, node, ctx) {
            let can_continue = match &error {
                GenexError::TooDeep => false,
                GenexError::Message { expression, .. } => !expression.is_empty(),
                _ => true,
            };
            errors.push(error);
            if !can_continue {
                return Err(errors);
            }
            if let Err(error) = evaluator.charge(1) {
                errors.push(error);
                return Err(errors);
            }
        }
    }
    if errors.is_empty() {
        evaluator
            .validate_link_feature_output(ctx, &output)
            .map_err(|error| vec![error])?;
        Ok((output, evaluator.flags))
    } else {
        Err(errors)
    }
}

struct Evaluator<'a> {
    host: &'a mut dyn GenexHost,
    flags: EvalFlags,
    depth: u32,
    work: usize,
    regex_steps: u64,
    limits: Limits,
    bindings: Vec<Vec<BString>>,
    in_target_expression: bool,
    emitted_link_feature: bool,
    active_rules: Vec<(BString, BString)>,
}

thread_local! {
    static EVALUATION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct EvaluationDepth;

impl EvaluationDepth {
    fn enter(limit: u32) -> Result<Self, GenexError> {
        EVALUATION_DEPTH.with(|depth| {
            // Apply the same stack limit when a host callback calls evaluate again.
            let current = depth.get();
            if current >= limit.min(128) {
                return Err(GenexError::TooDeep);
            }
            depth.set(current + 1);
            Ok(Self)
        })
    }
}

impl Drop for EvaluationDepth {
    fn drop(&mut self) {
        EVALUATION_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

#[derive(Clone, Copy)]
struct Call<'a> {
    name: &'a [u8],
    expr: &'a GenexExpr,
    ctx: &'a EvalContext,
}

impl Call<'_> {
    fn error(self, message: impl Into<BString>) -> GenexError {
        GenexError::Message {
            expression: crate::parse::render_expr(self.expr),
            message: message.into(),
        }
    }

    fn arity(self, count: usize) -> GenexError {
        let name = BString::from(self.name);
        self.error(match count {
            0 => format!("$<{name}> expression requires no parameters."),
            1 => format!("$<{name}> expression requires exactly one parameter."),
            _ => format!(
                "$<{name}> expression requires {count} comma separated parameters, but got {} instead.",
                self.params()
            ),
        })
    }

    fn params(self) -> usize {
        self.expr.params.as_deref().map_or(0, <[_]>::len)
    }
}

impl<'a> Evaluator<'a> {
    fn new(host: &'a mut dyn GenexHost) -> Self {
        let limits = host.limits();
        Self {
            host,
            flags: EvalFlags::default(),
            depth: 0,
            work: limits.max_value_bytes,
            regex_steps: limits.regex_steps,
            limits,
            bindings: Vec::new(),
            in_target_expression: false,
            emitted_link_feature: false,
            active_rules: Vec::new(),
        }
    }

    fn limit_error(message: &'static str) -> GenexError {
        GenexError::Message {
            expression: BString::default(),
            message: message.into(),
        }
    }

    fn append(&self, output: &mut BString, value: &[u8]) -> Result<(), GenexError> {
        if value.len() > self.limits.max_value_bytes.saturating_sub(output.len()) {
            return Err(Self::limit_error(
                "Generator expression exceeds the value size limit.",
            ));
        }
        output.extend_from_slice(value);
        Ok(())
    }

    fn charge(&mut self, amount: usize) -> Result<(), GenexError> {
        self.work = self.work.checked_sub(amount).ok_or_else(|| {
            Self::limit_error("Generator expression exceeds the expansion limit.")
        })?;
        Ok(())
    }

    fn nodes(&mut self, nodes: &[GenexNode], ctx: &EvalContext) -> Value {
        let mut result = BString::default();
        for node in nodes {
            self.append_node(&mut result, node, ctx)?;
        }
        Ok(result)
    }

    fn append_node(
        &mut self,
        output: &mut BString,
        node: &GenexNode,
        ctx: &EvalContext,
    ) -> Result<(), GenexError> {
        match node {
            GenexNode::Text(text) => self.append(output, text),
            GenexNode::Expr(expr) => {
                let value = self.expression(expr, ctx)?;
                self.append(output, &value)
            }
        }
    }

    fn expression(&mut self, expr: &GenexExpr, ctx: &EvalContext) -> Value {
        let _depth = EvaluationDepth::enter(self.limits.max_recursion)?;
        self.depth += 1;
        let result = self.dispatch(expr, ctx);
        self.depth -= 1;
        let value = result?;
        self.charge(value.len().max(1))?;
        Ok(value)
    }

    fn dispatch(&mut self, expr: &GenexExpr, ctx: &EvalContext) -> Value {
        let name = self.nodes(&expr.identifier, ctx)?;
        let call = Call {
            name: &name,
            expr,
            ctx,
        };
        match name.as_slice() {
            b"0" | b"1" => {
                if call.params() == 0 {
                    return Err(if name == "0" {
                        call.error("$<0> expression requires a parameter.")
                    } else {
                        call.arity(1)
                    });
                }
                if name == "1" {
                    self.content(call)
                } else {
                    Ok(BString::default())
                }
            }
            b"IF" => self.conditional(call),
            b"AND" | b"OR" => self.boolean_operation(call),
            b"BOOL" | b"NOT" => {
                let [value] = self.fixed(call)?;
                if name == "BOOL" {
                    Ok(boolean(!truth::is_off(&value)))
                } else {
                    match value.as_slice() {
                        b"0" => Ok(boolean(true)),
                        b"1" => Ok(boolean(false)),
                        _ => Err(call.error(
                            "$<NOT> parameter must resolve to exactly one '0' or '1' value.",
                        )),
                    }
                }
            }
            b"EQUAL"
            | b"STREQUAL"
            | b"STRLESS"
            | b"STRGREATER"
            | b"STRLESS_EQUAL"
            | b"STRGREATER_EQUAL"
            | b"VERSION_LESS"
            | b"VERSION_GREATER"
            | b"VERSION_EQUAL"
            | b"VERSION_LESS_EQUAL"
            | b"VERSION_GREATER_EQUAL"
            | b"PATH_EQUAL" => self.compare(call),
            b"ANGLE-R" | b"COMMA" | b"SEMICOLON" | b"QUOTE" => {
                let [] = self.fixed(call)?;
                Ok(match name.as_slice() {
                    b"ANGLE-R" => ">",
                    b"COMMA" => ",",
                    b"SEMICOLON" => ";",
                    _ => "\"",
                }
                .into())
            }
            b"LOWER_CASE" | b"UPPER_CASE" | b"MAKE_C_IDENTIFIER" => {
                let value = self.content(call)?;
                Ok(strings::convert(&name, &value))
            }
            b"STRING" => self.string(call),
            b"LIST" | b"IN_LIST" | b"JOIN" | b"REMOVE_DUPLICATES" | b"FILTER" => self.list(call),
            b"PATH" | b"SHELL_PATH" => self.path(call),
            b"ARGS" => self.argument_list(call),
            b"_0" | b"_1" => {
                let [] = self.fixed(call)?;
                let index = usize::from(name == "_1");
                let Some(binding) = self.bindings.last().filter(|binding| !binding.is_empty())
                else {
                    return Err(call.error(format!(
                        "$<{name}> may only be used inside the body of a binding operation."
                    )));
                };
                binding.get(index).cloned().ok_or_else(|| {
                    let count = binding.len();
                    call.error(format!(
                        "$<{name}> is out of range for the current binding operation, which binds only {count} operand(s) (maximum $<_{}>).",
                        count - 1
                    ))
                })
            }
            b"GENEX_EVAL" | b"TARGET_GENEX_EVAL" => self.reevaluate(call),
            b"CONFIG"
            | b"CONFIGURATION"
            | b"PLATFORM_ID"
            | b"OUTPUT_CONFIG"
            | b"COMMAND_CONFIG"
            | b"COMPILE_LANGUAGE"
            | b"LINK_LANGUAGE"
            | b"COMPILE_LANG_AND_ID"
            | b"LINK_LANG_AND_ID"
            | b"COMPILE_FEATURES" => self.context_value(call),
            b"BUILD_INTERFACE"
            | b"BUILD_LOCAL_INTERFACE"
            | b"INSTALL_INTERFACE"
            | b"INSTALL_PREFIX"
            | b"LINK_ONLY"
            | b"COMPILE_ONLY"
            | b"HOST_LINK"
            | b"DEVICE_LINK" => self.interface(call),
            b"LINK_LIBRARY" | b"LINK_GROUP" => self.link_feature(call),
            _ if context::compiler_operator(&name) => self.compiler(call),
            b"SOURCE_PROPERTY" => self.source_property(call),
            b"RULE_PROPERTY" => self.rule_property(call),
            b"SOURCE_EXISTS"
            | b"FILE_SET_EXISTS"
            | b"FILE_SET_PROPERTY"
            | b"DIRECTORY_PROPERTY"
            | b"GLOBAL_PROPERTY" => self.scoped_property(call),
            _ if name.starts_with(b"TARGET_") => self.target(call),
            _ => Err(unknown(call)),
        }
    }

    fn parameter(&mut self, call: Call<'_>, index: usize) -> Value {
        let Some(nodes) = call
            .expr
            .params
            .as_ref()
            .and_then(|params| params.get(index))
        else {
            return Err(call.arity(index + 1));
        };
        self.nodes(nodes, call.ctx)
    }

    fn expanded_parameter(
        &mut self,
        nodes: &[GenexNode],
        ctx: &EvalContext,
    ) -> Result<Vec<BString>, GenexError> {
        let value = self.nodes(nodes, ctx)?;
        self.charge(value.len().max(1))?;
        self.expand_argument(value)
    }

    fn expand_argument(&mut self, value: BString) -> Result<Vec<BString>, GenexError> {
        if let Some(args) = value
            .strip_prefix(b"<ARGS>")
            .and_then(|v| v.strip_suffix(b"</ARGS>"))
        {
            let parts = args.split(|&byte| byte == b',');
            self.charge(
                parts
                    .clone()
                    .count()
                    .saturating_mul(std::mem::size_of::<BString>()),
            )?;
            return Ok(parts.map(BString::from).collect());
        }
        Ok(vec![value])
    }

    fn arguments(&mut self, call: Call<'_>) -> Result<Vec<BString>, GenexError> {
        let mut values = Vec::new();
        for nodes in call.expr.params.iter().flatten() {
            values.extend(self.expanded_parameter(nodes, call.ctx)?);
        }
        // Check the property read before it can enter the usage resolver.
        if call.name == b"TARGET_PROPERTY" {
            self.validate_target_property_arguments(call, &values)?;
        }
        Ok(values)
    }

    fn fixed<const N: usize>(&mut self, call: Call<'_>) -> Result<[BString; N], GenexError> {
        let mut values = self.arguments(call)?;
        if call.params() != N {
            return Err(call.arity(N));
        }
        values.truncate(N);
        values.try_into().map_err(|_| call.arity(N))
    }

    fn content(&mut self, call: Call<'_>) -> Value {
        self.arbitrary_content(call, 0)?
            .into_iter()
            .next()
            .ok_or_else(|| call.arity(1))
    }

    fn arbitrary_content(
        &mut self,
        call: Call<'_>,
        start: usize,
    ) -> Result<Vec<BString>, GenexError> {
        if call.params() <= start {
            return Err(call.arity(start + 1));
        }
        let mut out = BString::default();
        for index in start..call.params() {
            if index != start {
                self.append(&mut out, b",")?;
            }
            let value = self.parameter(call, index)?;
            self.append(&mut out, &value)?;
        }
        self.expand_argument(out)
    }

    fn argument_list(&mut self, call: Call<'_>) -> Value {
        let values = self.arguments(call)?;
        if values.is_empty() || matches!(values.as_slice(), [value] if value.is_empty()) {
            return Ok(BString::default());
        }
        let mut out = BString::from("<ARGS>");
        let mut first = true;
        for value in &values {
            let parts = list::iter(value, list::Empty::Keep);
            let empty = value
                .is_empty()
                .then_some(std::borrow::Cow::Borrowed(b"".as_slice()));
            for part in parts.chain(empty) {
                if !first {
                    self.append(&mut out, b",")?;
                }
                first = false;
                self.append(&mut out, &part)?;
            }
        }
        self.append(&mut out, b"</ARGS>")?;
        Ok(out)
    }

    fn conditional(&mut self, call: Call<'_>) -> Value {
        // Select branches before expanding argument lists.
        let mut raw = Vec::<BString>::new();
        for (index, nodes) in call.expr.params.iter().flatten().enumerate() {
            let skip = index.checked_sub(1).is_some_and(|branch| {
                raw.first()
                    .is_some_and(|condition| condition.as_slice() == branch.to_string().as_bytes())
            });
            let value = if skip {
                BString::default()
            } else {
                self.nodes(nodes, call.ctx)?
            };
            self.charge(value.len().max(1))?;
            raw.push(value);
        }
        if raw.len() != 3 {
            return Err(call.arity(3));
        }
        let mut values = Vec::new();
        for value in raw {
            values.extend(self.expand_argument(value)?);
        }
        let mut values = values.into_iter();
        let branch = match values.next().as_ref().map(|value| value.as_slice()) {
            Some(b"1") => 0,
            Some(b"0") => 1,
            _ => {
                return Err(call.error(
                    "First parameter to $<IF> must resolve to exactly one '0' or '1' value.",
                ));
            }
        };
        values.nth(branch).ok_or_else(|| call.arity(3))
    }

    fn boolean_operation(&mut self, call: Call<'_>) -> Value {
        let decisive = boolean(call.name == b"OR");
        if call.params() == 0 {
            return Err(call.error(format!(
                "$<{}> expression requires at least one parameter.",
                BString::from(call.name)
            )));
        }
        let mut raw = Vec::new();
        for nodes in call.expr.params.iter().flatten() {
            let value = self.nodes(nodes, call.ctx)?;
            self.charge(value.len().max(1))?;
            let done = value == decisive;
            raw.push(value);
            if done {
                break;
            }
        }
        for parameter in raw {
            for value in self.expand_argument(parameter)? {
                if value == decisive {
                    return Ok(decisive);
                }
                if !matches!(value.as_slice(), b"0" | b"1") {
                    return Err(call.error(format!(
                        "Parameters to $<{}> must resolve to either '0' or '1'.",
                        BString::from(call.name)
                    )));
                }
            }
        }
        Ok(boolean(call.name == b"AND"))
    }

    fn compare(&mut self, call: Call<'_>) -> Value {
        let [left, right] = self.fixed(call)?;
        let order = match call.name {
            b"EQUAL" => {
                let integer = |value: &BString| {
                    integer(value).ok_or_else(|| {
                        call.error(format!(
                            "$<EQUAL> parameter {value} is not a valid integer."
                        ))
                    })
                };
                integer(&left)?.cmp(&integer(&right)?)
            }
            b"PATH_EQUAL" => return Ok(boolean(Lexical::new(&left).equal(Lexical::new(&right)))),
            name if name.starts_with(b"VERSION_") => version::compare(&left, &right),
            _ => left.cmp(&right),
        };
        Ok(boolean(match call.name {
            b"STRLESS" | b"VERSION_LESS" => order.is_lt(),
            b"STRGREATER" | b"VERSION_GREATER" => order.is_gt(),
            b"STRLESS_EQUAL" | b"VERSION_LESS_EQUAL" => order.is_le(),
            b"STRGREATER_EQUAL" | b"VERSION_GREATER_EQUAL" => order.is_ge(),
            _ => order == Ordering::Equal,
        }))
    }

    fn bound(&mut self, nodes: &[GenexNode], ctx: &EvalContext, values: Vec<BString>) -> Value {
        self.bindings.push(values);
        let result = self.nodes(nodes, ctx);
        self.bindings.pop();
        result
    }
}

fn boolean(value: bool) -> BString {
    if value { "1" } else { "0" }.into()
}

fn integer(value: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(value)
        .ok()?
        .trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (sign, digits) = if let Some(rest) = text.strip_prefix('-') {
        ("-", rest)
    } else {
        ("", text.strip_prefix('+').unwrap_or(text))
    };
    let (radix, digits) = if let Some(rest) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (16, rest)
    } else if digits.starts_with('0') {
        (8, digits)
    } else {
        (10, digits)
    };
    if digits.starts_with(['+', '-']) {
        return None;
    }
    i64::from_str_radix(&format!("{sign}{digits}"), radix).ok()
}

fn unknown(call: Call<'_>) -> GenexError {
    call.error("Expression did not evaluate to a known generator expression")
}
