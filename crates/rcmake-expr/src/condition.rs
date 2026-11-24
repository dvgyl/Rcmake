use core::cmp::Ordering;
use std::path::PathBuf;

use rcmake_base::{
    BString,
    limits::Limits,
    list::{Empty, EmptyInput, split_with_empty_input},
    truth::{if_constant, is_off},
    version,
};
use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus,
};
use rcmake_host::{FileKind, Follow, path::to_native};
use rcmake_regex::MatchOptions;

use crate::{CondEnv, CondError, ExpandedArg, number};

const MAX_DEPTH: usize = 256;

/// Evaluate an expanded `if()` or `while()` condition.
pub fn eval_condition(args: &[ExpandedArg], env: &mut dyn CondEnv) -> Result<bool, CondError> {
    let mut args = args.iter();
    group(&mut args, env, 0)
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Arg(&'a ExpandedArg),
    Bool(bool),
}

impl Token<'_> {
    fn keyword(&self) -> &[u8] {
        match self {
            Self::Arg(arg) if !arg.is_quoted_or_bracket() => &arg.value,
            _ => b"",
        }
    }

    fn literal(&self) -> &[u8] {
        match self {
            Self::Arg(arg) => &arg.value,
            Self::Bool(true) => b"1",
            Self::Bool(false) => b"0",
        }
    }

    fn variable(&self, env: &mut dyn CondEnv) -> Option<BString> {
        match self {
            Self::Arg(arg) if !arg.is_quoted_or_bracket() => env.var(&arg.value),
            _ => None,
        }
    }

    fn value(&self, env: &mut dyn CondEnv) -> BString {
        self.variable(env).unwrap_or_else(|| self.literal().into())
    }

    fn truth(&self, env: &mut dyn CondEnv) -> bool {
        if_constant(self.literal())
            .or_else(|| number::whole(self.literal()).map(|value| value != 0.0))
            .unwrap_or_else(|| self.variable(env).is_some_and(|value| !is_off(&value)))
    }
}

fn group<'a>(
    args: &mut impl Iterator<Item = &'a ExpandedArg>,
    env: &mut dyn CondEnv,
    depth: usize,
) -> Result<bool, CondError> {
    if depth > MAX_DEPTH {
        return Err(error("Condition exceeded the nesting limit"));
    }
    let mut tokens = Vec::new();
    while let Some(arg) = args.next() {
        let token = Token::Arg(arg);
        match token.keyword() {
            b"(" => tokens.push(Token::Bool(group(args, env, depth + 1)?)),
            b")" if depth != 0 => return reduce(tokens, env),
            b")" => return Err(error("mismatched parenthesis in condition")),
            _ => tokens.push(token),
        }
    }
    if depth != 0 {
        return Err(error("mismatched parenthesis in condition"));
    }
    reduce(tokens, env)
}

fn reduce(mut tokens: Vec<Token<'_>>, env: &mut dyn CondEnv) -> Result<bool, CondError> {
    // Each pass consumes operands from left to right. A binary result can be
    // the left operand of the next operator in the same pass.
    tokens = prefix_pass(tokens, |op, operand| unary(op, operand.literal(), env));
    tokens = binary_pass(tokens, env)?;
    tokens = prefix_pass(tokens, |op, operand| {
        (op == b"NOT").then(|| !operand.truth(env))
    });

    while tokens.len() > 1 {
        let before = tokens.len();
        let mut input = tokens.into_iter();
        tokens = Vec::with_capacity(before);
        while let Some(left) = input.next() {
            let result = match input.as_slice() {
                [op, right, ..] if op.keyword() == b"AND" => left.truth(env) & right.truth(env),
                [op, right, ..] if op.keyword() == b"OR" => left.truth(env) | right.truth(env),
                _ => {
                    tokens.push(left);
                    continue;
                }
            };
            input.next();
            input.next();
            tokens.push(Token::Bool(result));
        }
        if tokens.len() == before {
            return Err(error("Unknown arguments specified"));
        }
    }
    Ok(tokens.first().is_some_and(|token| token.truth(env)))
}

fn prefix_pass<'a>(
    tokens: Vec<Token<'a>>,
    mut evaluate: impl FnMut(&[u8], Token<'a>) -> Option<bool>,
) -> Vec<Token<'a>> {
    let mut input = tokens.into_iter().peekable();
    let mut output = Vec::new();
    while let Some(token) = input.next() {
        if let Some(&operand) = input.peek()
            && let Some(result) = evaluate(token.keyword(), operand)
        {
            input.next();
            output.push(Token::Bool(result));
        } else {
            output.push(token);
        }
    }
    output
}

