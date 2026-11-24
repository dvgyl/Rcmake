use std::{path::Path, sync::Arc};

use rcmake_base::{BString, Limits};
use rcmake_diag::{Diagnostic, PolicyId, PolicyStatus};
use rcmake_expr::{CondEnv, ExpandEnv, ExpandedArg, eval_condition, expand_string};
use rcmake_host::{Host, PureHost};
use rcmake_regex::{Captures, Regex, RegexError};
use rcmake_syntax::ArgKind;

use super::{Check, in_memory_host, mutate, pick, preview, random_bytes, require, root};
use crate::Rng;

pub(super) fn check(rng: &mut Rng, limits: &Limits) -> Check {
    let mode = rng.below(8);
    let depth = match mode {
        0 => *pick(rng, &[255, 256, 257, 258]),
        _ => 1 + rng.below(12) as usize,
    };
    let mut source = format!("{}K{}", "${".repeat(depth), "}".repeat(depth)).into_bytes();
    let mut tokens = condition(rng, depth);
    match mode {
        1 => {
            mutate(rng, &mut source, b"$ENV{\xff");
            tokens.pop();
        }
        2 => {
            source = random_bytes(rng);
            tokens.push(arg(random_bytes(rng), ArgKind::Quoted));
        }
        3 => source.extend_from_slice(br";$ENV{K};$CACHE{K};\;"),
        _ => {}
    }

    let mut env = Environment(in_memory_host());
    let value = expand_string(&source, &mut env);
    let result = eval_condition(&tokens, &mut env);
    if mode == 0 {
        require(value.is_ok() == (depth <= 256), || {
            format!("Unexpected variable expansion result at depth {depth}. {value:?}")
        })?;
        require(result.is_ok() == (depth <= 256), || {
            format!("Unexpected condition result at depth {depth}. {result:?}")
        })?;
    }
    if let Ok(value) = &value {
        require(value.len() <= limits.max_value_bytes, || {
            format!(
                "Expansion returned {} bytes. Input {}.",
                value.len(),
                preview(&source)
            )
        })?;
    }
    for message in [
        value.as_ref().err().map(|e| &e.message),
        result.as_ref().err().map(|e| &e.message),
    ]
    .into_iter()
    .flatten()
    {
        require(!message.is_empty(), || {
            "Expression error has no diagnostic.".into()
        })?;
    }
    Ok(value.is_ok() && result.is_ok())
}

fn condition(rng: &mut Rng, depth: usize) -> Vec<ExpandedArg> {
    let unquoted = |token: &[u8]| arg(token, ArgKind::Unquoted);
    let mut args = vec![unquoted(b"("); depth];
    args.extend(match rng.below(5) {
        0 => vec![unquoted(b"DEFINED"), unquoted(b"K")],
        1 => vec![
            unquoted(b"K"),
            unquoted(b"STREQUAL"),
            arg("K", ArgKind::Quoted),
        ],
        2 => vec![
            arg("ab", ArgKind::Quoted),
            unquoted(b"MATCHES"),
            arg("^(a|b)+$", ArgKind::Bracket),
        ],
        3 => vec![unquoted(b"EXISTS"), arg("missing", ArgKind::Quoted)],
        _ => vec![
            unquoted(b"ON"),
            unquoted(b"OR"),
            unquoted(b"NOT"),
            unquoted(b"OFF"),
        ],
    });
    args.extend(std::iter::repeat_n(unquoted(b")"), depth));
    args
}

fn arg(value: impl Into<BString>, kind: ArgKind) -> ExpandedArg {
    ExpandedArg {
        value: value.into(),
        kind,
        line: 1,
    }
}

struct Environment(PureHost);

impl ExpandEnv for Environment {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        (name == b"K").then(|| "K".into())
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.var(name)
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.var(name)
    }

    fn current_line(&self) -> u32 {
        1
    }
}

impl CondEnv for Environment {
    fn is_defined(&mut self, name: &[u8]) -> bool {
        self.var(name).is_some()
    }

    fn cache_defined(&mut self, name: &[u8]) -> bool {
        self.cache_var(name).is_some()
    }

    fn env_defined(&mut self, name: &[u8]) -> bool {
        self.env_var(name).is_some()
    }

    fn command_exists(&self, _: &[u8]) -> bool {
        false
    }
    fn policy_known(&self, _: &[u8]) -> bool {
        false
    }
    fn diagnostic_known(&self, _: &[u8]) -> bool {
        false
    }
    fn target_exists(&self, _: &[u8]) -> bool {
        false
    }
    fn test_exists(&self, _: &[u8]) -> bool {
        false
    }
    fn rule_exists(&self, _: &[u8]) -> bool {
        false
    }

    fn policy(&self, _: PolicyId) -> PolicyStatus {
        PolicyStatus::New
    }

    fn host(&self) -> &dyn Host {
        &self.0
    }

    fn base_dir(&self) -> &Path {
        root()
    }

    fn regex(&mut self, pattern: &[u8]) -> Result<Arc<Regex>, RegexError> {
        Regex::new(pattern).map(Arc::new)
    }

    fn set_match_vars(&mut self, _: Option<(&Captures, &[u8])>) {}
    fn warn(&mut self, _: Diagnostic) {}
}
