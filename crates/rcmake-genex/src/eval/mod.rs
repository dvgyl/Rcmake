use std::{cell::Cell, cmp::Ordering};

use rcmake_base::{BString, Limits, list, path_lex::Lexical, truth, version};

use crate::{EvalContext, EvalFlags, GenexAst, GenexError, GenexExpr, GenexHost, GenexNode};

mod context;
mod lists;
mod paths;
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
    let limits = host.limits();
    let mut evaluator = Evaluator {
        host,
        flags: EvalFlags::default(),
        depth: 0,
        work: limits.max_value_bytes,
        regex_steps: limits.regex_steps,
        limits,
        bindings: Vec::new(),
    };
    let value = evaluator.nodes(&ast.nodes, ctx)?;
    Ok((value, evaluator.flags))
}

struct Evaluator<'a> {
    host: &'a mut dyn GenexHost,
    flags: EvalFlags,
    depth: u32,
    work: usize,
    regex_steps: u64,
    limits: Limits,
    bindings: Vec<Vec<BString>>,
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
        let parameter = if count == 1 {
            "parameter"
        } else {
            "parameters"
        };
        self.error(format!(
            "$<{name}> expression requires exactly {count} {parameter}."
        ))
    }

    fn params(self) -> usize {
        self.expr.params.as_deref().map_or(0, <[_]>::len)
    }
}

impl Evaluator<'_> {
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
            match node {
                GenexNode::Text(text) => self.append(&mut result, text)?,
                GenexNode::Expr(expr) => {
                    let value = self.expression(expr, ctx)?;
                    self.append(&mut result, &value)?;
                }
            }
        }
        Ok(result)
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
            b"IF" | b"AND" | b"OR" => self.lazy_boolean(call),
            b"BOOL" | b"NOT" => {
                let [value] = self.fixed(call)?;
                if name == "BOOL" {
                    Ok(boolean(!truth::is_off(&value)))
                } else {
                    Ok(boolean(!strict_bool(call, &value)?))
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
            b"ARGS" => {
                let value = self.content(call)?;
                if value.is_empty() {
                    return Ok(value);
                }
                let parts = list::split(&value, list::Empty::Keep);
                let mut out = BString::from("<ARGS>");
                self.append(&mut out, &parts.join(&b','))?;
                self.append(&mut out, b"</ARGS>")?;
                Ok(out)
            }
            b"_0" | b"_1" => {
                let [] = self.fixed(call)?;
                let index = usize::from(name == "_1");
                self.bindings
                    .last()
                    .and_then(|args| args.get(index))
                    .cloned()
                    .ok_or_else(|| {
                        call.error(format!(
                            "$<{name}> is only valid in a binding operation with operand {index}."
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
            | b"LINK_LIBRARY"
            | b"LINK_GROUP"
            | b"HOST_LINK"
            | b"DEVICE_LINK" => self.interface(call),
            _ if context::compiler_operator(&name) => self.compiler(call),
            b"SOURCE_PROPERTY" => self.source_property(call),
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
        if let Some(args) = value
            .strip_prefix(b"<ARGS>")
            .and_then(|v| v.strip_suffix(b"</ARGS>"))
        {
            return Ok(args
                .split(|&byte| byte == b',')
                .map(BString::from)
                .collect());
        }
        Ok(vec![value])
    }

    fn arguments(&mut self, call: Call<'_>) -> Result<Vec<BString>, GenexError> {
        let mut values = Vec::new();
        for nodes in call.expr.params.iter().flatten() {
            values.extend(self.expanded_parameter(nodes, call.ctx)?);
        }
        Ok(values)
    }

    fn fixed<const N: usize>(&mut self, call: Call<'_>) -> Result<[BString; N], GenexError> {
        if call.params() != N {
            return Err(match call.name {
                b"STREQUAL" | b"IF" => call.error(format!(
                    "$<{}> expression requires {N} comma separated parameters, but got {} instead.",
                    BString::from(call.name),
                    call.params()
                )),
                _ => call.arity(N),
            });
        }
        let mut values = self.arguments(call)?;
        values.truncate(N);
        values.try_into().map_err(|_| call.arity(N))
    }

    fn content(&mut self, call: Call<'_>) -> Value {
        if call.params() == 0 {
            return Err(call.arity(1));
        }
        let mut out = BString::default();
        for index in 0..call.params() {
            if index != 0 {
                self.append(&mut out, b",")?;
            }
            let value = self.parameter(call, index)?;
            self.append(&mut out, &value)?;
        }
        if let Some(args) = out
            .strip_prefix(b"<ARGS>")
            .and_then(|v| v.strip_suffix(b"</ARGS>"))
        {
            return Ok(args
                .split(|&byte| byte == b',')
                .next()
                .unwrap_or_default()
                .into());
        }
        Ok(out)
    }

    fn lazy_boolean(&mut self, call: Call<'_>) -> Value {
        if call.name == b"IF" {
            if call.params() != 3 {
                return Err(call.error(format!(
                    "$<IF> expression requires 3 comma separated parameters, but got {} instead.",
                    call.params()
                )));
            }
            let mut wanted = 0;
            let mut index = 0;
            for nodes in call.expr.params.iter().flatten() {
                if index != 0 && index < wanted {
                    index += 1;
                    continue;
                }
                for value in self.expanded_parameter(nodes, call.ctx)? {
                    if index == 0 {
                        wanted = if strict_bool(call, &value)? { 1 } else { 2 };
                    } else if index == wanted {
                        return Ok(value);
                    }
                    index += 1;
                }
            }
            return Err(call.arity(3));
        }

        let short = call.name == b"OR";
        if call.params() == 0 {
            return Err(call.error(format!(
                "$<{}> expression requires at least one parameter.",
                BString::from(call.name)
            )));
        }
        for nodes in call.expr.params.iter().flatten() {
            for value in self.expanded_parameter(nodes, call.ctx)? {
                if strict_bool(call, &value)? == short {
                    return Ok(boolean(short));
                }
            }
        }
        Ok(boolean(!short))
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

fn strict_bool(call: Call<'_>, value: &[u8]) -> Result<bool, GenexError> {
    match value {
        b"0" => Ok(false),
        b"1" => Ok(true),
        _ => Err(call.error(format!(
            "$<{}> parameter must resolve to exactly one '0' or '1' value.",
            BString::from(call.name)
        ))),
    }
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