fn binary_pass<'a>(
    tokens: Vec<Token<'a>>,
    env: &mut dyn CondEnv,
) -> Result<Vec<Token<'a>>, CondError> {
    let mut input = tokens.into_iter();
    let mut output = Vec::new();
    // An empty unquoted left operand disappears during argument expansion.
    let mut left = if let [first, _, ..] = input.as_slice()
        && first.keyword() == b"MATCHES"
    {
        input.next();
        input.next();
        Some(Token::Bool(false))
    } else {
        input.next()
    };
    while let Some(operand) = left {
        if let [operator, right, ..] = input.as_slice()
            && let Some(result) = binary(operand, operator.keyword(), *right, env)?
        {
            input.next();
            input.next();
            left = Some(Token::Bool(result));
        } else {
            output.push(operand);
            left = input.next();
        }
    }
    Ok(output)
}

fn unary(op: &[u8], operand: &[u8], env: &mut dyn CondEnv) -> Option<bool> {
    Some(match op {
        b"DEFINED" => {
            if let Some(name) = defined_name(operand, b"CACHE{") {
                env.cache_defined(name)
            } else if let Some(name) = defined_name(operand, b"ENV{") {
                env.env_defined(name)
            } else {
                env.is_defined(operand)
            }
        }
        b"COMMAND" => env.command_exists(operand),
        b"POLICY" => env.policy_known(operand),
        b"DIAGNOSTIC" => env.diagnostic_known(operand),
        b"TARGET" => env.target_exists(operand),
        b"TEST" => env.test_exists(operand),
        b"RULE" => env.rule_exists(operand),
        b"IS_ABSOLUTE" => operand.starts_with(b"~") || to_native(operand).has_root(),
        b"EXISTS" | b"IS_READABLE" | b"IS_WRITABLE" | b"IS_EXECUTABLE" | b"IS_DIRECTORY"
        | b"IS_SYMLINK" => file_predicate(op, operand, env),
        _ => return None,
    })
}

fn defined_name<'a>(operand: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    let tail = operand.strip_prefix(prefix)?;
    tail.get(..tail.iter().rposition(|&byte| byte == b'}')?)
}

fn file_predicate(op: &[u8], operand: &[u8], env: &dyn CondEnv) -> bool {
    if operand.is_empty() {
        return false;
    }
    let follow = if op == b"IS_SYMLINK" {
        Follow::No
    } else {
        Follow::Yes
    };
    let Ok(metadata) = env.host().metadata(&resolve_path(operand, env), follow) else {
        return false;
    };
    match op {
        b"EXISTS" | b"IS_READABLE" => metadata.mode & 0o444 != 0,
        b"IS_WRITABLE" => metadata.mode & 0o222 != 0,
        b"IS_EXECUTABLE" => metadata.mode & 0o111 != 0,
        b"IS_DIRECTORY" => metadata.kind == FileKind::Dir,
        b"IS_SYMLINK" => metadata.kind == FileKind::Symlink,
        _ => true,
    }
}

fn resolve_path(operand: &[u8], env: &dyn CondEnv) -> PathBuf {
    env.base_dir().join(to_native(operand))
}

enum Relation {
    Less,
    Greater,
    Equal,
    LessEqual,
    GreaterEqual,
}

impl Relation {
    fn parse(op: &[u8]) -> Option<Self> {
        Some(match op {
            b"LESS" => Self::Less,
            b"GREATER" => Self::Greater,
            b"EQUAL" => Self::Equal,
            b"LESS_EQUAL" => Self::LessEqual,
            b"GREATER_EQUAL" => Self::GreaterEqual,
            _ => return None,
        })
    }

    fn test(self, order: Ordering) -> bool {
        match self {
            Self::Less => order.is_lt(),
            Self::Greater => order.is_gt(),
            Self::Equal => order.is_eq(),
            Self::LessEqual => order.is_le(),
            Self::GreaterEqual => order.is_ge(),
        }
    }
}

fn binary(
    left: Token<'_>,
    operator: &[u8],
    right: Token<'_>,
    env: &mut dyn CondEnv,
) -> Result<Option<bool>, CondError> {
    if let Some(suffix) = operator.strip_prefix(b"STR")
        && let Some(relation) = Relation::parse(suffix)
    {
        return Ok(Some(relation.test(left.value(env).cmp(&right.value(env)))));
    }
    if let Some(suffix) = operator.strip_prefix(b"VERSION_")
        && let Some(relation) = Relation::parse(suffix)
    {
        return Ok(Some(
            relation.test(version::compare(&left.value(env), &right.value(env))),
        ));
    }
    if let Some(relation) = Relation::parse(operator) {
        let left = number::prefix(&left.value(env));
        let right = number::prefix(&right.value(env));
        let order = left.zip(right).and_then(|(a, b)| a.partial_cmp(&b));
        return Ok(Some(order.is_some_and(|order| relation.test(order))));
    }
    Ok(match operator {
        b"MATCHES" => Some(regex_matches(&left.value(env), right.literal(), env)?),
        b"IN_LIST" => Some(in_list(left, right.literal(), env)),
        b"IS_NEWER_THAN" => Some(is_newer(left.literal(), right.literal(), env)),
        b"PATH_EQUAL" if path_operator(139, operator, env) => {
            Some(path_equal(&left.value(env), &right.value(env)))
        }
        b"PATH_IS_PREFIX" if path_operator(222, operator, env) => {
            Some(path_prefix(&left.value(env), &right.value(env), env))
        }
        _ => None,
    })
}

fn in_list(left: Token<'_>, name: &[u8], env: &mut dyn CondEnv) -> bool {
    let value = left.value(env);
    let Some(list) = env.var(name) else {
        return false;
    };
    let mut elements = Vec::new();
    split_with_empty_input(
        &list,
        Empty::Keep,
        EmptyInput::OneEmptyElement,
        &mut elements,
    );
    elements.contains(&value)
}

fn is_newer(left: &[u8], right: &[u8], env: &dyn CondEnv) -> bool {
    if left.is_empty() || right.is_empty() {
        return true;
    }
    let left = env.host().metadata(&resolve_path(left, env), Follow::Yes);
    let right = env.host().metadata(&resolve_path(right, env), Follow::Yes);
    match (left, right) {
        (Ok(left), Ok(right)) => left.mtime >= right.mtime,
        _ => true,
    }
}

fn regex_matches(
    haystack: &[u8],
    pattern: &[u8],
    env: &mut dyn CondEnv,
) -> Result<bool, CondError> {
    env.set_match_vars(None);
    let regex = env.regex(pattern).map_err(|_| {
        let mut message = b"Regular expression \"".to_vec();
        message.extend_from_slice(pattern);
        message.extend_from_slice(b"\" cannot compile");
        error(message)
    })?;
    let captures = regex
        .find_at(
            haystack,
            0,
            MatchOptions::default(),
            &mut Limits::default().regex_steps,
        )
        .map_err(|limit| error(limit.to_string()))?;
    if let Some(captures) = captures {
        env.set_match_vars(Some((&captures, haystack)));
        Ok(true)
    } else {
        Ok(false)
    }
}

fn path_operator(number: u16, name: &[u8], env: &mut dyn CondEnv) -> bool {
    let id = PolicyId::new(number);
    match env.policy(id) {
        PolicyStatus::New => true,
        PolicyStatus::Old => false,
        PolicyStatus::Warn => {
            let note = format!(
                "{} will be interpreted as an operator when the policy is set to NEW.  Since the policy is not set the OLD behavior will be used.",
                String::from_utf8_lossy(name)
            );
            warn_policy(id, &note, env);
            false
        }
    }
}

fn warn_policy(id: PolicyId, note: &str, env: &mut dyn CondEnv) {
    let summary = match id.number() {
        139 => "The if() command supports path comparisons using PATH_EQUAL operator.",
        222 => "The if() command supports path prefix tests using PATH_IS_PREFIX operator.",
        223 => "An empty path is not a prefix of any path.",
        _ => return,
    };
    let mut body = format!(
        "Policy {id} is not set: {summary}  Run \"cmake --help-policy {id}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.",
    );
    if !note.is_empty() {
        body.push_str("\n\n");
        body.push_str(note);
    }
    env.warn(Diagnostic {
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(id),
        location: Location::None,
        body: body.into(),
    });
}

fn is_separator(byte: u8) -> bool {
    byte == b'/' || (cfg!(windows) && byte == b'\\')
}

fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&byte| is_separator(byte))
        .filter(|part| !part.is_empty())
}

fn rooted(path: &[u8]) -> bool {
    path.first().copied().is_some_and(is_separator)
}

fn has_trailing_separator(path: &[u8]) -> bool {
    path.last().copied().is_some_and(is_separator)
}

fn path_equal(left: &[u8], right: &[u8]) -> bool {
    rooted(left) == rooted(right)
        && has_trailing_separator(left) == has_trailing_separator(right)
        && components(left).eq(components(right))
}

fn path_prefix(prefix: &[u8], path: &[u8], env: &mut dyn CondEnv) -> bool {
    if prefix.is_empty() {
        let id = PolicyId::new(223);
        return match env.policy(id) {
            PolicyStatus::New => false,
            PolicyStatus::Old => true,
            PolicyStatus::Warn => {
                warn_policy(id, "", env);
                true
            }
        };
    }
    if rooted(prefix) != rooted(path) {
        return false;
    }
    let mut path_parts = components(path);
    for part in components(prefix) {
        if path_parts.next() != Some(part) {
            return false;
        }
    }
    path_parts.next().is_some() || !has_trailing_separator(prefix) || has_trailing_separator(path)
}

fn error(message: impl Into<BString>) -> CondError {
    CondError {
        message: message.into(),
    }
}
